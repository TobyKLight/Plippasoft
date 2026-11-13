//! Continuous UI frame loop: inputs → model → enqueue present; NFC; IPC.
//!
//! Used by the binary and by unit tests (scripted inputs + max iterations).
//!
//! On poc2, inputs/NFC arrive via channels from the I²C service, and display/LED
//! are [`DisplayProxy`] / [`LedProxy`] — the coordinator never touches i2c-1,
//! waits on SPI present, or drives WS2811 DMA (see `docs/design/ui-threading.md`).

use crate::app::{TransportLedState, UiApp, UiOutgoing};
use crate::display::DisplayBackend;
use crate::i2c_service::{NfcChannelClient, NfcServiceEvent};
use crate::input::{InputEvent, InputSource};
use crate::led::{
    LedCue, PlayButtonLed, BACK_LED_GLOW, NAV_LED_FLASH, NAV_LED_ON, PLAYMODE_LED_ACTIVE,
    POWER_LED_WARN, REC_LED_GLOW, TRANSPORT_LED_IDLE, TRANSPORT_LED_PRESSED,
};
use plippa_core::PlayModeState;
use plippa_board::{ControlRole, LedRole};
use crate::record_leds::{RecordFeed, RecordUpdate};
use crate::library::LibrarySource;
use crate::sys::SysProviders;
use crate::wifi::WifiBackend;
use plippa_hw::{
    FakeClock, MockNfc, MockTag, NfcDevice, NfcError, NfcEvent, NfcSession, PollClock,
    MIN_POLL_INTERVAL, TAG_HAS_LINK_MSG, TAG_READ_ONLY_MSG,
};
use plippa_ipc_proto::{
    resolve_paths, AssociateResponse, IpcConnection, IpcMessage,
};
use plippa_noubin::{
    encode_text_record, extract_noubin_key, normalise_noubin_key, parse_noudata, LibraryRoot,
    LibraryWriter, DEFAULT_TEXT_LANG,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::path::Path;
use std::time::{Duration, Instant};

/// Why the loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopExit {
    /// Hit `max_iters`.
    MaxIters,
    /// Hit wall-clock deadline.
    Deadline,
    /// App emitted [`UiOutgoing::ShutdownRequest`] with `power_off: false` (stop only).
    Shutdown,
    /// App emitted [`UiOutgoing::ShutdownRequest`] with `power_off: true` (hold-to-power).
    PowerOff,
    /// SIGINT / stop flag — exit quietly (no shutdown sentinel).
    Interrupted,
    /// Caller / env one-shot.
    Once,
}

/// Limits for test harnesses and smoke runs.
#[derive(Debug, Clone, Default)]
pub struct LoopLimits {
    pub max_iters: Option<u64>,
    pub deadline: Option<Instant>,
    /// Sleep between iterations (production ~10 ms; tests often 0).
    pub sleep: Duration,
    /// When set and true, exit so Drop can clear LEDs (SIGINT / ctrl-C).
    pub stop_flag: Option<Arc<AtomicBool>>,
}

/// Optional NFC session owned by the loop (mock or channel to I²C service).
pub enum NfcBackend {
    /// Deterministic mock with injectable clock (tests / desktop).
    Mock {
        session: NfcSession<MockNfc, FakeClock>,
    },
    /// Hardware path: commands/events via the I²C service (no blocking I²C here).
    Channel(NfcChannelClient),
}

impl NfcBackend {
    pub fn mock_empty() -> Self {
        Self::Mock {
            session: NfcSession::new(MockNfc::new(), FakeClock { now: Duration::ZERO }),
        }
    }

    pub fn mock_with_tag(tag: MockTag) -> Self {
        let mut nfc = MockNfc::new();
        nfc.inject(tag);
        Self::Mock {
            session: NfcSession::new(nfc, FakeClock { now: Duration::ZERO }),
        }
    }

    pub fn from_channel(client: NfcChannelClient) -> Self {
        Self::Channel(client)
    }

    fn advance_mock_clock(&mut self, dt: Duration) {
        if let Self::Mock { session } = self {
            let now = session.scheduler().clock().now() + dt;
            session.scheduler_mut().clock_mut().now = now;
        }
    }

    fn worker_gone(&self) -> bool {
        match self {
            Self::Mock { .. } => false,
            Self::Channel(c) => c.worker_gone(),
        }
    }

    fn mark_field_tag_as_loaded(&mut self) -> Result<(), String> {
        match self {
            Self::Mock { session } => {
                if let Some(info) = session.device_mut().poll().map_err(|e| e.to_string())? {
                    session.set_loaded(Some(info.uid));
                }
                Ok(())
            }
            Self::Channel(c) => {
                c.mark_field_loaded();
                Ok(())
            }
        }
    }
}

/// Pending NFC associate from Associate screen or [`IpcMessage::AssociateRequest`].
#[derive(Debug, Clone)]
pub struct PendingAssociate {
    pub request_id: u64,
    pub playable_id: i64,
    pub title: String,
    pub noubin_key: String,
    pub noudata_relpath: String,
    /// NB_18: `mediaList` position when this associate is for one track of a release (the
    /// target is that track's own `.noudata`); `None` for the whole playable.
    pub item: Option<u32>,
    /// NB_18: the document to create when the per-track `.noudata` does not exist yet. `None`
    /// means the write is a read-modify-write of `noudata_relpath` (today's behaviour).
    pub new_doc: Option<plippa_noubin::NoudataDocument>,
    /// NB_18: the library root that owns the target, resolved when the action was captured.
    /// Needed because a file about to be *created* cannot be found by probing `is_file()`.
    pub root: Option<std::path::PathBuf>,
    /// When true, UI writes `.noudata` after NFC success (local Associate screen).
    /// When false, portal/server owns write-through after [`AssociateResponse`].
    pub write_noudata_locally: bool,
    /// When true, read the key already on the tag instead of writing `noubin_key`.
    pub keep_existing_key: bool,
    /// NB_06: key the tag carried before we overwrote it (reported so the server can say
    /// which items still match it).
    pub previous_key: Option<String>,
    /// NB_06: the tag refused the write (locked); `noubin_key` is the key it already had.
    pub tag_locked: bool,
    /// NB_06: the write succeeded; waiting for a fresh full read of the tag to confirm the
    /// key before anything else (the server writes `.noudata` only after this).
    pub awaiting_verify: bool,
}

/// Throttle TFT redraws for position-only PlaybackStatus updates (~1 Hz).
const PLAYBACK_POS_DIRTY_MS: u64 = 1_000;
/// Same throttle while warped / scanning / seeking (WARP_07): the readout and playhead move.
const PLAYBACK_POS_DIRTY_WARPED_MS: u64 = 250;

/// Repaint interval for position-only status updates: faster while the speed is not 100%, a
/// scan runs or a seek is pending.
pub(crate) fn pos_dirty_interval_ms(s: &plippa_core::SessionState) -> u64 {
    if s.target_rate_permille != plippa_core::RATE_UNITY_PERMILLE
        || s.rate_permille != plippa_core::RATE_UNITY_PERMILLE
        || s.scan != plippa_core::ScanState::None
        || s.seek_target_ms.is_some()
    {
        PLAYBACK_POS_DIRTY_WARPED_MS
    } else {
        PLAYBACK_POS_DIRTY_MS
    }
}

/// Shared mutable state for one loop run.
pub struct LoopState {
    pub pending_associate: Option<PendingAssociate>,
    /// NB_06: NDEF of the tag last read completely (Channel backend); lets an associate
    /// learn the tag's previous key / fall back to reading a locked tag.
    pub last_read_ndef: Option<Vec<u8>>,
    pub next_local_request_id: u64,
    pub dirty: bool,
    pub sys_refresh_ms: u64,
    pub last_sys_ms: u64,
    /// Last reconnect attempt (ms since loop start).
    pub last_ipc_reconnect_ms: u64,
    /// Last NFC poll tick (ms) — coordinator-driven PollTick cadence.
    pub last_nfc_ms: u64,
    /// Idle NFC field-check interval (ms), from board `[nfc].poll_hz`.
    pub nfc_poll_interval_ms: u64,
    /// Loop time of last dirty caused by playback position (throttle).
    pub last_playback_pos_dirty_ms: u64,
    /// Last retry of a library root whose index was not readable (ms since loop start).
    pub last_library_retry_ms: u64,
    /// Loop clock epoch (`now_ms` = ms since this instant). `None` = the loop's own start.
    /// Shared with the I²C service so button-edge timestamps are on the same clock (WARP_06).
    pub epoch: Option<Instant>,
    /// Last PlayMode backlight state pushed (`Some((pressed, rung))`), only re-sent on change.
    pub playmode_led: Option<(bool, PlayModeState)>,
    /// ART_10: the NFC ring's record motion and what was last pushed to it.
    pub record_feed: RecordFeed,
    /// PWRSW_01: last power-LED warning state pushed (`Some(warning)`), re-sent on change.
    pub power_led: Option<bool>,
    /// Last Rewind / FF backlight states pushed (`Some(state)`, steady + pulse), only
    /// re-sent on change.
    pub ffrw_leds: [Option<TransportLedState>; 2],
    /// UI_16 §8: last seen value of the Debug setting `Rec enables NFC poll`, so turning it
    /// On stops the poll at once and turning it Off resumes continuous polling.
    pub nfc_rec_gate: bool,
    /// UI_16 §8: whether the (gated) NFC poll is currently running. Always true while the
    /// gate is off; while it is on, REC toggles it.
    pub nfc_poll_running: bool,
    /// UI_16 §8: loop time (ms) by which an accepted tap must have started playback for the
    /// gated poll to stop. Expiring only forgets the tap: the poll never stopped for it.
    pub nfc_tap_wait_until_ms: Option<u64>,
    /// ACC_03: last accessory worker drain (ms). 20 ms matches the worker tick.
    pub last_accessory_ms: u64,
}

impl Default for LoopState {
    fn default() -> Self {
        Self {
            pending_associate: None,
            last_read_ndef: None,
            next_local_request_id: 1,
            dirty: true,
            sys_refresh_ms: 1_000,
            last_sys_ms: 0,
            last_ipc_reconnect_ms: 0,
            // u64::MAX → first NFC poll is always due.
            last_nfc_ms: u64::MAX,
            // Noubin ≥5 Hz default (200 ms).
            nfc_poll_interval_ms: 200,
            last_playback_pos_dirty_ms: 0,
            last_library_retry_ms: 0,
            epoch: None,
            playmode_led: None,
            record_feed: RecordFeed::new(),
            power_led: None,
            ffrw_leds: [None; 2],
            // Gate off by default: the poll runs continuously, exactly as before UI_16.
            nfc_rec_gate: false,
            nfc_poll_running: true,
            nfc_tap_wait_until_ms: None,
            // u64::MAX → the first snapshot is drained on the first frame.
            last_accessory_ms: u64::MAX,
        }
    }
}

/// UI_16 §8: how long an accepted tap has to start playback. The engine answers a tap within
/// a frame or two; the window only covers a tag the engine does not recognise (that tap never
/// stops the poll, so a real tap still works without pressing REC first).
const NFC_TAP_PLAY_WAIT_MS: u64 = 3_000;

/// UI_16 §8: mirror the Debug `Rec enables NFC poll` setting into the loop. Turning it On
/// stops the poll at once (REC restarts it); turning it Off resumes the continuous poll.
/// Silent: the setting's value column is the only feedback.
fn sync_nfc_gate(app: &UiApp, state: &mut LoopState) {
    let gate = app.nfc_rec_gate();
    if gate != state.nfc_rec_gate {
        state.nfc_rec_gate = gate;
        state.nfc_poll_running = !gate;
        state.nfc_tap_wait_until_ms = None;
    }
}

/// UI_16 §8: an accepted tap starts the "did it play?" window. It is silent either way.
fn note_accepted_tap(state: &mut LoopState, now_ms: u64) {
    if state.nfc_rec_gate && state.nfc_poll_running {
        state.nfc_tap_wait_until_ms = Some(now_ms.saturating_add(NFC_TAP_PLAY_WAIT_MS));
    }
}

/// UI_16 §8: stop the gated poll once the accepted tap actually plays (the owner's
/// "successfully tapped and played"). A tap the engine never plays leaves the poll running:
/// the window simply expires, so an unrecognised tag cannot switch the reader off.
fn settle_tap_wait(app: &UiApp, state: &mut LoopState, now_ms: u64) {
    if !state.nfc_rec_gate {
        return;
    }
    let Some(deadline) = state.nfc_tap_wait_until_ms else {
        return;
    };
    if app.transport_playing() {
        // A tap while something already plays is a successful tap too: the transport is
        // playing, so the poll stops. REC brings it back.
        state.nfc_tap_wait_until_ms = None;
        state.nfc_poll_running = false;
    } else if now_ms >= deadline {
        state.nfc_tap_wait_until_ms = None;
    }
}

/// PlayMode (SW2) backlight: [`TRANSPORT_LED_PRESSED`] while the button is held, otherwise
/// [`TRANSPORT_LED_IDLE`] on the default rung and [`PLAYMODE_LED_ACTIVE`] on any other rung, so
/// the button shows at a glance that a mode is set. The cache key is the whole `(pressed, rung)`
/// tuple: a mode change repaints even though the press state did not move. Absent role: nothing.
pub fn sync_playmode_led(
    app: &UiApp,
    led: &mut dyn LedCue,
    last: &mut Option<(bool, PlayModeState)>,
) {
    let pressed = app.playmode_pressed();
    let state = app.play_mode_state();
    let key = (pressed, state);
    if *last == Some(key) || !led.has_role(LedRole::PlayModeButton) {
        return;
    }
    *last = Some(key);
    let level = if pressed {
        TRANSPORT_LED_PRESSED
    } else if state == plippa_core::PLAY_MODE_CYCLE[0] {
        TRANSPORT_LED_IDLE
    } else {
        PLAYMODE_LED_ACTIVE
    };
    led.set_role_colour(LedRole::PlayModeButton, Some(level));
}

/// ART_10: the NFC ring plays the Now Playing cover like a record. Runs every loop pass so the
/// motion keeps its angle under a tap animation, but pushes to the LED backend only when the
/// picture changed: a paused or stopped session sends nothing. Boards without the ring are
/// skipped.
pub fn sync_record_led(app: &UiApp, led: &mut dyn LedCue, feed: &mut RecordFeed, now_ms: u64) {
    if !led.has_role(LedRole::NfcRing) {
        return;
    }
    let update = feed.update(app.record_led_input(), now_ms);
    let delivered = match &update {
        RecordUpdate::Keep => return,
        RecordUpdate::Show(frame) => led.set_record_art(Some(frame.clone())),
        RecordUpdate::Clear => led.set_record_art(None),
    };
    if !delivered {
        feed.not_delivered();
    }
}

/// PWRSW_01: the power LED turns amber while the hold-to-power warning is up and returns to the
/// standard blue (`None` = no override) when the press is cancelled or the warning clears.
pub fn sync_power_led(app: &UiApp, led: &mut dyn LedCue, last: &mut Option<bool>) {
    let warning = app.power_warning_active();
    if *last == Some(warning) || !led.has_role(LedRole::PowerLed) {
        return;
    }
    *last = Some(warning);
    led.set_role_colour(LedRole::PowerLed, warning.then_some(POWER_LED_WARN));
}

/// Rewind / FF backlights (WARP_12, WARP_21): steady purple glow while the engine says the
/// control is usable (high-intensity purple while the button is held, and for
/// `TRANSPORT_LED_MIN_PRESSED_MS` after the press so a quick tap is visible), off when
/// unavailable. When the set speed is warped the idle role carries a pulse instead: FF pulses
/// purple above 100%, blue below, RW pulses purple for any reverse speed. Roles the board
/// lacks are skipped; pushes only on change. `now_ms` is the loop clock (the edge-stamp clock).
pub fn sync_transport_leds(
    app: &UiApp,
    led: &mut dyn LedCue,
    now_ms: u64,
    last: &mut [Option<TransportLedState>; 2],
) {
    let (rw, ff) = app.ffrw_led_states(now_ms);
    for (i, (role, state)) in [(LedRole::RewindButton, rw), (LedRole::FastForwardButton, ff)]
        .into_iter()
        .enumerate()
    {
        if last[i] == Some(state) || !led.has_role(role) {
            continue;
        }
        last[i] = Some(state);
        led.set_role_colour(role, state.steady);
        led.set_role_pulse(role, state.pulse);
    }
}

/// Steady glows that do not depend on any state: the top-panel REC button (SW6, no transport
/// function yet — a red glow so it reads as live) and the front-panel Back button (SW6, white
/// glow). Pushed once at startup; roles the board lacks are skipped.
pub fn sync_static_leds(led: &mut dyn LedCue) {
    for (role, colour) in [
        (LedRole::RecordButton, REC_LED_GLOW),
        (LedRole::MenuBackButton, BACK_LED_GLOW),
    ] {
        if led.has_role(role) {
            led.set_role_colour(role, Some(colour));
        }
    }
}

/// Prev/next backlights follow the engine's published availability (NB_14): steady
/// [`NAV_LED_ON`] when usable, off otherwise. Roles the board lacks are skipped.
pub fn sync_nav_leds(app: &UiApp, led: &mut dyn LedCue) {
    let avail = app.nav_availability();
    for (role, on) in [
        (LedRole::PrevTrackButton, avail.can_prev),
        (LedRole::NextTrackButton, avail.can_next),
    ] {
        if led.has_role(role) {
            led.set_role_colour(role, on.then_some(NAV_LED_ON));
        }
    }
}

/// Start the press flash for prev/next presses that were available (NB_14).
pub fn flash_nav_leds(app: &mut UiApp, led: &mut dyn LedCue) {
    for role in app.take_led_flashes() {
        if led.has_role(role) {
            led.flash_role(role, NAV_LED_FLASH);
        }
    }
}

/// Run until a limit or shutdown.
pub fn run_ui_loop(
    app: &mut UiApp,
    display: &mut dyn DisplayBackend,
    wifi: &mut dyn WifiBackend,
    input: &mut dyn InputSource,
    nfc: &mut Option<NfcBackend>,
    sys: &mut SysProviders,
    library: &mut Option<LibrarySource>,
    conn: &mut Option<IpcConnection>,
    led: &mut dyn LedCue,
    state: &mut LoopState,
    limits: LoopLimits,
) -> LoopExit {
    let start = state.epoch.unwrap_or_else(Instant::now);
    let mut iters = 0u64;

    // Colours that carry no state (REC / Back glows) are set once, not every frame.
    sync_static_leds(led);

    // Initial library + sys snapshot.
    refresh_library(app, library);
    refresh_sys(app, sys, state, 0);
    app.present(display);
    state.dirty = false;

    loop {
        if let Some(flag) = &limits.stop_flag {
            if flag.load(Ordering::Relaxed) {
                eprintln!("plippa-ui: stop flag set — leaving loop (LEDs clear on drop)");
                return LoopExit::Interrupted;
            }
        }
        if let Some(max) = limits.max_iters {
            if iters >= max {
                return LoopExit::MaxIters;
            }
        }
        if let Some(deadline) = limits.deadline {
            if Instant::now() >= deadline {
                return LoopExit::Deadline;
            }
        }

        let now_ms = start.elapsed().as_millis() as u64;
        app.set_engine_connected(conn.is_some());
        app.handle_input(InputEvent::Tick { now_ms }, wifi);
        if app.take_needs_redraw() {
            state.dirty = true;
        }
        // ACC_03: the accessory worker is drained on the same idle-tick budget as the rest of
        // the coordinator, never from the screen painter. 20 ms matches the worker tick, so a
        // prompt or transport press reaches the engine within one worker tick.
        if state.last_accessory_ms == u64::MAX
            || now_ms.saturating_sub(state.last_accessory_ms) >= 20
        {
            state.last_accessory_ms = now_ms;
            app.poll_accessories(now_ms);
            if app.take_needs_redraw() {
                state.dirty = true;
            }
        }

        // Drain pre-debounced input from MockInput or the I²C service channel.
        // No burst-sleep / no direct AW9523 reads on the coordinator.
        for ev in input.poll() {
            // One-frame optimistic play LED for snappy feedback; model stays on
            // engine revision and the next PlaybackStatus overwrites via sync.
            let optimistic_led = if app.event_toggles_play(&ev) {
                let playing = matches!(
                    app.model.playback.as_ref().map(|p| p.transport),
                    Some(plippa_core::TransportState::Playing)
                );
                Some(if playing {
                    PlayButtonLed::Standby
                } else {
                    PlayButtonLed::Playing
                })
            } else {
                None
            };
            // UI_16 §8: with the Debug setting `Rec enables NFC poll` On, a REC press starts
            // the otherwise stopped NFC poll and a second press stops it. Silent by design:
            // no popup, no notice (the owner wants clean product-video captures). The gate is
            // re-read after the input so a REC press in the frame that first sees the setting
            // still toggles the poll.
            let rec_press = matches!(ev, InputEvent::ControlPressed(ControlRole::Record));
            app.handle_input(ev, wifi);
            sync_nfc_gate(app, state);
            if rec_press && state.nfc_rec_gate {
                state.nfc_poll_running = !state.nfc_poll_running;
                state.nfc_tap_wait_until_ms = None;
                if state.nfc_poll_running {
                    // `u64::MAX` is the "never polled" sentinel, so the poll runs on this very
                    // frame rather than one interval after REC.
                    state.last_nfc_ms = u64::MAX;
                }
            }
            // NB_14: prev/next press flash starts now (from the press, not the engine ack).
            flash_nav_leds(app, led);
            if let Some(state) = optimistic_led {
                led.set_play_button(state);
            } else if app.pending_load_and_play() {
                // Library Detail Play → LoadAndPlay: show Playing before status.
                led.set_play_button(PlayButtonLed::Playing);
            }
            state.dirty = true;
        }
        sync_playmode_led(app, led, &mut state.playmode_led);
        sync_power_led(app, led, &mut state.power_led);
        sync_record_led(app, led, &mut state.record_feed, now_ms);
        sync_transport_leds(app, led, now_ms, &mut state.ffrw_leds);
        // Category / detail lookups requested by this batch of inputs.
        service_browse(app, library);
        service_art(app, library);
        // Flush play/pause IPC immediately after input drain.
        for out in app.take_outgoing() {
            match out {
                UiOutgoing::Ipc(msg) => {
                    if let Some(c) = conn.as_mut() {
                        if c.send(&msg).is_err() {
                            *conn = None;
                            app.push_outgoing(UiOutgoing::Ipc(msg));
                        }
                    } else {
                        app.push_outgoing(UiOutgoing::Ipc(msg));
                    }
                }
                other => app.push_outgoing(other),
            }
        }

        // LED worker self-paces ~30 FPS; proxy tick is a no-op (MVP_16).
        led.tick();

        // Hot-plug / remount: re-discover roots and reopen readers when a stick appeared,
        // vanished, was swapped or remounted at the same path (a stale reader then reports an
        // empty library: 0 tracks / 0 albums even though the stick is mounted). Also retries a
        // root whose index was missing/unreadable when last opened (UI started before the
        // server created or migrated it, and the update message was missed).
        if now_ms.saturating_sub(state.last_library_retry_ms) >= 1_000 {
            state.last_library_retry_ms = now_ms;
            if library.as_mut().is_some_and(|l| l.refresh_if_changed()) {
                refresh_library(app, library);
                state.dirty = true;
            }
        }

        // Capture local associate selection → pending write.
        capture_associate_selected(app, state, library);

        // IPC in / reconnect (MVP_14): clear sticky offline when engine.sock appears.
        if conn.is_none() {
            if now_ms.saturating_sub(state.last_ipc_reconnect_ms) >= 1_000 {
                state.last_ipc_reconnect_ms = now_ms;
                let paths = resolve_paths();
                match IpcConnection::connect_timeout(
                    &paths.engine_sock,
                    Duration::from_millis(150),
                ) {
                    Ok(c) => {
                        eprintln!(
                            "plippa-ui: IPC reconnected to {}",
                            paths.engine_sock.display()
                        );
                        *conn = Some(c);
                    }
                    Err(_) => {}
                }
            }
        }
        if let Some(c) = conn.as_mut() {
            // Drain the whole socket before NFC so AssociateRequest is not
            // stuck behind PlaybackStatus (one-msg-per-frame was racing taps).
            for _ in 0..64 {
                match c.try_recv() {
                    Ok(Some(msg)) => {
                        if let IpcMessage::PlaybackStatus(ref status) = msg {
                            // Immediate dirty: transport / item / volume / revision /
                            // title / artist / duration. Position-only → ~1 Hz.
                            let prev = app.model.playback.as_ref().map(|p| {
                                (
                                    p.transport,
                                    p.session.current_item,
                                    p.session.volume.percent(),
                                    p.revision,
                                    p.session.title.clone(),
                                    p.session.artist.clone(),
                                    p.session.duration_ms,
                                    p.session.position_ms,
                                    (p.session.current_entry_index, p.session.current_cue_index),
                                )
                            });
                            let next = (
                                status.transport,
                                status.session.current_item,
                                status.session.volume.percent(),
                                status.revision,
                                status.session.title.clone(),
                                status.session.artist.clone(),
                                status.session.duration_ms,
                                status.session.position_ms,
                                (
                                    status.session.current_entry_index,
                                    status.session.current_cue_index,
                                ),
                            );
                            let pos_interval = pos_dirty_interval_ms(&status.session);
                            handle_ipc_in(app, state, library, msg);
                            // LED always follows model after status apply (never raw IPC).
                            led.set_play_button(app.play_button_led());
                            sync_nav_leds(app, led);
                            sync_transport_leds(app, led, now_ms, &mut state.ffrw_leds);
                            let should_dirty = match prev {
                                None => {
                                    state.last_playback_pos_dirty_ms = now_ms;
                                    true
                                }
                                Some((
                                    pt,
                                    pi,
                                    pv,
                                    pr,
                                    ptitle,
                                    partist,
                                    pdur,
                                    ppos,
                                    pcue,
                                )) => {
                                    let (
                                        nt,
                                        ni,
                                        nv,
                                        nr,
                                        ntitle,
                                        nartist,
                                        ndur,
                                        npos,
                                        ncue,
                                    ) = next;
                                    let immediate = pt != nt
                                        || pi != ni
                                        || pv != nv
                                        || pr != nr
                                        || ptitle != ntitle
                                        || partist != nartist
                                        || pdur != ndur
                                        // NB_15: the list highlight follows the engine's cue.
                                        || pcue != ncue;
                                    if immediate {
                                        state.last_playback_pos_dirty_ms = now_ms;
                                        true
                                    } else if ppos != npos
                                        && now_ms.saturating_sub(state.last_playback_pos_dirty_ms)
                                            >= pos_interval
                                    {
                                        state.last_playback_pos_dirty_ms = now_ms;
                                        true
                                    } else {
                                        false
                                    }
                                }
                            };
                            if should_dirty {
                                state.dirty = true;
                            }
                        } else {
                            handle_ipc_in(app, state, library, msg);
                            state.dirty = true;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        *conn = None;
                        break;
                    }
                }
            }
        }

        // Outgoing IPC / shutdown — send before TFT so play/pause reaches the engine ASAP.
        for out in app.take_outgoing() {
            match out {
                UiOutgoing::Ipc(msg) => {
                    if let Some(c) = conn.as_mut() {
                        if c.send(&msg).is_err() {
                            *conn = None;
                            app.push_outgoing(UiOutgoing::Ipc(msg));
                        }
                    } else {
                        app.push_outgoing(UiOutgoing::Ipc(msg));
                    }
                }
                UiOutgoing::ShutdownRequest { power_off } => {
                    if state.dirty {
                        app.present(display);
                    }
                    return if power_off {
                        LoopExit::PowerOff
                    } else {
                        LoopExit::Shutdown
                    };
                }
                UiOutgoing::TapFeedback { tap_id } => {
                    led.on_tap_feedback(tap_id);
                    // NFC copy is shown via status popup in handle_nfc_event (UI_05).
                    state.dirty = true;
                }
                UiOutgoing::AssociateSelected { .. } => {}
                UiOutgoing::CancelAssociate => {
                    state.pending_associate = None;
                    state.dirty = true;
                }
            }
        }

        // NFC after IPC — mock is in-process; Channel sends PollTick / drains events.
        // UI_16 §8: the Debug `Rec enables NFC poll` setting gates the idle poll. Off
        // (default) polls continuously; On keeps it stopped until REC starts it, and stops
        // it again once a tap has played. An associate in flight is never gated: it needs
        // reads to finish and is a deliberate, button-initiated action.
        sync_nfc_gate(app, state);
        settle_tap_wait(app, state, now_ms);
        let poll_running = !state.nfc_rec_gate || state.nfc_poll_running;
        let nfc_interval_ms = state.nfc_poll_interval_ms.max(1);
        if let Some(backend) = nfc.as_mut() {
            if backend.worker_gone() {
                eprintln!("plippa-ui: I²C NFC worker gone — leaving loop");
                return LoopExit::Interrupted;
            }
            backend.advance_mock_clock(MIN_POLL_INTERVAL);
            let due = poll_running
                && (state.last_nfc_ms == u64::MAX
                    || now_ms.saturating_sub(state.last_nfc_ms) >= nfc_interval_ms);

            match backend {
                NfcBackend::Mock { .. } => {
                    if state.pending_associate.is_some() {
                        try_associate_nfc_mock(app, backend, state, library);
                        state.dirty = true;
                        state.last_nfc_ms = now_ms;
                    } else if due {
                        state.last_nfc_ms = now_ms;
                        if let NfcBackend::Mock { session } = backend {
                            match session.poll_once() {
                                Ok(events) => {
                                    for ev in events {
                                        if let NfcEvent::ReadComplete { info, .. } = &ev {
                                            session.set_loaded(Some(info.uid.clone()));
                                        }
                                        if let NfcEvent::TagRemoved { .. } = &ev {
                                            session.set_loaded(None);
                                        }
                                        if matches!(ev, NfcEvent::ReadComplete { .. }) {
                                            note_accepted_tap(state, now_ms);
                                        }
                                        app.handle_nfc_event(ev);
                                        state.dirty = true;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("plippa-ui: NFC poll error: {e}");
                                }
                            }
                        }
                    }
                }
                NfcBackend::Channel(_) => {
                    let drained = {
                        let NfcBackend::Channel(client) = backend else {
                            unreachable!()
                        };
                        client.drain()
                    };
                    let mut write_results = Vec::new();
                    for sev in drained {
                        match sev {
                            NfcServiceEvent::WorkerGone => {
                                eprintln!("plippa-ui: I²C service WorkerGone");
                                return LoopExit::Interrupted;
                            }
                            NfcServiceEvent::Events(events) => {
                                for ev in events {
                                    if let NfcEvent::ReadComplete { info, ndef } = &ev {
                                        state.last_read_ndef = Some(ndef.clone());
                                        if let NfcBackend::Channel(c) = backend {
                                            c.set_loaded_uid(Some(info.uid.clone()));
                                        }
                                    }
                                    if let NfcEvent::TagRemoved { .. } = &ev {
                                        state.last_read_ndef = None;
                                        if let NfcBackend::Channel(c) = backend {
                                            c.set_loaded_uid(None);
                                        }
                                    }
                                    let keep_read = matches!(
                                        (&ev, state.pending_associate.as_ref()),
                                        (
                                            NfcEvent::ReadComplete { .. },
                                            Some(p)
                                        ) if p.keep_existing_key
                                    );
                                    let verify_read = matches!(
                                        (&ev, state.pending_associate.as_ref()),
                                        (
                                            NfcEvent::ReadComplete { .. },
                                            Some(p)
                                        ) if p.awaiting_verify
                                    );
                                    let write_pending = matches!(
                                        (&ev, state.pending_associate.as_ref()),
                                        (
                                            NfcEvent::ReadComplete { .. },
                                            Some(p)
                                        ) if !p.keep_existing_key
                                    );
                                    if verify_read {
                                        if let NfcEvent::ReadComplete { ndef, .. } = ev {
                                            verify_written_key(
                                                app, backend, state, library, Some(&ndef),
                                            );
                                            state.dirty = true;
                                        }
                                    } else if keep_read {
                                        if let NfcEvent::ReadComplete { ndef, .. } = ev {
                                            finish_associate_from_ndef(
                                                app, backend, state, library, &ndef,
                                            );
                                            state.dirty = true;
                                        }
                                    } else if write_pending {
                                        // Writing a new key — ignore read-complete (no TapNdef).
                                    } else {
                                        if matches!(ev, NfcEvent::ReadComplete { .. }) {
                                            note_accepted_tap(state, now_ms);
                                        }
                                        app.handle_nfc_event(ev);
                                        state.dirty = true;
                                    }
                                }
                            }
                            NfcServiceEvent::WriteResult { ok, error } => {
                                write_results.push((ok, error));
                            }
                        }
                    }
                    for (ok, error) in write_results {
                        handle_channel_write_result(app, backend, state, library, ok, error);
                        state.dirty = true;
                    }
                    if state
                        .pending_associate
                        .as_ref()
                        .is_some_and(|p| !p.keep_existing_key && !p.awaiting_verify)
                    {
                        try_associate_write_channel(app, backend, state);
                        if let NfcBackend::Channel(c) = backend {
                            c.request_poll_tick();
                        }
                        state.last_nfc_ms = now_ms;
                    } else if state
                        .pending_associate
                        .as_ref()
                        .is_some_and(|p| p.keep_existing_key || p.awaiting_verify)
                    {
                        // Keep-existing / read-back verify: PollTick only — ReadComplete
                        // finishes associate.
                        if let NfcBackend::Channel(c) = backend {
                            c.request_poll_tick();
                        }
                        state.last_nfc_ms = now_ms;
                    } else if due {
                        state.last_nfc_ms = now_ms;
                        if let NfcBackend::Channel(c) = backend {
                            c.request_poll_tick();
                        }
                    }
                }
            }
        }

        // Periodic clock / temp / fan — only dirty when painted fields change.
        if now_ms.saturating_sub(state.last_sys_ms) >= state.sys_refresh_ms {
            if refresh_sys(app, sys, state, now_ms) {
                state.dirty = true;
            }
        }

        if state.dirty {
            app.present(display);
            state.dirty = false;
        }

        iters += 1;
        if !limits.sleep.is_zero() {
            std::thread::sleep(limits.sleep);
        }
    }
}

/// Index changed (startup, `LibraryIndexUpdated`, associate write-through): refresh the
/// category counts and whatever list / detail is on screen (NB_12).
fn refresh_library(app: &mut UiApp, library: &mut Option<LibrarySource>) {
    app.library_index_changed();
    service_browse(app, library);
}

/// Cover art of the playing item (ART_08): resolve when the playing item changed. Cheap while
/// it has not.
fn service_art(app: &mut UiApp, library: &Option<LibrarySource>) {
    if let Some(lib) = library.as_ref() {
        app.service_art(lib);
    }
}

/// Answer the browse lookups queued by navigation (SQLite reads happen here, not in
/// `handle_input` or the paint path).
fn service_browse(app: &mut UiApp, library: &Option<LibrarySource>) {
    if let Some(lib) = library.as_ref() {
        app.service_browse(lib);
    }
}

/// Refresh clock / temp / fan. Returns `true` when any painted field changed.
fn refresh_sys(app: &mut UiApp, sys: &SysProviders, state: &mut LoopState, now_ms: u64) -> bool {
    let (time, temp, fan) = sys.snapshot();
    let changed = app.model.system.time_hhmm != time
        || app.model.system.temperature_c != temp
        || app.model.system.fan_status != fan;
    app.model.system.time_hhmm = time;
    app.model.system.temperature_c = temp;
    app.model.system.fan_status = fan;
    state.last_sys_ms = now_ms;
    changed
}

fn handle_ipc_in(
    app: &mut UiApp,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
    msg: IpcMessage,
) {
    match msg {
        IpcMessage::LibraryIndexUpdated(_) => {
            if let Some(lib) = library.as_mut() {
                lib.reopen();
            }
            refresh_library(app, library);
        }
        IpcMessage::AssociateRequest(req) => {
            let mode = if req.keep_existing_key {
                "read existing key"
            } else {
                "write NFC"
            };
            eprintln!(
                "plippa-ui: AssociateRequest armed request_id={} playable_id={} key={} mode={} — next tap {}",
                req.request_id,
                req.playable_id.as_u64(),
                req.noubin_key,
                mode,
                if req.keep_existing_key {
                    "reads tag key"
                } else {
                    "writes NFC"
                }
            );
            state.pending_associate = Some(PendingAssociate {
                request_id: req.request_id,
                playable_id: req.playable_id.as_u64() as i64,
                title: req.playable_item_title,
                noubin_key: req.noubin_key,
                noudata_relpath: req.noudata_relpath,
                item: None,
                new_doc: None,
                root: None,
                write_noudata_locally: false,
                keep_existing_key: req.keep_existing_key,
                previous_key: None,
                tag_locked: false,
                awaiting_verify: false,
            });
            app.model.banner = Some(format!(
                "tap a noubin for {}",
                state
                    .pending_associate
                    .as_ref()
                    .map(|p| p.title.as_str())
                    .unwrap_or("?")
            ));
            app.compositor.show_status("tap a noubin", app.now_ms());
        }
        other => app.handle_ipc(other),
    }
}

fn capture_associate_selected(
    app: &mut UiApp,
    state: &mut LoopState,
    library: &Option<LibrarySource>,
) {
    // Peek outgoing without clearing unrelated messages: drain, re-queue non-associate.
    let outgoing = app.take_outgoing();
    let mut keep = Vec::new();
    for out in outgoing {
        match out {
            UiOutgoing::AssociateSelected {
                playable_id,
                item,
                title,
            } => {
                let mut noudata_relpath = app
                    .model
                    .library
                    .iter()
                    .find(|i| i.id == playable_id)
                    .map(|i| i.noudata_relpath.clone())
                    .unwrap_or_default();
                let mut new_doc = None;
                let mut root = None;

                // NB_18: a single track gets its own `linkTrack` `.noudata` next to its media
                // file. Resolve it now, before the wait, so a free name is taken while the
                // owner watches and a refusal is immediate — never a silent fallback to the
                // release document.
                if let Some(pos) = item {
                    // The item on screen knows its root; two roots' ids are not comparable.
                    let owner_root = app
                        .model
                        .library
                        .iter()
                        .find(|i| i.id == playable_id)
                        .and_then(|i| i.library_root.clone());
                    let resolved: Result<(std::path::PathBuf, plippa_noubin::TrackTarget), String> =
                        match library.as_ref() {
                            None => Err(
                                "Library unavailable — nothing was associated.".to_string()
                            ),
                            Some(lib) => {
                                match lib.track_of(owner_root.as_deref(), &noudata_relpath, pos) {
                                    crate::library::TrackLookup::Found(
                                        owner,
                                        media_rel,
                                        track_title,
                                    ) => {
                                        let existing = lib.track_file_for(
                                            &owner,
                                            &noudata_relpath,
                                            &media_rel,
                                        );
                                        plippa_noubin::track_associate_target(
                                            &owner,
                                            &noudata_relpath,
                                            &media_rel,
                                            &track_title,
                                            existing.as_deref(),
                                        )
                                        .map(|t| (owner, t))
                                    }
                                    crate::library::TrackLookup::NotARelease => Err(
                                        "Only an album, mix, audiobook or podcast has tracks \
                                         that can be associated one by one."
                                            .to_string(),
                                    ),
                                    crate::library::TrackLookup::Missing => Err(
                                        "This track is no longer in the library, so it cannot \
                                         be associated. Nothing was changed."
                                            .to_string(),
                                    ),
                                }
                            }
                        };
                    match resolved {
                        Ok((owner, target)) => {
                            noudata_relpath = target.relpath;
                            new_doc = target.created.then_some(target.doc);
                            root = Some(owner);
                        }
                        Err(msg) => {
                            decline_associate(app, state, &msg);
                            continue;
                        }
                    }
                }

                let request_id = state.next_local_request_id;
                state.next_local_request_id = state.next_local_request_id.saturating_add(1);
                // Local key: title-based placeholder until tag capacity known;
                // final key is the one we write (server path supplies key).
                let noubin_key = format!(
                    "local:{}",
                    title
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .take(24)
                        .collect::<String>()
                        .to_ascii_lowercase()
                );
                state.pending_associate = Some(PendingAssociate {
                    request_id,
                    playable_id,
                    title: title.clone(),
                    noubin_key,
                    noudata_relpath,
                    item,
                    new_doc,
                    root,
                    write_noudata_locally: true,
                    keep_existing_key: false,
                    previous_key: None,
                    tag_locked: false,
                    awaiting_verify: false,
                });
                app.model.banner = Some(format!("tap a noubin for {title}"));
                app.compositor.show_status("tap a noubin", app.now_ms());
            }
            other => keep.push(other),
        }
    }
    // Re-queue kept outgoing by pushing back through a side channel:
    // UiApp has no push_outgoing — emit via temporary field.
    for out in keep {
        app.push_outgoing(out);
    }
}

/// Refuse an associate before the wait is armed: answer the request and leave the screen.
fn decline_associate(app: &mut UiApp, state: &mut LoopState, message: &str) {
    let request_id = state.next_local_request_id;
    state.next_local_request_id = state.next_local_request_id.saturating_add(1);
    app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
        AssociateResponse {
            request_id,
            ok: false,
            error: Some(message.to_string()),
            ..Default::default()
        },
    )));
    app.model.banner = None;
    app.compositor.show_status(message, app.now_ms());
    if *app.nav.current() == crate::Route::LibraryAssociate {
        let _ = app.nav.back();
    }
}

fn try_associate_nfc_mock(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
) {
    let Some(pending) = state.pending_associate.clone() else {
        return;
    };
    if pending.keep_existing_key {
        try_associate_read_mock(app, backend, state, library);
    } else {
        try_associate_write_mock(app, backend, state, library);
    }
}

fn try_associate_read_mock(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
) {
    let NfcBackend::Mock { session } = backend else {
        return;
    };
    if session
        .device_mut()
        .poll()
        .ok()
        .and_then(|t| t)
        .is_none()
    {
        return;
    }
    let ndef = match session.device_mut().read_full_ndef() {
        Ok(b) => b,
        Err(e) => {
            let Some(pending) = state.pending_associate.clone() else {
                return;
            };
            app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
                AssociateResponse {
                    request_id: pending.request_id,
                    ok: false,
                    noubin_key: None,
                    noubin_key_normalised: None,
                    error: Some(e.to_string()),
                    ..Default::default()
                },
            )));
            state.pending_associate = None;
            app.model.banner = Some("Associate read failed".into());
            return;
        }
    };
    finish_associate_from_ndef(app, backend, state, library, &ndef);
}

fn finish_associate_from_ndef(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
    ndef: &[u8],
) {
    let Some(mut pending) = state.pending_associate.clone() else {
        return;
    };
    let key = match extract_noubin_key(ndef) {
        Ok(k) => k,
        Err(e) => {
            app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
                AssociateResponse {
                    request_id: pending.request_id,
                    ok: false,
                    noubin_key: None,
                    noubin_key_normalised: None,
                    error: Some(format!("Could not extract key from tag: {e}")),
                    ..Default::default()
                },
            )));
            state.pending_associate = None;
            app.model.banner = Some("Tag has no usable key".into());
            return;
        }
    };
    let norm = normalise_noubin_key(&key);
    if norm.is_empty() {
        app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
            AssociateResponse {
                request_id: pending.request_id,
                ok: false,
                noubin_key: None,
                noubin_key_normalised: None,
                error: Some(
                    "Tag key is blank after normalisation — cannot keep existing key".into(),
                ),
                ..Default::default()
            },
        )));
        state.pending_associate = None;
        app.model.banner = Some("Tag key is blank".into());
        return;
    }
    pending.noubin_key = key;
    state.pending_associate = Some(pending.clone());
    finish_associate_ok(app, backend, state, library, &pending);
}

fn try_associate_write_mock(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
) {
    let Some(mut pending) = state.pending_associate.clone() else {
        return;
    };
    // (write result, NDEF read before writing — the tag's previous key / locked fallback)
    let (write_result, before) = {
        let NfcBackend::Mock { session } = backend else {
            return;
        };
        if session
            .device_mut()
            .poll()
            .ok()
            .and_then(|t| t)
            .is_none()
        {
            return;
        }
        let ndef = match encode_text_record(&pending.noubin_key, DEFAULT_TEXT_LANG) {
            Ok(b) => b,
            Err(e) => {
                app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
                    AssociateResponse {
                        request_id: pending.request_id,
                        ok: false,
                        noubin_key: None,
                        noubin_key_normalised: None,
                        error: Some(e.to_string()),
                        ..Default::default()
                    },
                )));
                state.pending_associate = None;
                app.model.banner = Some("Associate encode failed".into());
                return;
            }
        };
        let before = session.device_mut().read_full_ndef().ok();
        (session.device_mut().write_first_ndef_record(&ndef), before)
    };
    pending.previous_key = before.as_deref().and_then(usable_key_from_ndef).map(|(k, _)| k);
    match write_result {
        Ok(()) => {
            state.pending_associate = Some(pending.clone());
            // Read the tag back before reporting success (the server writes `.noudata`
            // only on a verified tag).
            let after = match backend {
                NfcBackend::Mock { session } => session.device_mut().read_full_ndef().ok(),
                NfcBackend::Channel(_) => None,
            };
            verify_written_key(app, backend, state, library, after.as_deref());
        }
        Err(NfcError::ReadOnly) => {
            finish_associate_locked(app, backend, state, library, before.as_deref(), false)
        }
        // A link on the tag is kept exactly like a locked tag's key.
        Err(NfcError::HasLink) => {
            finish_associate_locked(app, backend, state, library, before.as_deref(), true)
        }
        Err(e) => {
            app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
                AssociateResponse {
                    request_id: pending.request_id,
                    ok: false,
                    noubin_key: None,
                    noubin_key_normalised: None,
                    error: Some(e.to_string()),
                    ..Default::default()
                },
            )));
            app.model.banner = Some("NFC write failed".into());
            state.pending_associate = None;
        }
    }
}

/// NB_06 write-ordering step 2: confirm the tag now carries the key we wrote. Only then
/// does [`finish_associate_ok`] report success (which lets the server write `.noudata`).
fn verify_written_key(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
    ndef: Option<&[u8]>,
) {
    let Some(mut pending) = state.pending_associate.clone() else {
        return;
    };
    let want = normalise_noubin_key(&pending.noubin_key);
    let got = ndef.and_then(usable_key_from_ndef).map(|(_, n)| n);
    if got.as_deref() != Some(want.as_str()) {
        app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
            AssociateResponse {
                request_id: pending.request_id,
                ok: false,
                error: Some(
                    "The tag did not read back the new key after writing — nothing was saved. \
                     Hold the tag steady and try again."
                        .into(),
                ),
                ..Default::default()
            },
        )));
        app.model.banner = Some("Tag verify failed".into());
        state.pending_associate = None;
        return;
    }
    pending.awaiting_verify = false;
    state.pending_associate = Some(pending.clone());
    finish_associate_ok(app, backend, state, library, &pending);
}

/// A non-blank key (raw, normalised) carried by `ndef`, if any.
fn usable_key_from_ndef(ndef: &[u8]) -> Option<(String, String)> {
    let key = extract_noubin_key(ndef).ok()?;
    let norm = normalise_noubin_key(&key);
    (!norm.is_empty()).then_some((key, norm))
}

/// NB_06 locked-tag path: the tag refused the write, so bind the key it already carries
/// (`tag_locked` tells the server to apply move semantics). A locked tag with no readable
/// key fails with a clear message and nothing is modified.
///
/// Also the path for a rewritable tag that already carries a link (`link_kept`): the device
/// refused to overwrite it, so the link is the key, exactly as for a locked tag.
fn finish_associate_locked(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
    ndef: Option<&[u8]>,
    link_kept: bool,
) {
    let Some(mut pending) = state.pending_associate.clone() else {
        return;
    };
    let fail = |app: &mut UiApp, state: &mut LoopState, msg: &str, banner: &str| {
        app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
            AssociateResponse {
                request_id: pending.request_id,
                ok: false,
                error: Some(msg.into()),
                ..Default::default()
            },
        )));
        state.pending_associate = None;
        app.model.banner = Some(banner.into());
    };
    if pending.write_noudata_locally {
        if link_kept {
            fail(
                app,
                state,
                "This tag already carries a link, which is never overwritten. Associate it from \
                 the web portal (\"use the key already on the tag\").",
                "Tag has a link",
            );
        } else {
            fail(
                app,
                state,
                "This tag is locked. Associate it from the web portal (\"use the key already on the tag\").",
                "Tag is locked",
            );
        }
        return;
    }
    let Some((key, _)) = ndef.and_then(usable_key_from_ndef) else {
        if link_kept {
            fail(
                app,
                state,
                "The link on this tag could not be read. Hold the tag steady and try again. \
                 Nothing was changed.",
                "Tag link unreadable",
            );
        } else {
            fail(
                app,
                state,
                "This tag is locked (read-only) and has no readable Noubin key, so it cannot be \
                 associated. Nothing was changed.",
                "Locked tag has no key",
            );
        }
        return;
    };
    pending.noubin_key = key;
    pending.tag_locked = true;
    pending.previous_key = None;
    state.pending_associate = Some(pending.clone());
    finish_associate_ok(app, backend, state, library, &pending);
}

fn try_associate_write_channel(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
) {
    let Some(pending) = state.pending_associate.clone() else {
        return;
    };
    let NfcBackend::Channel(client) = backend else {
        return;
    };
    if client.write_inflight() {
        return;
    }
    let mut pending = pending;
    if pending.previous_key.is_none() {
        pending.previous_key = state
            .last_read_ndef
            .as_deref()
            .and_then(usable_key_from_ndef)
            .map(|(k, _)| k);
        state.pending_associate = Some(pending.clone());
    }
    let ndef = match encode_text_record(&pending.noubin_key, DEFAULT_TEXT_LANG) {
        Ok(b) => b,
        Err(e) => {
            app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
                AssociateResponse {
                    request_id: pending.request_id,
                    ok: false,
                    noubin_key: None,
                    noubin_key_normalised: None,
                    error: Some(e.to_string()),
                    ..Default::default()
                },
            )));
            state.pending_associate = None;
            app.model.banner = Some("Associate encode failed".into());
            return;
        }
    };
    let _ = client.request_write(ndef);
}

fn handle_channel_write_result(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
    ok: bool,
    error: Option<String>,
) {
    let Some(pending) = state.pending_associate.clone() else {
        return;
    };
    if ok {
        // Do not trust the write status alone: re-read the tag (the next PollTick emits a
        // fresh ReadComplete once the "loaded" mark is cleared) and verify the key.
        if let Some(p) = state.pending_associate.as_mut() {
            p.awaiting_verify = true;
        }
        state.last_read_ndef = None;
        if let NfcBackend::Channel(c) = backend {
            c.set_loaded_uid(None);
        }
        return;
    }
    // Keep pending and retry — tag may not be in field yet (same as mock path).
    if error.as_deref() == Some("no tag in field") {
        return;
    }
    let locked = error.as_deref() == Some(TAG_READ_ONLY_MSG);
    if locked || error.as_deref() == Some(TAG_HAS_LINK_MSG) {
        let stash = state.last_read_ndef.clone();
        finish_associate_locked(app, backend, state, library, stash.as_deref(), !locked);
        return;
    }
    app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
        AssociateResponse {
            request_id: pending.request_id,
            ok: false,
            noubin_key: None,
            noubin_key_normalised: None,
            error,
            ..Default::default()
        },
    )));
    app.model.banner = Some("NFC write failed".into());
    state.pending_associate = None;
}

fn finish_associate_ok(
    app: &mut UiApp,
    backend: &mut NfcBackend,
    state: &mut LoopState,
    library: &mut Option<LibrarySource>,
    pending: &PendingAssociate,
) {
    let norm = normalise_noubin_key(&pending.noubin_key);
    eprintln!(
        "plippa-ui: Associate NFC ok request_id={} key={} norm={} keep_existing={} item={:?}",
        pending.request_id, pending.noubin_key, norm, pending.keep_existing_key, pending.item
    );
    if pending.write_noudata_locally {
        if let Err(e) = write_noudata_keys_through(
            library,
            pending,
            &pending.noubin_key,
            &norm,
        ) {
            app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
                AssociateResponse {
                    request_id: pending.request_id,
                    ok: false,
                    noubin_key: None,
                    noubin_key_normalised: None,
                    error: Some(e),
                    ..Default::default()
                },
            )));
            state.pending_associate = None;
            app.model.banner = Some("Associate metadata write failed".into());
            app.compositor
                .show_status("Associate metadata write failed", app.now_ms());
            return;
        }
        if let Some(lib) = library.as_mut() {
            lib.reopen();
        }
        refresh_library(app, library);
    }
    app.push_outgoing(UiOutgoing::Ipc(IpcMessage::AssociateResponse(
        AssociateResponse {
            request_id: pending.request_id,
            ok: true,
            noubin_key: Some(pending.noubin_key.clone()),
            noubin_key_normalised: Some(norm),
            error: None,
            tag_locked: pending.keep_existing_key || pending.tag_locked,
            previous_key: pending.previous_key.clone(),
        },
    )));
    // Status popup only — do not sticky-paint via model.banner on Now Playing.
    app.model.banner = None;
    app.compositor
        .show_status(&format!("Associated {}", pending.title), app.now_ms());
    // Return to detail after success when we came from LibraryAssociate.
    if *app.nav.current() == crate::Route::LibraryAssociate {
        let _ = app.nav.back();
    }
    state.pending_associate = None;
    let _ = backend.mark_field_tag_as_loaded();
}

/// After NFC success only: set keys in `.noudata` and refresh the WAL index.
///
/// NB_18: for a single track the document is either one the caller resolved (a new per-track
/// file, created here) or the existing per-track file (read-modify-write). The release's
/// document is never opened on that path.
fn write_noudata_keys_through(
    library: &Option<LibrarySource>,
    pending: &PendingAssociate,
    key: &str,
    normalised: &str,
) -> Result<(), String> {
    let noudata_relpath = &pending.noudata_relpath;
    if noudata_relpath.is_empty() {
        return Ok(());
    }
    let Some(lib) = library.as_ref() else {
        return Ok(());
    };
    // NB_18: a file that does not exist yet cannot be found by probing `is_file()` on each
    // root, so the root resolved with the target wins. The read-modify-write case keeps the
    // old probe (it also covers a stick mounted at a new path).
    let probed;
    let root: &Path = match pending.root.as_deref() {
        Some(root) => root,
        None => {
            probed = lib
                .roots()
                .iter()
                .find(|r| r.join(noudata_relpath).is_file())
                .cloned()
                .or_else(|| lib.root().map(Path::to_path_buf));
            let Some(root) = probed.as_deref() else {
                return Ok(());
            };
            root
        }
    };
    let path = root.join(noudata_relpath);
    let mut doc = match &pending.new_doc {
        Some(doc) => {
            if path.exists() {
                // Never overwrite someone else's file: a name that was free when we looked is
                // not ours to take now.
                return Err(
                    "a .noudata with that name appeared while the tag was being written; \
                     nothing was overwritten"
                        .into(),
                );
            }
            doc.clone()
        }
        None => {
            if !path.is_file() {
                return Err(
                    "the track's .noudata is not on the stick any more; nothing was written"
                        .into(),
                );
            }
            let bytes = plippa_noubin::read_noudata_capped(&path)
                .map_err(|_| "the track's .noudata could not be read")?;
            parse_noudata(&bytes)
                .map_err(|_| "the track's .noudata is not valid Noubin JSON")?
        }
    };
    doc.set_noubin_keys(key, normalised);
    match &pending.new_doc {
        Some(_) => {
            plippa_noubin::write_noudata_atomic(
                &doc,
                &path,
                plippa_noubin::WriteGuard::MustNotExist,
            )
            .map_err(associate_write_error)?;
        }
        None => {
            doc.write_to_path(&path)
                .map_err(|_| "the track's .noudata could not be written".to_string())?;
        }
    }
    // Touch the index so engine/UI see the new key without waiting for server.
    let _ = LibraryWriter::open_with(&LibraryRoot::new(root), plippa_noubin::OpenWait::Interactive)
        .and_then(|mut w| w.full_scan());
    Ok(())
}

/// A sentence for a human, never an OS error string.
fn associate_write_error(e: plippa_noubin::AtomicWriteError) -> String {
    match e {
        plippa_noubin::AtomicWriteError::Conflict => {
            "the .noudata changed while the tag was being written; try again".into()
        }
        plippa_noubin::AtomicWriteError::Io(_) => {
            "the .noudata could not be written to the stick; nothing was saved".into()
        }
        plippa_noubin::AtomicWriteError::Noudata(_) => {
            "the .noudata is not valid Noubin JSON; nothing was saved".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::MockDisplay;
    use crate::input::{InputEvent, MockInput};
    use crate::wifi::MockWifi;
    use crate::{UiConfig, SystemStatus};
    use plippa_hw::{MockTag, TagType};
    use plippa_ipc_proto::{NOTICE_LIBRARY_MISS, NoticeSeverity, UserNotice};

    #[test]
    fn frame_loop_scripted_inputs_exit_cleanly() {
        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        app.init_wifi(&mut wifi);
        let mut input = MockInput::new();
        input.extend([
            InputEvent::Back, // open main-menu overlay
            InputEvent::MenuRotate(-1), // UI_16 §7: left = down = Library (Main=0)
            InputEvent::MenuEnter,
            InputEvent::Back, // pop to Main (+ show overlay)
        ]);
        let mut nfc = Some(NfcBackend::mock_empty());
        let mut sys = SysProviders::mock("12:00", Some(40), "off");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();
        let exit = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(20),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        assert_eq!(exit, LoopExit::MaxIters);
        assert!(display.frames >= 1);
        assert_eq!(*app.nav.current(), crate::Route::Main);
    }

    #[test]
    fn role_mapped_events_drive_nav_and_volume() {
        let mut app = UiApp::new(UiConfig {
            volume_step: 5,
            ..UiConfig::default()
        });
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        input.extend([
            InputEvent::VolumeRotate(1),
            InputEvent::Back, // open overlay (MenuEnter on Main is a no-op)
            InputEvent::Back, // hide overlay
        ]);
        let mut nfc = None;
        let mut sys = SysProviders::mock("01:02", None, "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(5),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        assert_eq!(app.model.volume.percent(), Volume::DEFAULT_PERCENT + 5);
    }

    #[test]
    fn nfc_read_complete_emits_tap_ndef() {
        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        // Text NDEF "hi"
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"hi");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        let tag = MockTag {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef,
            locked: false,
        };
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(3),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::TapNdef(p)) if !p.ndef_message.is_empty()
            )),
            "expected TapNdef in {out:?}"
        );
        assert!(
            !led.flashes.is_empty(),
            "expected LED tap feedback after ReadComplete"
        );
    }

    // ---- UI_16 §8: Debug `Rec enables NFC poll` gates the idle NFC poll ------------------

    /// A text-NDEF `hi` tag: any text record reads as a `.noubin` key, so a tap is
    /// "recognised" and the play path is the one a real tap takes.
    fn nfc_text_tag() -> MockTag {
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"hi");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        MockTag {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef,
            locked: false,
        }
    }

    /// An app whose persisted Debug setting is `on` (or explicitly `off`), plus the temp dir
    /// that keeps its `play_mode.json` alive.
    fn nfc_gate_app(on: bool) -> (UiApp, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::play_mode::PLAY_MODE_FILE_NAME);
        let mut store = crate::play_mode::PlayModeStore::with_path(&path);
        store.settings.rec_enables_nfc_poll = Some(on);
        store.save().unwrap();
        let app = UiApp::new(UiConfig {
            play_mode_path: Some(path),
            ..UiConfig::default()
        });
        (app, dir)
    }

    /// Run `iters` frames of the real loop with `events` injected in the first one.
    fn run_nfc_frames(
        app: &mut UiApp,
        nfc: &mut Option<NfcBackend>,
        state: &mut LoopState,
        events: &[InputEvent],
        iters: u64,
    ) {
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        input.extend(events.iter().cloned());
        let mut sys = SysProviders::mock("00:00", Some(40), "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let exit = run_ui_loop(
            app,
            &mut display,
            &mut wifi,
            &mut input,
            nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            state,
            LoopLimits {
                max_iters: Some(iters),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        assert_eq!(exit, LoopExit::MaxIters, "the frames ran to the limit");
    }

    fn taps(app: &mut UiApp) -> usize {
        app.take_outgoing()
            .iter()
            .filter(|o| matches!(o, UiOutgoing::Ipc(IpcMessage::TapNdef(_))))
            .count()
    }

    /// Off (the default) is exactly the pre-UI_16 behaviour: the reader polls continuously.
    #[test]
    fn rec_gate_off_polls_the_nfc_continuously() {
        let (mut app, _dir) = nfc_gate_app(false);
        assert!(!app.nfc_rec_gate());
        let mut nfc = Some(NfcBackend::mock_with_tag(nfc_text_tag()));
        let mut state = LoopState::default();
        run_nfc_frames(&mut app, &mut nfc, &mut state, &[], 3);
        assert!(!state.nfc_rec_gate);
        assert!(state.nfc_poll_running, "Off: the poll never stops");
        assert_ne!(state.last_nfc_ms, u64::MAX, "polled on the first frame");
        assert_eq!(taps(&mut app), 1, "the tag in the field was read");
    }

    /// On starts with the poll stopped: a tag in the field is not read until REC.
    #[test]
    fn rec_gate_on_starts_with_the_poll_stopped() {
        let (mut app, _dir) = nfc_gate_app(true);
        assert!(app.nfc_rec_gate(), "the stored setting is On");
        let mut nfc = Some(NfcBackend::mock_with_tag(nfc_text_tag()));
        let mut state = LoopState::default();
        run_nfc_frames(&mut app, &mut nfc, &mut state, &[], 5);
        assert!(state.nfc_rec_gate);
        assert!(!state.nfc_poll_running, "On: stopped until REC");
        assert_eq!(state.last_nfc_ms, u64::MAX, "not a single poll was issued");
        assert_eq!(taps(&mut app), 0, "the tag was never read");
        assert!(app.compositor.status().is_none(), "silent");
    }

    /// REC starts the gated poll; a second REC stops it again. No popup, no notice.
    #[test]
    fn rec_press_starts_and_stops_the_gated_poll_silently() {
        let (mut app, _dir) = nfc_gate_app(true);
        let mut nfc = Some(NfcBackend::mock_empty());
        let mut state = LoopState::default();
        run_nfc_frames(&mut app, &mut nfc, &mut state, &[], 2);
        assert!(!state.nfc_poll_running);
        let rec = [InputEvent::ControlPressed(ControlRole::Record)];
        run_nfc_frames(&mut app, &mut nfc, &mut state, &rec, 2);
        assert!(state.nfc_poll_running, "REC starts polling");
        assert!(state.nfc_tap_wait_until_ms.is_none(), "no tap was taken");
        assert!(app.compositor.status().is_none(), "no notice when it starts");
        run_nfc_frames(&mut app, &mut nfc, &mut state, &rec, 2);
        assert!(!state.nfc_poll_running, "a second REC stops polling");
        assert!(app.compositor.status().is_none(), "no notice when it stops");
    }

    /// A tap while the gated poll runs stops the poll once playback actually starts.
    #[test]
    fn a_tapped_tag_that_plays_stops_the_gated_poll() {
        let (mut app, _dir) = nfc_gate_app(true);
        // The engine reports the tapped item playing: "successfully tapped and played".
        let mut s = SessionState {
            transport: TransportState::Playing,
            current_item: Some(PlayableItemId::new(1)),
            ..SessionState::default()
        };
        s.position_ms = 1_000;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(s, 1)));
        assert!(app.transport_playing());
        let mut nfc = Some(NfcBackend::mock_with_tag(nfc_text_tag()));
        let mut state = LoopState::default();
        run_nfc_frames(
            &mut app,
            &mut nfc,
            &mut state,
            &[InputEvent::ControlPressed(ControlRole::Record)],
            4,
        );
        assert_eq!(taps(&mut app), 1, "REC started the poll and the tag was read");
        assert!(app.transport_playing(), "the tap played");
        assert!(!state.nfc_poll_running, "played: polling stopped");
        assert!(
            state.nfc_tap_wait_until_ms.is_none(),
            "no give-up timer left behind"
        );
        // The only popup is the existing tap copy; the gate itself says nothing.
        let popup = app.compositor.status().unwrap().lines().join(" ");
        assert!(popup.contains("hi"), "{popup}");
        let lower = popup.to_lowercase();
        assert!(
            !lower.contains("rec") && !lower.contains("poll"),
            "the gate must not explain itself: {popup}"
        );
    }

    /// A tag the engine never plays must not stop the poll: the window simply expires, so an
    /// unrecognised tag cannot leave the reader switched off.
    #[test]
    fn an_accepted_tap_that_never_plays_leaves_the_poll_running() {
        let (mut app, _dir) = nfc_gate_app(true);
        let mut nfc = Some(NfcBackend::mock_with_tag(nfc_text_tag()));
        let mut state = LoopState::default();
        run_nfc_frames(
            &mut app,
            &mut nfc,
            &mut state,
            &[InputEvent::ControlPressed(ControlRole::Record)],
            4,
        );
        assert_eq!(taps(&mut app), 1, "the tap was accepted");
        assert!(!app.transport_playing(), "nothing played");
        assert!(state.nfc_tap_wait_until_ms.is_some(), "waiting for a play");
        assert!(state.nfc_poll_running, "the poll never stopped");
        // Past the window the tap is forgotten; the reader is still polling.
        let deadline = state.nfc_tap_wait_until_ms.unwrap();
        settle_tap_wait(&app, &mut state, deadline);
        assert!(state.nfc_tap_wait_until_ms.is_none());
        assert!(state.nfc_poll_running, "the reader was not left off");
        assert!(
            !app.transport_playing(),
            "settling only reads the transport"
        );
    }

    #[test]
    fn pending_associate_writes_instead_of_tap_ndef() {
        use plippa_ipc_proto::AssociateResponse;

        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"oldkey");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        let tag = MockTag {
            uid: vec![9, 9, 9],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef,
            locked: false,
        };
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState {
            pending_associate: Some(PendingAssociate {
                request_id: 42,
                playable_id: 1,
                title: "Tone".into(),
                noubin_key: "newkey".into(),
                noudata_relpath: "_tone.noudata".into(),
                item: None,
                new_doc: None,
                root: None,
                write_noudata_locally: false,
                keep_existing_key: false,
                previous_key: None,
                tag_locked: false,
                awaiting_verify: false,
            }),
            ..Default::default()
        };
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(5),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::AssociateResponse(AssociateResponse {
                    request_id: 42,
                    ok: true,
                    ..
                }))
            )),
            "expected AssociateResponse ok in {out:?}"
        );
        assert!(
            !out.iter().any(|o| matches!(o, UiOutgoing::Ipc(IpcMessage::TapNdef(_)))),
            "must not TapNdef/play while associate is pending: {out:?}"
        );
        assert!(state.pending_associate.is_none());
    }

    #[test]
    fn pending_associate_keep_existing_reads_instead_of_write() {
        use plippa_ipc_proto::AssociateResponse;

        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"KeepMe.noubin.XYZ");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        let tag = MockTag {
            uid: vec![9, 9, 9],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef.clone(),
            locked: false,
        };
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState {
            pending_associate: Some(PendingAssociate {
                request_id: 77,
                playable_id: 1,
                title: "Reuse".into(),
                noubin_key: String::new(),
                noudata_relpath: "_reuse.noudata".into(),
                item: None,
                new_doc: None,
                root: None,
                write_noudata_locally: false,
                keep_existing_key: true,
                previous_key: None,
                tag_locked: false,
                awaiting_verify: false,
            }),
            ..Default::default()
        };
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(5),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::AssociateResponse(AssociateResponse {
                    request_id: 77,
                    ok: true,
                    noubin_key: Some(k),
                    ..
                })) if k == "KeepMe.noubin.XYZ"
            )),
            "expected AssociateResponse with tag key in {out:?}"
        );
        assert!(
            !out.iter().any(|o| matches!(o, UiOutgoing::Ipc(IpcMessage::TapNdef(_)))),
            "must not TapNdef while keep-existing associate is pending: {out:?}"
        );
        // Tag NDEF must be unchanged (read-only associate).
        if let Some(NfcBackend::Mock { session }) = nfc.as_mut() {
            let after = session.device_mut().read_full_ndef().unwrap();
            assert_eq!(after, ndef);
        }
        assert!(state.pending_associate.is_none());
    }

    #[test]
    fn pending_associate_keep_existing_rejects_blank_key() {
        use plippa_ipc_proto::AssociateResponse;

        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"   ");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        let tag = MockTag {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef,
            locked: false,
        };
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState {
            pending_associate: Some(PendingAssociate {
                request_id: 78,
                playable_id: 1,
                title: "Blank".into(),
                noubin_key: String::new(),
                noudata_relpath: "_blank.noudata".into(),
                item: None,
                new_doc: None,
                root: None,
                write_noudata_locally: false,
                keep_existing_key: true,
                previous_key: None,
                tag_locked: false,
                awaiting_verify: false,
            }),
            ..Default::default()
        };
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(5),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::AssociateResponse(AssociateResponse {
                    request_id: 78,
                    ok: false,
                    ..
                }))
            )),
            "expected AssociateResponse failure for blank key in {out:?}"
        );
        assert!(state.pending_associate.is_none());
    }

    #[test]
    fn library_detail_associate_mock_nfc_completes() {
        use plippa_ipc_proto::AssociateResponse;
        use plippa_noubin::{normalise_noubin_key, LibraryRoot, LibraryWriter};
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("tone.wav"), b"RIFF").unwrap();
        let noudata = format!(
            r#"{{
              "playableItemTitle": "Tone",
              "mediaList": [{{"localFilePaths": ["tone.wav"]}}]
            }}"#
        );
        fs::write(root.join("_tone.noudata"), noudata).unwrap();
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();

        let mut app = UiApp::new(UiConfig::default());
        let mut lib_src = Some(crate::LibrarySource::open(Some(root)));
        let items = lib_src.as_ref().unwrap().list_items();
        assert_eq!(items.len(), 1);
        let playable_id = items[0].id;
        app.set_library(items);
        app.model.detail_playable_id = Some(playable_id);
        app.nav.force_push(crate::Route::Library);
        app.nav.force_push(crate::Route::LibraryDetail);
        app.nav.focus = 1; // Associate

        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), crate::Route::LibraryAssociate);

        let mut display = MockDisplay::new(320, 240);
        let mut input = MockInput::new();
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"old");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        let tag = MockTag {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef,
            locked: false,
        };
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();

        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut lib_src,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(8),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );

        assert!(
            state.pending_associate.is_none(),
            "pending should clear after NFC"
        );
        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::AssociateResponse(AssociateResponse {
                    ok: true,
                    ..
                }))
            )),
            "expected AssociateResponse ok, got {out:?}"
        );
        assert!(
            app.compositor
                .status()
                .is_some_and(|s| s.lines().iter().any(|l| l.contains("Associated"))),
            "expected status popup"
        );
        assert!(
            !app.model
                .banner
                .as_ref()
                .is_some_and(|s| s.contains("Associated")),
            "associate success must not sticky-set banner, got {:?}",
            app.model.banner
        );
        // Write-through should have set keys.
        let bytes = fs::read(root.join("_tone.noudata")).unwrap();
        let doc = parse_noudata(&bytes).unwrap();
        let key = doc.noubin_key.expect("noubin key written");
        let norm = normalise_noubin_key(&key);
        assert_eq!(doc.noubin_key_normalised.as_deref(), Some(norm.as_str()));
        assert_eq!(*app.nav.current(), crate::Route::LibraryDetail);
    }

    #[test]
    fn associating_one_track_writes_only_its_own_noudata() {
        use plippa_ipc_proto::AssociateResponse;
        use plippa_noubin::{LibraryKind, LibraryRoot, LibraryWriter, NoudataType};
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("Music/Album")).unwrap();
        fs::write(root.join("Music/Album/01.wav"), b"RIFF").unwrap();
        fs::write(root.join("Music/Album/02.wav"), b"RIFF").unwrap();
        fs::write(
            root.join("Music/Album/_album.noudata"),
            r#"{"playableItemTitle":"Album","playableItemProfile":"music","releaseData":{},"mediaList":[
                 {"mediaTitle":"One","localFilePaths":["01.wav"]},
                 {"mediaTitle":"Two","localFilePaths":["02.wav"]}]}"#,
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();
        let release_before = fs::read(root.join("Music/Album/_album.noudata")).unwrap();

        let mut app = UiApp::new(UiConfig::default());
        let mut lib_src = Some(crate::LibrarySource::open(Some(root)));
        let items = lib_src.as_ref().unwrap().list_items();
        let playable_id = items
            .iter()
            .find(|i| i.noudata_relpath == "Music/Album/_album.noudata")
            .expect("album row")
            .id;
        app.set_library(items);
        app.model.detail_playable_id = Some(playable_id);
        app.nav.force_push(crate::Route::Library);
        app.nav.force_push(crate::Route::LibraryDetail);
        app.nav.force_push(crate::Route::LibraryTracks);
        app.nav.focus = 1; // "Two" (position 1)
        // The track list is sourced from the reader by the loop; ask for it directly.
        let caps = app.cfg.capabilities.clone();
        let info = lib_src.as_ref().unwrap().detail_info(
            &app.model.detail_item().cloned().unwrap(),
            None,
            &caps,
        );
        app.model.detail_info = Some(info);

        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::MenuEnter, &mut wifi); // opens the track menu
        assert_eq!(*app.nav.current(), crate::Route::LibraryTrackActions);
        assert_eq!(
            app.model.track_menu_row().map(|t| t.title.as_str()),
            Some("Two")
        );
        app.nav.focus = 2; // Associate
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), crate::Route::LibraryAssociate);

        let mut display = MockDisplay::new(320, 240);
        let mut input = MockInput::new();
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"old");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        let tag = MockTag {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef,
            locked: false,
        };
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut lib_src,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(8),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );

        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::AssociateResponse(AssociateResponse {
                    ok: true,
                    ..
                }))
            )),
            "expected AssociateResponse ok, got {out:?}"
        );
        assert!(
            app.compositor
                .status()
                .is_some_and(|s| s.lines().iter().any(|l| l.contains("Associated Two"))),
            "the popup names the track, got {:?}",
            app.compositor.status()
        );

        // (a) the release document is untouched.
        let release_after = fs::read(root.join("Music/Album/_album.noudata")).unwrap();
        assert_eq!(
            release_before, release_after,
            "the release .noudata must be byte-identical"
        );
        // (b) the track has its own linkTrack next to the media, with the key.
        let bytes = fs::read(root.join("Music/Album/02.noudata")).expect("per-track file");
        let doc = parse_noudata(&bytes).unwrap();
        assert_eq!(doc.noudata_type, Some(NoudataType::LinkTrack));
        assert_eq!(doc.playable_item_title.as_deref(), Some("Two"));
        assert_eq!(
            doc.refer_to_release_noudata.as_deref(),
            Some("_album.noudata")
        );
        assert_eq!(doc.media_list.len(), 1);
        assert_eq!(
            doc.media_list[0].local_file_paths.as_deref(),
            Some(&["02.wav".to_string()][..])
        );
        let key = doc.noubin_key.clone().expect("key on the track file");
        assert_eq!(
            doc.noubin_key_normalised.as_deref(),
            Some(normalise_noubin_key(&key).as_str())
        );
        // (c) the index lists it as its own Track playable that refers to the release.
        let reader = plippa_noubin::LibraryReader::open_with(
            &LibraryRoot::new(root),
            plippa_noubin::OpenWait::Interactive,
        )
        .unwrap();
        let rec = reader
            .list_by_kind(LibraryKind::Track)
            .unwrap()
            .into_iter()
            .find(|r| r.noudata_relpath == "Music/Album/02.noudata")
            .expect("a Track record for the per-track file");
        assert_eq!(
            rec.refers_to_relpath.as_deref(),
            Some("Music/Album/_album.noudata")
        );
        assert!(rec.noubin_key_normalised.is_some());
        // (d) the flat Tracks list keeps one row per track (the release already supplies
        // them); the per-track file must not add a duplicate row, and the row for the
        // associated track — and only that row — reads as associated (NB_18 §3).
        drop(reader);
        let reopened = crate::LibrarySource::open(Some(root));
        let track_rows = reopened.list_kind(LibraryKind::Track);
        assert_eq!(
            track_rows
                .iter()
                .filter(|i| i.noudata_relpath == "Music/Album/_album.noudata")
                .count(),
            2,
            "two rows from the release: {track_rows:?}"
        );
        assert!(
            !track_rows
                .iter()
                .any(|i| i.noudata_relpath == "Music/Album/02.noudata"),
            "the per-track file must not duplicate a row: {track_rows:?}"
        );
        assert_eq!(
            track_rows
                .iter()
                .filter(|i| i.item_position == Some(1))
                .map(|i| i.associated)
                .collect::<Vec<_>>(),
            vec![true],
            "track 2 reads as associated: {track_rows:?}"
        );
        assert_eq!(
            track_rows
                .iter()
                .filter(|i| i.item_position == Some(0))
                .map(|i| i.associated)
                .collect::<Vec<_>>(),
            vec![false],
            "track 1 is untouched: {track_rows:?}"
        );
        // (e) track 1 did not gain a file of its own.
        assert!(!root.join("Music/Album/01.noudata").exists());
    }

    #[test]
    fn associating_the_release_still_writes_its_own_noudata() {
        use plippa_ipc_proto::AssociateResponse;
        use plippa_noubin::{LibraryRoot, LibraryWriter};
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("Music/Album")).unwrap();
        fs::write(root.join("Music/Album/01.wav"), b"RIFF").unwrap();
        fs::write(
            root.join("Music/Album/_album.noudata"),
            r#"{"playableItemTitle":"Album","playableItemProfile":"music","releaseData":{},"mediaList":[
                 {"mediaTitle":"One","localFilePaths":["01.wav"]}]}"#,
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();

        let mut app = UiApp::new(UiConfig::default());
        let mut lib_src = Some(crate::LibrarySource::open(Some(root)));
        let items = lib_src.as_ref().unwrap().list_items();
        let playable_id = items
            .iter()
            .find(|i| i.noudata_relpath == "Music/Album/_album.noudata")
            .expect("album row")
            .id;
        app.set_library(items);
        app.model.detail_playable_id = Some(playable_id);
        app.nav.force_push(crate::Route::Library);
        app.nav.force_push(crate::Route::LibraryDetail);
        app.nav.focus = 1; // the release's own Associate

        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), crate::Route::LibraryAssociate);

        let mut display = MockDisplay::new(320, 240);
        let mut input = MockInput::new();
        let mut payload = vec![2];
        payload.extend_from_slice(b"en");
        payload.extend_from_slice(b"old");
        let mut ndef = vec![0xD1, 0x01, payload.len() as u8, b'T'];
        ndef.extend_from_slice(&payload);
        let tag = MockTag {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type4,
            ndef_capacity: 128,
            ndef_message: ndef,
            locked: false,
        };
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut lib_src,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(8),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );

        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::AssociateResponse(AssociateResponse {
                    ok: true,
                    ..
                }))
            )),
            "expected AssociateResponse ok, got {out:?}"
        );
        let doc = parse_noudata(&fs::read(root.join("Music/Album/_album.noudata")).unwrap())
            .expect("release document");
        assert!(doc.noubin_key.is_some(), "the release itself keeps the key");
        assert!(!root.join("Music/Album/01.noudata").exists());
    }

    #[test]
    fn wifi_mock_updates_network_fields() {
        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        app.init_wifi(&mut wifi);
        let mut input = MockInput::new();
        // Back opens overlay → System (index 2) → Enter → Network (index 0) → Enter → Wi‑Fi
        // (the fifth Network row) → Enter. SECFIX_02: Wi-Fi lives under Network.
        // POLISH_01: the main menu strip is horizontal and follows the raw direction (right = next);
        // vertical lists (System, Network) keep the UI_16 §7 inversion (left/negative = down).
        input.extend([
            InputEvent::Back,
            InputEvent::MenuRotate(2), // System (Now Playing=0, Library=1)
            InputEvent::MenuEnter,
            InputEvent::MenuEnter, // Network
            // Up from the first row wraps to the last: Wi‑Fi, whatever rows precede it.
            InputEvent::MenuRotate(1),
            InputEvent::MenuEnter,
        ]);
        let mut nfc = None;
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(12),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        assert_eq!(*app.nav.current(), crate::Route::Wifi);
        let _ = wifi.connect("HomeLAN", "secret");
        app.sync_network_from_wifi(&wifi);
        assert_eq!(app.model.system.ip.as_deref(), Some("192.168.1.42"));
        assert!(app
            .model
            .system
            .portal_url
            .as_deref()
            .unwrap()
            .contains("192.168.1.42"));
    }

    #[test]
    fn temp_fan_providers_render_on_debug() {
        let mut app = UiApp::new(UiConfig::default()).with_system(SystemStatus::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        // Debug demoted off main menu — force_push for lab/telemetry check.
        app.nav.force_push(crate::Route::Debug);
        let mut nfc = None;
        let mut sys = SysProviders::mock("09:15", Some(51), "pwm 22%");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState::default();
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(5),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        assert_eq!(*app.nav.current(), crate::Route::Debug);
        assert_eq!(app.model.system.temperature_c, Some(51));
        assert!(app.model.system.fan_status.contains("pwm"));
        assert_eq!(app.model.system.time_hhmm, "09:15");
    }

    #[test]
    fn miss_notice_paints_status_popup() {
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(IpcMessage::UserNotice(UserNotice {
            severity: NoticeSeverity::Error,
            code: NOTICE_LIBRARY_MISS.into(),
            message: "No local media for this Noubin".into(),
        }));
        assert!(app.model.banner.as_ref().unwrap().contains("No local media"));
        let status = app.compositor.status().expect("miss popup");
        assert!(status.lines().iter().any(|l| l.contains("Tag not recognised")));
    }

    /// Run the mock loop with a pending associate against `tag`; return the response.
    fn associate_against(tag: MockTag) -> plippa_ipc_proto::AssociateResponse {
        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        let mut nfc = Some(NfcBackend::mock_with_tag(tag));
        let mut sys = SysProviders::mock("00:00", None, "n/a");
        let mut library = None;
        let mut conn = None;
        let mut led = crate::MockLedCue::default();
        let mut state = LoopState {
            pending_associate: Some(PendingAssociate {
                request_id: 7,
                playable_id: 1,
                title: "Tone".into(),
                noubin_key: "Tone.noubin.NEW123".into(),
                noudata_relpath: "_tone.noudata".into(),
                item: None,
                new_doc: None,
                root: None,
                write_noudata_locally: false,
                keep_existing_key: false,
                previous_key: None,
                tag_locked: false,
                awaiting_verify: false,
            }),
            ..Default::default()
        };
        let _ = run_ui_loop(
            &mut app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            &mut led,
            &mut state,
            LoopLimits {
                max_iters: Some(5),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
        assert!(state.pending_associate.is_none(), "associate must finish");
        let mut resp: Vec<_> = app
            .take_outgoing()
            .into_iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::AssociateResponse(r)) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(resp.len(), 1, "exactly one response: {resp:?}");
        resp.remove(0)
    }

    fn key_ndef(key: &str) -> Vec<u8> {
        encode_text_record(key, DEFAULT_TEXT_LANG).unwrap()
    }

    #[test]
    fn associate_blank_rewritable_tag_writes_and_verifies() {
        let tag = MockTag::blank_type4(vec![1], 128);
        let r = associate_against(tag);
        assert!(r.ok, "{r:?}");
        assert!(!r.tag_locked);
        assert_eq!(r.previous_key, None);
        assert_eq!(r.noubin_key.as_deref(), Some("Tone.noubin.NEW123"));
    }

    #[test]
    fn associate_keyed_rewritable_tag_reports_previous_key() {
        let tag = MockTag::blank_type2(vec![2], 144).with_ndef_message(key_ndef("Old.noubin.OLD999"));
        let r = associate_against(tag);
        assert!(r.ok && !r.tag_locked, "{r:?}");
        assert_eq!(r.previous_key.as_deref(), Some("Old.noubin.OLD999"));
        assert_eq!(r.noubin_key.as_deref(), Some("Tone.noubin.NEW123"));
    }

    #[test]
    fn associate_keyed_locked_tag_binds_the_key_it_has() {
        let tag = MockTag::blank_type4(vec![3], 128)
            .with_ndef_message(key_ndef("Fixed.noubin.LOCK1"))
            .locked();
        let r = associate_against(tag);
        assert!(r.ok, "{r:?}");
        assert!(r.tag_locked, "server must apply move semantics");
        assert_eq!(r.noubin_key.as_deref(), Some("Fixed.noubin.LOCK1"));
        assert_eq!(r.noubin_key_normalised.as_deref(), Some("fixed.noubin.lock1"));
        assert_eq!(r.previous_key, None);
    }

    #[test]
    fn associate_rewritable_tag_with_a_link_keeps_the_link_and_binds_it() {
        let link = "https://noubin.com/artist/release/";
        let tag = MockTag::blank_type2(vec![6], 144)
            .with_ndef_message(plippa_noubin::encode_uri_record(link).unwrap());
        let r = associate_against(tag);
        assert!(r.ok, "{r:?}");
        assert!(r.tag_locked, "server must apply move semantics, as for a locked tag");
        assert_eq!(r.noubin_key.as_deref(), Some(link));
        assert_eq!(r.previous_key, None);
    }

    #[test]
    fn associate_blank_locked_tag_fails_with_a_clear_message() {
        let tag = MockTag::blank_type4(vec![4], 128).locked();
        let r = associate_against(tag);
        assert!(!r.ok);
        let e = r.error.unwrap();
        assert!(e.contains("locked") && e.contains("no readable"), "{e}");
    }

    #[test]
    fn associate_tiny_capacity_tag_fails_untouched() {
        let tag = MockTag::blank_type4(vec![5], 8);
        let r = associate_against(tag);
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("capacity"));
    }

    // --- Channel (hardware) backend: write -> read-back verify, locked fallback -----------

    struct ChannelRig {
        app: UiApp,
        state: LoopState,
        nfc: Option<NfcBackend>,
        evt_tx: std::sync::mpsc::SyncSender<NfcServiceEvent>,
        cmd_rx: std::sync::mpsc::Receiver<crate::i2c_service::I2cCmd>,
    }

    impl ChannelRig {
        fn new() -> Self {
            let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel(64);
            let (evt_tx, evt_rx) = std::sync::mpsc::sync_channel(64);
            let client = NfcChannelClient::new(cmd_tx, evt_rx);
            let state = LoopState {
                pending_associate: Some(PendingAssociate {
                    request_id: 9,
                    playable_id: 1,
                    title: "Tone".into(),
                    noubin_key: "Tone.noubin.NEW123".into(),
                    noudata_relpath: "_tone.noudata".into(),
                    item: None,
                    new_doc: None,
                    root: None,
                    write_noudata_locally: false,
                    keep_existing_key: false,
                    previous_key: None,
                    tag_locked: false,
                    awaiting_verify: false,
                }),
                ..Default::default()
            };
            Self {
                app: UiApp::new(UiConfig::default()),
                state,
                nfc: Some(NfcBackend::Channel(client)),
                evt_tx,
                cmd_rx,
            }
        }

        fn read_complete(&self, key: &str) {
            let info = plippa_hw::TagInfo {
                uid: vec![1, 2, 3, 4],
                tag_type: TagType::Type4,
                ndef_capacity: Some(128),
            };
            self.evt_tx
                .send(NfcServiceEvent::Events(vec![NfcEvent::ReadComplete {
                    info,
                    ndef: key_ndef(key),
                }]))
                .unwrap();
        }

        fn write_result(&self, ok: bool, error: Option<&str>) {
            self.evt_tx
                .send(NfcServiceEvent::WriteResult {
                    ok,
                    error: error.map(str::to_string),
                })
                .unwrap();
        }

        fn step(&mut self) {
            let mut display = MockDisplay::new(240, 320);
            let mut wifi = MockWifi::default();
            let mut input = MockInput::new();
            let mut sys = SysProviders::mock("00:00", None, "n/a");
            let mut library = None;
            let mut conn = None;
            let mut led = crate::MockLedCue::default();
            let _ = run_ui_loop(
                &mut self.app,
                &mut display,
                &mut wifi,
                &mut input,
                &mut self.nfc,
                &mut sys,
                &mut library,
                &mut conn,
                &mut led,
                &mut self.state,
                LoopLimits {
                    max_iters: Some(1),
                    sleep: Duration::ZERO,
                    ..Default::default()
                },
            );
        }

        fn responses(&mut self) -> Vec<plippa_ipc_proto::AssociateResponse> {
            self.app
                .take_outgoing()
                .into_iter()
                .filter_map(|o| match o {
                    UiOutgoing::Ipc(IpcMessage::AssociateResponse(r)) => Some(r),
                    _ => None,
                })
                .collect()
        }

        fn wrote(&self) -> bool {
            self.cmd_rx
                .try_iter()
                .any(|c| matches!(c, crate::i2c_service::I2cCmd::WriteNdef { .. }))
        }
    }

    #[test]
    fn associate_channel_write_is_verified_by_a_fresh_read_before_success() {
        let mut rig = ChannelRig::new();
        rig.read_complete("Old.noubin.OLD999");
        rig.step();
        assert!(rig.wrote(), "write requested once the tag was seen");
        rig.write_result(true, None);
        rig.step();
        assert!(rig.responses().is_empty(), "no success before the read-back");
        assert!(rig.state.pending_associate.as_ref().unwrap().awaiting_verify);
        rig.read_complete("Tone.noubin.NEW123");
        rig.step();
        let r = rig.responses();
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].ok && !r[0].tag_locked, "{:?}", r[0]);
        assert_eq!(r[0].previous_key.as_deref(), Some("Old.noubin.OLD999"));
        assert!(rig.state.pending_associate.is_none());
    }

    #[test]
    fn associate_channel_verify_mismatch_fails_without_success() {
        let mut rig = ChannelRig::new();
        rig.read_complete("Old.noubin.OLD999");
        rig.step();
        rig.write_result(true, None);
        rig.step();
        rig.read_complete("Old.noubin.OLD999"); // write did not stick
        rig.step();
        let r = rig.responses();
        assert_eq!(r.len(), 1);
        assert!(!r[0].ok);
        assert!(r[0].error.as_deref().unwrap().contains("read back"));
    }

    #[test]
    fn associate_channel_locked_tag_falls_back_to_its_key() {
        let mut rig = ChannelRig::new();
        rig.read_complete("Fixed.noubin.LOCK1");
        rig.step();
        rig.write_result(false, Some(TAG_READ_ONLY_MSG));
        rig.step();
        let r = rig.responses();
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].ok && r[0].tag_locked);
        assert_eq!(r[0].noubin_key.as_deref(), Some("Fixed.noubin.LOCK1"));
    }

    #[test]
    fn associate_channel_locked_blank_tag_reports_clear_error() {
        let mut rig = ChannelRig::new();
        rig.step(); // blank tag: no ReadComplete is ever emitted
        rig.write_result(false, Some(TAG_READ_ONLY_MSG));
        rig.step();
        let r = rig.responses();
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(!r[0].ok);
        assert!(r[0].error.as_deref().unwrap().contains("locked"));
    }
    // ---- NB_14: prev/next buttons + LEDs -------------------------------------------------

    use plippa_board::{ControlRole, LedRole};
    use plippa_core::{PlayableItemId, SessionState, TransportState, Volume};
    use plippa_ipc_proto::{PlaybackCommand, PlaybackStatus};

    fn status_app(can_prev: bool, can_next: bool) -> UiApp {
        let mut app = UiApp::new(UiConfig::default());
        let session = SessionState {
            transport: TransportState::Paused,
            current_item: Some(PlayableItemId::new(1)),
            can_prev,
            can_next,
            ..SessionState::default()
        };
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        app
    }

    /// Run a few frames of `events` with no engine connection (commands stay queued).
    fn run_presses(app: &mut UiApp, led: &mut crate::MockLedCue, events: &[ControlRole]) {
        let mut display = MockDisplay::new(240, 320);
        let mut wifi = MockWifi::default();
        let mut input = MockInput::new();
        input.extend(events.iter().map(|r| InputEvent::ControlPressed(*r)));
        let (mut nfc, mut sys, mut library, mut conn) =
            (None, SysProviders::mock("09:15", None, ""), None, None);
        let mut state = LoopState::default();
        let _ = run_ui_loop(
            app,
            &mut display,
            &mut wifi,
            &mut input,
            &mut nfc,
            &mut sys,
            &mut library,
            &mut conn,
            led,
            &mut state,
            LoopLimits {
                max_iters: Some(3),
                sleep: Duration::ZERO,
                ..Default::default()
            },
        );
    }

    fn queued(app: &mut UiApp) -> Vec<PlaybackCommand> {
        app.take_outgoing()
            .into_iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn nav_leds_follow_published_availability() {
        let mut led = crate::MockLedCue::default();
        for (p, n) in [(true, true), (true, false), (false, false)] {
            sync_nav_leds(&status_app(p, n), &mut led);
            assert_eq!(led.role_colours.contains_key(&LedRole::PrevTrackButton), p);
            assert_eq!(led.role_colours.contains_key(&LedRole::NextTrackButton), n);
        }
        // Lit level is the shared steady colour.
        sync_nav_leds(&status_app(true, true), &mut led);
        assert_eq!(led.role_colours[&LedRole::NextTrackButton], NAV_LED_ON);
        // No status at all (engine not connected): both off.
        sync_nav_leds(&UiApp::new(UiConfig::default()), &mut led);
        assert!(led.role_colours.is_empty());
    }

    #[test]
    fn available_press_sends_command_and_flashes_from_the_press() {
        let mut app = status_app(true, true);
        let mut led = crate::MockLedCue::default();
        run_presses(&mut app, &mut led, &[ControlRole::NextTrack, ControlRole::PrevTrack]);
        assert_eq!(queued(&mut app), vec![PlaybackCommand::Next, PlaybackCommand::Previous]);
        assert_eq!(
            led.role_flashes,
            vec![
                (LedRole::NextTrackButton, NAV_LED_FLASH),
                (LedRole::PrevTrackButton, NAV_LED_FLASH)
            ]
        );
    }

    #[test]
    fn unavailable_press_does_nothing_and_does_not_flash() {
        // Album on the last track: Next unlit.
        let mut app = status_app(true, false);
        let mut led = crate::MockLedCue::default();
        run_presses(&mut app, &mut led, &[ControlRole::NextTrack]);
        assert!(queued(&mut app).is_empty());
        assert!(led.role_flashes.is_empty());
        // Nothing loaded / no engine: neither button reacts.
        let mut app = UiApp::new(UiConfig::default());
        run_presses(&mut app, &mut led, &[ControlRole::PrevTrack, ControlRole::NextTrack]);
        assert!(queued(&mut app).is_empty());
        assert!(led.role_flashes.is_empty());
    }

    #[test]
    fn buttons_without_led_roles_still_navigate() {
        let mut app = status_app(true, true);
        let profile = crate::control_roles::tests_support::minimal_profile();
        let mut led = crate::MockLedCue::for_profile(&profile); // no LED roles
        run_presses(&mut app, &mut led, &[ControlRole::NextTrack, ControlRole::PrevTrack]);
        assert_eq!(queued(&mut app), vec![PlaybackCommand::Next, PlaybackCommand::Previous]);
        assert!(led.role_flashes.is_empty() && led.role_colours.is_empty());
        sync_nav_leds(&app, &mut led);
        assert!(led.role_colours.is_empty());
    }

    #[test]
    fn static_leds_glow_rec_and_back_and_skip_absent_roles() {
        use crate::led::{BACK_LED_GLOW, REC_LED_GLOW};
        let profile = crate::control_roles::tests_support::poc2_profile();
        let mut led = crate::MockLedCue::for_profile(&profile);
        sync_static_leds(&mut led);
        assert_eq!(led.role_colours[&LedRole::RecordButton], REC_LED_GLOW);
        assert_eq!(led.role_colours[&LedRole::MenuBackButton], BACK_LED_GLOW);
        // A board without those backlights: no commands, no panic.
        let profile = crate::control_roles::tests_support::minimal_profile();
        let mut bare = crate::MockLedCue::for_profile(&profile);
        sync_static_leds(&mut bare);
        assert!(bare.role_colours.is_empty());
    }

    #[test]
    fn playmode_led_idle_then_pressed_and_absent_role_is_safe() {
        use crate::led::{TRANSPORT_LED_IDLE, TRANSPORT_LED_PRESSED};
        let mut app = UiApp::new(UiConfig::default());
        let mut led = crate::MockLedCue::default();
        let mut last = None;
        sync_playmode_led(&app, &mut led, &mut last);
        assert_eq!(led.role_colours[&LedRole::PlayModeButton], TRANSPORT_LED_IDLE);
        let mut wifi = crate::MockWifi::default();
        app.handle_input(
            InputEvent::ControlButton { role: ControlRole::PlayMode, down: true, at_ms: 10 },
            &mut wifi,
        );
        sync_playmode_led(&app, &mut led, &mut last);
        assert_eq!(led.role_colours[&LedRole::PlayModeButton], TRANSPORT_LED_PRESSED);
        app.handle_input(
            InputEvent::ControlButton { role: ControlRole::PlayMode, down: false, at_ms: 20 },
            &mut wifi,
        );
        // The release is a tap (PLAY_04), so the mode moved off the default rung: the
        // backlight is amber while it settles, then idle once the engine reports Normal/Off.
        sync_playmode_led(&app, &mut led, &mut last);
        assert_eq!(led.role_colours[&LedRole::PlayModeButton], PLAYMODE_LED_ACTIVE);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            SessionState::default(),
            2,
        )));
        sync_playmode_led(&app, &mut led, &mut last);
        assert_eq!(led.role_colours[&LedRole::PlayModeButton], TRANSPORT_LED_IDLE);
        // A board without the LED role: nothing happens.
        let profile = crate::control_roles::tests_support::minimal_profile();
        let mut bare = crate::MockLedCue::for_profile(&profile);
        let mut last = None;
        sync_playmode_led(&app, &mut bare, &mut last);
        assert!(bare.role_colours.is_empty());
    }

    #[test]
    fn playmode_led_shows_an_active_colour_off_the_default_rung() {
        use crate::led::{PLAYMODE_LED_ACTIVE, TRANSPORT_LED_IDLE};
        use plippa_core::{OrderMode, RepeatMode};
        let mut app = UiApp::new(UiConfig::default());
        let mut led = crate::MockLedCue::default();
        let mut wifi = crate::MockWifi::default();
        let mut last = None;
        sync_playmode_led(&app, &mut led, &mut last);
        assert_eq!(last, Some((false, plippa_core::PLAY_MODE_CYCLE[0])));
        assert_eq!(led.role_colours[&LedRole::PlayModeButton], TRANSPORT_LED_IDLE);
        // A shuffle status (no button press) turns the backlight amber.
        let session = SessionState {
            order_mode: OrderMode::Shuffle,
            repeat_mode: RepeatMode::Off,
            ..SessionState::default()
        };
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        sync_playmode_led(&app, &mut led, &mut last);
        assert_eq!(led.role_colours[&LedRole::PlayModeButton], PLAYMODE_LED_ACTIVE);
        // Back on the default rung: idle white again.
        let session = SessionState::default();
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 2,
        )));
        sync_playmode_led(&app, &mut led, &mut last);
        assert_eq!(led.role_colours[&LedRole::PlayModeButton], TRANSPORT_LED_IDLE);
    }

    #[test]
    fn power_led_turns_amber_for_the_warning_then_returns_to_blue() {
        use crate::led::POWER_LED_WARN;
        let mut app = UiApp::new(UiConfig {
            poweroff_armed: true,
            ..UiConfig::default()
        });
        let mut led = crate::MockLedCue::default();
        let mut last = None;
        let mut wifi = crate::MockWifi::default();

        // Unarmed press arms the tracker, then a real press reaches 1 s.
        app.handle_input(InputEvent::PowerButton { down: false }, &mut wifi);
        app.handle_input(InputEvent::PowerButton { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 1_000 }, &mut wifi);
        sync_power_led(&app, &mut led, &mut last);
        assert_eq!(led.role_colours[&LedRole::PowerLed], POWER_LED_WARN);

        app.handle_input(InputEvent::PowerButton { down: false }, &mut wifi);
        sync_power_led(&app, &mut led, &mut last);
        assert!(
            !led.role_colours.contains_key(&LedRole::PowerLed),
            "no override = the composer's standard blue"
        );

        // A board without the power LED role is a no-op.
        let profile = crate::control_roles::tests_support::minimal_profile();
        let mut bare = crate::MockLedCue::for_profile(&profile);
        let mut last = None;
        sync_power_led(&app, &mut bare, &mut last);
        assert!(bare.role_colours.is_empty());
    }

    fn ffrw_app(can_rw: bool, can_ff: bool) -> UiApp {
        let mut app = UiApp::new(UiConfig::default());
        let session = SessionState {
            transport: TransportState::Paused,
            current_item: Some(PlayableItemId::new(1)),
            can_rw,
            can_ff,
            ..SessionState::default()
        };
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        app
    }

    fn edge(app: &mut UiApp, role: ControlRole, down: bool, at_ms: u64) {
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::ControlButton { role, down, at_ms }, &mut wifi);
    }

    /// WARP_12: loaded -> both glow; nothing loaded -> off; a press brightens, a 20 ms tap
    /// stays bright for the 90 ms minimum, then back to the glow. Injected clock only.
    #[test]
    fn ffrw_leds_glow_brighten_on_press_and_hold_the_minimum() {
        use crate::warp_ui::{FFRW_LED_IDLE, FFRW_LED_MIN_PRESSED_MS, FFRW_LED_PRESSED};
        let mut led = crate::MockLedCue::default();
        let mut last = [None; 2];
        let mut app = ffrw_app(true, true);
        sync_transport_leds(&app, &mut led, 0, &mut last);
        assert_eq!(led.role_colours[&LedRole::RewindButton], FFRW_LED_IDLE);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_IDLE);
        // FF pressed at t=100: bright at once; Rewind untouched.
        edge(&mut app, ControlRole::FastForward, true, 100);
        sync_transport_leds(&app, &mut led, 100, &mut last);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_PRESSED);
        assert_eq!(led.role_colours[&LedRole::RewindButton], FFRW_LED_IDLE);
        // Released after 20 ms: still bright until the minimum has passed.
        edge(&mut app, ControlRole::FastForward, false, 120);
        sync_transport_leds(&app, &mut led, 120, &mut last);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_PRESSED);
        sync_transport_leds(&app, &mut led, 100 + FFRW_LED_MIN_PRESSED_MS - 1, &mut last);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_PRESSED);
        sync_transport_leds(&app, &mut led, 100 + FFRW_LED_MIN_PRESSED_MS, &mut last);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_IDLE);
        // Held for a long time: stays bright however long.
        edge(&mut app, ControlRole::Rewind, true, 1_000);
        sync_transport_leds(&app, &mut led, 9_000, &mut last);
        assert_eq!(led.role_colours[&LedRole::RewindButton], FFRW_LED_PRESSED);
        edge(&mut app, ControlRole::Rewind, false, 9_010);
        sync_transport_leds(&app, &mut led, 9_011, &mut last);
        assert_eq!(led.role_colours[&LedRole::RewindButton], FFRW_LED_IDLE);
        // Steady colours, not flashes.
        assert!(led.role_flashes.is_empty());
    }

    #[test]
    fn ffrw_leds_follow_engine_availability_and_nothing_loaded_stays_dark() {
        use crate::warp_ui::FFRW_LED_IDLE;
        let mut led = crate::MockLedCue::default();
        let mut last = [None; 2];
        // Only FF usable (e.g. at the first track of a list the engine decides): RW dark.
        let app = ffrw_app(false, true);
        sync_transport_leds(&app, &mut led, 0, &mut last);
        assert!(!led.role_colours.contains_key(&LedRole::RewindButton));
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_IDLE);
        // Nothing loaded / no engine: dark, and a press does not brighten anything.
        let mut app = UiApp::new(UiConfig::default());
        let mut led = crate::MockLedCue::default();
        let mut last = [None; 2];
        edge(&mut app, ControlRole::FastForward, true, 10);
        edge(&mut app, ControlRole::Rewind, true, 10);
        sync_transport_leds(&app, &mut led, 20, &mut last);
        assert!(led.role_colours.is_empty());
        // Engine status flips availability: the LED repaints (status-driven sync).
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            SessionState {
                current_item: Some(PlayableItemId::new(1)),
                can_rw: true,
                can_ff: true,
                ..SessionState::default()
            },
            2,
        )));
        edge(&mut app, ControlRole::FastForward, false, 30);
        edge(&mut app, ControlRole::Rewind, false, 30);
        sync_transport_leds(&app, &mut led, 5_000, &mut last);
        assert_eq!(led.role_colours[&LedRole::RewindButton], FFRW_LED_IDLE);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_IDLE);
    }

    #[test]
    fn ffrw_leds_without_roles_make_no_calls_and_buttons_without_leds_still_work() {
        // Board without FF/RW LED roles: nothing is ever set, nothing panics.
        let profile = crate::control_roles::tests_support::minimal_profile();
        let mut led = crate::MockLedCue::for_profile(&profile);
        let mut last = [None; 2];
        let mut app = ffrw_app(true, true);
        edge(&mut app, ControlRole::FastForward, true, 5);
        sync_transport_leds(&app, &mut led, 6, &mut last);
        assert!(led.role_colours.is_empty() && led.role_flashes.is_empty());
        assert_eq!(last, [None; 2], "no cache churn for absent roles");
        // LEDs but no presses ever (no buttons): availability glow only.
        let mut led = crate::MockLedCue::default();
        let mut last = [None; 2];
        let app = ffrw_app(true, true);
        sync_transport_leds(&app, &mut led, 1_000_000, &mut last);
        assert_eq!(led.role_colours.len(), 2);
    }

    #[test]
    fn ffrw_led_is_only_pushed_on_change() {
        struct Counting(crate::MockLedCue, usize);
        impl LedCue for Counting {
            fn on_tap_feedback(&mut self, _: u64) {}
            fn has_role(&self, r: LedRole) -> bool {
                self.0.has_role(r)
            }
            fn set_role_colour(&mut self, r: LedRole, c: Option<crate::led::Rgb>) {
                self.1 += 1;
                self.0.set_role_colour(r, c);
            }
        }
        let mut led = Counting(crate::MockLedCue::default(), 0);
        let mut last = [None; 2];
        let app = ffrw_app(true, true);
        for t in 0..50 {
            sync_transport_leds(&app, &mut led, t * 33, &mut last);
        }
        assert_eq!(led.1, 2, "two roles, one push each");
    }

    /// WARP_21: a warped set speed puts a pulse on the matching button (over the idle steady),
    /// purple for FF above 100% and for reverse on RW, blue for FF below 100%; 100% is steady.
    #[test]
    fn ffrw_pulses_follow_the_set_speed_and_ride_over_the_idle_steady() {
        use crate::warp_ui::{
            FFRW_LED_IDLE, FFRW_LED_PRESSED, FFRW_LED_SLOW_BLUE,
        };
        let set_rate = |app: &mut UiApp, rev: u64, permille: i32| {
            app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
                SessionState {
                    transport: TransportState::Playing,
                    current_item: Some(PlayableItemId::new(1)),
                    can_rw: true,
                    can_ff: true,
                    target_rate_permille: permille,
                    ..SessionState::default()
                },
                rev,
            )));
        };
        let mut app = ffrw_app(true, true);
        let mut led = crate::MockLedCue::default();
        let mut last = [None; 2];

        // 1.25x: FF pulses purple; RW just glows.
        set_rate(&mut app, 2, 1250);
        sync_transport_leds(&app, &mut led, 0, &mut last);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_IDLE);
        let ff = led.role_pulses[&LedRole::FastForwardButton];
        assert_eq!((ff.lo, ff.hi), (FFRW_LED_IDLE, FFRW_LED_PRESSED));
        assert!(!led.role_pulses.contains_key(&LedRole::RewindButton));

        // 0.75x: FF pulses blue.
        set_rate(&mut app, 3, 750);
        sync_transport_leds(&app, &mut led, 33, &mut last);
        assert_eq!(led.role_pulses[&LedRole::FastForwardButton].hi, FFRW_LED_SLOW_BLUE);

        // Reverse: RW pulses purple (there is no steady reverse state); FF is quiet.
        set_rate(&mut app, 4, -1000);
        sync_transport_leds(&app, &mut led, 66, &mut last);
        assert!(!led.role_pulses.contains_key(&LedRole::FastForwardButton));
        let rw = led.role_pulses[&LedRole::RewindButton];
        assert_eq!((rw.lo, rw.hi), (FFRW_LED_IDLE, FFRW_LED_PRESSED));

        // Back to 100%: both steady, no pulses.
        set_rate(&mut app, 5, 1000);
        sync_transport_leds(&app, &mut led, 99, &mut last);
        assert!(led.role_pulses.is_empty());
        assert_eq!(led.role_colours[&LedRole::RewindButton], FFRW_LED_IDLE);
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_IDLE);
    }

    /// WARP_21: a control the engine says is unusable stays dark — its steady level is off and
    /// the set speed must not put a pulse on it either.
    #[test]
    fn ffrw_pulse_is_suppressed_for_an_unusable_control() {
        use crate::warp_ui::FFRW_LED_IDLE;
        let set = |app: &mut UiApp, rev: u64, permille: i32, can_rw: bool, can_ff: bool| {
            app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
                SessionState {
                    transport: TransportState::Playing,
                    current_item: Some(PlayableItemId::new(1)),
                    can_rw,
                    can_ff,
                    target_rate_permille: permille,
                    ..SessionState::default()
                },
                rev,
            )));
        };
        let mut led = crate::MockLedCue::default();
        let mut last = [None; 2];
        // Both unusable, warped forward (FF would pulse at 1.25x): nothing lights up.
        let mut app = ffrw_app(false, false);
        set(&mut app, 2, 1250, false, false);
        sync_transport_leds(&app, &mut led, 0, &mut last);
        assert!(led.role_pulses.is_empty(), "no pulse on an unusable control");
        assert!(led.role_colours.is_empty(), "an unusable control stays dark");
        assert!(crate::warp_ui::ffrw_pulse_ff(1250).is_some(), "the speed alone would pulse");
        // RW unusable while the speed is reverse: the pulse is the RW one and is dropped.
        let mut app = ffrw_app(false, true);
        set(&mut app, 3, -1000, false, true);
        sync_transport_leds(&app, &mut led, 33, &mut last);
        assert!(!led.role_colours.contains_key(&LedRole::RewindButton));
        assert!(!led.role_pulses.contains_key(&LedRole::RewindButton));
        assert_eq!(led.role_colours[&LedRole::FastForwardButton], FFRW_LED_IDLE);
        assert!(!led.role_pulses.contains_key(&LedRole::FastForwardButton));
    }

    #[test]
    fn other_control_roles_and_buttons_are_unchanged() {
        let mut app = status_app(true, true);
        let mut led = crate::MockLedCue::default();
        run_presses(
            &mut app,
            &mut led,
            &[ControlRole::Rewind, ControlRole::FastForward, ControlRole::Record],
        );
        assert!(queued(&mut app).is_empty());
        assert!(led.role_flashes.is_empty());
    }

    /// WARP_07: repaint throttle is 250 ms while warped / scanning / seeking, 1 s when pure.
    #[test]
    fn position_repaint_interval_follows_warp_state() {
        let mut s = plippa_core::SessionState::default();
        assert_eq!(pos_dirty_interval_ms(&s), 1_000);
        s.target_rate_permille = 1250;
        assert_eq!(pos_dirty_interval_ms(&s), 250);
        s.target_rate_permille = 1000;
        s.scan = plippa_core::ScanState::Forward;
        assert_eq!(pos_dirty_interval_ms(&s), 250);
        s.scan = plippa_core::ScanState::None;
        s.seek_target_ms = Some(5);
        assert_eq!(pos_dirty_interval_ms(&s), 250);
    }
}
