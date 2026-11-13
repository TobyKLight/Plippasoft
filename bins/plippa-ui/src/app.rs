//! UI application: inputs → nav/screens → IPC out; IPC in → screen updates.

use crate::assign::{AssignableId, AssignStore};
use crate::accessories::{
    address_like_name, pair_failure_text, prompt_keyboard, AccessoriesState, AccessoryStore,
    FoundItem, PairStage, MIN_SCAN_SECONDS,
};
use crate::accessory_worker::{AccessoryCmd, AccessoryHandle, AccessoryMsg};
use crate::audio::{
    delay_blocked, delay_step, volume_step, wanted_from_route, AudioRow, AudioState, AudioStore,
    AudioView, AUDIO_ROWS,
};
use crate::compose::{Compositor, STATUS_POPUP_IDLE_MS};
use crate::control::{AdjustParam, ControlGraph, InteractionMode, NpMode, NpPush, NpTarget};
use crate::diag;
use crate::display::{DisplayBackend, FrameBuffer, Rect};
use crate::hold::{HoldConfig, HoldDetector, HoldEvent, HoldTarget};
use crate::input::InputEvent;
use crate::keyboard::{KeyboardAction, KeyboardEvent, KeyboardField};
use crate::nav::{NavAction, NavStack, Route};
use plippa_hw::{
    auto_connect_target, normalise_address, props_match_address, AccessoryEvent, AccessoryKind,
    PairPrompt, PromptAnswer, ScanWindow, TransportAction, MAX_SCAN_SECONDS,
};
use crate::play_mode::{
    speed_row_step, warp_mode_text, zoom_step, DebugRow, PlayModesRow, PlayModeRow, PlayModeStore,
    TimecodeUnit, DEBUG_ROWS, PLAYBACK_SETTINGS_ROWS,
};
use crate::power::{PowerHold, PowerHoldConfig, PowerHoldEvent};
use crate::record_leds::RecordInput;
use crate::art::{ArtFade, ArtSubject, CoverTarget, CoverViewer, NowPlayingArt};
use crate::library::LibrarySource;
use crate::analysis::{AnalysisAction, AnalysisView};
use crate::screens::{
    screen_text, BrowseRequest, LibraryDetailAction, LibraryItem, ScreenModel, StoreItem,
    SystemStatus, TrackAction, WaveState, WaveView, WAVE_CLOCK_MS, WAVE_FRAME_MS,
};
use crate::store_api::{
    DownloadState, HttpStoreApi, PortalSettingsInfo, StoreApi, StoreApiError, TapAnswer, TapBlocked,
};
use crate::store_browse::{
    ConnectAction, ConnectPhase, ConnectView, DeleteAction, DeletePhase, DeleteView, DownloadPhase,
    DownloadedRelease, ReleaseAction, ReleasePhase, ReleaseView, ReleasesPhase, ReleasesRow,
    ReleasesView, StoresAction, StoresPhase, StoresView, RELEASE_PAGE_SIZE,
};
use crate::store_tap::{
    apply_error, apply_progress, apply_sync, blocked_message, CoverState, StoreTapDecision,
    StoreTapPhase, StoreTapView, SyncStep,
    DOWNLOAD_POLL_MS,
};
use crate::network::{parse_typed_pin, NetworkPhase, NetworkRow, NetworkView};
use crate::pinpad::{PinAction, PinEvent, PinPad};
use crate::store_worker::{Pin, StoreEvent, StoreRequest, StoreWorker};
use crate::warp_ui::{
    ladder_step_enabled, tap_end_text, tap_rate_text, tempo_apply_click, tempo_step_permille,
    Commit, DesiredRate, HoldTap, HoldTapEvent, JogAccel, JogDisplay, JogGesture, NeighbourCtx, PlayKey,
    PlayKeyAction, StatusSample, TempoSticky, Velocity, PLAYMODE_HOLD_MS, PLAY_RESET_POPUP,
};
use crate::wifi::{MockWifi, WifiBackend, WifiForm, WifiFormEvent};
use plippa_board::{ControlRole, LedRole};
use plippa_core::{Capabilities, PlayableItemId, TransportState, Volume};
use plippa_hw::NfcEvent;
use plippa_ipc_proto::{
    IpcMessage, PlaybackCommand, TapKeyPayload, TapNdefPayload, UserNotice, NOTICE_LIBRARY_MISS, NOTICE_MEDIA_UNPLAYABLE,
    NOTICE_NO_FIXED_ORDER,
};
use plippa_core::{NowPlayingEntry, NowPlayingEntryKind};
use plippa_noubin::{encode_playable_id, extract_noubin_key, LibraryKind};
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::time::Duration;

use crate::ui::tokens::BG;

/// Outbound signals beyond Postcard IPC (launcher control).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiOutgoing {
    Ipc(IpcMessage),
    /// Stop the app. `power_off` distinguishes the hold-to-power path (host halt) from a
    /// plain stop; only the launcher decides what to do with it (PWRSW_01).
    ShutdownRequest { power_off: bool },
    /// Tap feedback after full NDEF read (SFX/LED coordination with MVP_06).
    TapFeedback { tap_id: u64 },
    /// Local associate selection ready for NFC write (server/UI flow).
    ///
    /// `item` is the `mediaList` position when the owner picked a single track's `Associate`
    /// (`Library > Browse tracks` → track menu); `None` associates the whole playable
    /// (release, loose track or playlist). NB_18: a track gets its own `linkTrack` `.noudata`
    /// next to its media file, and the release's document is not touched.
    AssociateSelected {
        playable_id: i64,
        item: Option<u32>,
        title: String,
    },
    /// User backed out of the associate-wait screen — clear [`PendingAssociate`].
    CancelAssociate,
}

/// Runtime config for the UI loop.
#[derive(Debug)]
pub struct UiConfig {
    pub width: u16,
    pub height: u16,
    pub power: PowerHoldConfig,
    pub volume_step: u8,
    /// Host power-off is armed (`PLIPPA_POWEROFF=1`): the 4 s hold asks the launcher to
    /// power the host off, and the acknowledgement says "Powering off…" rather than
    /// "Shutting down…". Dev / CI runs leave it false (PWRSW_01).
    pub poweroff_armed: bool,
    /// Override Encoder A binding file (tests / lab). `None` → env default path.
    pub assign_path: Option<PathBuf>,
    /// Override the `play_mode.json` path (tests / lab). `None` -> env default path (unbound
    /// in unit tests so the host state dir is never touched).
    pub play_mode_path: Option<PathBuf>,
    /// Override the `audio_outputs.json` path (tests / lab), same rule as the play-mode path.
    pub audio_path: Option<PathBuf>,
    /// Override the `accessories.json` path (tests / lab).
    pub accessories_path: Option<PathBuf>,
    /// Injected radio worker. `None` means the board has no accessory radio (or the feature is
    /// off); the screen then says `No radio` and no fake data is shown.
    pub accessories: Option<AccessoryHandle>,
    /// BCS_07: `host:port` of the local store service the tap screen asks. `None` (the default
    /// in tests) leaves the device without a store: a tap that names a store link then says so
    /// plainly instead of pretending to look anything up.
    pub store_base: Option<String>,
    /// SECFIX_02: the file the server writes its per-boot control token to (`<ipc dir>/
    /// portal.token`). With `store_base` set, every call to the service carries that token, which
    /// is how the loopback control listener knows this is the device's own UI. `None` sends no
    /// token (a test server that does not ask for one).
    pub control_token_path: Option<PathBuf>,
    /// What this board can decode (its board profile). A library row whose format is outside
    /// it is marked unplayable on this player; the index itself stays portable (MEDIA_01).
    pub capabilities: Capabilities,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            width: 320,
            height: 240,
            power: PowerHoldConfig::default(),
            volume_step: 2,
            poweroff_armed: false,
            assign_path: None,
            play_mode_path: None,
            audio_path: None,
            accessories_path: None,
            accessories: None,
            store_base: None,
            control_token_path: None,
            capabilities: Capabilities::desktop_mock(),
        }
    }
}

/// The link a tap should be asked about: a Bandcamp page when the tag declares one, else the
/// first HTTPS URL. A tag is untrusted, so only absolute HTTPS links are considered, and a
/// control character in the middle of one disqualifies it.
pub fn pick_store_url(candidate_urls: &[String]) -> Option<String> {
    let usable = |url: &&String| {
        url.starts_with("https://")
            && url.len() <= 512
            && url.contains('.')
            && plippa_noubin::sanitise_line(url, 512) == **url
    };
    candidate_urls
        .iter()
        .filter(usable)
        .find(|url| url.contains("bandcamp.com"))
        .or_else(|| candidate_urls.iter().find(usable))
        .cloned()
}

/// "Play from here" waiting for the engine to publish the loaded collection's list
/// (`LoadCollection` just sent) or for its cue rows (track jump done, cue jump to follow).
/// The UI never edits the `nowPlayingList`; it only sends `JumpToEntry` for the row it finds.
#[derive(Debug, Clone)]
struct PendingJump {
    /// Engine-side id of the collection; a list only counts once `session.current_item` is it.
    target: PlayableItemId,
    /// 0-based `mediaList` position of the track to start.
    track_pos: u32,
    title: String,
    /// Chapter start inside that track (`None` = the track itself).
    cue_start_ms: Option<u64>,
    /// The track row has been jumped to; waiting for its cue rows.
    jumped_track: bool,
    /// Lists that arrived without the row we need.
    tries: u8,
}

/// Lists without the wanted row tolerated before a pending jump is dropped.
const PENDING_JUMP_MAX_TRIES: u8 = 4;

/// Largest `.wfm` sidecar we are willing to read: the 48-byte header plus the payload cap.
const MAX_WFM_BYTES: u64 =
    (plippa_core::waveform::MAX_PAYLOAD_BYTES + plippa_core::waveform::HEADER_LEN) as u64;

/// Retry interval while the sidecar is missing or not `COMPLETE` (WAVE_02).
const WAVE_RETRY_MS: u64 = 1_000;

/// PWRSW_01: hold-to-power warning text (sticky from 1 s until release or 4 s).
const POWER_HOLD_WARNING: &str = "Keep holding to power off";

/// PWRSW_01: how long the "Powering off…" acknowledgement is shown before the app asks the
/// launcher to stop — long enough for the popup to be composed and the audio to pause.
const POWER_OFF_ACK_MS: u64 = 700;

/// Repaint interval for the `Decimal` footer clock: tenths change at 10 Hz, so the waveform
/// tick need not run at 30 FPS when there are no bars (UI_14 §7).
const WAVE_DECIMAL_MS: u64 = 100;

/// Share of the pixels **outside** the waveform band that must change before a whole frame is
/// presented (the spec's "≥ 60 % of the screen" intent; see [`diff_needs_full_frame`]).
const PRESENT_FULL_PERCENT: u32 = 60;

/// How long the auto-opened playlist overlay stays up after the last encoder detent or
/// Next / Previous press (UI_13). A turn re-arms it, so active scrolling never times out.
const LIST_OVERLAY_HOLD_MS: u64 = 4_000;

/// In-process UI controller (hardware frame loop plugs into this).
pub struct UiApp {
    pub cfg: UiConfig,
    pub nav: NavStack,
    pub model: ScreenModel,
    /// Layered compositor (screen + main-menu overlay + status popup).
    pub compositor: Compositor,
    /// Navigate vs edit control graph (screens own painting of selection).
    pub controls: ControlGraph,
    /// Assignable Encoder A bindings (loaded at start; saved on assign).
    pub assign: AssignStore,
    /// Fired hold-duration events (menu enter / assignable); product actions deferred.
    pub hold_events: Vec<HoldEvent>,
    power: PowerHold,
    /// PWRSW_01: acknowledged shutdown deadline in loop time (`None` = no shutdown pending).
    shutdown_at: Option<u64>,
    /// The outgoing request has been emitted; all further input is ignored.
    shutdown_sent: bool,
    menu_enter_hold: HoldDetector,
    assignable_hold: HoldDetector,
    now_ms: u64,
    tap_id: u64,
    outgoing: Vec<UiOutgoing>,
    frame: FrameBuffer,
    /// Last applied [`PlaybackStatus::revision`]; older snapshots are dropped.
    last_status_revision: u64,
    /// Set when compositor auto-hide/clear needs a redraw (consumed by the loop).
    needs_redraw: bool,
    /// Index lookups queued by navigation; serviced by [`Self::service_browse`].
    pending_browse: Vec<BrowseRequest>,
    pending_jump: Option<PendingJump>,
    /// Prev/next button LED roles pressed while available; drained by the loop runner
    /// ([`Self::take_led_flashes`]) so the flash starts on the press, not the engine ack (NB_14).
    led_flashes: Vec<plippa_board::LedRole>,
    /// Tempo knob state (WARP_07): click-rate estimator, sticky 100%, optimistic set speed.
    tempo_vel: Velocity,
    tempo_sticky: TempoSticky,
    desired_rate: DesiredRate,
    /// Engine socket connected (set by the loop). Offline: the optimistic speed is held.
    engine_connected: bool,
    /// Play button timing FSM (WARP_08).
    play_key: PlayKey,
    /// Persisted Playback settings / Debug choices (WARP_10, UI_16).
    pub play_mode: PlayModeStore,
    /// Persisted audio choices (`audio_outputs.json`, AUDIO_04).
    pub audio: AudioStore,
    /// The engine's last `AudioRoute` plus the Delay screen's edit state (AUDIO_04).
    pub audio_state: AudioState,
    /// Persisted accessory choices (`accessories.json`, ACC_03).
    pub accessory_store: AccessoryStore,
    /// The radio's mirrored state and the Pair new/detail bookkeeping.
    pub accessories: AccessoriesState,
    /// The worker handle; all backend I/O goes through it (never from this loop).
    accessory_handle: Option<AccessoryHandle>,
    /// PlayMode button (SW2): hold [`PLAYMODE_HOLD_MS`] opens System > Playback settings.
    playmode_key: HoldTap,
    /// Rewind / FF button edge stamps for the backlights (WARP_12).
    rewind_edges: ButtonStamp,
    ff_edges: ButtonStamp,
    /// Tap / hold classifiers of Rewind and FF (WARP_13: taps; holds are WARP_18).
    rewind_key: HoldTap,
    ff_key: HoldTap,
    /// WARP_18: which FF/RW hold owns the running scan (`-1` RW, `+1` FF).
    scan_owner: Option<i8>,
    /// UI_15/UI_18: the encoder-selected speed of the held-scan, in permille of the usual
    /// cassette scan speed (`plippa_core::SCAN_SPEED_LADDER_PERMILLE`). **Sticky while a track
    /// is loaded**: it survives every release and re-hold, and only returns to the default when
    /// the loaded item changes. A scan starts at the default, so a sticky value is re-sent with
    /// each hold's `ScanStart`. **Scan-only** — the session's set speed (`desired_rate` /
    /// `target_rate_permille`) is never written from here, and the release restores that.
    scan_speed_permille: i32,
    /// UI_18: the item [`scan_speed_permille`](Self::scan_speed_permille) was chosen on. A
    /// different item means the stored speed belongs to a track that is no longer loaded, so
    /// the next hold starts from the default again.
    scan_speed_item: Option<PlayableItemId>,
    /// UI_14/WARP_21: the FF/RW-hold + Play speed-snap gesture. **Disabled by UI_15** (Play
    /// toggles pause/play again while FF/RW is held): kept so the mechanism is not lost, but
    /// nothing sets it any more, so the guards that read it are inert.
    #[allow(dead_code)]
    ffrw_reset_lock: bool,
    /// Status revision for which each warp-mode command was last (re)sent (T10 reconcile).
    speed_sent_rev: Option<u64>,
    seek_sent_rev: Option<u64>,
    outside_sent_rev: Option<u64>,
    /// Jog (WARP_15): live target gesture, acceleration state, and the display model (WARP_16).
    jog: JogGesture,
    jog_accel: JogAccel,
    jog_display: JogDisplay,
    /// ART_08: the playing item's cover (resolver, loader, cache) and its scan / jog fade.
    art: NowPlayingArt,
    art_fade: ArtFade,
    /// ART_09: the Library cover screen and the detail art box, and the image shown on the
    /// former (0-based).
    cover_viewer: CoverViewer,
    cover_index: usize,
    /// Last composed frame, for the partial-present diff (WAVE_02).
    prev_frame: Option<FrameBuffer>,
    /// Loop time of the last waveform repaint request (rate chosen in [`Self::tick_waveform`]).
    last_wave_paint: u64,
    /// Loop time of the last sidecar read attempt (bounded retry).
    wave_probe_at: u64,
    /// `(size, mtime_ns)` of the sidecar when it last failed to decode: a failure is only
    /// retried when the file changes, never on a timer.
    wave_fail_stamp: Option<(u64, i64)>,
    /// UI_13: deadline of the auto-opened playlist overlay (`None` = not auto-opened).
    list_overlay_until_ms: Option<u64>,
    /// Whether the user has scrolled the auto-opened overlay. Enter on an unscrolled overlay
    /// closes it without jumping (the cursor is on the row that is already playing).
    list_overlay_scrolled: bool,
    /// UI_CONTROL_01: a physical / accessory Next or Previous was just pressed with the list
    /// overlay in list mode; the engine's answer (list or status) moves the encoder cursor to
    /// the playing row. Cleared once the cursor moved, when the person scrolls, and with the
    /// overlay, so a stale flag can never move the cursor later.
    np_follow_pending: bool,
    /// The playing row when [`Self::np_follow_pending`] was set: the flag ends once the
    /// engine reports a different one (a Previous that only restarts the entry keeps it).
    np_follow_from_row: usize,
    /// BCS_07: the one store worker; every store call is queued here, never made on this loop.
    store: Option<StoreWorker>,
    /// A download whose screen was hidden ("Hide" while it runs): `(session, next poll)`.
    /// The worker keeps polling and the person is told when it lands.
    store_watch: Option<(String, u64)>,
    /// BCS_08: a "Remove from Plippa" press on the release screen waiting for the library
    /// lookup that names the item, so the confirm opens as soon as the detail lands.
    pending_remove: Option<String>,
    /// BCS_11: decoded store covers by `(release id, side)`, bounded like library art.
    store_covers: crate::art::StoreCoverCache,
}

/// Press / release stamps (loop clock, ms) of one transport button, for its backlight level
/// (WARP_12; WARP_13/18 hang the tap/hold logic on the same edges).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ButtonStamp {
    pub pressed_at: Option<u64>,
    pub released_at: Option<u64>,
}

/// Whether the Previous / Next controls are usable, as published by the engine (NB_14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NavAvailability {
    pub can_prev: bool,
    pub can_next: bool,
}

/// One FF/RW backlight's state: the steady level (`None` = off) plus an optional WARP_21
/// speed pulse that the composer animates over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransportLedState {
    pub steady: Option<crate::led::Rgb>,
    pub pulse: Option<crate::led::TransportPulse>,
}

impl UiApp {
    pub fn new(mut cfg: UiConfig) -> Self {
        let assign = match &cfg.assign_path {
            Some(p) => AssignStore::load_or_default(p),
            // Unit tests: stay unbound and avoid touching the host state dir.
            None if cfg!(test) => AssignStore::default(),
            None => AssignStore::load_default(),
        };
        let play_mode = match &cfg.play_mode_path {
            Some(p) => PlayModeStore::load_or_default(p),
            None if cfg!(test) => PlayModeStore::default(),
            None => PlayModeStore::load_default(),
        };
        let audio = match &cfg.audio_path {
            Some(p) => AudioStore::load_or_default(p),
            None if cfg!(test) => AudioStore::default(),
            None => AudioStore::load_default(),
        };
        let audio_state = AudioState::default();
        let accessory_store = match &cfg.accessories_path {
            Some(p) => AccessoryStore::load_or_default(p),
            None if cfg!(test) => AccessoryStore::default(),
            None => AccessoryStore::load_default(),
        };
        let accessories = AccessoriesState::new(
            cfg.accessories.is_some(),
            accessory_store.settings.clone(),
        );
        let accessory_handle = cfg.accessories.take();
        // UI_16 §5: the finest jog click is derived from the stored Fine time unit (13 ms at
        // the 75 fps default) and re-read live on every jog click.
        let fine_jog_step_ms = u64::from(play_mode.settings.resolve_jog_step());
        let frame = FrameBuffer::new(cfg.width, cfg.height);
        let mut model = ScreenModel {
            play_mode: play_mode.settings.view(),
            audio: AudioView {
                route: audio_state.route.clone(),
                settings: audio.settings.clone(),
                editing: audio_state.edit,
            },
            accessories: accessories.clone(),
            ..ScreenModel::default()
        };
        // UI_13: the live zoom is seeded once at startup; changing the persisted default
        // later must not yank the current window.
        model.wave_window_ms = play_mode.settings.resolve_waveform_zoom();
        // BCS_07: the store worker only exists when this device was told where the service
        // is; a tap with no worker degrades to the "no store connected" screen.
        let store = cfg.store_base.as_ref().map(|base| {
            let api = match cfg.control_token_path.as_ref() {
                Some(path) => HttpStoreApi::with_token_file(base.clone(), path.clone()),
                None => HttpStoreApi::new(base.clone()),
            };
            StoreWorker::new(Box::new(api))
        });
        Self {
            power: PowerHold::new(cfg.power),
            shutdown_at: None,
            shutdown_sent: false,
            menu_enter_hold: HoldDetector::new(HoldTarget::MenuEnter, HoldConfig::default()),
            assignable_hold: HoldDetector::new(
                HoldTarget::AssignableEncoder,
                HoldConfig::default(),
            ),
            cfg,
            nav: NavStack::new(),
            model,
            compositor: Compositor::new(),
            controls: ControlGraph::dummy(),
            assign,
            hold_events: Vec::new(),
            now_ms: 0,
            tap_id: 0,
            outgoing: Vec::new(),
            led_flashes: Vec::new(),
            frame,
            last_status_revision: 0,
            needs_redraw: false,
            pending_browse: Vec::new(),
            pending_jump: None,
            tempo_vel: Velocity::new(),
            tempo_sticky: TempoSticky::default(),
            desired_rate: DesiredRate::default(),
            engine_connected: true,
            play_key: PlayKey::new(),
            play_mode,
            audio,
            audio_state,
            accessory_store,
            accessories,
            accessory_handle,
            playmode_key: HoldTap::with_threshold(PLAYMODE_HOLD_MS),
            rewind_edges: ButtonStamp::default(),
            ff_edges: ButtonStamp::default(),
            rewind_key: HoldTap::new(),
            ff_key: HoldTap::new(),
            scan_owner: None,
            scan_speed_permille: plippa_core::SCAN_SPEED_DEFAULT_PERMILLE,
            scan_speed_item: None,
            ffrw_reset_lock: false,
            speed_sent_rev: None,
            seek_sent_rev: None,
            outside_sent_rev: None,
            jog: JogGesture::new(),
            jog_accel: JogAccel::new(fine_jog_step_ms),
            jog_display: JogDisplay::new(),
            art: NowPlayingArt::new(),
            art_fade: ArtFade::new(),
            cover_viewer: CoverViewer::new(),
            cover_index: 0,
            prev_frame: None,
            last_wave_paint: 0,
            wave_probe_at: 0,
            wave_fail_stamp: None,
            list_overlay_until_ms: None,
            list_overlay_scrolled: false,
            np_follow_pending: false,
            np_follow_from_row: 0,
            store,
            store_watch: None,
            pending_remove: None,
            store_covers: crate::art::StoreCoverCache::new(),
        }
    }

    /// Engine is warped (set speed != 100%, voice active) or a seek family action is running
    /// (scan, chase). Uses the optimistic set speed while it is fresh. Read at the Play press
    /// edge to choose Immediate vs Deferred (WARP_08). An open jog gesture counts too.
    fn warped_or_seeking(&self) -> bool {
        let unity = plippa_core::RATE_UNITY_PERMILLE;
        if self.jog.is_open() {
            return true;
        }
        if let Some(v) = self.desired_rate.fresh(self.now_ms) {
            return v != unity
                || self
                    .model
                    .playback
                    .as_ref()
                    .is_some_and(|p| p.session.scan != plippa_core::ScanState::None
                        || p.session.seek_target_ms.is_some());
        }
        self.model.playback.as_ref().is_some_and(|p| {
            let s = &p.session;
            s.warp_active
                || s.target_rate_permille != unity
                || s.scan != plippa_core::ScanState::None
                || s.seek_target_ms.is_some()
        })
    }

    /// True while an FF/RW hold scan is running (the button is down, or the engine still
    /// reports a scan that the release has not settled yet). A seek chase is **not** a scan.
    fn scan_held(&self) -> bool {
        self.scan_owner.is_some()
            || self
                .model
                .playback
                .as_ref()
                .is_some_and(|p| p.session.scan != plippa_core::ScanState::None)
    }

    /// Should a Play press be *deferred* to its release (WARP_08) rather than toggling at once?
    ///
    /// False during an FF/RW hold scan: there Play must be a plain pause/play toggle with the
    /// LED following immediately. Deferring it there also re-armed the WARP_21 long-press
    /// "reset to pure", which silently threw away the owner's set speed mid-scan — exactly the
    /// gesture UI_15 disabled. Every other warp/seek case keeps the deferred behaviour.
    fn play_defers(&self) -> bool {
        self.warped_or_seeking() && !self.scan_held()
    }

    /// True while a Rewind or FastForward button is physically down (WARP_21).
    ///
    /// UI_15: kept with the disabled hold+Play gesture (`ffrw_play_reset`); nothing calls it
    /// while Play is an ordinary toggle again.
    pub fn transport_hold_active(&self) -> bool {
        self.rewind_key.is_down() || self.ff_key.is_down()
    }

    /// Would handling `ev` right now toggle Play/Pause? (The loop shows the optimistic Play LED
    /// from this, before the app handles the event.)
    ///
    /// UI_15: no longer suppressed while FF/RW is held — Play is an ordinary pause/play toggle
    /// during a scan again (the UI_14 hold+Play speed snap is disabled). The answer stays
    /// `false` on a *press* during a warp/scan because [`PlayKey`] then defers the toggle to the
    /// release, which is exactly when the command goes out.
    pub fn event_toggles_play(&self, ev: &InputEvent) -> bool {
        match ev {
            InputEvent::PlayStop => true,
            InputEvent::ControlButton {
                role: ControlRole::Play,
                down,
                at_ms,
            } => self
                .play_key
                .would_toggle(*down, *at_ms, self.play_defers()),
            _ => false,
        }
    }

    /// Toggle Play/Pause (engine is the transport authority; command only).
    fn toggle_play(&mut self) {
        self.note_menu_activity();
        let playing = matches!(
            self.model.playback.as_ref().map(|p| p.transport),
            Some(TransportState::Playing)
        );
        let has_item = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item.is_some());
        // Engine Play silently no-ops with nothing loaded — surface that here instead of a
        // dead button press.
        if !playing && !has_item {
            eprintln!("plippa-ui: PlayStop → nothing loaded");
            self.compositor.show_status("Nothing loaded", self.now_ms);
            return;
        }
        let cmd = if playing {
            PlaybackCommand::Pause
        } else {
            PlaybackCommand::Play
        };
        eprintln!(
            "plippa-ui: PlayStop → {cmd:?} (was {})",
            if playing {
                "Playing"
            } else {
                "Paused/Stopped"
            }
        );
        self.outgoing
            .push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(cmd)));
    }

    /// Speed-only status popups (tap rate, tap end, play reset) are redundant on Now Playing:
    /// the footer and the Adjust band already show the speed (UI_14 §5). Every other route
    /// keeps them, and no other popup is affected. One place so the rule cannot drift.
    fn show_speed_status(&mut self, text: &str) {
        if *self.nav.current() != Route::Main {
            self.compositor.show_status(text, self.now_ms);
        }
    }

    /// Long-held Play: speed family back to pure 100% (WARP_08). Keeps the transport state, all
    /// play-mode settings and any running scan / chase (only `ResetRate` is sent).
    fn reset_to_pure(&mut self) {
        self.reset_rate(PlaybackCommand::ResetRate);
    }

    /// Shared body of both 100 % resets: optimistic readout, popup rule, LED flash, redraw. The
    /// only difference is the command (`ResetRate` ramps, `ResetRateInstant` snaps).
    fn reset_rate(&mut self, cmd: PlaybackCommand) {
        self.note_menu_activity();
        self.desired_rate
            .set(plippa_core::RATE_UNITY_PERMILLE, self.now_ms);
        self.sync_rate_override();
        self.queue_cmd(cmd);
        self.show_speed_status(PLAY_RESET_POPUP);
        self.led_flashes.push(LedRole::PlayButton);
        self.needs_redraw = true;
    }

    /// WARP_21/UI_14: the FF/RW-hold + Play speed snap.
    ///
    /// **Disabled by UI_15** (owner decision): Play while FF/RW is held is an ordinary
    /// pause/play toggle again, so this is never called. It is kept — not deleted — so the
    /// gesture can be restored without rebuilding it (call it from the Play handler and the
    /// lock guards in `on_tick` / the FF/RW handler become live again).
    #[allow(dead_code)]
    fn ffrw_play_reset(&mut self) {
        self.ffrw_reset_lock = true;
        if self.scan_owner.take().is_some() {
            self.queue_cmd(PlaybackCommand::ScanStop);
        }
        self.reset_rate(PlaybackCommand::ResetRateInstant);
    }

    fn on_play_action(&mut self, action: Option<PlayKeyAction>) {
        match action {
            Some(PlayKeyAction::Toggle) => self.toggle_play(),
            Some(PlayKeyAction::Reset) => self.reset_to_pure(),
            None => {}
        }
    }

    /// The loop reports the engine connection. While offline the readout keeps following the
    /// optimistic set speed; on reconnect the engine status wins (tempo is never re-sent).
    pub fn set_engine_connected(&mut self, connected: bool) {
        let reconnected = connected && !self.engine_connected;
        self.engine_connected = connected;
        if reconnected {
            // A (new) engine: its revisions start over, and it knows nothing of the stored
            // play-mode choices (T10): send both once; each status that still differs
            // re-sends at most once per revision.
            self.last_status_revision = 0;
            let (speed, seek) = self.play_mode.settings.resolve_warp();
            self.queue_cmd(PlaybackCommand::SetSpeedWarpMode(speed));
            self.queue_cmd(PlaybackCommand::SetSeekWarpMode(seek));
            self.speed_sent_rev = None;
            self.seek_sent_rev = None;
            self.outside_sent_rev = None;
            // WARP_19: the engine starts with "Seek outside track" off: only an explicit On
            // needs sending (a later mismatch is reconciled from the status).
            if self.play_mode.settings.resolve_seek_outside() {
                self.queue_cmd(PlaybackCommand::SetSeekOutsideTrack(true));
            }
        }
    }

    /// FF / RW availability as published by the engine (`session.can_ff / can_rw`, the single
    /// source of truth); no status = unavailable. Returns `(can_rw, can_ff)`.
    pub fn ffrw_availability(&self) -> (bool, bool) {
        self.model
            .playback
            .as_ref()
            .map(|p| (p.session.can_rw, p.session.can_ff))
            .unwrap_or_default()
    }

    /// Backlight levels of the Rewind and FF buttons at loop time `now_ms`:
    /// `(rewind, ff)`, each `None` = off. Steady glow while available, the pressed level while
    /// held and for at least `TRANSPORT_LED_MIN_PRESSED_MS` after the press.
    pub fn ffrw_led_levels(&self, now_ms: u64) -> (Option<crate::led::Rgb>, Option<crate::led::Rgb>) {
        let (rw, ff) = self.ffrw_led_states(now_ms);
        (rw.steady, ff.steady)
    }

    /// Full FF/RW backlight state at loop time `now_ms`, `(rewind, ff)`: the steady level
    /// (idle purple / pressed purple / off) plus the speed pulse when the button is idle
    /// (WARP_21). The pulse rides over the steady colour and carries the speed so the composer
    /// can animate it.
    pub fn ffrw_led_states(&self, now_ms: u64) -> (TransportLedState, TransportLedState) {
        let (can_rw, can_ff) = self.ffrw_availability();
        let permille = self.model.set_speed_permille();
        let rw_pulse = crate::warp_ui::ffrw_pulse_rw(permille);
        let ff_pulse = crate::warp_ui::ffrw_pulse_ff(permille);
        let state = |avail: bool, s: &ButtonStamp, pulse: Option<crate::led::TransportPulse>| {
            let steady = crate::warp_ui::ffrw_led_level(avail, s.pressed_at, s.released_at, now_ms);
            // Only a usable, idle control pulses: a held (or just-released) button shows its
            // pressed colour, and an unusable one stays dark (WARP_21).
            let pulse = (steady == Some(crate::warp_ui::FFRW_LED_IDLE))
                .then_some(pulse)
                .flatten();
            TransportLedState { steady, pulse }
        };
        (
            state(can_rw, &self.rewind_edges, rw_pulse),
            state(can_ff, &self.ff_edges, ff_pulse),
        )
    }

    /// True while the PlayMode button (SW2) is held (LED pressed level).
    pub fn playmode_pressed(&self) -> bool {
        self.playmode_key.is_down()
    }

    /// PLAY_04: the session play-mode rung as the engine last reported it (optimistic after a
    /// tap until the next status confirms it). Drives the SW2 cycle and the SW2 LED.
    pub fn play_mode_state(&self) -> plippa_core::PlayModeState {
        self.model.play_mode_state()
    }

    /// One SW2 tap: advance the five-rung cycle and send **only** the fields that changed
    /// (order first, then repeat). The mirror moves at once; the engine confirms through the
    /// next status. No retry and no reconcile re-send: a same-value `SetOrderMode` is a no-op
    /// in the engine by design, and a UI loop would reshuffle a shuffled list.
    ///
    /// Works with nothing loaded (the setting applies to the next load), so it is not gated on
    /// `playback.is_some()`.
    fn cycle_play_mode(&mut self) {
        let cur = self.play_mode_state();
        let next = plippa_core::cycle_next(cur);
        if next.order != cur.order {
            self.queue_cmd(PlaybackCommand::SetOrderMode(next.order));
        }
        if next.repeat != cur.repeat {
            self.queue_cmd(PlaybackCommand::SetRepeatMode(next.repeat));
        }
        if next == cur {
            return;
        }
        self.model.order_mode = next.order;
        self.model.repeat_mode = next.repeat;
        let text = format!(
            "Play mode\n{}",
            plippa_core::play_mode_text(next)
        );
        self.compositor.show_status(&text, self.now_ms);
        self.needs_redraw = true;
    }

    /// Open System > Playback settings from SW2: pushed on the current stack so Back returns
    /// to the
    /// origin screen. Ignored on the Wi-Fi keyboard, the associate-wait screen and when the
    /// screen is already open.
    fn open_play_mode(&mut self) {
        let route = self.nav.current().clone();
        if route == Route::SystemPlayMode || route == Route::LibraryAssociate {
            return;
        }
        if route == Route::Wifi && self.model.wifi.as_ref().is_some_and(|f| f.in_keyboard()) {
            return;
        }
        self.note_menu_activity();
        if self.compositor.main_menu_visible() {
            self.compositor.hide_main_menu();
        }
        self.nav.push(Route::SystemPlayMode);
        self.needs_redraw = true;
    }

    fn play_mode_changed(&mut self) {
        if let Err(e) = self.play_mode.save() {
            eprintln!("plippa-ui: play_mode save failed: {e}");
        }
        self.model.play_mode = self.play_mode.settings.view();
        // PLAY_04: `view()` is a fresh settings view (unity speed); keep the live set speed
        // that the row shows. It is session state and never part of the settings.
        self.sync_rate_override();
        self.needs_redraw = true;
    }

    /// Arm / re-arm the auto-opened playlist overlay (UI_13): four seconds from `now_ms`.
    /// `scrolled` records whether Enter may jump (true) or must only close (false).
    fn arm_list_overlay(&mut self, scrolled: bool) {
        self.list_overlay_until_ms = Some(self.now_ms.saturating_add(LIST_OVERLAY_HOLD_MS));
        self.list_overlay_scrolled = scrolled;
        if scrolled {
            // The person is driving the list: their cursor wins over the engine's answer.
            self.np_follow_pending = false;
        }
        self.needs_redraw = true;
    }

    /// UI_CONTROL_01: the encoder's reaction to a physical (or accessory) Next / Previous
    /// that was just queued. Only Now Playing is touched, and only when the encoder is
    /// operating the list: a ring parameter (Tempo, jog, ...) stays entirely under the
    /// person's control.
    fn np_after_transport(&mut self) {
        if *self.nav.current() != Route::Main || !self.model.list_panel_visible() {
            return;
        }
        match self.model.np_focus.mode {
            NpMode::Adjust(_) => {}
            NpMode::Select => {
                self.model.np_focus.mode = NpMode::List;
                self.model.np_focus.target = NpTarget::List;
                self.model.np_focus.cursor = self.model.now_playing_row().unwrap_or(0);
                self.arm_list_overlay(false);
                self.begin_np_follow();
            }
            NpMode::List => {
                self.arm_list_overlay(false);
                self.begin_np_follow();
            }
        }
    }

    fn begin_np_follow(&mut self) {
        self.np_follow_pending = true;
        self.np_follow_from_row = self.model.now_playing_row().unwrap_or(0);
    }

    /// Apply a pending transport follow (see [`Self::np_follow_pending`]): called from both
    /// the list and the status handlers, since the engine's two answers can arrive in either
    /// order. The flag ends once the cursor actually moved to the new playing row.
    fn apply_np_follow(&mut self) {
        if !self.np_follow_pending {
            return;
        }
        if self.list_overlay_until_ms.is_none()
            || self.model.np_focus.mode != NpMode::List
            || !self.model.list_panel_visible()
        {
            return;
        }
        let row = self.model.now_playing_row().unwrap_or(0);
        if self.model.np_focus.cursor != row {
            self.model.np_focus.cursor = row;
            self.needs_redraw = true;
        }
        if row != self.np_follow_from_row {
            self.np_follow_pending = false;
        }
    }

    /// Forget the overlay deadline (Back, Jump, timeout, or leaving Now Playing).
    fn clear_list_overlay(&mut self) {
        self.list_overlay_until_ms = None;
        self.list_overlay_scrolled = false;
        self.np_follow_pending = false;
    }

    /// Enter on the merged Playback settings screen (UI_16 §1, PLAY_04).
    ///
    /// The four session-mode rows come first and toggle or cycle on the click like they did on
    /// their own screen. A two-option settings row (`Speed warp`, `Seek warp`, `Seek outside
    /// track`, `Art LEDs`) also changes on the click. A longer ladder (`Playback speed`,
    /// `Default zoom`, `Fine time`) **enters an editing state** for that row instead: entering
    /// does not change the value, rotation steps it (clamped) and Enter leaves again. The one
    /// new value is persisted on exit, in [`Self::play_mode_leave_edit`].
    fn play_mode_enter(&mut self) {
        let row = PLAYBACK_SETTINGS_ROWS[self.nav.focus.min(PLAYBACK_SETTINGS_ROWS.len() - 1)];
        if self.model.play_mode.editing == row.setting() && row.setting().is_some() {
            self.play_mode_leave_edit();
            return;
        }
        // Defensive: a value being edited can only belong to the focused row (rotation steps
        // the value, it never moves the cursor), but persisting before any other change keeps
        // the file and the screen in step if that ever changes.
        if self.model.play_mode.editing.is_some() {
            self.play_mode_leave_edit();
        }
        if let Some(mode) = row.mode() {
            self.play_modes_enter(mode);
            return;
        }
        let Some(row) = row.setting() else {
            return;
        };
        if row.edits_by_scroll() {
            // Enter only arms the row: no value change, no popup that could flicker while the
            // encoder turns. The accent value column is the visible cue.
            self.model.play_mode.editing = Some(row);
            self.needs_redraw = true;
            return;
        }
        let flip = |m: plippa_core::WarpMode| match m {
            plippa_core::WarpMode::PitchCorrect => plippa_core::WarpMode::PitchShift,
            plippa_core::WarpMode::PitchShift => plippa_core::WarpMode::PitchCorrect,
        };
        let text = match row {
            PlayModeRow::SpeedWarp => {
                let next = flip(self.play_mode.settings.resolve_warp().0);
                self.play_mode.settings.warp.speed = Some(next);
                self.queue_cmd(PlaybackCommand::SetSpeedWarpMode(next));
                format!("Speed: {}", warp_mode_text(next).to_lowercase())
            }
            PlayModeRow::SeekWarp => {
                let next = flip(self.play_mode.settings.resolve_warp().1);
                self.play_mode.settings.warp.seek = Some(next);
                self.queue_cmd(PlaybackCommand::SetSeekWarpMode(next));
                format!("Seek: {}", warp_mode_text(next).to_lowercase())
            }
            PlayModeRow::SeekOutsideTrack => {
                let next = !self.play_mode.settings.resolve_seek_outside();
                self.play_mode.settings.seek_outside_track = Some(next);
                self.queue_cmd(PlaybackCommand::SetSeekOutsideTrack(next));
                format!("Seek outside track: {}", if next { "On" } else { "Off" })
            }
            PlayModeRow::ArtLeds => {
                // Read live by `record_led_input` on the next loop pass: no restart.
                let next = !self.play_mode.settings.resolve_art_leds();
                self.play_mode.settings.art_leds = Some(next);
                format!("Art LEDs: {}", if next { "On" } else { "Off" })
            }
            // Handled above: these rows are edited by scrolling.
            PlayModeRow::PlaybackSpeed | PlayModeRow::WaveformZoom | PlayModeRow::FineTime => {
                return
            }
        };
        self.play_mode_changed();
        self.compositor.show_status(&text, self.now_ms);
    }

    /// PLAY_04: set the live set speed from the Playback speed row. One call per detent, via
    /// the same path the TEMPO ring uses ([`DesiredRate`] optimistic value + one coalesced
    /// `SetRate`), so the row value, the footer and the engine cannot drift apart.
    ///
    /// Never sends 0: [`speed_row_step`] has no zero rung. Works with nothing loaded (the
    /// engine stores `target_rate_permille` and it applies to the next start).
    fn playback_speed_set(&mut self, permille: i32) {
        if !plippa_core::rate_permille_valid(permille) {
            // Unreachable through the ladder; a guard so a future caller cannot send 0 / out
            // of range and have the engine drop it silently.
            return;
        }
        self.desired_rate.set(permille, self.now_ms);
        self.sync_rate_override();
        self.queue_cmd(PlaybackCommand::SetRate {
            permille,
        });
    }

    /// Leave the Playback settings value-edit state (Enter or Back). Persists the one new
    /// value here — not on every detent — and names it in the single status popup, so a long
    /// turn cannot flicker the popup (UI_16 §1). `PlayModeView::editing` is cleared by
    /// [`Self::play_mode_changed`], because `view()` never carries the edit state.
    fn play_mode_leave_edit(&mut self) {
        let Some(row) = self.model.play_mode.editing else {
            return;
        };
        self.play_mode_changed();
        let text = self.model.play_mode.change_text(row);
        self.compositor.show_status(&text, self.now_ms);
    }

    /// Encoder while a Playback settings row is in the edit state (UI_16 §1/§6/§7): step the
    /// bounded ladder, clamped at both ends (no wrap), and apply the live effect at once.
    /// Nothing is persisted until [`Self::play_mode_leave_edit`].
    ///
    /// The step uses [`crate::nav::menu_step`] so a value ladder reads top-down exactly like
    /// the menu rows around it: left = down/next value, right = up/previous value.
    fn play_mode_edit_rotate(&mut self, delta: i8) {
        let Some(row) = self.model.play_mode.editing else {
            return;
        };
        let step = crate::nav::menu_step(delta);
        if step == 0 {
            return;
        }
        match row {
            PlayModeRow::WaveformZoom => {
                // Startup-only by design (owner, UI_13): the live session zoom is untouched,
                // so this only moves the stored default and the row's value.
                let next = zoom_step(self.play_mode.settings.resolve_waveform_zoom(), step);
                if self.model.play_mode.waveform_zoom_ms == next {
                    return;
                }
                self.play_mode.settings.waveform_zoom_ms = Some(next);
                self.model.play_mode.waveform_zoom_ms = next;
            }
            PlayModeRow::FineTime => {
                // Read live by the footer clocks and by the jog path: the next frame and the
                // next jog click already see it, with no restart and no engine command.
                let next = self.play_mode.settings.resolve_timecode_unit().step(step);
                if self.model.play_mode.timecode_unit == next {
                    return;
                }
                self.play_mode.settings.timecode_unit = Some(next);
                self.model.play_mode.timecode_unit = next;
            }
            PlayModeRow::PlaybackSpeed => {
                // Live set speed: every detent is one `SetRate`, the value column follows at
                // once through `desired_rate`, and nothing here is persisted.
                let cur = self.model.set_speed_permille();
                let next = speed_row_step(cur, step);
                if next == cur {
                    return; // clamped at an end: no command
                }
                self.playback_speed_set(next);
                self.model.play_mode.rate_permille = next;
            }
            // Only the rows above can enter the edit state.
            PlayModeRow::SpeedWarp
            | PlayModeRow::SeekWarp
            | PlayModeRow::SeekOutsideTrack
            | PlayModeRow::ArtLeds => return,
        }
        self.needs_redraw = true;
    }

    /// PLAY_04: Enter on a session-mode row of the merged Playback settings screen. Every row
    /// toggles or cycles (three values at most, so no scroll-editing state), sends exactly one
    /// command and names the new value in the status popup. The mirror moves at once; the
    /// engine status confirms it.
    fn play_modes_enter(&mut self, row: PlayModesRow) {
        let values = self.model.play_mode_values();
        match row {
            PlayModesRow::Shuffle => {
                let next = if values.order == plippa_core::OrderMode::Shuffle {
                    plippa_core::OrderMode::Normal
                } else {
                    plippa_core::OrderMode::Shuffle
                };
                self.queue_cmd(PlaybackCommand::SetOrderMode(next));
                self.model.order_mode = next;
            }
            PlayModesRow::Repeat => {
                let next = match values.repeat {
                    plippa_core::RepeatMode::Off => plippa_core::RepeatMode::Queue,
                    plippa_core::RepeatMode::Queue => plippa_core::RepeatMode::Item,
                    plippa_core::RepeatMode::Item => plippa_core::RepeatMode::Off,
                };
                self.queue_cmd(PlaybackCommand::SetRepeatMode(next));
                self.model.repeat_mode = next;
            }
            PlayModesRow::PauseAfterEach => {
                let next = !values.pause_after_each;
                self.queue_cmd(PlaybackCommand::SetPauseAfterEach(next));
                self.model.pause_after_each = next;
            }
            PlayModesRow::RepeatReshuffles => {
                let next = !values.reshuffle_on_repeat;
                self.queue_cmd(PlaybackCommand::SetReshuffleOnRepeat(next));
                self.model.reshuffle_on_repeat = next;
            }
        }
        let text = format!(
            "{}: {}",
            row.label(),
            row.value_text(&self.model.play_mode_values())
        );
        self.compositor.show_status(&text, self.now_ms);
        self.needs_redraw = true;
    }

    /// Enter on the System > Debug screen: navigation rows push their screen (and dump the
    /// report to the journal); two-value rows toggle. No popup and no notice — the value
    /// column is the feedback (the owner asked for no explanation of the REC/NFC behaviour).
    fn debug_enter(&mut self) {
        let row = DEBUG_ROWS[self.nav.focus.min(DEBUG_ROWS.len() - 1)];
        if row.opens_screen() {
            self.dump_audio_info();
            let _ = self.nav.push(Route::SystemDebugAudio);
            return;
        }
        match row {
            DebugRow::RecEnablesNfcPoll => {
                let next = !self.play_mode.settings.resolve_rec_enables_nfc_poll();
                self.play_mode.settings.rec_enables_nfc_poll = Some(next);
                self.play_mode_changed();
            }
            // Handled by `opens_screen` above.
            DebugRow::AudioInfo => {}
        }
    }

    /// AUDIO_05: one journal line per Audio info report line, for
    /// `journalctl -u plippa | grep audio-info`.
    fn dump_audio_info(&self) {
        for line in diag::audio_report(
            &self.audio_state.route,
            &self.audio.settings,
            &self.accessories,
            self.accessories.available,
            self.now_ms,
        ) {
            eprintln!(
                "plippa-ui: audio-info: {} = {}",
                line.label.trim_start(),
                line.value
            );
        }
    }

    /// The current AudioView, cloned from the engine mirror + persisted settings.
    fn current_audio_view(&self) -> AudioView {
        AudioView {
            route: self.audio_state.route.clone(),
            settings: self.audio.settings.clone(),
            editing: self.audio_state.edit,
        }
    }

    /// Copy the audio mirror into the screen model and ask for a repaint. The playhead is
    /// recomputed here too, because a new route (or the vis-delay toggle) changes its offset.
    fn sync_audio_view(&mut self) {
        self.model.audio = self.current_audio_view();
        self.sync_playhead();
        self.needs_redraw = true;
    }

    fn save_audio(&mut self) {
        if let Err(e) = self.audio.save() {
            eprintln!("plippa-ui: audio_outputs save failed: {e}");
        }
    }

    /// System > Audio: the two child screens and the UI-only visual-delay toggle.
    fn audio_enter(&mut self) {
        let row = AUDIO_ROWS[self.nav.focus.min(AUDIO_ROWS.len() - 1)];
        match row {
            AudioRow::Outputs => {
                let _ = self.nav.push(Route::SystemAudioOutputs);
            }
            AudioRow::Delay => {
                let _ = self.nav.push(Route::SystemAudioDelay);
            }
            AudioRow::DefaultVolume => {
                if self.audio_state.volume_edit {
                    self.audio_commit_default_volume();
                    return;
                }
                self.audio_state.volume_edit = true;
            }
            AudioRow::Accessories => {
                self.send_accessory(AccessoryCmd::Refresh);
                let _ = self.nav.push(Route::SystemAudioAccessories);
            }
            AudioRow::WaveformVisDelay => {
                let next = !self.audio.settings.waveform_vis_delay;
                self.audio.settings.waveform_vis_delay = next;
                self.save_audio();
                let value = if next { "On" } else { "Off" };
                self.compositor
                    .show_status(&format!("Waveform vis delay: {value}"), self.now_ms);
            }
        }
        self.sync_audio_view();
    }

    /// Accessories list: one row per known accessory (connected first, then name), then the
    /// transport setting and `Pair new…`.
    fn accessories_enter(&mut self) {
        if matches!(self.accessories.stage, PairStage::Prompt(..)) {
            self.accessory_prompt_enter();
            return;
        }
        if !self.accessories.has_radio() {
            return;
        }
        let id = {
            let sorted = self.accessories.sorted_known();
            let focus = self.nav.focus.min(sorted.len());
            sorted.get(focus).map(|a| a.id.clone())
        };
        if let Some(id) = id {
            self.accessories.detail = Some(id);
            let _ = self.nav.push(Route::SystemAudioAccessory);
        } else {
            self.open_pair_new();
        }
        self.sync_accessories_view();
    }

    fn start_pair_scan(&mut self) {
        if !self.accessories.has_radio() {
            return;
        }
        let seconds = self
            .accessories
            .settings
            .scan_seconds
            .clamp(MIN_SCAN_SECONDS, MAX_SCAN_SECONDS);
        self.accessories.found.clear();
        self.accessories.stage = PairStage::Scanning;
        self.accessories.scan_window = Some(ScanWindow::start(
            Duration::from_millis(self.now_ms),
            seconds,
        ));
        self.send_accessory(AccessoryCmd::StartScan(seconds));
        self.accessories.note_scan_shown();
        self.sync_accessories_view();
    }

    fn open_pair_new(&mut self) {
        if !self.accessories.has_radio() {
            return;
        }
        let _ = self.nav.push(Route::SystemAudioPairNew);
        self.start_pair_scan();
    }

    /// Enter on Pair new: a found row pairs, an empty finished scan starts another.
    fn accessory_pair_enter(&mut self) {
        if matches!(self.accessories.stage, PairStage::Prompt(..)) {
            self.accessory_prompt_enter();
            return;
        }
        if !self.accessories.has_radio() {
            return;
        }
        if self.accessories.found.is_empty() {
            if matches!(self.accessories.stage, PairStage::Idle) {
                self.start_pair_scan();
            }
            return;
        }
        let focus = self.nav.focus.min(self.accessories.found.len() - 1);
        let item = self.accessories.found[focus].clone();
        self.accessories.stage = PairStage::Pairing(item.id.clone());
        self.accessories.pairing_id = Some(item.id.clone());
        self.accessories.set_pairing_deadline(self.now_ms);
        self.accessories.pending_action = None;
        self.accessories.pending_connect = None;
        // Pairing stops discovery: close the window now, not at its deadline, so
        // auto-connect is allowed again as soon as the pair completes.
        if self.accessories.scan_window.take().is_some() {
            self.send_accessory(AccessoryCmd::StopScan);
        }
        self.send_accessory(AccessoryCmd::Pair(item.id));
        self.sync_accessories_view();
    }

    /// Enter on a prompt: Confirm/Authorise accept; the keyboards submit their text.
    fn accessory_prompt_enter(&mut self) {
        let prompt = match &self.accessories.stage {
            PairStage::Prompt(prompt, _) => prompt.clone(),
            _ => return,
        };
        match prompt {
            PairPrompt::Confirm { .. } | PairPrompt::Authorise { .. } => {
                self.send_accessory(AccessoryCmd::Answer(PromptAnswer::Accept));
                let id = self.accessories.pairing_id.clone().unwrap_or_default();
                self.accessories.stage = PairStage::Pairing(id);
                self.accessories.keyboard = None;
            }
            PairPrompt::EnterPasskey { .. } | PairPrompt::EnterPin { .. } => {
                let Some(kb) = self.accessories.keyboard.as_mut() else {
                    return;
                };
                let KeyboardAction::Submit(text) = kb.handle(KeyboardEvent::Enter) else {
                    return;
                };
                if matches!(prompt, PairPrompt::EnterPasskey { .. }) {
                    match text.trim().parse::<u32>() {
                        Ok(n) if !text.trim().is_empty() => {
                            self.send_accessory(AccessoryCmd::Answer(PromptAnswer::Passkey(n)));
                            let id = self.accessories.pairing_id.clone().unwrap_or_default();
                            self.accessories.stage = PairStage::Pairing(id);
                            self.accessories.keyboard = None;
                        }
                        _ => {
                            self.compositor.show_status("Enter a passkey", self.now_ms);
                        }
                    }
                } else {
                    self.send_accessory(AccessoryCmd::Answer(PromptAnswer::Pin(text)));
                    let id = self.accessories.pairing_id.clone().unwrap_or_default();
                    self.accessories.stage = PairStage::Pairing(id);
                    self.accessories.keyboard = None;
                }
            }
        }
        self.sync_accessories_view();
    }

    fn accessory_prompt_rotate(&mut self, delta: i8) {
        if let Some(kb) = self.accessories.keyboard.as_mut() {
            kb.handle(KeyboardEvent::Rotate(delta));
            self.sync_accessories_view();
        }
    }

    /// Enter on the detail screen: Connect/Disconnect, Auto connect, Forget (with confirm).
    fn accessory_detail_enter(&mut self) {
        if self.accessories.forget_confirm.is_some() {
            // Only the still-armed confirmation (and its still-showing popup) may forget;
            // a stale one is dropped instead of acting on an Enter meant for another row.
            if self.accessories.forget_confirm_active(self.now_ms)
                && self.compositor.status().is_some()
            {
                self.forget_selected();
            } else {
                self.accessories.clear_forget_confirm();
            }
            self.sync_accessories_view();
            return;
        }
        let Some(id) = self.accessories.detail.clone() else {
            return;
        };
        let Some(accessory) = self.accessories.accessory(&id).cloned() else {
            return;
        };
        if self.accessories.pending_action.as_ref() == Some(&id) {
            return;
        }
        match self.nav.focus {
            0 => {
                self.accessories.pending_action = Some(id.clone());
                if accessory.connected {
                    self.accessories.pending_connect = None;
                    self.send_accessory(AccessoryCmd::Disconnect(id));
                } else {
                    self.accessories.pending_connect = Some(id.clone());
                    self.send_accessory(AccessoryCmd::Connect(id));
                }
            }
            1 => {
                let on = !self.accessories.is_auto_connect(&id);
                self.accessories.set_auto_connect(&id, on);
                self.save_accessories();
                self.compositor.show_status(
                    &format!("Auto connect: {}", if on { "On" } else { "Off" }),
                    self.now_ms,
                );
            }
            2 => {
                let on = !self.accessories.is_transport_enabled(&id);
                self.accessories.set_transport_enabled(&id, on);
                self.save_accessories();
                self.compositor.show_status(
                    &format!("Transport commands: {}", if on { "On" } else { "Off" }),
                    self.now_ms,
                );
            }
            3 => {
                self.accessories.arm_forget_confirm(
                    &id,
                    self.now_ms,
                    STATUS_POPUP_IDLE_MS,
                );
                self.compositor.show_status(
                    &format!(
                        "Forget {}?\nEnter = forget, Back = cancel",
                        screen_text(&accessory.name)
                    ),
                    self.now_ms,
                );
            }
            _ => {}
        }
        self.sync_accessories_view();
    }

    /// Confirmed Forget: send it, drop auto-connect, untick a matching output through the same
    /// `SetAudioOutputs` path as the Output devices screen, then return to the list.
    fn forget_selected(&mut self) {
        let Some(id) = self.accessories.forget_confirm.take() else {
            return;
        };
        self.accessories.forget_confirm_until_ms = None;
        self.accessories.clear_unavailable(&id);
        let route = self.audio_state.route.clone();
        if let Some(output) = route
            .outputs
            .iter()
            .find(|o| props_match_address(&o.props, &id))
        {
            let output_id = output.id.clone();
            // `enabled` holds the effective set; under Local only the other ticks live in
            // `paused`. Forget must drop the id from either list, so build the wanted set from
            // both and remove it directly (a `toggled` on a paused extra would re-enable it).
            let mut enabled = route.enabled.clone();
            for paused in &route.paused {
                if !enabled.iter().any(|e| e == paused) {
                    enabled.push(paused.clone());
                }
            }
            if let Some(pos) = enabled.iter().position(|e| e == &output_id) {
                enabled.remove(pos);
                if enabled.is_empty() {
                    if let Some(local) = route.local() {
                        enabled.push(local.id.clone());
                    }
                }
                self.send_outputs(enabled, route.local_only);
            }
        }
        self.accessories.set_auto_connect(&id, false);
        self.accessories.set_transport_enabled(&id, true);
        self.save_accessories();
        self.send_accessory(AccessoryCmd::Forget(id.clone()));
        self.send_accessory(AccessoryCmd::Refresh);
        self.accessories.detail = None;
        if self
            .accessories
            .pending_output
            .as_ref()
            .is_some_and(|(pending, _)| pending == &id)
        {
            self.accessories.pending_output = None;
        }
        let _ = self.nav.back();
        self.sync_accessories_view();
    }

    /// Back on the accessory screens. Returns true when it consumed the press.
    fn accessory_back(&mut self) -> bool {
        match self.nav.current().clone() {
            Route::SystemAudioAccessory => {
                if self.accessories.forget_confirm.is_some() {
                    self.accessories.clear_forget_confirm();
                    self.compositor.clear_status();
                    self.sync_accessories_view();
                    return true;
                }
                self.accessories.detail = None;
                let _ = self.nav.back();
                self.sync_accessories_view();
                true
            }
            Route::SystemAudioAccessories => {
                if matches!(self.accessories.stage, PairStage::Prompt(..)) {
                    self.send_accessory(AccessoryCmd::Answer(PromptAnswer::Reject));
                    self.accessories.stage = PairStage::Idle;
                    self.accessories.clear_pairing();
                    // The prompt is a sub-level of the list: dismiss it back to the list.
                    self.sync_accessories_view();
                    return true;
                }
                false
            }
            Route::SystemAudioPairNew => {
                match self.accessories.stage.clone() {
                    PairStage::Prompt(..) => {
                        self.send_accessory(AccessoryCmd::Answer(PromptAnswer::Reject));
                        self.accessories.clear_pairing();
                    }
                    PairStage::Scanning => {
                        self.send_accessory(AccessoryCmd::StopScan);
                        self.accessories.scan_window = None;
                        self.accessories.found.clear();
                    }
                    // A pairing without a prompt yet keeps running: its prompt is still shown
                    // wherever the owner goes, and the 60 s timeout answers it.
                    _ => {}
                }
                self.accessories.stage = PairStage::Idle;
                let _ = self.nav.back();
                self.sync_accessories_view();
                true
            }
            _ => false,
        }
    }

    /// Output devices: Local only, then the sink ticks. A toggle sends exactly one
    /// `SetAudioOutputs` built through `AudioRoute::toggled`, so the last-output rule lives in
    /// `plippa_core`; the row changes when the engine publishes the new route.
    fn audio_outputs_enter(&mut self) {
        let route = self.audio_state.route.clone();
        if route.outputs.is_empty() {
            return;
        }
        let outputs = self.current_audio_view().sorted_outputs();
        let focus = self.nav.focus.min(outputs.len());
        if focus == 0 {
            let next = !route.local_only;
            // Leaving Local only restores the extras the engine paused: send the effective
            // set union the paused ids, in route order (local first) and deduped.
            let mut enabled = route.enabled.clone();
            if !next {
                for id in &route.paused {
                    if !enabled.iter().any(|e| e == id) {
                        enabled.push(id.clone());
                    }
                }
            }
            self.send_outputs(enabled.clone(), next);
            let value = if next { "On" } else { "Off" };
            self.compositor
                .show_status(&format!("Local only: {value}"), self.now_ms);
            self.sync_audio_view();
            return;
        }
        let output = outputs[focus - 1].clone();
        if route.local_only {
            self.compositor.show_status("Local only is on", self.now_ms);
            return;
        }
        if !output.available {
            self.compositor.show_status("Not connected", self.now_ms);
            return;
        }
        let wanted = wanted_from_route(&route);
        let mut next = plippa_core::audio_route::AudioRoute::toggled(&wanted, &output.id);
        // Apply the same cap the engine applies, so neither the command nor the saved
        // `extras` ever names more outputs than MAX_ENABLED_OUTPUTS. The engine still sends
        // its own notice.
        let _ = plippa_core::audio_route::sanitise_wanted(&mut next, &route.outputs);
        self.send_outputs(next.enabled.clone(), next.local_only);
    }

    /// The one path for "the set of outputs": sends `SetAudioOutputs`, persists every ticked
    /// output except the built-in one (series decision 7) and refreshes the mirror.
    fn send_outputs(&mut self, enabled: Vec<String>, local_only: bool) {
        self.queue_cmd(PlaybackCommand::SetAudioOutputs {
            enabled: enabled.clone(),
            local_only,
        });
        let local_id = self.audio_state.route.local().map(|o| o.id.clone());
        self.audio.settings.extras = enabled
            .iter()
            .filter(|id| local_id.as_deref() != Some(id.as_str()))
            .cloned()
            .collect();
        self.audio.settings.local_only = local_only;
        self.save_audio();
        self.sync_audio_view();
    }

    // ---- ACC_03: accessories -----------------------------------------------------------

    /// Copy the accessory mirror into the screen model and ask for a repaint.
    fn sync_accessories_view(&mut self) {
        self.clamp_accessory_focus();
        self.model.accessories = self.accessories.clone();
        self.needs_redraw = true;
    }

    /// Keep the cursor inside the visible accessory list after the list shrinks.
    fn clamp_accessory_focus(&mut self) {
        match *self.nav.current() {
            Route::SystemAudioAccessories => {
                let len = self.accessories.sorted_known().len() + 1;
                if self.nav.focus >= len {
                    self.nav.focus = len - 1;
                }
            }
            Route::SystemAudioPairNew => {
                let len = self.accessories.found.len();
                if len == 0 {
                    self.nav.focus = 0;
                } else if self.nav.focus >= len {
                    self.nav.focus = len - 1;
                }
            }
            _ => {}
        }
    }

    fn save_accessories(&mut self) {
        self.accessory_store.settings = self.accessories.settings.clone();
        if let Err(e) = self.accessory_store.save() {
            eprintln!("plippa-ui: accessories save failed: {e}");
        }
    }

    /// Queue a command on the worker. The frame loop never calls the backend itself.
    fn send_accessory(&mut self, cmd: AccessoryCmd) {
        if let Some(handle) = &self.accessory_handle {
            if handle.cmd.send(cmd).is_err() {
                eprintln!("plippa-ui: accessory worker is gone");
            }
        }
    }

    /// Drain the worker and advance the scan / pairing / pending-output timers. Called from the
    /// loop's idle tick; also the test entry point.
    pub fn poll_accessories(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        self.accessories.now_ms = now_ms;
        if self.accessory_handle.is_none() {
            return;
        }
        let (messages, worker_gone) = {
            let handle = self.accessory_handle.as_ref().expect("checked above");
            let mut out = Vec::new();
            let mut gone = false;
            loop {
                match handle.evt.try_recv() {
                    Ok(msg) => out.push(msg),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
            (out, gone)
        };
        if worker_gone {
            if self.accessories.available {
                eprintln!("plippa-ui: accessory worker is gone");
            }
            // A dead worker must never look like an empty, usable list.
            self.accessories.available = false;
            self.accessories.radio_present = false;
        }
        let had_messages = !messages.is_empty();
        for msg in messages {
            self.apply_accessory_msg(msg, now_ms);
        }
        let timed = self.tick_accessory_timers(now_ms);
        if had_messages || timed || worker_gone {
            self.sync_accessories_view();
        }
    }

    fn apply_accessory_msg(&mut self, msg: AccessoryMsg, now_ms: u64) {
        match msg {
            AccessoryMsg::Events(events) => self.apply_accessory_events(events, now_ms),
            AccessoryMsg::Snapshot {
                adapters,
                known,
                unavailable,
                events,
            } => {
                self.apply_accessory_events(events, now_ms);
                // The snapshot, not mere handle presence, says whether the radio is alive.
                self.accessories.available = !unavailable;
                self.accessories.radio_present = !unavailable;
                self.accessories.adapters = adapters;
                if !unavailable {
                    if let Some(adapter) = self.accessories.take_power_request() {
                        self.send_accessory(AccessoryCmd::PowerOn(adapter));
                    }
                }
                self.accessories.known = known
                    .into_iter()
                    // Unpaired devices are scan results, not accessories: they belong in
                    // the Pair new list only. Showing them here offered Connect/Forget on
                    // devices that cannot be used yet.
                    .filter(|a| a.paired)
                    .map(|mut a| {
                        a.id = normalise_address(&a.id);
                        a
                    })
                    .collect();
                if let Some(id) = self.accessories.detail.clone() {
                    if self.accessories.accessory(&id).is_none() {
                        self.accessories.detail = None;
                        self.accessories.clear_forget_confirm();
                        if *self.nav.current() == Route::SystemAudioAccessory {
                            let _ = self.nav.back();
                        }
                    }
                }
                self.maybe_auto_connect(now_ms);
                self.retry_pending_output(now_ms);
            }
        }
    }

    fn apply_accessory_events(&mut self, events: Vec<AccessoryEvent>, now_ms: u64) {
        if events.is_empty() {
            return;
        }
        for event in events {
            self.apply_accessory_event(event, now_ms);
        }
        self.maybe_auto_connect(now_ms);
        self.retry_pending_output(now_ms);
    }

    fn apply_accessory_event(&mut self, event: AccessoryEvent, now_ms: u64) {
        match event {
            AccessoryEvent::Adapters(adapters) => {
                self.accessories.adapters = adapters;
                self.accessories.available = true;
                self.accessories.radio_present = true;
                if let Some(adapter) = self.accessories.take_power_request() {
                    self.send_accessory(AccessoryCmd::PowerOn(adapter));
                }
            }
            AccessoryEvent::Discovered { id, name, kind } => {
                if !self.accessories.is_scanning() {
                    return;
                }
                // Only audio/input accessories are useful here. `Other` is a phone, a
                // beacon or a nameless BLE device; listing those made the speaker hard to
                // find among raw hex aliases.
                if kind == AccessoryKind::Other {
                    return;
                }
                let id = normalise_address(&id);
                let name = if address_like_name(&name) {
                    "Unknown accessory".to_string()
                } else {
                    name
                };
                let item = FoundItem { id: id.clone(), name, kind };
                if let Some(slot) = self.accessories.found.iter_mut().find(|f| f.id == id) {
                    *slot = item;
                } else {
                    self.accessories.found.push(item);
                }
                self.accessories.found.sort_by(|a, b| {
                    a.name
                        .to_lowercase()
                        .cmp(&b.name.to_lowercase())
                        .then_with(|| a.id.cmp(&b.id))
                });
            }
            AccessoryEvent::Prompt(prompt) => {
                self.accessories.stage = PairStage::Prompt(prompt.clone(), None);
                self.accessories.keyboard = prompt_keyboard(&prompt);
                if self.accessories.pairing_deadline_open() {
                    self.accessories.set_pairing_deadline(now_ms);
                }
                self.accessories.pending_action = None;
                self.accessories.pending_connect = None;
                // A prompt nobody can see would hang the radio. If the owner walked away from
                // the Pair new screen, bring the list (which draws the prompt) back up.
                if !matches!(
                    self.nav.current(),
                    Route::SystemAudioAccessories | Route::SystemAudioPairNew
                ) {
                    self.nav.force_push(Route::SystemAudioAccessories);
                }
            }
            AccessoryEvent::Paired { id, ok, error } => {
                let id = normalise_address(&id);
                let name = screen_text(&self.accessories.name_for(&id));
                let text = if ok {
                    format!("{name} paired")
                } else {
                    format!("Could not pair\n{}", pair_failure_text(error.as_deref()))
                };
                self.accessories.stage = PairStage::Idle;
                self.accessories.clear_pairing();
                self.accessories.pending_action = None;
                self.accessories.pending_connect = None;
                if ok {
                    self.accessories.clear_unavailable(&id);
                    // Trust first, then connect. Recording the budget attempt keeps a
                    // Connected in this same batch from making auto-connect resend it.
                    let _ = self
                        .accessories
                        .budget
                        .try_take(Duration::from_millis(now_ms), &id);
                    self.accessories.pending_action = Some(id.clone());
                    self.accessories.pending_connect = Some(id.clone());
                    self.send_accessory(AccessoryCmd::SetTrusted {
                        id: id.clone(),
                        trusted: true,
                    });
                    self.send_accessory(AccessoryCmd::Connect(id));
                }
                self.send_accessory(AccessoryCmd::Refresh);
                self.compositor.show_status(&text, now_ms);
                if *self.nav.current() == Route::SystemAudioPairNew {
                    let _ = self.nav.back();
                }
            }
            AccessoryEvent::Connected(id) => {
                let id = normalise_address(&id);
                self.accessories.pending_action = None;
                self.accessories.pending_connect = None;
                self.accessories.clear_unavailable(&id);
                self.send_accessory(AccessoryCmd::Refresh);
                let name = screen_text(&self.accessories.name_for(&id));
                let is_audio = self
                    .accessories
                    .accessory(&id)
                    .map(|a| a.kind == AccessoryKind::Audio)
                    .or_else(|| {
                        self.accessories
                            .found
                            .iter()
                            .find(|f| f.id == id)
                            .map(|f| f.kind == AccessoryKind::Audio)
                    })
                    .unwrap_or(false);
                if is_audio {
                    if self.audio_state.route.local_only {
                        self.accessories.pending_output = None;
                        self.compositor.show_status(
                            &format!("{name} connected (output paused: local only)"),
                            now_ms,
                        );
                    } else {
                        self.accessories.pending_output =
                            Some((id.clone(), now_ms.saturating_add(5_000)));
                        if self.try_enable_output(&id) {
                            self.accessories.pending_output = None;
                        }
                    }
                }
            }
            AccessoryEvent::Disconnected(id) => {
                let id = normalise_address(&id);
                self.accessories.pending_action = None;
                self.accessories.pending_connect = None;
                if self
                    .accessories
                    .pending_output
                    .as_ref()
                    .is_some_and(|(pending, _)| pending == &id)
                {
                    self.accessories.pending_output = None;
                }
                self.send_accessory(AccessoryCmd::Refresh);
            }
            AccessoryEvent::Transport(action) => {
                // The player is registered once for the adapter, so the event carries no id.
                // Apply the per-accessory policy of the connected accessory: if every
                // connected accessory has transport disabled, ignore the press.
                let connected: Vec<&plippa_hw::Accessory> = self
                    .accessories
                    .known
                    .iter()
                    .filter(|a| a.connected)
                    .collect();
                if !connected.is_empty()
                    && connected
                        .iter()
                        .all(|a| !self.accessories.is_transport_enabled(&a.id))
                {
                    return;
                }
                let cmd = match action {
                    TransportAction::Play => PlaybackCommand::Play,
                    TransportAction::Pause => PlaybackCommand::Pause,
                    TransportAction::Stop => PlaybackCommand::Stop,
                    TransportAction::Next => PlaybackCommand::Next,
                    TransportAction::Previous => PlaybackCommand::Previous,
                    TransportAction::FastForward | TransportAction::Rewind => return,
                };
                self.outgoing
                    .push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(cmd)));
                // The same follow as the hardware buttons, when the press can take effect.
                let avail = self.nav_availability();
                let moves = match action {
                    TransportAction::Next => avail.can_next,
                    TransportAction::Previous => avail.can_prev,
                    _ => false,
                };
                if moves {
                    self.np_after_transport();
                }
            }
            AccessoryEvent::ScanFinished => {
                let was_scanning = self.accessories.is_scanning();
                // The window closes even when pairing started mid-scan, so it can never
                // keep auto-connect blocked for the rest of the boot.
                self.accessories.scan_window = None;
                if was_scanning {
                    self.accessories.stage = PairStage::Idle;
                    let count = self.accessories.found.len();
                    self.compositor
                        .show_status(&format!("{count} found"), now_ms);
                }
            }
            AccessoryEvent::Failed(text) => {
                let text = if text.trim().is_empty() {
                    "Accessory operation failed".to_string()
                } else {
                    screen_text(&text)
                };
                if self.accessories.should_show_failure(&text, now_ms) {
                    self.compositor.show_status(&text, now_ms);
                }
                // Only an in-flight Connect marks the accessory as out of range; a failed
                // Disconnect, scan or pairing must not claim the device is unreachable.
                if let Some(id) = self.accessories.pending_action.clone() {
                    let scan_or_pair_in_flight = self.accessories.is_scanning()
                        || self.accessories.is_busy_pairing();
                    let failed_disconnect = self.accessories.pending_connect.as_ref() != Some(&id)
                        && self
                            .accessories
                            .accessory(&id)
                            .is_some_and(|a| a.connected);
                    if !scan_or_pair_in_flight && !failed_disconnect {
                        self.accessories.mark_unavailable(&id);
                    }
                }
                self.accessories.pending_action = None;
                self.accessories.pending_connect = None;
                // A rejected StartScan must leave the screen usable, not counting down.
                if self.accessories.is_scanning() {
                    self.accessories.scan_window = None;
                    self.accessories.stage = PairStage::Idle;
                }
                if matches!(self.accessories.stage, PairStage::Pairing(_)) {
                    self.accessories.stage = PairStage::Idle;
                    self.accessories.clear_pairing();
                    if *self.nav.current() == Route::SystemAudioPairNew {
                        let _ = self.nav.back();
                    }
                }
            }
        }
    }

    /// Scan countdown, the 60 s pairing timeout and the pending-output deadline.
    fn tick_accessory_timers(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        if self.accessories.expire_forget_confirm(now_ms) {
            changed = true;
        }
        if let Some(window) = self.accessories.scan_window {
            let closed = !window.is_open(Duration::from_millis(now_ms));
            let was_scanning = self.accessories.is_scanning();
            if closed {
                self.send_accessory(AccessoryCmd::StopScan);
                self.accessories.scan_window = None;
                if was_scanning {
                    self.accessories.stage = PairStage::Idle;
                    let count = self.accessories.found.len();
                    self.compositor
                        .show_status(&format!("{count} found"), now_ms);
                }
                changed = true;
            } else if was_scanning && self.accessories.note_scan_shown() {
                changed = true;
            }
        }
        if self.accessories.pairing_expired(now_ms) {
            let had_prompt = matches!(self.accessories.stage, PairStage::Prompt(..));
            if had_prompt {
                self.send_accessory(AccessoryCmd::Answer(PromptAnswer::Reject));
            } else {
                // No prompt is outstanding: Answer would return NoPairing and pop the
                // wrong sentence. End the wait with the timeout notice instead.
                self.compositor
                    .show_status("Pairing timed out", now_ms);
            }
            self.accessories.stage = PairStage::Idle;
            self.accessories.clear_pairing();
            if *self.nav.current() == Route::SystemAudioPairNew {
                let _ = self.nav.back();
            }
            changed = true;
        }
        if let Some((id, deadline)) = self.accessories.pending_output.clone() {
            if now_ms >= deadline {
                let name = screen_text(&self.accessories.name_for(&id));
                self.compositor.show_status(
                    &format!("{name} connected, no audio output found"),
                    now_ms,
                );
                self.accessories.pending_output = None;
                changed = true;
            }
        }
        changed
    }

    /// Auto-connect: once per event batch and on the first route after launch. Policy decides
    /// whether anything is eligible; the budget stops retries and duplicate sends.
    fn maybe_auto_connect(&mut self, now_ms: u64) {
        if self.accessory_handle.is_none()
            || !self.accessories.has_radio()
            || !self.accessories.auto_connect_allowed()
            || self.accessories.pending_action.is_some()
        {
            return;
        }
        let Some(target) = auto_connect_target(
            &self.accessories.known,
            &self.accessories.settings.auto_connect,
        ) else {
            return;
        };
        let id = target.id.clone();
        if self
            .accessories
            .budget
            .try_take(Duration::from_millis(now_ms), &id)
        {
            self.accessories.pending_action = Some(id.clone());
            self.accessories.pending_connect = Some(id.clone());
            self.send_accessory(AccessoryCmd::Connect(id));
        }
    }

    /// Try to tick the accessory's output. Returns true when a matching output was found
    /// (already ticked counts as success).
    fn try_enable_output(&mut self, id: &str) -> bool {
        let route = self.audio_state.route.clone();
        if route.outputs.is_empty() || route.local_only {
            return false;
        }
        let Some(output) = route
            .outputs
            .iter()
            .find(|o| o.available && props_match_address(&o.props, id))
        else {
            return false;
        };
        let output_id = output.id.clone();
        let local = route.local().map(|o| (o.id.clone(), o.available));
        let local_available = local.as_ref().is_some_and(|(_, available)| *available);
        let local_present = local
            .as_ref()
            .is_some_and(|(id, _)| route.enabled.iter().any(|e| e == id));
        let accessory_enabled = route.enabled.iter().any(|e| e == &output_id);
        if accessory_enabled && (!local_available || local_present) {
            return true;
        }
        let wanted = wanted_from_route(&route);
        let mut next = if accessory_enabled {
            wanted
        } else {
            plippa_core::audio_route::AudioRoute::toggled(&wanted, &output_id)
        };
        if let Some((local_id, _)) = &local {
            if local_available {
                if let Some(pos) = next.enabled.iter().position(|e| e == local_id) {
                    let local_id = next.enabled.remove(pos);
                    next.enabled.insert(0, local_id);
                } else {
                    next.enabled.insert(0, local_id.clone());
                }
            }
        }
        let _ = plippa_core::audio_route::sanitise_wanted(&mut next, &route.outputs);
        self.send_outputs(next.enabled.clone(), next.local_only);
        true
    }

    /// Retry the pending output on a route update; notice at the deadline.
    fn retry_pending_output(&mut self, now_ms: u64) -> bool {
        let Some((id, deadline)) = self.accessories.pending_output.clone() else {
            return false;
        };
        if now_ms >= deadline {
            let name = screen_text(&self.accessories.name_for(&id));
            self.compositor.show_status(
                &format!("{name} connected, no audio output found"),
                now_ms,
            );
            self.accessories.pending_output = None;
            return true;
        }
        if self.audio_state.route.local_only {
            return false;
        }
        if self.try_enable_output(&id) {
            self.accessories.pending_output = None;
            return true;
        }
        false
    }

    /// Delay: Auto toggles immediately; a ms row arms the enter-then-scroll editor and sends
    /// nothing until [`Self::audio_commit_delay`] leaves it.
    fn audio_delay_enter(&mut self) {
        if self.audio_state.edit.is_some() {
            self.audio_commit_delay();
            return;
        }
        let route = self.audio_state.route.clone();
        if route.local_only {
            self.compositor.show_status("Local only is on", self.now_ms);
            return;
        }
        let focus = self.nav.focus.min(route.effective_enabled().len());
        if focus == 0 {
            let next = !route.delay_auto;
            self.queue_cmd(PlaybackCommand::SetAudioDelayAuto(next));
            self.audio.settings.delay_auto = next;
            self.save_audio();
            let value = if next { "Auto" } else { "Manual" };
            self.compositor
                .show_status(&format!("Delay: {value}"), self.now_ms);
            self.sync_audio_view();
            return;
        }
        if let Some(why) = delay_blocked(&route) {
            self.compositor.show_status(why, self.now_ms);
            return;
        }
        let Some(id) = route.effective_enabled().get(focus - 1).cloned() else {
            return;
        };
        self.audio_state
            .route
            .manual_delay_ms
            .entry(id)
            .or_insert(0);
        self.audio_state.edit = Some(focus);
        self.sync_audio_view();
    }

    /// Encoder while a Delay row is being edited: ±10 ms, clamped to 0..=1000, no wrap and no
    /// command; the value is sent once when Enter / Back leaves the row.
    fn audio_delay_rotate(&mut self, delta: i8) {
        let Some(row) = self.audio_state.edit else {
            return;
        };
        if delta == 0 {
            return;
        }
        let Some(id) = self
            .audio_state
            .route
            .effective_enabled()
            .get(row - 1)
            .cloned()
        else {
            return;
        };
        let current = self
            .audio_state
            .route
            .manual_delay_ms
            .get(&id)
            .copied()
            .unwrap_or(0);
        let next = delay_step(current, delta);
        if next == current {
            return;
        }
        self.audio_state
            .route
            .manual_delay_ms
            .insert(id, next);
        self.sync_audio_view();
    }

    /// Leave the Delay row editor: exactly one `SetOutputDelay` and one save, then the popup.
    fn audio_commit_delay(&mut self) {
        let Some(row) = self.audio_state.edit.take() else {
            return;
        };
        let Some(id) = self
            .audio_state
            .route
            .effective_enabled()
            .get(row - 1)
            .cloned()
        else {
            self.sync_audio_view();
            return;
        };
        let ms = self
            .audio_state
            .route
            .manual_delay_ms
            .get(&id)
            .copied()
            .unwrap_or(0);
        self.queue_cmd(PlaybackCommand::SetOutputDelay {
            output: id,
            delay_ms: ms,
        });
        self.audio.settings.delays_ms = self.audio_state.route.manual_delay_ms.clone();
        self.audio.settings.delay_auto = self.audio_state.route.delay_auto;
        self.save_audio();
        self.compositor
            .show_status(&format!("Delay: {ms} ms"), self.now_ms);
        self.sync_audio_view();
    }

    /// Encoder while Default volume is being edited: ±5 %, clamped to 0..=100, no wrap and
    /// no command; the value is sent once when Enter / Back leaves the row.
    fn audio_default_volume_rotate(&mut self, delta: i8) {
        if delta == 0 {
            return;
        }
        let current = self.audio.settings.default_volume;
        let next = volume_step(current, delta);
        if next == current {
            return;
        }
        self.audio.settings.default_volume = next;
        self.sync_audio_view();
    }

    /// Leave the Default volume row editor: exactly one `SetVolume` and one save, then the
    /// popup. Nothing is sent while the knob turns.
    fn audio_commit_default_volume(&mut self) {
        if !self.audio_state.volume_edit {
            return;
        }
        self.audio_state.volume_edit = false;
        let percent = self.audio.settings.default_volume;
        self.queue_cmd(PlaybackCommand::SetVolume(Volume::new(percent)));
        self.save_audio();
        self.compositor
            .show_status(&format!("Default volume: {percent}%"), self.now_ms);
        self.sync_audio_view();
    }

    /// Startup reconciler (AUDIO_04 §4.3): the first `AudioRouteChanged` after launch sends
    /// the persisted routing once, then the persisted delay mode (and, for Manual, each stored
    /// trim that names a known output), then the default main volume. Every later route update
    /// sends nothing.
    fn reconcile_audio_route(&mut self) {
        if self.audio_state.sent_initial || self.audio_state.route.outputs.is_empty() {
            return;
        }
        self.audio_state.sent_initial = true;
        let route = self.audio_state.route.clone();
        let settings = self.audio.settings.clone();
        let local = route.local().map(|o| o.id.clone());
        let mut enabled: Vec<String> = Vec::new();
        if let Some(id) = &local {
            enabled.push(id.clone());
        }
        for id in &settings.extras {
            if Some(id) == local.as_ref() {
                continue;
            }
            let available = route.output(id).is_some_and(|o| o.available);
            if available && !enabled.iter().any(|e| e == id) {
                enabled.push(id.clone());
            }
        }
        self.queue_cmd(PlaybackCommand::SetAudioOutputs {
            enabled,
            local_only: settings.local_only,
        });
        if settings.delay_auto {
            self.queue_cmd(PlaybackCommand::SetAudioDelayAuto(true));
        } else {
            self.queue_cmd(PlaybackCommand::SetAudioDelayAuto(false));
            for (id, ms) in &settings.delays_ms {
                if route.output(id).is_some() {
                    self.queue_cmd(PlaybackCommand::SetOutputDelay {
                        output: id.clone(),
                        delay_ms: *ms,
                    });
                }
            }
        }
        self.queue_cmd(PlaybackCommand::SetVolume(Volume::new(
            settings.default_volume,
        )));
    }

    /// UI_16 §8: true when the Debug setting "Rec enables NFC poll" is On. The loop gates its
    /// NFC poll on this; with it Off the poll runs continuously exactly as before.
    pub fn nfc_rec_gate(&self) -> bool {
        self.play_mode.settings.resolve_rec_enables_nfc_poll()
    }

    /// True while the engine reports a playing transport (UI_16 §8: a tap has "successfully
    /// played" once playback actually starts).
    pub fn transport_playing(&self) -> bool {
        self.model
            .playback
            .as_ref()
            .is_some_and(|p| p.transport == TransportState::Playing)
    }

    /// Re-send a warp mode the engine does not have (idempotent; once per status revision).
    fn reconcile_warp_modes(
        &mut self,
        rev: u64,
        engine_speed: plippa_core::WarpMode,
        engine_seek: plippa_core::WarpMode,
        engine_outside: bool,
    ) {
        let (speed, seek) = self.play_mode.settings.resolve_warp();
        if engine_speed != speed && self.speed_sent_rev != Some(rev) {
            self.speed_sent_rev = Some(rev);
            self.queue_cmd(PlaybackCommand::SetSpeedWarpMode(speed));
        }
        if engine_seek != seek && self.seek_sent_rev != Some(rev) {
            self.seek_sent_rev = Some(rev);
            self.queue_cmd(PlaybackCommand::SetSeekWarpMode(seek));
        }
        let outside = self.play_mode.settings.resolve_seek_outside();
        if engine_outside != outside && self.outside_sent_rev != Some(rev) {
            self.outside_sent_rev = Some(rev);
            self.queue_cmd(PlaybackCommand::SetSeekOutsideTrack(outside));
        }
    }

    /// Mirror the optimistic speed into the screen model.
    fn sync_rate_override(&mut self) {
        let want = self.desired_rate.fresh(self.now_ms);
        if self.model.rate_override != want {
            self.model.rate_override = want;
            self.needs_redraw = true;
        }
        // PLAY_04: the Playback speed row shows the same live value as the footer/TEMPO ring.
        let rate = self.model.set_speed_permille();
        if self.model.play_mode.rate_permille != rate {
            self.model.play_mode.rate_permille = rate;
            self.needs_redraw = true;
        }
    }

    /// Monotonic clock last seen on [`InputEvent::Tick`] (ms).
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    /// Take-and-clear the compositor redraw flag (menu hide / status clear).
    pub fn take_needs_redraw(&mut self) -> bool {
        std::mem::take(&mut self.needs_redraw)
    }

    pub fn with_system(mut self, system: SystemStatus) -> Self {
        self.model.system = system;
        self
    }

    pub fn set_library(&mut self, items: Vec<LibraryItem>) {
        self.model.library = items;
        // Drop stale detail selection if the playable vanished.
        if let Some(id) = self.model.detail_playable_id {
            let pos = self.model.detail_track_pos;
            if !self
                .model
                .library
                .iter()
                .any(|i| i.id == id && i.item_position == pos)
            {
                self.model.detail_playable_id = None;
                self.model.detail_track_pos = None;
            }
        }
    }

    /// Index lookups queued since the last call (tests / custom loops).
    pub fn take_browse_requests(&mut self) -> Vec<BrowseRequest> {
        std::mem::take(&mut self.pending_browse)
    }

    /// Queue `req` unless an identical one is already waiting.
    fn request_browse(&mut self, req: BrowseRequest) {
        if !self.pending_browse.contains(&req) {
            self.pending_browse.push(req);
        }
    }

    /// Answer the queued browse requests from the read-only index. Called by the loop
    /// runner after input / IPC handling, so `handle_input` and painting stay I/O free.
    pub fn service_browse(&mut self, src: &LibrarySource) {
        for req in self.take_browse_requests() {
            match req {
                BrowseRequest::Counts => {
                    self.model.storage_present = src.storage_present();
                    self.model.kind_counts = src.kind_counts();
                }
                BrowseRequest::Category(kind) => {
                    self.model.storage_present = src.storage_present();
                    // Ignore a late answer for a category we already left.
                    if self.model.browse_kind == Some(kind) {
                        let items = src.list_kind(kind);
                        self.set_library(items);
                    }
                }
                BrowseRequest::FindForPlay { relpath } => match src.find_by_relpath(&relpath) {
                    Some(item) => {
                        // The engine ids are namespaced by library root (P6); the row carries
                        // the root it was listed from.
                        let id = match item.library_root.as_deref() {
                            Some(root) => encode_playable_id(root, item.id),
                            None => item.id as u64,
                        };
                        self.outgoing.push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(
                            PlaybackCommand::LoadAndPlay(PlayableItemId::new(id)),
                        )));
                        self.compositor
                            .show_status(&format!("Playing {}", item.title), self.now_ms);
                    }
                    None => {
                        // The scanner may not have reached the new files yet: say so and leave
                        // the person in the library rather than pretending to play silence.
                        self.compositor
                            .show_status("Not in the library yet", self.now_ms);
                        self.model.browse_kind = None;
                        self.model.library.clear();
                        self.request_browse(BrowseRequest::Counts);
                        let _ = self.nav.push(Route::Library);
                        self.needs_redraw = true;
                    }
                },
                BrowseRequest::FindForDetail { relpath } => match src.find_by_relpath(&relpath) {
                    Some(item) => {
                        self.model.browse_kind = None;
                        self.model.detail_playable_id = Some(item.id);
                        self.model.detail_track_pos = item.item_position;
                        self.model.detail_info = Some(src.detail_info(&item, None, &self.cfg.capabilities));
                        self.model.detail_covers = Some(src.detail_covers(&item));
                        let _ = self.nav.push(Route::LibraryDetail);
                        if self.pending_remove.take().is_some() {
                            // "Remove from Plippa" on the release screen: the confirm opens
                            // on top of the item's detail as soon as the lookup lands.
                            self.open_remove_for_detail();
                        }
                        self.needs_redraw = true;
                    }
                    None => {
                        // The scanner may not have reached the new files yet: the person
                        // lands in the library rather than on a screen for a missing item.
                        self.pending_remove = None;
                        self.compositor
                            .show_status("Not in the library yet", self.now_ms);
                        self.model.browse_kind = None;
                        self.model.library.clear();
                        self.request_browse(BrowseRequest::Counts);
                        let _ = self.nav.push(Route::Library);
                        self.needs_redraw = true;
                    }
                },
                BrowseRequest::Detail { item, kind } => {
                    if self.model.detail_playable_id == Some(item.id)
                        && self.model.detail_track_pos == item.item_position
                    {
                        self.model.detail_info = Some(src.detail_info(&item, kind, &self.cfg.capabilities));
                        self.model.detail_covers = Some(src.detail_covers(&item));
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // BCS_07: the store tap flow
    // ---------------------------------------------------------------------

    /// Install a store client (tests, or a transport other than the local HTTP service).
    /// Replaces the worker built from [`UiConfig::store_base`].
    pub fn set_store_api(&mut self, api: Box<dyn StoreApi>) {
        self.store = Some(StoreWorker::new(api));
    }

    /// True when this device has a store service to ask.
    pub fn store_configured(&self) -> bool {
        self.store.is_some()
    }

    fn store_send(&self, request: StoreRequest) -> bool {
        match self.store.as_ref() {
            Some(worker) => worker.send(request),
            None => false,
        }
    }

    /// Open the store tap screen for an unmatched tag that declared platform URLs. Returns
    /// false when the tag named no usable link, so the caller keeps today's popup.
    pub fn open_store_tap(
        &mut self,
        candidate_urls: &[String],
        noubin_key: Option<String>,
        title: &str,
    ) -> bool {
        let Some(url) = pick_store_url(candidate_urls) else {
            return false;
        };
        // A new tap supersedes a hidden download's progress messages.
        self.store_watch = None;
        let mut view = StoreTapView::lookup(url.clone(), noubin_key, screen_text(title));
        view.begin_call(self.now_ms);
        self.model.store_tap = Some(view);
        self.nav.push(Route::LibraryStoreTap);
        if self.store.is_some() {
            self.store_send(StoreRequest::Tap {
                url,
                expected_duration_s: None,
                consent: false,
            });
        } else if let Some(view) = self.model.store_tap.as_mut() {
            // No service configured: say so instead of spinning on a call nobody can make.
            view.phase = StoreTapPhase::Blocked {
                blocked: TapBlocked::NoStore,
                message: blocked_message(TapBlocked::NoStore).to_string(),
            };
        }
        self.needs_redraw = true;
        true
    }

    /// Leave the store screen. A download still running is cancelled on the way out, so
    /// leaving never leaves an orphan transfer; a status popup reports the outcome once the
    /// service acknowledges the cancel.
    pub fn close_store_tap(&mut self) {
        if let Some(StoreTapPhase::Downloading { session, progress }) =
            self.model.store_tap.as_ref().map(|view| view.phase.clone())
        {
            if !progress.finished() {
                self.store_send(StoreRequest::Cancel {
                    session: session.clone(),
                });
                self.store_watch = Some((session, self.now_ms.saturating_add(DOWNLOAD_POLL_MS)));
            }
        }
        self.model.store_tap = None;
        if *self.nav.current() == Route::LibraryStoreTap {
            let _ = self.nav.back();
        }
        self.needs_redraw = true;
    }

    /// Enter on the store screen: execute the focused action.
    fn store_enter(&mut self) {
        let Some(view) = self.model.store_tap.as_mut() else {
            return;
        };
        let Some(action) = view.focused_action() else {
            return;
        };
        match view.resolve(action) {
            StoreTapDecision::None => {}
            StoreTapDecision::Back => self.close_store_tap(),
            StoreTapDecision::Prepare { release } => self.store_begin_download(release),
            StoreTapDecision::StartDownload { consent_id } => {
                let started = self.store_send(StoreRequest::Start {
                    consent_id: consent_id.clone(),
                });
                if !started {
                    if let Some(view) = self.model.store_tap.as_mut() {
                        apply_error(
                            view,
                            &StoreApiError::Offline("no store service".into()),
                            true,
                        );
                    }
                }
                self.needs_redraw = true;
            }
            StoreTapDecision::ConfirmPick { candidate } => {
                self.store_begin_download(crate::store_tap::TapRelease {
                    store_release_id: candidate.store_release_id,
                    title: candidate.title,
                    artist: candidate.artist,
                    track_count: candidate.track_count,
                });
            }
            StoreTapDecision::RecordWant { url, .. } => {
                if self.store_send(StoreRequest::Want { url }) {
                    self.compositor.show_status("Saved to your wants", self.now_ms);
                } else {
                    self.compositor
                        .show_status("No store service\nInterest not saved", self.now_ms);
                }
                self.needs_redraw = true;
            }
            StoreTapDecision::ShowPurchase {
                title,
                artist,
                url,
                release_id,
            } => match url {
                Some(url) => {
                    if let Some(view) = self.model.store_tap.as_mut() {
                        view.phase = StoreTapPhase::NotOwned {
                            title,
                            artist,
                            url,
                            interest_saved: false,
                            from_pick: true,
                            release_id: release_id.filter(|id| !id.is_empty()),
                        };
                        view.focus = 0;
                    }
                    self.store_want_cover();
                    self.needs_redraw = true;
                }
                None => {
                    self.compositor
                        .show_status("No purchase page\nfor this release", self.now_ms);
                }
            },
            StoreTapDecision::Sync => self.store_tap_begin_sync(),
            StoreTapDecision::Play => {
                let relpath = match self.model.store_tap.as_ref().map(|v| &v.phase) {
                    Some(StoreTapPhase::Complete {
                        noudata: Some(path), ..
                    }) => Some(path.clone()),
                    _ => None,
                };
                self.close_store_tap();
                match relpath {
                    Some(relpath) => self.request_browse(BrowseRequest::FindForPlay { relpath }),
                    // No `.noudata` (placed from store metadata): the library is the place to
                    // find it, and it is one press away.
                    None => self.store_show_in_library(),
                }
            }
            StoreTapDecision::ShowInLibrary => self.store_show_in_library(),
            StoreTapDecision::AllowLookup => {
                // The consent just given covers this tap; a later tap asks again.
                let ask = self
                    .model
                    .store_tap
                    .as_mut()
                    .map(|view| {
                        view.lookup_allowed = true;
                        view.begin_call(self.now_ms);
                        view.phase = StoreTapPhase::Lookup;
                        view.url.clone()
                    })
                    .filter(|_| self.store.is_some());
                match ask {
                    Some(url) => {
                        self.store_send(StoreRequest::Tap {
                            url,
                            expected_duration_s: None,
                            consent: true,
                        });
                    }
                    None => {
                        if let Some(view) = self.model.store_tap.as_mut() {
                            view.phase = StoreTapPhase::Blocked {
                                blocked: TapBlocked::NoStore,
                                message: blocked_message(TapBlocked::NoStore).to_string(),
                            };
                        }
                    }
                }
                self.needs_redraw = true;
            }
            StoreTapDecision::Retry => {
                let retry = self
                    .model
                    .store_tap
                    .as_mut()
                    .map(|view| {
                        view.retries = view.retries.saturating_add(1);
                        view.begin_call(self.now_ms);
                        view.phase = StoreTapPhase::Lookup;
                        (view.url.clone(), view.lookup_allowed)
                    })
                    .filter(|_| self.store.is_some());
                match retry {
                    Some((url, consent)) => {
                        self.store_send(StoreRequest::Tap {
                            url,
                            expected_duration_s: None,
                            consent,
                        });
                        self.needs_redraw = true;
                    }
                    None => {
                        if let Some(view) = self.model.store_tap.as_mut() {
                            view.phase = StoreTapPhase::Blocked {
                                blocked: TapBlocked::NoStore,
                                message: blocked_message(TapBlocked::NoStore).to_string(),
                            };
                        }
                        self.needs_redraw = true;
                    }
                }
            }
        }
    }

    /// Price the download, then start it when the person confirms the consent screen.
    fn store_begin_download(&mut self, release: crate::store_tap::TapRelease) {
        let key = self
            .model
            .store_tap
            .as_ref()
            .and_then(|view| view.noubin_key.clone())
            .unwrap_or_default();
        let release_id = release.store_release_id.clone();
        if let Some(view) = self.model.store_tap.as_mut() {
            view.pending_release = Some(release);
            view.phase = StoreTapPhase::Preparing;
            view.begin_call(self.now_ms);
            view.focus = 0;
        }
        if !self.store_send(StoreRequest::Prepare {
            noubin_key: key,
            release_id,
        }) {
            if let Some(view) = self.model.store_tap.as_mut() {
                apply_error(
                    view,
                    &StoreApiError::Offline("no store service".into()),
                    false,
                );
            }
        }
        self.needs_redraw = true;
    }

    fn store_show_in_library(&mut self) {
        self.close_store_tap();
        self.model.browse_kind = None;
        self.model.library.clear();
        self.request_browse(BrowseRequest::Counts);
        let _ = self.nav.push(Route::Library);
        self.needs_redraw = true;
    }

    /// Rotate on the store screen: the action rows read top-down like every other list.
    fn store_rotate(&mut self, delta: i8) {
        if let Some(view) = self.model.store_tap.as_mut() {
            view.move_focus(crate::nav::menu_step(delta));
            self.needs_redraw = true;
        }
    }

    /// Serve the store worker each frame: answers, the lookup timeout and download polling.
    /// Called from the `Tick` input path so the in-process harness exercises it too.
    pub fn tick_store(&mut self, now_ms: u64) {
        if self.store.is_none()
            && self.model.store_tap.is_none()
            && self.store_watch.is_none()
            && self.model.store_stores.is_none()
            && self.model.store_connect.is_none()
            && self.model.store_releases.is_none()
            && self.model.store_release.is_none()
            && self.model.store_delete.is_none()
            && self.model.track_analysis.is_none()
        {
            return;
        }
        // Bounded drain: one chatty answer cannot starve the frame.
        for _ in 0..8 {
            let Some(event) = self.store.as_ref().and_then(StoreWorker::try_recv) else {
                break;
            };
            self.apply_store_event(event, now_ms);
        }

        // A call nobody answered is reported as offline rather than a spinner forever.
        let timed_out = self
            .model
            .store_tap
            .as_mut()
            .filter(|view| view.timed_out(now_ms))
            .map(|view| {
                apply_error(
                    view,
                    &StoreApiError::Offline("the store service did not answer".into()),
                    false,
                );
                view.focus = 0;
            })
            .is_some();
        if timed_out {
            self.needs_redraw = true;
        }

        // BCS_08: the same rule for every store screen — a call nobody answered becomes a
        // state the person can read and retry, never a spinner that never ends.
        let timeout = StoreApiError::Offline("the store service did not answer".into());
        let mut expired = false;
        if self
            .model
            .store_connect
            .as_ref()
            .is_some_and(|view| view.timed_out(now_ms))
        {
            if let Some(view) = self.model.store_connect.as_mut() {
                view.apply(Err(timeout.clone()));
            }
            expired = true;
        }
        if self
            .model
            .network
            .as_ref()
            .is_some_and(|view| view.timed_out(now_ms))
        {
            if let Some(view) = self.model.network.as_mut() {
                view.apply(Err(timeout.clone()));
            }
            expired = true;
        }
        if self
            .model
            .store_stores
            .as_ref()
            .is_some_and(|view| view.timed_out(now_ms))
        {
            if let Some(view) = self.model.store_stores.as_mut() {
                view.apply(Err(timeout.clone()));
            }
            expired = true;
        }
        if self
            .model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.timed_out(now_ms))
        {
            if let Some(view) = self.model.store_releases.as_mut() {
                view.fail(&timeout);
            }
            expired = true;
        }
        if self
            .model
            .store_release
            .as_ref()
            .is_some_and(|view| view.timed_out(now_ms))
        {
            if let Some(view) = self.model.store_release.as_mut() {
                if matches!(view.phase, ReleasePhase::Loading) {
                    view.apply_info(Err(StoreApiError::Offline(
                        "the store service did not answer".into(),
                    )));
                } else {
                    view.apply_error(&StoreApiError::Offline(
                        "the store service did not answer".into(),
                    ));
                }
            }
            expired = true;
        }
        if self
            .model
            .store_delete
            .as_ref()
            .is_some_and(|view| view.timed_out(now_ms))
        {
            if let Some(view) = self.model.store_delete.as_mut() {
                if matches!(view.phase, DeletePhase::Deleting) {
                    view.apply_done(Err(StoreApiError::Offline(
                        "the store service did not answer".into(),
                    )));
                } else {
                    view.apply_preview(Err(StoreApiError::Offline(
                        "the store service did not answer".into(),
                    )));
                }
            }
            expired = true;
        }
        // TRACK_01: the analysis screen follows the same rule, and polls only while it is the
        // screen showing and something is still queued or analysing.
        if self
            .model
            .track_analysis
            .as_ref()
            .is_some_and(|view| view.timed_out(now_ms))
        {
            if let Some(view) = self.model.track_analysis.as_mut() {
                view.apply_status(Err(timeout.clone()), now_ms);
            }
            expired = true;
        }
        if *self.nav.current() == Route::LibraryTrackAnalysis {
            let poll = self
                .model
                .track_analysis
                .as_mut()
                .filter(|view| view.poll_due(now_ms))
                .map(|view| {
                    view.begin_poll(now_ms);
                    (view.playable_id, view.item)
                });
            if let Some((playable_id, item)) = poll {
                self.send_analysis_status(playable_id, item, now_ms);
            }
        }
        // SECFIX_03: an armed "Regenerate certificate" row lapses rather than waiting forever
        // for a press the owner has forgotten about.
        if self
            .model
            .network
            .as_mut()
            .is_some_and(|view| view.expire_regenerate(now_ms))
        {
            expired = true;
        }
        if expired {
            self.needs_redraw = true;
        }

        // BCS_11: poll a sync the tap screen started, and one the Stores screen is following
        // (only while that screen is the one showing). One request a second, stopping at a
        // terminal answer; the poll sends no secrets.
        let tap_sync_due = self
            .model
            .store_tap
            .as_mut()
            .filter(|view| view.sync_poll_due(now_ms))
            .map(|view| {
                view.poll_at_ms = now_ms.saturating_add(crate::store_tap::SYNC_POLL_MS);
            })
            .is_some();
        if tap_sync_due {
            self.store_send(StoreRequest::SyncStatus);
        }
        let stores_visible = matches!(
            self.nav.current(),
            Route::SystemStores | Route::LibraryStores
        );
        if stores_visible {
            let stores_sync_due = self
                .model
                .store_stores
                .as_mut()
                .filter(|view| view.sync_poll_due(now_ms))
                .map(|view| {
                    view.sync_due_ms = now_ms.saturating_add(crate::store_tap::SYNC_POLL_MS);
                })
                .is_some();
            if stores_sync_due {
                self.store_send(StoreRequest::SyncStatus);
            }
        }

        // Poll a running download (screen showing).
        let poll = match self.model.store_tap.as_mut() {
            Some(view) => match &view.phase {
                StoreTapPhase::Downloading { session, .. } if now_ms >= view.poll_at_ms => {
                    view.poll_at_ms = now_ms.saturating_add(DOWNLOAD_POLL_MS);
                    Some(session.clone())
                }
                _ => None,
            },
            None => None,
        };
        if let Some(session) = poll {
            self.store_send(StoreRequest::Status { session });
        }
        // Poll the download started from the release screen.
        let release_poll = match self.model.store_release.as_ref() {
            Some(view) => match &view.download {
                DownloadPhase::Running { session, .. } if now_ms >= view.poll_at_ms => {
                    Some(session.clone())
                }
                _ => None,
            },
            None => None,
        };
        if let Some(session) = release_poll {
            if let Some(view) = self.model.store_release.as_mut() {
                view.poll_at_ms = now_ms.saturating_add(DOWNLOAD_POLL_MS);
            }
            self.store_send(StoreRequest::Status { session });
        }
        // Poll a hidden download.
        if let Some((session, due)) = self.store_watch.clone() {
            if now_ms >= due {
                self.store_watch = Some((session.clone(), now_ms.saturating_add(DOWNLOAD_POLL_MS)));
                self.store_send(StoreRequest::Status { session });
            }
        }
    }

    fn apply_store_event(&mut self, event: StoreEvent, now_ms: u64) {
        match event {
            StoreEvent::Tap(result) => match result {
                Ok(answer) => self.store_apply_answer(&answer),
                Err(error) => {
                    if let Some(view) = self.model.store_tap.as_mut() {
                        apply_error(view, &error, false);
                    }
                    self.needs_redraw = true;
                }
            },
            StoreEvent::Want(result) => {
                match result {
                    Ok(()) => {
                        if let Some(StoreTapPhase::NotOwned { interest_saved, .. }) =
                            self.model.store_tap.as_mut().map(|view| &mut view.phase)
                        {
                            *interest_saved = true;
                        }
                    }
                    Err(error) => {
                        self.compositor
                            .show_status(&format!("Interest not saved\n{error}"), now_ms);
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::Consent(result) => {
                // BCS_08: a price asked for from the release screen lands there.
                let tap_asking = self
                    .model
                    .store_tap
                    .as_ref()
                    .is_some_and(|view| matches!(view.phase, StoreTapPhase::Preparing));
                if !tap_asking {
                    if let Some(view) = self.model.store_release.as_mut() {
                        if matches!(view.download, DownloadPhase::Preparing) {
                            view.apply_consent(result);
                            self.needs_redraw = true;
                            return;
                        }
                    }
                }
                match result {
                    Ok(consent) => {
                        let release = self
                            .model
                            .store_tap
                            .as_mut()
                            .filter(|view| matches!(view.phase, StoreTapPhase::Preparing))
                            .and_then(|view| view.pending_release.clone());
                        // Nothing happens when the tap screen moved on while the price was
                        // being worked out: the answer has no screen to land on.
                        if let (Some(release), Some(view)) =
                            (release, self.model.store_tap.as_mut())
                        {
                            view.phase = StoreTapPhase::Confirm { release, consent };
                            view.focus = 0;
                            view.armed = true;
                        }
                        self.store_want_cover();
                    }
                    Err(error) => {
                        if let Some(view) = self.model.store_tap.as_mut() {
                            apply_error(view, &error, true);
                        }
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::Session(result) => {
                let tap_asking = self.model.store_tap.as_ref().is_some_and(|view| {
                    matches!(
                        view.phase,
                        StoreTapPhase::Confirm { .. } | StoreTapPhase::Preparing
                    )
                });
                if !tap_asking {
                    // BCS_08: a download started from the release screen owns its answer.
                    let release_asking =
                        self.model.store_release.as_ref().is_some_and(|view| {
                            matches!(
                                view.download,
                                DownloadPhase::Confirm { .. } | DownloadPhase::Preparing
                            )
                        });
                    if release_asking {
                        if let Some(view) = self.model.store_release.as_mut() {
                            view.apply_session(result, now_ms);
                        }
                        self.needs_redraw = true;
                        return;
                    }
                }
                match result {
                    Ok(session) => {
                        if let Some(view) = self.model.store_tap.as_mut() {
                            let asking = matches!(
                                view.phase,
                                StoreTapPhase::Confirm { .. } | StoreTapPhase::Preparing
                            );
                            if asking {
                                view.phase = StoreTapPhase::Downloading {
                                    session: session.session.clone(),
                                    progress: crate::store_api::DownloadProgress::queued(),
                                };
                                view.poll_at_ms = now_ms.saturating_add(DOWNLOAD_POLL_MS);
                                view.focus = 0;
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(view) = self.model.store_tap.as_mut() {
                            apply_error(view, &error, true);
                        }
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::Progress { session, progress } => {
                // BCS_08: a download started from the release screen owns its own answers.
                if let Ok(answer) = progress.as_ref() {
                    if self.store_release_progress(&session, answer, now_ms) {
                        return;
                    }
                }
                if let Err(error) = progress.as_ref() {
                    // BCS_08: the release screen's own session failing is its state to show.
                    let mine = self.model.store_release.as_ref().is_some_and(|view| {
                        matches!(&view.download, DownloadPhase::Running { session: current, .. }
                            if current == &session)
                    });
                    if mine {
                        if let Some(view) = self.model.store_release.as_mut() {
                            view.apply_error(error);
                        }
                        self.needs_redraw = true;
                        return;
                    }
                }
                match progress {
                Ok(progress) => self.store_apply_progress(&session, &progress, now_ms),
                Err(error) => {
                    let watching = self.model.store_tap.is_some();
                    if let Some(view) = self.model.store_tap.as_mut() {
                        apply_error(view, &error, true);
                    } else if self
                        .store_watch
                        .as_ref()
                        .is_some_and(|(hidden, _)| hidden == &session)
                    {
                        self.store_watch = None;
                        self.compositor
                            .show_status(&format!("Download stopped\n{error}"), now_ms);
                    }
                    if watching {
                        self.needs_redraw = true;
                    }
                }
                }
            }
            StoreEvent::Cancel(_) => {
                // The cancel answer is only an acknowledgement: the next status poll is what
                // moves the screen to "cancelled", so one code path owns that transition.
                if let Some(view) = self.model.store_tap.as_mut() {
                    view.poll_at_ms = now_ms;
                }
                if let Some(view) = self.model.store_release.as_mut() {
                    view.poll_at_ms = now_ms;
                }
            }
            // ---- BCS_08 ---------------------------------------------------------------
            StoreEvent::Portal(result) => {
                if let Ok(address) = &result {
                    self.model.portal_enabled = Some(!address.portal_off);
                }
                if let Some(view) = self.model.store_connect.as_mut() {
                    view.apply(result);
                }
                self.needs_redraw = true;
            }
            // ---- SECFIX_02 ------------------------------------------------------------
            StoreEvent::PortalSettings(result) => self.network_apply(result),
            StoreEvent::Stores(result) => {
                let mut connected = false;
                if let Some(view) = self.model.store_stores.as_mut() {
                    view.apply(result);
                    connected = view.connected();
                }
                if connected {
                    // BCS_11: a connected store may be mid-sync (started elsewhere) and has a
                    // "last synced" time to show: ask once, the poll takes over if it runs.
                    self.store_send(StoreRequest::SyncStatus);
                    self.store_send(StoreRequest::LastSynced);
                }
                self.needs_redraw = true;
            }
            StoreEvent::Collection {
                query,
                offset,
                page,
            } => {
                if let Some(view) = self.model.store_releases.as_mut() {
                    // A slow page for a list the person already replaced (a new search, or
                    // Back to the old one) is not this list's answer.
                    if view.query == query {
                        let first_page = !matches!(
                            view.phase,
                            ReleasesPhase::Ready
                                | ReleasesPhase::LoadingMore
                                | ReleasesPhase::PageFailed { .. }
                        );
                        if offset == 0 && first_page {
                            view.apply_first(page);
                        } else {
                            view.apply_more(offset, page);
                        }
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::Release { release_id, info } => {
                if let Some(view) = self.model.store_release.as_mut() {
                    // A slow answer for a release the person already left is dropped by the id.
                    if view.release_id == release_id {
                        view.apply_info(info);
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::DeletePreview(result) => {
                if let Some(view) = self.model.store_delete.as_mut() {
                    view.apply_preview(result);
                }
                self.needs_redraw = true;
            }
            StoreEvent::Sync(result) => self.store_apply_sync(result, now_ms),
            StoreEvent::LastSynced(result) => {
                if let (Some(view), Ok(at)) = (self.model.store_stores.as_mut(), result) {
                    view.synced_at = at;
                }
                self.needs_redraw = true;
            }
            StoreEvent::ReleaseCover {
                release_id,
                side,
                result,
            } => {
                let image = result.ok();
                if let Some(image) = &image {
                    self.store_covers.insert(&release_id, side, image.clone());
                }
                // A late answer for a screen that moved on is cached but not shown.
                if let Some(view) = self.model.store_tap.as_mut() {
                    if view.cover_for.as_deref() == Some(release_id.as_str())
                        && side == crate::screens::STORE_COVER_SIDE
                    {
                        view.cover = match image {
                            Some(image) => CoverState::Ready(image),
                            None => CoverState::Failed,
                        };
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::AnalysisStatus {
                playable_id,
                item,
                result,
            } => {
                // A late answer for a track the person already left is dropped by its ids.
                if let Some(view) = self.model.track_analysis.as_mut() {
                    if view.playable_id == playable_id && view.item == item {
                        view.apply_status(result, now_ms);
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::Reanalysed {
                playable_id,
                item,
                kind,
                result,
            } => {
                if let Some(view) = self.model.track_analysis.as_mut() {
                    if view.playable_id == playable_id && view.item == item {
                        view.apply_reanalyse(kind, result, now_ms);
                    }
                }
                self.needs_redraw = true;
            }
            StoreEvent::Deleted(result) => {
                let ok = result.is_ok();
                if let Some(view) = self.model.store_delete.as_mut() {
                    view.apply_done(result);
                }
                if ok {
                    self.store_delete_finished();
                }
                self.needs_redraw = true;
            }
        }
    }

    /// A sync answer belongs to the tap screen while it waits on one, else to the Stores
    /// screen that is following the sync.
    fn store_apply_sync(
        &mut self,
        result: Result<crate::store_api::SyncStatus, StoreApiError>,
        now_ms: u64,
    ) {
        let waiting = self
            .model
            .store_tap
            .as_ref()
            .is_some_and(|view| matches!(view.phase, StoreTapPhase::Syncing { .. }));
        if waiting {
            let step = self
                .model
                .store_tap
                .as_mut()
                .map(|view| apply_sync(view, result, now_ms))
                .unwrap_or(SyncStep::Ignored);
            if step == SyncStep::Resend {
                // The collection was just written: ask the original question again, once.
                let ask = self
                    .model
                    .store_tap
                    .as_ref()
                    .map(|view| (view.url.clone(), view.lookup_allowed));
                if let Some((url, consent)) = ask {
                    self.store_send(StoreRequest::Tap {
                        url,
                        expected_duration_s: None,
                        consent,
                    });
                }
                self.request_browse(BrowseRequest::Counts);
            }
            self.needs_redraw = true;
            return;
        }
        let Some(view) = self.model.store_stores.as_mut() else {
            return;
        };
        let note = view.apply_sync(result, now_ms);
        if let crate::store_browse::SyncNote::Finished { releases } = note {
            self.compositor
                .show_status(&format!("Collection synced: {releases} releases"), now_ms);
            // The row's count and the "last synced" line move with the new cache.
            self.refresh_stores();
        }
        self.needs_redraw = true;
    }

    fn store_apply_answer(&mut self, answer: &TapAnswer) {
        // BCS_11: with "Skip internet ask" on, the tag-page lookup goes ahead as if the person
        // had pressed Look up release. Once per tap (`lookup_allowed`), so a server that keeps
        // asking cannot loop; a Bandcamp link never reaches this state.
        let skip = answer.state == crate::store_api::TapState::LookupConsent
            && self.play_mode.settings.resolve_skip_internet_consent()
            && self.store.is_some()
            && self
                .model
                .store_tap
                .as_ref()
                .is_some_and(|view| !view.lookup_allowed);
        if skip {
            let host = answer.lookup.as_ref().map(|l| l.host.as_str()).unwrap_or("");
            eprintln!("plippa-ui: {}", consent_skipped_log_line(host));
            let now = self.now_ms;
            let url = self.model.store_tap.as_mut().map(|view| {
                view.lookup_allowed = true;
                view.begin_call(now);
                view.phase = StoreTapPhase::Lookup;
                view.armed = true;
                view.url.clone()
            });
            if let Some(url) = url {
                self.store_send(StoreRequest::Tap {
                    url,
                    expected_duration_s: None,
                    consent: true,
                });
            }
            self.needs_redraw = true;
            return;
        }
        let Some(view) = self.model.store_tap.as_mut() else {
            return;
        };
        // SEC-12: every string on this screen comes from the tag or the air.
        let mut answer = answer.clone();
        answer.message = screen_text(&answer.message);
        for candidate in &mut answer.candidates {
            candidate.title = screen_text(&candidate.title);
            candidate.artist = screen_text(&candidate.artist);
        }
        if let Some(release) = &mut answer.release {
            release.title = screen_text(&release.title);
            release.artist = screen_text(&release.artist);
        }
        answer.purchase_url = answer
            .purchase_url
            .filter(|url| url.starts_with("https://") && url.len() <= 512);
        view.apply_answer(&answer);
        self.store_want_cover();
        self.needs_redraw = true;
    }

    /// Ask for the cover of the release the tap screen now names, unless it is already known.
    /// The fetch and decode run on the store worker; the screen draws its placeholder until the
    /// answer lands and keeps it if the cover cannot be used.
    fn store_want_cover(&mut self) {
        let side = crate::screens::STORE_COVER_SIDE;
        let Some(view) = self.model.store_tap.as_mut() else {
            return;
        };
        let Some(id) = view.cover_release_id().map(str::to_string) else {
            return;
        };
        if view.cover_for.as_deref() == Some(id.as_str()) && view.cover != CoverState::None {
            return;
        }
        view.cover_for = Some(id.clone());
        if let Some(image) = self.store_covers.get(&id, side) {
            view.cover = CoverState::Ready(image);
            return;
        }
        view.cover = CoverState::Loading;
        if !self.store_send(StoreRequest::ReleaseCover {
            release_id: id,
            side,
        }) {
            if let Some(view) = self.model.store_tap.as_mut() {
                view.cover = CoverState::Failed;
            }
        }
    }

    /// Start a collection sync from the tap screen; the finished sync repeats the tap once.
    fn store_tap_begin_sync(&mut self) {
        let now = self.now_ms;
        if let Some(view) = self.model.store_tap.as_mut() {
            view.resync_pending = true;
            view.begin_call(now);
            view.phase = StoreTapPhase::Syncing { status: None };
            view.poll_at_ms = now.saturating_add(crate::store_tap::SYNC_POLL_MS);
        }
        if !self.store_send(StoreRequest::SyncStart) {
            if let Some(view) = self.model.store_tap.as_mut() {
                view.phase = StoreTapPhase::SyncFailed {
                    message: "No store service is available to sync with.".to_string(),
                };
                view.resync_pending = false;
                view.armed = true;
            }
        }
        self.needs_redraw = true;
    }

    fn store_apply_progress(
        &mut self,
        session: &str,
        progress: &crate::store_api::DownloadProgress,
        now_ms: u64,
    ) {
        let watching = self
            .model
            .store_tap
            .as_ref()
            .is_some_and(|view| matches!(&view.phase, StoreTapPhase::Downloading { session: s, .. } if s == session));
        if watching {
            let landed = {
                let view = self
                    .model
                    .store_tap
                    .as_mut()
                    .expect("checked just above");
                apply_progress(view, session, progress);
                view.poll_at_ms = now_ms.saturating_add(DOWNLOAD_POLL_MS);
                matches!(view.phase, StoreTapPhase::Complete { .. })
            };
            if landed {
                self.request_browse(BrowseRequest::Counts);
                if let Some(StoreTapPhase::Complete { files, .. }) =
                    self.model.store_tap.as_ref().map(|view| &view.phase)
                {
                    let text = if *files == 0 {
                        "Already in your library".to_string()
                    } else if *files == 1 {
                        "Added 1 file".to_string()
                    } else {
                        format!("Added {files} files")
                    };
                    self.compositor.show_status(&text, now_ms);
                }
            }
            self.needs_redraw = true;
            return;
        }
        // A hidden download: report the outcome, then stop watching.
        if self
            .store_watch
            .as_ref()
            .is_some_and(|(hidden, _)| hidden == session)
            && progress.finished()
        {
            self.store_watch = None;
            self.request_browse(BrowseRequest::Counts);
            let text = match progress.state {
                DownloadState::Done if progress.placed_files > 0 => {
                    format!("Added {} files", progress.placed_files)
                }
                DownloadState::Done if progress.already_present => {
                    "Already in your library".to_string()
                }
                DownloadState::Done => "Download finished, nothing added".to_string(),
                DownloadState::Partial => "Download partly finished".to_string(),
                DownloadState::Cancelled => "Download cancelled".to_string(),
                DownloadState::Failed => "Download failed".to_string(),
                // `Running` cannot reach here (behind `finished()`), but a total match keeps
                // the wording honest if that ever changes.
                DownloadState::Running => "Download running".to_string(),
            };
            self.compositor.show_status(&text, now_ms);
            self.needs_redraw = true;
        }
    }

    // ---------------------------------------------------------------------
    // BCS_08: stores, connect by QR, browsing and removing
    // ---------------------------------------------------------------------

    /// The error a screen carries when this device has no store service at all.
    fn store_unavailable(&self) -> StoreApiError {
        StoreApiError::Offline("no store service is configured".into())
    }

    /// Open the Stores screen from `route` (System > Stores, or Library > Stores).
    pub fn open_stores(&mut self, route: Route) {
        self.begin_stores();
        let _ = self.nav.push(route);
        self.needs_redraw = true;
    }

    /// Load the store status without touching the stack (the System menu already pushed it).
    pub fn begin_stores(&mut self) {
        self.model.store_stores = Some(StoresView::loading(self.now_ms));
        if !self.store_send(StoreRequest::Stores) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_stores.as_mut() {
                view.apply(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Ask the status again (Refresh / Retry).
    fn refresh_stores(&mut self) {
        if let Some(view) = self.model.store_stores.as_mut() {
            view.phase = StoresPhase::Loading;
            view.requested_at_ms = self.now_ms;
        }
        if !self.store_send(StoreRequest::Stores) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_stores.as_mut() {
                view.apply(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    // ---------------------------------------------------------------------
    // SECFIX_02: System > Network
    // ---------------------------------------------------------------------

    /// The rows of the Network menu as they are now.
    fn network_rows(&self) -> Vec<NetworkRow> {
        self.model
            .network
            .as_ref()
            .map_or_else(|| NetworkView::looking(0).rows(), NetworkView::rows)
    }

    /// The System menu pushed Network: refresh the addresses and ask the server for the portal's
    /// settings. The frame loop never waits for the answer.
    fn begin_network(&mut self, wifi: &mut dyn WifiBackend) {
        self.sync_network_from_wifi(wifi);
        self.model.network = Some(NetworkView::looking(self.now_ms));
        if !self.store_send(StoreRequest::PortalSettings) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.network.as_mut() {
                view.apply(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Fold a portal-settings answer in (also the answer to every change).
    fn network_apply(&mut self, result: Result<PortalSettingsInfo, StoreApiError>) {
        if let Ok(settings) = &result {
            self.model.portal_enabled = Some(settings.enabled);
        }
        let was_typing = self
            .model
            .network
            .as_mut()
            .map(|view| std::mem::take(&mut view.pin_pending))
            .unwrap_or(false);
        let succeeded = result.is_ok();
        if let Some(view) = self.model.network.as_mut() {
            view.apply(result);
        }
        if was_typing && succeeded && *self.nav.current() == Route::SystemNetworkPin {
            // The new PIN is saved: back to the menu, which now shows it.
            let _ = self.nav.back();
            if let Some(view) = self.model.network.as_mut() {
                view.pin_pad = None;
            }
            self.compositor.show_status("PIN changed", self.now_ms);
        }
        self.needs_redraw = true;
    }

    /// Send one change and mark the view busy until the answer comes back.
    fn network_send(&mut self, request: StoreRequest) {
        if let Some(view) = self.model.network.as_mut() {
            view.busy = true;
            view.notice = None;
        }
        if !self.store_send(request) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.network.as_mut() {
                view.apply(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Enter on a row of the Network menu.
    fn network_enter(&mut self, wifi: &mut dyn WifiBackend) {
        let rows = self.network_rows();
        // The rows can shrink under the cursor (the certificate row goes with HTTPS off, the
        // lockout row with the lockout), so the focus is clamped before it is read.
        if rows.is_empty() {
            return;
        }
        if self.nav.focus >= rows.len() {
            self.nav.focus = rows.len() - 1;
            self.needs_redraw = true;
        }
        let Some(row) = rows.get(self.nav.focus).copied() else {
            return;
        };
        match row {
            NetworkRow::Ip => {
                self.sync_network_from_wifi(wifi);
                let _ = self.nav.push(Route::SystemIp);
                self.needs_redraw = true;
                return;
            }
            NetworkRow::Certificate => {
                // The fingerprint screen reads the same `NetworkView`, so no request is made.
                let _ = self.nav.push(Route::SystemCertificate);
                self.needs_redraw = true;
                return;
            }
            NetworkRow::SkipInternetAsk => {
                // BCS_11: a UI setting stored in `play_mode.json`, not a portal setting, so it
                // never waits for the server.
                let next = !self.play_mode.settings.resolve_skip_internet_consent();
                self.play_mode.settings.skip_internet_consent = Some(next);
                self.play_mode_changed();
                let text = format!(
                    "{}: {}",
                    NetworkRow::SkipInternetAsk.label(),
                    if next { "On" } else { "Off" }
                );
                self.compositor.show_status(&text, self.now_ms);
                return;
            }
            NetworkRow::Wifi => {
                if self.model.wifi.is_none() {
                    self.init_wifi(wifi);
                } else {
                    self.sync_network_from_wifi(wifi);
                }
                let _ = self.nav.push(Route::Wifi);
                self.needs_redraw = true;
                return;
            }
            _ => {}
        }
        // The portal rows act on settings the server owns.
        let Some(view) = self.model.network.as_ref() else {
            return;
        };
        let Some(settings) = view.settings.clone() else {
            // Never read (or the read failed): a press asks again instead of doing nothing.
            if !matches!(view.phase, NetworkPhase::Loading) {
                self.model.network = Some(NetworkView::looking(self.now_ms));
                if !self.store_send(StoreRequest::PortalSettings) {
                    let error = self.store_unavailable();
                    if let Some(view) = self.model.network.as_mut() {
                        view.apply(Err(error));
                    }
                }
                self.needs_redraw = true;
            }
            return;
        };
        if view.busy {
            return;
        }
        if !settings.available {
            let why = view
                .status_line()
                .unwrap_or_else(|| "The portal cannot be turned on here".into());
            self.compositor.show_status(&why, self.now_ms);
            return;
        }
        match row {
            NetworkRow::Portal => self.network_send(StoreRequest::SetPortalEnabled(!settings.enabled)),
            NetworkRow::Https => self.network_send(StoreRequest::SetHttps(!settings.https_enabled)),
            NetworkRow::Regenerate => {
                // A new certificate changes the fingerprint the owner verified, so the row asks
                // for a second press instead of acting on the first.
                let armed = self
                    .model
                    .network
                    .as_mut()
                    .is_some_and(|view| view.take_regenerate());
                if armed {
                    self.network_send(StoreRequest::RegenerateCertificate);
                } else {
                    if let Some(view) = self.model.network.as_mut() {
                        view.arm_regenerate(self.now_ms);
                    }
                    self.compositor.show_status(
                        "New certificate: every browser must accept it again",
                        self.now_ms,
                    );
                    self.needs_redraw = true;
                }
            }
            NetworkRow::PinRequired => {
                self.network_send(StoreRequest::SetPinRequired(!settings.pin_required))
            }
            NetworkRow::ClearLockout => self.network_send(StoreRequest::ClearLockout),
            NetworkRow::Pin => {
                if let Some(view) = self.model.network.as_mut() {
                    // UI_14: six fixed boxes, not the pairing keypad.
                    view.pin_pad = Some(PinPad::new());
                    view.notice = None;
                    view.pin_pending = false;
                }
                let _ = self.nav.push(Route::SystemNetworkPin);
                self.needs_redraw = true;
            }
            NetworkRow::Ip | NetworkRow::Wifi | NetworkRow::Certificate | NetworkRow::SkipInternetAsk => {}
        }
    }

    /// Enter on the PIN keypad: a digit pick, or on OK the six digits go to the server.
    fn network_pin_enter(&mut self) {
        let Some(view) = self.model.network.as_mut() else {
            return;
        };
        if view.pin_pending {
            return;
        }
        let Some(pad) = view.pin_pad.as_mut() else {
            return;
        };
        let PinAction::Submit(text) = pad.handle(PinEvent::Enter) else {
            self.needs_redraw = true;
            return;
        };
        match parse_typed_pin(&text) {
            Err(why) => view.notice = Some(why.to_string()),
            Ok(pin) => {
                view.pin_pending = true;
                view.notice = None;
                if !self.store_send(StoreRequest::SetPin(Pin(pin))) {
                    let error = self.store_unavailable();
                    self.network_apply(Err(error));
                }
            }
        }
        self.needs_redraw = true;
    }

    /// Open Connect Bandcamp (the QR code for the portal address).
    pub fn open_connect(&mut self) {
        self.begin_connect();
        let _ = self.nav.push(Route::StoreConnect);
        self.needs_redraw = true;
    }

    fn begin_connect(&mut self) {
        self.model.store_connect = Some(ConnectView::looking(self.now_ms));
        if !self.store_send(StoreRequest::Portal) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_connect.as_mut() {
                view.apply(Err(error));
            }
        }
    }

    /// Ask for the portal address again (after Wi-Fi came back, or the address changed).
    fn refresh_portal(&mut self) {
        if let Some(view) = self.model.store_connect.as_mut() {
            view.phase = ConnectPhase::Looking;
            view.requested_at_ms = self.now_ms;
        }
        if !self.store_send(StoreRequest::Portal) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_connect.as_mut() {
                view.apply(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Ask for one page of the collection as the current query left it.
    fn ask_collection(&mut self, offset: u32) {
        let Some(query) = self
            .model
            .store_releases
            .as_ref()
            .map(|view| view.query.clone())
        else {
            return;
        };
        if !self.store_send(StoreRequest::Collection {
            query,
            offset,
            size: RELEASE_PAGE_SIZE,
        }) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_releases.as_mut() {
                view.fail(&error);
            }
        }
    }

    /// Open the Bandcamp collection, first page.
    pub fn open_releases(&mut self) {
        self.model.store_releases = Some(ReleasesView::loading(self.now_ms));
        let _ = self.nav.push(Route::StoreReleases);
        self.ask_collection(0);
        self.needs_redraw = true;
    }

    /// Reload the first page (Retry after the whole list failed, or a new query).
    fn reload_releases(&mut self) {
        if let Some(view) = self.model.store_releases.as_mut() {
            view.phase = ReleasesPhase::Loading;
            view.requested_at_ms = self.now_ms;
        }
        self.ask_collection(0);
        self.needs_redraw = true;
    }

    /// Open the query keyboard, seeded with the query already shown.
    pub fn begin_search(&mut self) {
        let Some(view) = self.model.store_releases.as_mut() else {
            return;
        };
        view.begin_search();
        if !view.searching {
            return;
        }
        let seeded = view.query.clone();
        self.model.store_search = Some(KeyboardField::from_str(&seeded));
        self.needs_redraw = true;
    }

    /// Leave the keyboard, keeping the list that is on screen.
    pub fn cancel_search(&mut self) {
        self.model.store_search = None;
        if let Some(view) = self.model.store_releases.as_mut() {
            view.cancel_search();
        }
        self.needs_redraw = true;
    }

    /// Run the typed query from its first page.
    fn apply_search(&mut self, query: String) {
        self.model.store_search = None;
        let query = query.trim().to_string();
        if let Some(view) = self.model.store_releases.as_mut() {
            view.begin_query(query, self.now_ms);
        }
        self.nav.focus = 0;
        self.ask_collection(0);
        self.needs_redraw = true;
    }

    /// Ask for the next page (More / Retry after a page failed).
    fn load_more_releases(&mut self) {
        let Some(offset) = self
            .model
            .store_releases
            .as_ref()
            .and_then(|view| view.next_offset)
        else {
            return;
        };
        if let Some(view) = self.model.store_releases.as_mut() {
            view.begin_more(self.now_ms);
        }
        self.ask_collection(offset);
        self.needs_redraw = true;
    }

    /// Open one release, marking it downloaded when this session already placed it.
    pub fn open_release(&mut self, release_id: impl Into<String>) {
        let release_id = release_id.into();
        let mut view = ReleaseView::loading(release_id.clone(), self.now_ms);
        if let Some(item) = self
            .model
            .store_items
            .iter()
            .find(|item| item.release_id == release_id)
        {
            view.downloaded = Some(DownloadedRelease {
                noudata: item.noudata.clone(),
                files: item.files,
            });
        }
        self.model.store_release = Some(view);
        let _ = self.nav.push(Route::StoreRelease);
        if !self.store_send(StoreRequest::Release {
            release_id: release_id.clone(),
        }) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_release.as_mut() {
                view.apply_info(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Ask for the release again (Retry after the detail itself failed).
    fn reload_release(&mut self) {
        let Some(release_id) = self
            .model
            .store_release
            .as_ref()
            .map(|view| view.release_id.clone())
        else {
            return;
        };
        if let Some(view) = self.model.store_release.as_mut() {
            view.phase = ReleasePhase::Loading;
            view.requested_at_ms = self.now_ms;
        }
        if !self.store_send(StoreRequest::Release {
            release_id: release_id.clone(),
        }) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_release.as_mut() {
                view.apply_info(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Price the release, then start it when the person confirms.
    fn start_release_prepare(&mut self, release_id: String) {
        let key = self
            .model
            .store_release
            .as_ref()
            .and_then(|view| view.noubin_key.clone())
            .unwrap_or_default();
        if let Some(view) = self.model.store_release.as_mut() {
            view.begin_prepare(self.now_ms);
        }
        if !self.store_send(StoreRequest::Prepare {
            noubin_key: key,
            release_id,
        }) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_release.as_mut() {
                view.apply_consent(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Open Remove from Plippa for one library item (BCS_08 / BCS_10).
    pub fn open_remove(&mut self, playable_id: u64, title: &str, release_id: Option<String>) {
        self.model.store_delete = Some(DeleteView::loading(
            playable_id,
            screen_text(title),
            release_id,
            self.now_ms,
        ));
        let _ = self.nav.push(Route::LibraryRemove);
        if !self.store_send(StoreRequest::DeletePreview { playable_id }) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_delete.as_mut() {
                view.apply_preview(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// Open Remove from Plippa for the detail screen's item, when it is a store download.
    fn open_remove_for_detail(&mut self) {
        let Some(item) = self.model.detail_item().cloned() else {
            return;
        };
        let Some(record) = self.model.store_item_for_detail().cloned() else {
            return;
        };
        let Some(root) = item.library_root.clone() else {
            self.compositor
                .show_status("Library root missing", self.now_ms);
            return;
        };
        let playable_id = encode_playable_id(&root, item.id);
        self.open_remove(playable_id, &item.title, Some(record.release_id));
    }

    /// Ask for the delete preview again (Retry after it failed).
    fn reload_remove(&mut self) {
        let Some(playable_id) = self.model.store_delete.as_ref().map(|view| view.playable_id)
        else {
            return;
        };
        if let Some(view) = self.model.store_delete.as_mut() {
            view.phase = DeletePhase::Loading;
            view.requested_at_ms = self.now_ms;
        }
        if !self.store_send(StoreRequest::DeletePreview { playable_id }) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_delete.as_mut() {
                view.apply_preview(Err(error));
            }
        }
        self.needs_redraw = true;
    }

    /// "Show in library": land on the downloaded item's detail screen, not the library root.
    fn open_library_for_release(&mut self, release_id: &str) {
        let relpath = self
            .model
            .store_items
            .iter()
            .find(|item| item.release_id == release_id)
            .and_then(|item| item.noudata.clone());
        match relpath {
            Some(relpath) => self.request_browse(BrowseRequest::FindForDetail { relpath }),
            None => {
                // The server named no `.noudata` (placed from store metadata): the library
                // root is where it can be found, and it is one press away.
                self.model.browse_kind = None;
                self.model.library.clear();
                self.request_browse(BrowseRequest::Counts);
                let _ = self.nav.push(Route::Library);
            }
        }
        self.needs_redraw = true;
    }

    /// Leave the release screen, cancelling a download still running on the way out.
    fn close_store_release(&mut self) {
        if let Some(DownloadPhase::Running { session, progress }) =
            self.model.store_release.as_ref().map(|v| v.download.clone())
        {
            if !progress.finished() {
                self.store_send(StoreRequest::Cancel {
                    session: session.clone(),
                });
                self.store_watch = Some((session, self.now_ms.saturating_add(DOWNLOAD_POLL_MS)));
            }
        }
        self.model.store_release = None;
    }

    /// Leave whichever BCS_08 screen is current, clearing its state.
    fn leave_store_screen(&mut self) {
        match self.nav.current() {
            Route::SystemStores | Route::LibraryStores => {
                self.model.store_stores = None;
            }
            Route::StoreConnect => {
                self.model.store_connect = None;
            }
            Route::StoreReleases => {
                self.model.store_releases = None;
            }
            Route::StoreRelease => self.close_store_release(),
            Route::LibraryRemove => {
                self.model.store_delete = None;
            }
            _ => return,
        }
        let _ = self.nav.back();
        self.needs_redraw = true;
    }

    fn stores_enter(&mut self) {
        let (actions, connected) = match self.model.store_stores.as_ref() {
            Some(view) => (view.actions(), view.connected()),
            None => return,
        };
        let Some(action) = actions.get(self.nav.focus).copied() else {
            return;
        };
        match action {
            // The Bandcamp row opens the collection when connected, the QR when not.
            StoresAction::Bandcamp if connected => self.open_releases(),
            StoresAction::Bandcamp | StoresAction::Connect => self.open_connect(),
            StoresAction::Refresh | StoresAction::Retry => self.refresh_stores(),
            StoresAction::Sync => self.stores_start_sync(),
            StoresAction::Back => self.leave_store_screen(),
        }
    }

    /// Start a collection sync from the Stores screen and follow it with the 1 s poll. The
    /// screen keeps going if the person leaves: the server runs the crawl, and re-entering
    /// reads its state again.
    fn stores_start_sync(&mut self) {
        let now = self.now_ms;
        let allowed = self
            .model
            .store_stores
            .as_ref()
            .is_some_and(|view| view.enabled(StoresAction::Sync));
        if !allowed {
            return;
        }
        if let Some(view) = self.model.store_stores.as_mut() {
            view.sync_requested(now);
        }
        if !self.store_send(StoreRequest::SyncStart) {
            let error = self.store_unavailable();
            if let Some(view) = self.model.store_stores.as_mut() {
                view.apply_sync(Err(error), now);
            }
        }
        // The Sync row turns into progress and cannot be chosen: rest on the first row.
        self.nav.focus = 0;
        self.needs_redraw = true;
    }

    fn connect_enter(&mut self) {
        let actions = match self.model.store_connect.as_ref() {
            Some(view) => view.actions(),
            None => return,
        };
        match actions.get(self.nav.focus).copied() {
            Some(ConnectAction::Retry) => self.refresh_portal(),
            Some(ConnectAction::Back) => self.leave_store_screen(),
            None => {}
        }
    }

    fn releases_enter(&mut self) {
        // While the keyboard is open, Enter edits a slot or submits; the rows are gone.
        if self
            .model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.searching)
        {
            let action = self
                .model
                .store_search
                .as_mut()
                .map(|kb| kb.handle(KeyboardEvent::Enter));
            if let Some(KeyboardAction::Submit(text)) = action {
                self.apply_search(text);
            }
            self.needs_redraw = true;
            return;
        }
        let row = self
            .model
            .store_releases
            .as_ref()
            .and_then(|view| view.rows().get(self.nav.focus).copied());
        let Some(row) = row else {
            return;
        };
        match row {
            ReleasesRow::Release(index) => {
                let Some(release) = self
                    .model
                    .store_releases
                    .as_ref()
                    .and_then(|view| view.releases.get(index))
                    .cloned()
                else {
                    return;
                };
                if self
                    .model
                    .store_items
                    .iter()
                    .any(|item| item.release_id == release.id)
                {
                    // Downloaded already: open the library entry, never a second download.
                    self.open_library_for_release(&release.id);
                } else {
                    self.open_release(release.id);
                }
            }
            ReleasesRow::More => self.load_more_releases(),
            ReleasesRow::Retry => match self.model.store_releases.as_ref().map(|v| &v.phase) {
                Some(ReleasesPhase::Blocked { .. }) => self.reload_releases(),
                _ => self.load_more_releases(),
            },
            ReleasesRow::Search => self.begin_search(),
            ReleasesRow::Back => self.leave_store_screen(),
        }
    }

    fn release_enter(&mut self) {
        let (actions, release_id, download) = match self.model.store_release.as_ref() {
            Some(view) => (
                view.actions(),
                view.release_id.clone(),
                view.download.clone(),
            ),
            None => return,
        };
        let Some(action) = actions.get(self.nav.focus).copied() else {
            return;
        };
        match action {
            ReleaseAction::Download => match download {
                DownloadPhase::Confirm { consent } => {
                    // Keep the screen on "preparing" until the session answers, so a second
                    // press cannot start the same consent twice.
                    if let Some(view) = self.model.store_release.as_mut() {
                        view.download = DownloadPhase::Preparing;
                    }
                    if !self.store_send(StoreRequest::Start {
                        consent_id: consent.consent_id,
                    }) {
                        let error = self.store_unavailable();
                        if let Some(view) = self.model.store_release.as_mut() {
                            view.apply_error(&error);
                        }
                    }
                }
                _ => self.start_release_prepare(release_id),
            },
            ReleaseAction::CancelDownload => {
                if let DownloadPhase::Running { session, .. } = download {
                    if self.store_send(StoreRequest::Cancel {
                        session: session.clone(),
                    }) {
                        self.compositor.show_status("Cancelling…", self.now_ms);
                    }
                    // Poll once more so the screen settles even if the cancel answer is late.
                    if let Some(view) = self.model.store_release.as_mut() {
                        view.poll_at_ms = self.now_ms;
                    }
                }
            }
            ReleaseAction::OpenInLibrary => self.open_library_for_release(&release_id),
            ReleaseAction::Remove => {
                // The delete path needs the library item's id, so walk to the item and open
                // the confirm as soon as the lookup lands (one press for the person).
                self.pending_remove = Some(release_id.clone());
                self.open_library_for_release(&release_id);
            }
            ReleaseAction::Retry => match self.model.store_release.as_ref().map(|v| &v.phase) {
                Some(ReleasePhase::Blocked { .. }) => self.reload_release(),
                // A failed download: back to Idle so Download is offered again.
                _ => {
                    if let Some(view) = self.model.store_release.as_mut() {
                        view.download = DownloadPhase::Idle;
                    }
                    self.needs_redraw = true;
                }
            },
            ReleaseAction::Back => self.leave_store_screen(),
        }
        self.needs_redraw = true;
    }

    fn remove_enter(&mut self) {
        let actions = match self.model.store_delete.as_ref() {
            Some(view) => view.actions(),
            None => return,
        };
        let Some(action) = actions.get(self.nav.focus).copied() else {
            return;
        };
        match action {
            DeleteAction::Remove => {
                let Some(playable_id) = self.model.store_delete.as_ref().map(|v| v.playable_id)
                else {
                    return;
                };
                if let Some(view) = self.model.store_delete.as_mut() {
                    view.begin_delete(self.now_ms);
                }
                if !self.store_send(StoreRequest::Delete { playable_id }) {
                    let error = self.store_unavailable();
                    if let Some(view) = self.model.store_delete.as_mut() {
                        view.apply_done(Err(error));
                    }
                }
            }
            DeleteAction::Retry => self.reload_remove(),
            DeleteAction::Back => self.leave_store_screen(),
        }
        self.needs_redraw = true;
    }

    /// The delete landed: drop the session's record, refresh the index and the collection.
    fn store_delete_finished(&mut self) {
        let release_id = self
            .model
            .store_delete
            .as_ref()
            .and_then(|view| view.release_id.clone());
        if let Some(release_id) = release_id.as_deref() {
            self.model
                .store_items
                .retain(|item| item.release_id != release_id);
            if let Some(view) = self.model.store_release.as_mut() {
                if view.release_id == release_id {
                    view.downloaded = None;
                    view.download = DownloadPhase::Idle;
                }
            }
        }
        self.request_browse(BrowseRequest::Counts);
        if let Some(kind) = self.model.browse_kind {
            self.request_browse(BrowseRequest::Category(kind));
        }
        self.needs_redraw = true;
    }

    /// Feed one status answer to the release screen's download. `false` when it belongs to
    /// another session, so the tap screen's handler can have it.
    fn store_release_progress(
        &mut self,
        session: &str,
        progress: &crate::store_api::DownloadProgress,
        now_ms: u64,
    ) -> bool {
        let record = {
            let Some(view) = self.model.store_release.as_mut() else {
                return false;
            };
            if !view.apply_progress(session, progress) {
                return false;
            }
            view.poll_at_ms = now_ms.saturating_add(DOWNLOAD_POLL_MS);
            view.downloaded.as_ref().map(|downloaded| StoreItem {
                release_id: view.release_id.clone(),
                title: view
                    .info
                    .as_ref()
                    .map(|info| info.release.title.clone())
                    .unwrap_or_default(),
                noudata: downloaded.noudata.clone(),
                files: downloaded.files,
            })
        };
        if let Some(record) = record {
            self.model
                .store_items
                .retain(|item| item.release_id != record.release_id);
            self.model.store_items.push(record);
            self.request_browse(BrowseRequest::Counts);
            if let Some(completed) = self.model.store_release.as_ref().map(|v| v.download.clone())
            {
                if let DownloadPhase::Done { files, partial } = completed {
                    let text = if partial {
                        "Download partly finished".to_string()
                    } else if files == 1 {
                        "Added 1 file".to_string()
                    } else {
                        format!("Added {files} files")
                    };
                    self.compositor.show_status(&text, now_ms);
                }
            }
        }
        self.needs_redraw = true;
        true
    }

    /// The library index changed (or a stick came/went): refresh whatever is on screen.
    pub fn library_index_changed(&mut self) {        // The covers may have been edited (or the stick swapped): resolve and decode afresh.
        self.art.invalidate();
        self.cover_viewer.invalidate();
        self.request_browse(BrowseRequest::Counts);
        if let Some(kind) = self.model.browse_kind {
            self.request_browse(BrowseRequest::Category(kind));
            if let Some(item) = self.model.detail_item().cloned() {
                self.request_browse(BrowseRequest::Detail {
                    item,
                    kind: Some(kind),
                });
            }
        }
    }

    pub fn init_wifi(&mut self, wifi: &mut dyn WifiBackend) {
        let mut form = WifiForm::new(wifi.scan());
        form.selected_ssid = wifi.current_ssid();
        form.observe(wifi);
        self.model.wifi = Some(form);
        self.sync_network_from_wifi(wifi);
    }

    pub fn sync_network_from_wifi(&mut self, wifi: &dyn WifiBackend) {
        self.model.system.ip = wifi.current_ip();
        self.model.system.ips = wifi.all_ips();
        if self.model.system.ips.is_empty() {
            if let Some(ip) = &self.model.system.ip {
                self.model.system.ips.push(ip.clone());
            }
        }
        self.model.system.portal_url = wifi.portal_url();
        if let Some(form) = self.model.wifi.as_mut() {
            // UI_14: the root menu's rows are computed from the radio state, so every sync
            // refreshes it — the rows appear and disappear as the association does.
            form.observe(wifi);
            if form.selected_ssid.is_none() {
                form.selected_ssid = wifi.current_ssid();
            }
        }
    }

    /// Enter / leave keyboard mode: stash or restore Encoder A (UI_04/UI_05).
    fn sync_keyboard_a_override(&mut self) {
        let in_kb = self
            .model
            .wifi
            .as_ref()
            .is_some_and(|f| f.in_keyboard());
        if in_kb {
            self.assign.begin_keyboard_override();
        } else if self.assign.keyboard_override_active() {
            self.assign.end_keyboard_override();
        }
    }

    pub fn take_outgoing(&mut self) -> Vec<UiOutgoing> {
        std::mem::take(&mut self.outgoing)
    }

    /// PWRSW_01: the 1 s warning popup is up (the loop paints the power LED amber).
    pub fn power_warning_active(&self) -> bool {
        self.power.warning_active()
    }

    /// PWRSW_01: the 4 s hold has fired and the acknowledgement delay is running.
    pub fn shutdown_pending(&self) -> bool {
        self.shutdown_at.is_some()
    }

    fn on_power_hold(&mut self, ev: PowerHoldEvent) {
        match ev {
            PowerHoldEvent::Warning => {
                self.compositor.show_status_sticky(POWER_HOLD_WARNING);
                self.needs_redraw = true;
            }
            PowerHoldEvent::Cancelled => {
                self.compositor.clear_status();
                self.needs_redraw = true;
            }
            PowerHoldEvent::ShutdownRequest => {
                // Audio stops immediately: the ack delay is only for the popup.
                self.queue_cmd(PlaybackCommand::Pause);
                self.compositor.show_status_sticky(if self.cfg.poweroff_armed {
                    "Powering off…"
                } else {
                    "Shutting down…"
                });
                self.shutdown_at = Some(self.now_ms.saturating_add(POWER_OFF_ACK_MS));
                self.needs_redraw = true;
            }
        }
    }

    pub fn take_hold_events(&mut self) -> Vec<HoldEvent> {
        std::mem::take(&mut self.hold_events)
    }

    /// Re-queue an outgoing signal (loop runner / tests).
    ///
    /// Coalesces the warp commands (see [`crate::warp_ui::push_coalesced`]) so a dead engine
    /// connection never accumulates stale `SetRate` / `SeekTo` messages.
    pub fn push_outgoing(&mut self, out: UiOutgoing) {
        crate::warp_ui::push_coalesced(&mut self.outgoing, out);
    }

    /// Queue an outgoing IPC command through the coalescer (all warp UI code uses this).
    pub(crate) fn queue_cmd(&mut self, cmd: PlaybackCommand) {
        crate::warp_ui::push_coalesced(
            &mut self.outgoing,
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(cmd)),
        );
    }

    /// True when a `LoadAndPlay` is queued (library Play path) — loop runner uses this
    /// for a one-frame optimistic Playing LED before PlaybackStatus arrives.
    pub fn pending_load_and_play(&self) -> bool {
        self.outgoing.iter().any(|o| {
            matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(
                    PlaybackCommand::LoadAndPlay(_) | PlaybackCommand::LoadCollection { .. }
                ))
            )
        })
    }

    /// Previous/Next availability from the engine's status (`session.can_prev/can_next`,
    /// computed by `NowPlayingList::can_prev/can_next` in the engine — the UI never
    /// re-derives it). No status yet (engine not connected) = both unavailable.
    pub fn nav_availability(&self) -> NavAvailability {
        self.model
            .playback
            .as_ref()
            .map(|p| NavAvailability {
                can_prev: p.session.can_prev,
                can_next: p.session.can_next,
            })
            .unwrap_or_default()
    }

    /// Prev/next button-LED roles whose control was just pressed while available. The loop
    /// runner turns these into LED flashes (skipped for roles the board lacks).
    pub fn take_led_flashes(&mut self) -> Vec<plippa_board::LedRole> {
        std::mem::take(&mut self.led_flashes)
    }

    /// Play-button LED colour from the mirrored engine transport (source of truth).
    pub fn play_button_led(&self) -> crate::led::PlayButtonLed {
        use crate::led::PlayButtonLed;
        let transport = self
            .model
            .playback
            .as_ref()
            .map(|p| p.transport)
            .unwrap_or(TransportState::Stopped);
        let has_item = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item.is_some());
        PlayButtonLed::from_transport(transport, has_item)
    }

    /// Handle Postcard IPC from engine / server.
    pub fn handle_ipc(&mut self, msg: IpcMessage) {
        match msg {
            IpcMessage::PlaybackStatus(status) => {
                // Drop stale snapshots (older than last applied). Equal revision
                // is allowed so periodic hub refreshes still update volume/UI.
                if status.revision < self.last_status_revision {
                    return;
                }
                self.last_status_revision = status.revision;
                let prev_item = self
                    .model
                    .playback
                    .as_ref()
                    .and_then(|p| p.session.current_item);
                let (new_item, target, mode) = (
                    status.session.current_item,
                    status.session.target_rate_permille,
                    status.session.speed_warp_mode,
                );
                let (rev, engine_seek, engine_outside) = (
                    status.revision,
                    status.session.seek_warp_mode,
                    status.session.seek_outside_track,
                );
                self.model.apply_playback(status);
                // WAVE_02: follow the item's sidecar; a new path clears the old peaks before
                // the read so one track's shape is never drawn for another.
                self.sync_wave_path();
                if new_item != prev_item {
                    // A different item: an open gesture or committed target is stale, and so is
                    // the dead-reckoning baseline (same handle_ipc re-samples below).
                    self.discard_jog();
                    self.jog_display.reset_baseline();
                }
                self.sync_jog_sample();
                // The engine confirmed (or 600 ms passed): status wins over the optimistic speed.
                let _ = self.desired_rate.effective(target, self.now_ms);
                self.sync_rate_override();
                // Tempo is session-wide: tell the owner why a new item is not at 100%.
                if new_item.is_some()
                    && new_item != prev_item
                    && target != plippa_core::RATE_UNITY_PERMILLE
                {
                    let text = crate::screens::rate_text(target, mode);
                    self.compositor.show_status(&text, self.now_ms);
                }
                self.reconcile_warp_modes(rev, mode, engine_seek, engine_outside);
                self.apply_np_follow();
                self.resolve_jump();
            }
            IpcMessage::UserNotice(notice) => {
                self.on_user_notice(notice);
            }
            IpcMessage::NowPlayingListChanged(mut list) => {
                // SEC-12: entry text comes from tags / cue lists.
                for e in &mut list.entries {
                    e.title = screen_text(&e.title);
                    e.artist = screen_text(&e.artist);
                }
                self.model.replace_now_playing(list);
                // UI_13: an auto-opened, unscrolled overlay follows the new playing row
                // (Next / Previous), even when the engine replaced the collection. Once the
                // user scrolls, their cursor wins.
                if self.list_overlay_until_ms.is_some() && !self.list_overlay_scrolled {
                    if self.model.list_panel_visible() {
                        self.model.np_focus.mode = NpMode::List;
                        self.model.np_focus.target = NpTarget::List;
                        self.model.np_focus.cursor = self.model.now_playing_row().unwrap_or(0);
                    }
                }
                self.apply_np_follow();
                self.resolve_jump();
            }
            IpcMessage::AudioRouteChanged(mut route) => {
                // AUDIO_04 §4.11 / SEC-12: a sink's `device.description` is untrusted, and the
                // model is what every Audio screen draws.
                for output in &mut route.outputs {
                    output.description = screen_text(&output.description);
                }
                self.audio_state.apply_route(route);
                // The first route after launch seeds the persisted routing + delay once.
                self.reconcile_audio_route();
                self.sync_audio_view();
                if self.accessories.mark_first_route() {
                    self.maybe_auto_connect(self.now_ms);
                }
                self.retry_pending_output(self.now_ms);
                self.sync_accessories_view();
                self.needs_redraw = true;
            }
            IpcMessage::UnmatchedNoubin(notice) => {
                // BCS_07: when the tag declared platform links, the UI (not the engine) asks
                // the local server what they mean. No link: today's portal popup.
                //
                // The key handed on must be the one the tag carried, scheme included: the
                // server fetches `…/web.noudata` from it when the download is linked, and a
                // normalised key (`host/path`, no scheme) is not a URL it can fetch.
                let key = [notice.noubin_key.trim(), notice.normalised_key.trim()]
                    .into_iter()
                    .find(|key| !key.is_empty())
                    .map(str::to_string);
                if self.open_store_tap(&notice.candidate_urls, key, "") {
                    // The engine sends the miss notice just before this event, and the tap
                    // itself has already shown a "Playing …" popup; both would be drawn over
                    // the store screen. That screen *is* the answer (it names the host we
                    // would contact), so drop the popups rather than cover it.
                    self.compositor.clear_status();
                } else {
                    // NB_09: the full unite flow lives in the web portal. The key is untrusted
                    // tag text and is not drawn here; the portal shows it (as text).
                    self.compositor.show_status(
                        "Not in your library\nContinue on the web portal or phone",
                        self.now_ms,
                    );
                }
            }
            _ => {}
        }
    }

    fn on_user_notice(&mut self, notice: UserNotice) {
        // Status popup is the product surface for miss / NFC feedback (UI_05) and, from
        // PLAY_04, for "there is no fixed order to restore" (owner decision 6). Exactly two
        // `Display` lines each; the second is engine-authored and truncated to one line.
        let popup = match notice.code.as_str() {
            NOTICE_LIBRARY_MISS => {
                Some(format!("Tag not recognised\n{}", notice.message))
            }
            // The file is there but this board cannot decode it: not a missing file.
            NOTICE_MEDIA_UNPLAYABLE => Some(format!(
                "Can't play this file\n{}",
                screen_text(&notice.message)
            )),
            NOTICE_NO_FIXED_ORDER => Some(format!(
                "No fixed order\n{}",
                truncate_popup_line(&screen_text(&notice.message), 36)
            )),
            _ => None,
        };
        if let Some(text) = popup {
            self.compositor.show_status(&text, self.now_ms);
        }
        self.model.push_notice(notice);
    }

    /// NFC session event after poll (full-read-before-feedback already enforced by hw).
    pub fn handle_nfc_event(&mut self, event: NfcEvent) {
        match event {
            NfcEvent::ReadComplete { ndef, info, .. } => {
                self.tap_id = self.tap_id.saturating_add(1);
                let tap_id = self.tap_id;
                log_ndef_tap(tap_id, &info.uid, &ndef);
                self.show_nfc_tap_status(&ndef);
                self.outgoing.push(UiOutgoing::Ipc(IpcMessage::TapNdef(TapNdefPayload {
                    ndef_message: ndef,
                    tap_id,
                })));
                self.outgoing.push(UiOutgoing::TapFeedback { tap_id });
            }
            // v0.5.2: no NDEF Text/URI record, so the tag UID is the key (`uid:<hex>`), tapped
            // exactly like an NDEF key (the engine matches it against the library).
            NfcEvent::UidTag { info, key } => match key {
                Ok(key) => {
                    self.tap_id = self.tap_id.saturating_add(1);
                    let tap_id = self.tap_id;
                    eprintln!("plippa-ui: tap #{tap_id} uid={:02x?} → {key}", info.uid);
                    self.compositor.show_status(
                        &format!("Playing …\n{}", truncate_popup_line(&key, 36)),
                        self.now_ms,
                    );
                    self.outgoing.push(UiOutgoing::Ipc(IpcMessage::TapKey(TapKeyPayload {
                        noubin_key: key,
                        tap_id,
                        already_normalised: false,
                    })));
                    self.outgoing.push(UiOutgoing::TapFeedback { tap_id });
                }
                Err(e) => {
                    let why = match e {
                        plippa_noubin::UidKeyError::RandomUid
                        | plippa_noubin::UidKeyError::Unstable => "It shows a random ID",
                        plippa_noubin::UidKeyError::BadLength => "No readable ID",
                    };
                    self.compositor
                        .show_status(&format!("Tag not usable\n{why}"), self.now_ms);
                }
            },
            NfcEvent::TagRemoved { .. } => {}
        }
    }

    /// Status popup for NFC tap (not a title-bar banner). The popup caps at two `Display`
    /// lines / 60 px, so both callers below pass exactly two lines.
    fn show_nfc_tap_status(&mut self, ndef: &[u8]) {
        match extract_noubin_key(ndef) {
            Ok(key) => {
                let title = truncate_popup_line(&key, 36);
                self.compositor
                    .show_status(&format!("Playing …\n{title}"), self.now_ms);
            }
            Err(_) => {
                let snippet = ndef_payload_snippet(ndef);
                self.compositor
                    .show_status(&format!("Tag not recognised\n{snippet}"), self.now_ms);
            }
        }
    }

    /// Hold Menu Enter while in keyboard → delete character slots.
    fn on_menu_enter_hold(&mut self, wifi: &mut dyn WifiBackend) {
        if let Some(form) = self.model.wifi.as_mut() {
            if form.in_keyboard() {
                form.handle(WifiFormEvent::HoldDelete, wifi);
            }
        }
        if self
            .model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.searching)
        {
            if let Some(kb) = self.model.store_search.as_mut() {
                kb.handle(KeyboardEvent::HoldDelete);
            }
            self.needs_redraw = true;
        }
    }

    fn note_menu_activity(&mut self) {
        self.compositor.note_activity(self.now_ms);
    }

    /// Hold Assignable A while editing a control → bind + status popup + persist.
    fn on_assignable_hold(&mut self) {
        if self.controls.mode() != InteractionMode::Edit {
            return;
        }
        let Some(item) = self.controls.focused() else {
            return;
        };
        let id = AssignableId::new(item.id);
        self.assign.set_encoder_a(Some(id.clone()));
        if let Err(e) = self.assign.save() {
            eprintln!("plippa-ui: assign save failed: {e}");
            self.compositor
                .show_status(&format!("Assign failed ({id})"), self.now_ms);
            return;
        }
        self.compositor
            .show_status(&format!("Assigned {id}"), self.now_ms);
    }

    /// Rotate Assignable A → nudge the bound control (unbound = no-op).
    fn apply_assignable_a_rotate(&mut self, delta: i8) {
        let Some(id) = self.assign.encoder_a().cloned() else {
            eprintln!("plippa-ui: Assignable A rotate (unbound) — ignored");
            return;
        };
        match id.as_str() {
            AssignableId::VOLUME => {
                self.apply_volume_delta(delta);
            }
            other => {
                if self.controls.nudge_id(other, delta) {
                    if let Some(v) = self.controls.value_of(other) {
                        self.compositor
                            .show_status(&format!("{other} {v}"), self.now_ms);
                    }
                } else {
                    eprintln!(
                        "plippa-ui: Assignable A bound to unknown `{other}` — ignored"
                    );
                }
            }
        }
    }

    fn apply_volume_delta(&mut self, delta: i8) {
        let cur = self.model.volume.percent() as i16;
        let step = self.cfg.volume_step as i16;
        let next = (cur + delta as i16 * step).clamp(0, 100) as u8;
        let vol = Volume::new(next);
        self.model.volume = vol;
        self.compositor
            .show_status(&format!("volume {}%", vol.percent()), self.now_ms);
        self.outgoing.push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(
            PlaybackCommand::SetVolume(vol),
        )));
    }

    pub fn handle_input(&mut self, ev: InputEvent, wifi: &mut dyn WifiBackend) {
        // PWRSW_01: after the 4 s acknowledgement only the clock still runs; knob, buttons
        // and NFC must not reach the app while the host is going down.
        if self.shutdown_at.is_some() && !matches!(ev, InputEvent::Tick { .. }) {
            return;
        }
        match ev {
            InputEvent::Tick { now_ms } => {
                self.now_ms = now_ms;
                if !self.shutdown_sent {
                    if let Some(ev) = self.power.on_tick(now_ms) {
                        self.on_power_hold(ev);
                    }
                    if self.shutdown_at.is_some_and(|at| now_ms >= at) {
                        self.shutdown_sent = true;
                        self.outgoing.push(UiOutgoing::ShutdownRequest {
                            power_off: self.cfg.poweroff_armed,
                        });
                    }
                }
                if let Some(h) = self.menu_enter_hold.on_tick(now_ms) {
                    self.hold_events.push(h);
                    self.on_menu_enter_hold(wifi);
                }
                if let Some(h) = self.assignable_hold.on_tick(now_ms) {
                    self.hold_events.push(h);
                    self.on_assignable_hold();
                }
                if let Some(form) = self.model.wifi.as_mut() {
                    if form.phase == crate::wifi::WifiPhase::Connecting {
                        form.handle(WifiFormEvent::TickConnect, wifi);
                        self.sync_network_from_wifi(wifi);
                        self.sync_keyboard_a_override();
                    }
                }
                if self.compositor.on_tick(now_ms) {
                    self.needs_redraw = true;
                }
                // UI_13: the auto-opened playlist overlay closes itself after four seconds of
                // no rotation / Next / Previous. A navigation clears it so a stale deadline
                // cannot later close a deliberately opened menu / route.
                if *self.nav.current() != Route::Main {
                    self.clear_list_overlay();
                } else if self.list_overlay_until_ms.is_some_and(|d| now_ms >= d) {
                    if self.model.np_focus.mode == NpMode::List {
                        self.model.np_focus.back();
                    }
                    self.clear_list_overlay();
                    self.needs_redraw = true;
                }
                let action = self.play_key.on_tick(now_ms);
                self.on_play_action(action);
                if let Some(HoldTapEvent::HoldStart) = self.playmode_key.on_tick(now_ms) {
                    self.open_play_mode();
                }
                // WARP_18: FF/RW held past the threshold = scan. The UI_14 hold+Play lock is
                // disabled (UI_15: Play toggles again), so `ffrw_reset_lock` is never set and
                // these guards are inert; they stay so the gesture can be restored as one piece.
                if let Some(HoldTapEvent::HoldStart) = self.rewind_key.on_tick(now_ms) {
                    if !self.ffrw_reset_lock {
                        self.ffrw_hold_start(-1);
                    }
                }
                if let Some(HoldTapEvent::HoldStart) = self.ff_key.on_tick(now_ms) {
                    if !self.ffrw_reset_lock {
                        self.ffrw_hold_start(1);
                    }
                }
                if !self.engine_connected {
                    // Offline: nothing confirms the speed, keep showing what was set.
                    self.desired_rate.touch(now_ms);
                }
                self.sync_rate_override();
                // Jog: one `SeekTo` once the knob has been idle for `JOG_COMMIT_IDLE_MS`.
                if let Some(c) = self.jog.tick(now_ms) {
                    self.commit_jog(c);
                }
                self.sync_jog_view();
                self.tick_art(now_ms);
                // BCS_07: store answers, the lookup timeout and download polling.
                self.tick_store(now_ms);
                // WAVE_02: sidecar retry (bounded) and the ~30 FPS waveform repaint.
                self.tick_wave(now_ms);
                self.tick_waveform(now_ms);
            }
            InputEvent::MenuEnterButton { down } => {
                self.menu_enter_hold.on_button(down, self.now_ms);
                if down {
                    self.note_menu_activity();
                }
            }
            InputEvent::AssignableEncoderPush { down } => {
                self.assignable_hold.on_button(down, self.now_ms);
                if down {
                    self.note_menu_activity();
                }
            }
            InputEvent::AssignableARotate(delta) => {
                self.note_menu_activity();
                if self
                    .model
                    .wifi
                    .as_ref()
                    .is_some_and(|f| f.in_keyboard())
                {
                    if let Some(form) = self.model.wifi.as_mut() {
                        form.handle(WifiFormEvent::RegionJump(delta), wifi);
                    }
                } else {
                    self.apply_assignable_a_rotate(delta);
                }
            }
            InputEvent::PowerButton { down } => {
                if let Some(ev) = self.power.on_button(down, self.now_ms) {
                    self.on_power_hold(ev);
                }
            }
            InputEvent::VolumeRotate(delta) => {
                self.note_menu_activity();
                self.apply_volume_delta(delta);
            }
            InputEvent::Back => {
                self.note_menu_activity();
                self.flush_jog();
                // ACC_03: Back cancels a prompt/scan or leaves the detail/list screens.
                if self.accessory_back() {
                    return;
                }
                // AUDIO_04 §4.8: Back while a Delay row is being edited commits the one new
                // value (one command + one save) and stays on the screen.
                if self.audio_state.edit.is_some() {
                    self.audio_commit_delay();
                    return;
                }
                // The Default volume row uses the same enter-then-scroll idiom.
                if self.audio_state.volume_edit {
                    self.audio_commit_default_volume();
                    return;
                }
                // UI_16 §1: Back while a Playback settings row is being edited leaves only
                // the edit state (the route stays); a second Back then leaves the screen.
                if self.model.play_mode.editing.is_some() {
                    self.play_mode_leave_edit();
                    return;
                }
                // BCS_08: Back leaves the search keyboard before it leaves the collection.
                if *self.nav.current() == Route::StoreReleases
                    && self
                        .model
                        .store_releases
                        .as_ref()
                        .is_some_and(|view| view.searching)
                {
                    self.cancel_search();
                    return;
                }
                if self.controls.back() {
                    return;
                }
                if self.compositor.main_menu_visible() {
                    self.compositor.hide_main_menu();
                    return;
                }
                // NB_15 / WARP_07: a ring parameter (jog, tempo) or list mode returns to the
                // item-select level (panel) / the implicit level (single track), one level
                // per press, never further out.
                if *self.nav.current() == Route::Main && self.model.np_focus.back() {
                    // UI_13: Back dismisses the overlay (list or ring) and never opens the menu.
                    self.clear_list_overlay();
                    return;
                }
                // UI_14: Back in a PIN box discards the pending digit and stays; Back from
                // navigate falls through and leaves the screen.
                if *self.nav.current() == Route::SystemNetworkPin {
                    if let Some(pad) = self.model.network.as_mut().and_then(|v| v.pin_pad.as_mut()) {
                        if pad.handle(PinEvent::Back) != PinAction::LeaveCancel {
                            self.needs_redraw = true;
                            return;
                        }
                    }
                }
                if *self.nav.current() == Route::Wifi {
                    if let Some(form) = self.model.wifi.as_mut() {
                        if form.phase != crate::wifi::WifiPhase::Current {
                            form.handle(WifiFormEvent::Back, wifi);
                            self.sync_network_from_wifi(wifi);
                            self.sync_keyboard_a_override();
                            return;
                        }
                        // Current: fall through to pop the Wifi route.
                    }
                }
                match self.nav.back() {
                    NavAction::OpenMainMenu => {
                        self.compositor.show_main_menu(self.now_ms);
                    }
                    NavAction::Popped(Route::LibraryAssociate) => {
                        // Cancel in-progress local associate wait.
                        self.outgoing.push(UiOutgoing::CancelAssociate);
                        self.model.banner = None;
                        self.compositor
                            .show_status("Associate cancelled", self.now_ms);
                    }
                    NavAction::Popped(Route::SystemPlayMode) => {
                        // Straight back to the origin screen (also when that is Now Playing):
                        // no main-menu overlay.
                    }
                    NavAction::Popped(Route::LibraryDetail) => {
                        self.model.detail_playable_id = None;
                        self.model.detail_track_pos = None;
                        self.model.detail_info = None;
                        self.model.detail_covers = None;
                    }
                    NavAction::Popped(Route::LibraryTrackActions) => {
                        self.model.track_menu_pos = None;
                    }
                    NavAction::Popped(Route::LibraryTrackAnalysis) => {
                        // Leaving stops the polling: no view, nothing due.
                        self.model.track_analysis = None;
                    }
                    NavAction::Popped(Route::LibraryStoreTap) => {
                        // Back leaves the screen and cancels a running download.
                        self.close_store_tap();
                    }
                    NavAction::Popped(Route::SystemStores | Route::LibraryStores) => {
                        self.model.store_stores = None;
                    }
                    NavAction::Popped(Route::StoreConnect) => {
                        self.model.store_connect = None;
                    }
                    NavAction::Popped(Route::SystemNetwork) => {
                        self.model.network = None;
                    }
                    NavAction::Popped(Route::SystemNetworkPin) => {
                        // Also covers switching the portal's PIN requirement off: the pad goes
                        // with the screen, so a half-typed PIN cannot reappear later.
                        if let Some(view) = self.model.network.as_mut() {
                            view.pin_pad = None;
                            view.pin_pending = false;
                            view.notice = None;
                        }
                    }
                    NavAction::Popped(Route::StoreReleases) => {
                        self.model.store_releases = None;
                    }
                    NavAction::Popped(Route::StoreRelease) => {
                        // Back cancels a download still running, exactly like the tap screen.
                        self.close_store_release();
                    }
                    NavAction::Popped(Route::LibraryRemove) => {
                        let succeeded = self
                            .model
                            .store_delete
                            .as_ref()
                            .is_some_and(|view| view.succeeded());
                        self.model.store_delete = None;
                        if succeeded && *self.nav.current() == Route::LibraryDetail {
                            // The item is gone; the detail screen under the confirm is stale.
                            self.model.detail_playable_id = None;
                            self.model.detail_track_pos = None;
                            self.model.detail_info = None;
                            self.model.detail_covers = None;
                            let _ = self.nav.back();
                        }
                    }
                    NavAction::Popped(Route::LibraryCategory) => {
                        self.model.browse_kind = None;
                        self.model.library.clear();
                        // Counts may have changed while browsing (index update).
                        self.request_browse(BrowseRequest::Counts);
                    }
                    NavAction::Popped(_) if *self.nav.current() == Route::Main => {
                        // Library/System (or other top-level) → Now Playing: show
                        // overlay so Now Playing / Library / System are reachable.
                        self.compositor.show_main_menu(self.now_ms);
                    }
                    _ => {}
                }
                // Drop sticky associate prompts when returning to Now Playing.
                if *self.nav.current() == Route::Main {
                    if self.model.banner.as_ref().is_some_and(|s| {
                        s.starts_with("Associated ") || s.starts_with("tap a noubin")
                    }) {
                        self.model.banner = None;
                    }
                }
            }
            InputEvent::PlayStop => {
                // Mock tests / keyboards: always "toggle" (the hardware path uses
                // `ControlButton{Play}` + `PlayKey`).
                self.toggle_play();
            }
            InputEvent::ControlPressed(role @ (ControlRole::PrevTrack | ControlRole::NextTrack)) => {
                // NB_14: an unavailable press does nothing (no command, no flash). The
                // engine owns the semantics (Previous = restart after 1 s; at the entry
                // start, or a second press within the window, = back).
                let avail = self.nav_availability();
                let (usable, cmd, led) = if role == ControlRole::PrevTrack {
                    (avail.can_prev, PlaybackCommand::Previous, LedRole::PrevTrackButton)
                } else {
                    (avail.can_next, PlaybackCommand::Next, LedRole::NextTrackButton)
                };
                if usable {
                    self.note_menu_activity();
                    // Navigating away: the open gesture belongs to the old track.
                    self.discard_jog();
                    self.outgoing
                        .push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(cmd)));
                    // UI_13 / UI_CONTROL_01: the overlay follows the engine's new playing row.
                    self.np_after_transport();
                    self.led_flashes.push(led);
                }
            }
            InputEvent::ControlButton {
                role: ControlRole::Play,
                down,
                at_ms,
            } => {
                // UI_15: Play is an ordinary pause/play toggle again, including while FF/RW is
                // held for a scan (the UI_14 hold+Play speed snap is disabled, see
                // `ffrw_play_reset`). Elsewhere while warped the press is *deferred* by
                // `PlayKey` so the LED and the transport cannot disagree about which command a
                // press meant; a held scan uses `play_defers()`, which turns the deferral (and
                // with it the WARP_21 long-press 100 % reset) off.
                let deferred = self.play_defers();
                let action = self.play_key.on_edge(down, at_ms, deferred);
                self.on_play_action(action);
            }
            InputEvent::ControlButton {
                role: ControlRole::PlayMode,
                down,
                at_ms,
            } => {
                // PLAY_04: hold opens the menu once; a short press cycles the play mode.
                for e in self.playmode_key.on_edge(down, at_ms) {
                    match e {
                        HoldTapEvent::HoldStart => self.open_play_mode(),
                        HoldTapEvent::Tap => self.cycle_play_mode(),
                        HoldTapEvent::HoldEnd => {}
                    }
                }
            }
            InputEvent::ControlButton {
                role: role @ (ControlRole::Rewind | ControlRole::FastForward),
                down,
                at_ms,
            } => {
                // WARP_12: remember the edges for the backlight (the tap/hold logic is
                // WARP_13/18). A press with nothing loaded does not brighten the LED: the
                // level function turns an unavailable button off whatever its edges say.
                let s = if role == ControlRole::Rewind {
                    &mut self.rewind_edges
                } else {
                    &mut self.ff_edges
                };
                if down {
                    s.pressed_at = Some(at_ms);
                } else {
                    s.released_at = Some(at_ms);
                }
                // WARP_13: a tap (release before the hold threshold) steps the speed ladder
                // on button-up. A hold does nothing until WARP_18.
                let (dir, key) = if role == ControlRole::Rewind {
                    (-1i8, &mut self.rewind_key)
                } else {
                    (1i8, &mut self.ff_key)
                };
                for e in key.on_edge(down, at_ms) {
                    // UI_15: the UI_14 lock is disabled (Play no longer resets the speed), so
                    // this is inert; kept with the gesture so restoring it is one change.
                    if self.ffrw_reset_lock {
                        continue;
                    }
                    match e {
                        HoldTapEvent::Tap => self.ffrw_tap(dir),
                        HoldTapEvent::HoldStart => self.ffrw_hold_start(dir),
                        HoldTapEvent::HoldEnd => self.ffrw_hold_end(dir),
                    }
                }
                // Both transport buttons up again: the lock lifts.
                if !self.rewind_key.is_down() && !self.ff_key.is_down() {
                    self.ffrw_reset_lock = false;
                }
            }
            InputEvent::ControlButton { .. } => {}
            InputEvent::ControlPressed(role) => {
                // No behaviour bound yet (rewind/FF/record/play-mode); role-keyed, not pin-keyed.
                eprintln!("plippa-ui: control `{}` pressed (no handler yet)", role.name());
            }
            InputEvent::MenuRotate(delta) => {
                self.note_menu_activity();
                self.on_menu_rotate(delta, wifi);
            }
            InputEvent::MenuEnter => {
                self.note_menu_activity();
                self.on_menu_enter(wifi);
            }
        }
    }

    fn on_menu_rotate(&mut self, delta: i8, wifi: &mut dyn WifiBackend) {
        if self.compositor.main_menu_visible() {
            let len = self.nav.menu_items().len();
            // The strip is horizontal (Now Playing | Library | System), so it follows the raw
            // detent direction: left = previous, right = next. `NavStack::rotate` applies the
            // vertical-list inversion (UI_16 §7); negating first cancels it for this strip only.
            let _ = self.nav.rotate(delta.saturating_neg(), len);
            return;
        }
        if self.controls.mode() == InteractionMode::Edit {
            self.controls.rotate(delta);
            return;
        }
        // UI_15: while FF/RW is held the encoder is the **scan speed** — a scan-only value
        // that re-targets the running scan and never touches the set speed. Checked before
        // everything else so a scan is steerable from any route without opening a parameter.
        if self.scan_owner.is_some() {
            self.scan_speed_rotate(delta);
            return;
        }
        match self.nav.current() {
            Route::Main => {
                if !self.model.list_panel_visible() {
                    // Single track without cues: Now Playing is itself the control; the
                    // implicit level is jog until Enter walks the ring.
                    match self.model.np_focus.param() {
                        Some(p) => self.adjust_rotate(p, delta),
                        None => self.jog_now_playing(delta),
                    }
                } else {
                    match self.model.np_focus.mode {
                        // UI_13: rotation opens the overlay on the playing row (either
                        // direction); a further turn scrolls it and re-arms the timeout.
                        NpMode::Select => {
                            let len = self.model.np_len();
                            let playing = self.model.now_playing_row().unwrap_or(0);
                            if self.model.np_focus.rotate_opens_list(delta, len, playing) {
                                self.arm_list_overlay(true);
                            }
                        }
                        NpMode::Adjust(p) => self.adjust_rotate(p, delta),
                        NpMode::List => {
                            let len = self.model.np_len();
                            self.model.np_focus.move_cursor(delta, len);
                            self.arm_list_overlay(true);
                        }
                    }
                }
            }
            Route::Menu => {
                let len = self.nav.menu_items().len();
                let _ = self.nav.rotate(delta, len);
            }
            Route::Library => {
                let len = if self.model.storage_present {
                    // BCS_08: the Stores row sits after the categories.
                    self.model.library_root_kinds().len() + 1
                } else {
                    0
                };
                let _ = self.nav.rotate(delta, len);
            }
            Route::LibraryCategory => {
                let len = self.model.library.len();
                let _ = self.nav.rotate(delta, len);
            }
            Route::LibraryDetail => {
                let len = self.model.detail_actions().len();
                let _ = self.nav.rotate(delta, len);
            }
            Route::LibraryTracks => {
                let len = self
                    .model
                    .current_detail_info()
                    .map_or(0, |d| d.tracks.len());
                let _ = self.nav.rotate(delta, len);
            }
            Route::LibraryTrackActions => {
                let _ = self.nav.rotate(delta, TrackAction::ALL.len());
            }
            Route::LibraryTrackAnalysis => {
                let len = self
                    .model
                    .track_analysis
                    .as_ref()
                    .map_or(0, |v| v.actions().len());
                let _ = self.nav.rotate(delta, len);
            }
            Route::LibraryChapters => {
                let len = self
                    .model
                    .current_detail_info()
                    .map_or(0, |d| d.chapters.len());
                let _ = self.nav.rotate(delta, len);
            }
            Route::LibraryCover => {
                // Previous / next image, clamped; right (positive) is next.
                let count = self
                    .model
                    .current_detail_covers()
                    .map_or(0, |c| c.covers.len());
                let next = (self.cover_index as i64 + i64::from(delta.signum()))
                    .clamp(0, count.saturating_sub(1) as i64);
                self.cover_index = next as usize;
            }
            Route::LibraryAssociate => {}
            Route::LibraryStoreTap => self.store_rotate(delta),
            Route::SystemStores | Route::LibraryStores => {
                let actions = self
                    .model
                    .store_stores
                    .as_ref()
                    .map(|view| (view.actions(), view.clone()));
                if let Some((actions, view)) = actions {
                    // A row that cannot be chosen (Sync while it runs) is stepped over.
                    for _ in 0..actions.len() {
                        let _ = self.nav.rotate(delta, actions.len());
                        let landed = actions.get(self.nav.focus).copied();
                        if landed.is_none_or(|action| view.enabled(action)) {
                            break;
                        }
                    }
                }
            }
            Route::StoreConnect => {
                let len = self
                    .model
                    .store_connect
                    .as_ref()
                    .map_or(0, |view| view.actions().len());
                let _ = self.nav.rotate(delta, len);
            }
            Route::StoreReleases => {
                // The query keyboard takes the encoder while it is open.
                if self
                    .model
                    .store_releases
                    .as_ref()
                    .is_some_and(|view| view.searching)
                {
                    if let Some(kb) = self.model.store_search.as_mut() {
                        kb.handle(KeyboardEvent::Rotate(delta));
                    }
                    self.needs_redraw = true;
                } else {
                    let len = self
                        .model
                        .store_releases
                        .as_ref()
                        .map_or(0, |view| view.rows().len());
                    let _ = self.nav.rotate(delta, len);
                }
            }
            Route::StoreRelease => {
                let len = self
                    .model
                    .store_release
                    .as_ref()
                    .map_or(0, |view| view.actions().len());
                let _ = self.nav.rotate(delta, len);
            }
            Route::LibraryRemove => {
                let len = self
                    .model
                    .store_delete
                    .as_ref()
                    .map_or(0, |view| view.actions().len());
                let _ = self.nav.rotate(delta, len);
            }
            Route::Wifi => {
                if let Some(form) = self.model.wifi.as_mut() {
                    // UI_16: every Wi-Fi list is read top-down like the other menu lists, so it
                    // follows the same inversion (left = down). UI_14 added the root menu, the
                    // known list, its action menu and the confirm to that set. The password
                    // keyboard keeps the raw detent: it is a character grid, not a list.
                    let d = if form.phase.is_list() {
                        crate::nav::menu_step(delta)
                    } else {
                        delta
                    };
                    form.handle(WifiFormEvent::Rotate(d), wifi);
                }
            }
            Route::System => {
                let len = self.nav.system_items().len();
                let _ = self.nav.rotate(delta, len);
            }
            Route::SystemNetwork => {
                let len = self.network_rows().len();
                let _ = self.nav.rotate(delta, len);
            }
            Route::SystemNetworkPin => {
                // A box grid, not a list: the raw detent (right = next digit).
                if let Some(pad) = self.model.network.as_mut().and_then(|v| v.pin_pad.as_mut()) {
                    pad.handle(PinEvent::Rotate(delta));
                    self.needs_redraw = true;
                }
            }
            Route::SystemIp => {
                let len = self.model.system.ips.len().max(1);
                let _ = self.nav.rotate(delta, len);
            }
            Route::SystemPlayMode => {
                if self.model.play_mode.editing.is_some() {
                    // UI_16 §1/§6: a row in the edit state steps its bounded value ladder
                    // (clamped at the ends) instead of moving the cursor.
                    self.play_mode_edit_rotate(delta);
                } else {
                    // UI_16 §6: the row list wraps, like the library menus. The merged list
                    // holds the four session modes and the settings.
                    let len = PLAYBACK_SETTINGS_ROWS.len();
                    let _ = self.nav.rotate(delta, len);
                }
            }
            Route::SystemAudioAccessories => {
                if matches!(self.accessories.stage, PairStage::Prompt(..)) {
                    self.accessory_prompt_rotate(delta);
                } else {
                    let len = self.accessories.sorted_known().len() + 1;
                    let _ = self.nav.rotate(delta, len);
                }
            }
            Route::SystemAudioAccessory => {
                if self.accessories.forget_confirm.is_some() {
                    // Moving the selection cancels a pending confirmation.
                    self.accessories.clear_forget_confirm();
                    self.compositor.clear_status();
                }
                let _ = self.nav.rotate(delta, 4);
            }
            Route::SystemAudioPairNew => {
                if matches!(self.accessories.stage, PairStage::Prompt(..)) {
                    self.accessory_prompt_rotate(delta);
                } else {
                    let len = self.accessories.found.len();
                    let _ = self.nav.rotate(delta, len);
                }
            }
            Route::SystemDebug => {
                // UI_16 §8: the settings list wraps like every other menu list.
                let len = DEBUG_ROWS.len();
                let _ = self.nav.rotate(delta, len);
            }
            Route::SystemDebugAudio => {
                // AUDIO_05: a read-only report scrolls (clamped, never wrapped).
                let len = diag::audio_report(
                    &self.audio_state.route,
                    &self.audio.settings,
                    &self.accessories,
                    self.accessories.available,
                    self.now_ms,
                )
                .len();
                self.nav.scroll(delta, len);
            }
            Route::SystemAudio => {
                // AUDIO_04: navigation / toggle rows wrap; the Default volume editor steps.
                if self.audio_state.volume_edit {
                    self.audio_default_volume_rotate(delta);
                } else {
                    let _ = self.nav.rotate(delta, AUDIO_ROWS.len());
                }
            }
            Route::SystemAudioOutputs => {
                // Local only plus one row per sink; an empty route keeps the cursor on row 0.
                let len = 1 + self.audio_state.route.outputs.len();
                let _ = self.nav.rotate(delta, len);
            }
            Route::SystemAudioDelay => {
                if self.audio_state.edit.is_some() {
                    // Enter-then-scroll: the bounded delay ladder, no wrap.
                    self.audio_delay_rotate(delta);
                } else {
                    // Auto plus one row per enabled output.
                    let len = 1 + self.audio_state.route.effective_enabled().len();
                    let _ = self.nav.rotate(delta, len);
                }
            }
            Route::Debug => {
                // Navigate-mode focus among dummy controls on Debug.
                self.controls.rotate(delta);
            }
            _ => {}
        }
    }

    /// Knob handler of the ring parameter `p` (one arm per [`AdjustParam`]; extension point).
    fn adjust_rotate(&mut self, p: AdjustParam, delta: i8) {
        match p {
            AdjustParam::Jog => self.jog_now_playing(delta),
            AdjustParam::Tempo => self.tempo_rotate(delta),
            AdjustParam::Zoom => self.zoom_rotate(delta),
        }
    }

    /// ZOOM knob (UI_13): one ladder step per detent, clamped at 1 s and 10 min, never
    /// wrapped. Session-only: the value is never persisted and never sent to the engine.
    fn zoom_rotate(&mut self, delta: i8) {
        let loaded = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item.is_some());
        if !loaded || delta == 0 {
            return;
        }
        let mut next = self.model.wave_window_ms;
        for _ in 0..delta.unsigned_abs() {
            next = zoom_step(next, delta.signum());
        }
        if next != self.model.wave_window_ms {
            self.model.wave_window_ms = next;
            self.needs_redraw = true;
        }
    }

    /// Tempo knob: edits the magnitude of the signed set speed, 1%..10% steps by turn speed,
    /// sticky at 100%. Each accepted click updates the optimistic speed at once and queues
    /// one coalesced `SetRate`. Nothing loaded: no effect.
    fn tempo_rotate(&mut self, delta: i8) {
        let loaded = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item.is_some());
        if !loaded || delta == 0 {
            return;
        }
        let dir = delta.signum();
        let count = delta.unsigned_abs() as u32;
        let v = self.tempo_vel.update(self.now_ms, dir, count);
        let step = tempo_step_permille(v);
        let status_target = self
            .model
            .playback
            .as_ref()
            .map_or(plippa_core::RATE_UNITY_PERMILLE, |p| p.session.target_rate_permille);
        let mut cur = self.desired_rate.effective(status_target, self.now_ms);
        let mut changed = false;
        for _ in 0..count {
            if let Some(next) =
                tempo_apply_click(&mut self.tempo_sticky, cur, dir, step, self.now_ms)
            {
                cur = next;
                changed = true;
            }
        }
        if changed {
            self.desired_rate.set(cur, self.now_ms);
            self.sync_rate_override();
            self.queue_cmd(PlaybackCommand::SetRate { permille: cur });
        }
    }

    /// FF/RW held past the threshold: start the cassette scan (the engine owns the speed
    /// profile and the sound). The most recently pressed button wins; an earlier one stays
    /// ignored until it is released and pressed again. Nothing loaded: popup, no command.
    fn ffrw_hold_start(&mut self, dir: i8) {
        self.note_menu_activity();
        let loaded = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item.is_some());
        if !loaded {
            self.compositor.show_status("Nothing loaded", self.now_ms);
            self.needs_redraw = true;
            return;
        }
        let (mine, other) = if dir > 0 {
            (&self.ff_key, &self.rewind_key)
        } else {
            (&self.rewind_key, &self.ff_key)
        };
        if other.is_down() && other.down_since() > mine.down_since() {
            // The other button went down later: it wins (its own hold start follows).
            return;
        }
        // An open jog gesture is dropped (the engine ignores `SeekTo` while scanning).
        let _ = self.jog.flush();
        self.sync_jog_view();
        // UI_18: the encoder's scan speed is **sticky for the loaded track** — it survives
        // every release and re-hold, and only a different item clears it back to the default.
        // A new `ScanStart` always begins at the default in the audio thread, so the sticky
        // value is re-sent right after it (same batch, so it applies before the first quantum).
        if self.scan_owner.is_none() {
            let item = self
                .model
                .playback
                .as_ref()
                .and_then(|p| p.session.current_item);
            if self.scan_speed_item != item {
                self.scan_speed_permille = plippa_core::SCAN_SPEED_DEFAULT_PERMILLE;
                self.scan_speed_item = item;
            }
        }
        self.scan_owner = Some(dir);
        self.queue_cmd(PlaybackCommand::ScanStart {
            dir: if dir > 0 { plippa_core::ScanDir::Forward } else { plippa_core::ScanDir::Backward },
        });
        if self.scan_speed_permille != plippa_core::SCAN_SPEED_DEFAULT_PERMILLE {
            self.queue_cmd(PlaybackCommand::ScanRate {
                permille: self.scan_speed_permille,
            });
        }
        self.needs_redraw = true;
    }

    /// UI_15: one encoder turn while FF/RW is held steps the **scan** speed through
    /// [`plippa_core::SCAN_SPEED_LADDER_PERMILLE`] (clamped at both ends, never wrapped) and
    /// re-targets the running scan. It deliberately does not touch `desired_rate`, the session's
    /// `target_rate_permille` or the persisted set speed: the release restores the set speed.
    fn scan_speed_rotate(&mut self, delta: i8) {
        if delta == 0 {
            return;
        }
        let next = plippa_core::scan_speed_step(self.scan_speed_permille, delta);
        if next == self.scan_speed_permille {
            return; // clamp: already at the end of the ladder
        }
        self.scan_speed_permille = next;
        self.queue_cmd(PlaybackCommand::ScanRate { permille: next });
        // UI_15 §5: show the selected scan speed. Unlike the set-speed popups this one is not
        // redundant on Now Playing — the footer keeps showing the *set* speed (that is what the
        // release will restore) and nothing else displays the scan speed while held.
        self.compositor
            .show_status(&crate::warp_ui::scan_speed_text(next), self.now_ms);
        self.needs_redraw = true;
    }

    /// The scan speed selected for the running hold, in permille of the usual scan speed.
    pub fn scan_speed_permille(&self) -> i32 {
        self.scan_speed_permille
    }

    /// Release of an FF/RW hold: the winner's release stops the scan (idempotent in the
    /// engine); the speed readout is never touched (`desired_rate` stays). UI_15: the engine
    /// restores the session's set speed immediately (no brake) — not the scan speed the encoder
    /// landed on — and from paused it stops audio exactly where it was.
    fn ffrw_hold_end(&mut self, dir: i8) {
        if self.scan_owner == Some(dir) {
            self.scan_owner = None;
            self.queue_cmd(PlaybackCommand::ScanStop);
            self.needs_redraw = true;
        }
    }

    /// FF (`dir > 0`) / RW (`dir < 0`) tap: step the shared signed speed ladder from the
    /// current (optimistic) set speed. The same value the tempo knob edits; no inter-tap
    /// window, never moves the playhead, never touches the warp modes or a running seek.
    fn ffrw_tap(&mut self, dir: i8) {
        self.note_menu_activity();
        let loaded = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item.is_some());
        if !loaded {
            self.compositor.show_status("Nothing loaded", self.now_ms);
            self.needs_redraw = true;
            return;
        }
        // A tap while the other button is held (a scan, WARP_18) is ignored.
        let other_down = if dir > 0 { self.rewind_key.is_down() } else { self.ff_key.is_down() };
        if other_down {
            return;
        }
        let status_target = self
            .model
            .playback
            .as_ref()
            .map_or(plippa_core::RATE_UNITY_PERMILLE, |p| p.session.target_rate_permille);
        let cur = self.desired_rate.effective(status_target, self.now_ms);
        match ladder_step_enabled(cur, dir) {
            Some(next) => {
                self.desired_rate.set(next, self.now_ms);
                self.sync_rate_override();
                self.queue_cmd(PlaybackCommand::SetRate { permille: next });
                self.show_speed_status(&tap_rate_text(next));
            }
            None => {
                let end = if dir > 0 { 2000 } else { -2000 };
                self.show_speed_status(&tap_end_text(end));
            }
        }
        self.needs_redraw = true;
    }

    /// Jog knob (WARP_15): each click moves the **live target** (UI only, drawn by WARP_16)
    /// by the accelerated step; nothing is sent while the knob turns. The one `SeekTo` goes out
    /// from [`Self::commit_jog`] when the turn pauses or the user leaves jog.
    fn jog_now_playing(&mut self, delta: i8) {
        let loaded = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item.is_some());
        // A held FF/RW is a scan; the engine ignores `SeekTo` then, so the jog is too.
        if !loaded || delta == 0 || self.ff_key.is_down() || self.rewind_key.is_down() {
            return;
        }
        let dur = self
            .model
            .playback
            .as_ref()
            .and_then(|p| p.session.duration_ms);
        // UI_16 §5: the finest click is derived from Fine time and read **live** on every
        // click, so a change in the menu applies immediately (no restart). Acceleration and
        // the one-click/one-detent gesture are unchanged.
        self.jog_accel
            .set_fine_step_ms(u64::from(self.play_mode.settings.resolve_jog_step()));
        let base = self.jog_display.gesture_base(self.now_ms);
        // WARP_19: with the setting on the live target may run into a neighbouring item.
        let nb = self
            .model
            .playback
            .as_ref()
            .filter(|p| p.session.seek_outside_track)
            .map(|p| NeighbourCtx {
                prev_ms: p.session.prev_duration_ms,
                next_ms: p.session.next_duration_ms,
            })
            .unwrap_or(NeighbourCtx::NONE);
        let dir = delta.signum();
        let count = delta.unsigned_abs() as u32;
        let step = self.jog_accel.click(self.now_ms, dir, count, dur);
        for _ in 0..count {
            let click = self.jog.click_with(
                self.now_ms,
                i64::from(dir) * step as i64,
                base,
                dur,
                nb,
            );
            self.jog_display
                .on_click(click.target_ms, click.pinned, dir > 0, self.now_ms);
        }
        self.sync_jog_view();
    }

    /// Send the gesture's single `SeekTo` (engine offline: discard, never queue).
    fn commit_jog(&mut self, c: Commit) {
        if !self.engine_connected {
            self.jog_display.discard();
            self.sync_jog_view();
            return;
        }
        self.jog_display.on_commit(c.target_ms, c.item_delta, self.now_ms);
        if c.item_delta == 0 {
            self.queue_cmd(PlaybackCommand::SeekTo {
                target_ms: c.target_ms,
            });
        } else {
            self.queue_cmd(PlaybackCommand::SeekToItem {
                item_delta: c.item_delta,
                target_ms: c.target_ms,
            });
        }
        self.sync_jog_view();
    }

    /// Commit an open gesture now (leaving jog: Back, Enter, navigation).
    fn flush_jog(&mut self) {
        if let Some(c) = self.jog.flush() {
            self.commit_jog(c);
        }
    }

    /// Throw an open gesture / committed target away (track change).
    fn discard_jog(&mut self) {
        let _ = self.jog.flush();
        self.jog_display.discard();
        self.sync_jog_view();
    }

    /// Mirror the engine status into the jog display model.
    fn sync_jog_sample(&mut self) {
        if let Some(p) = self.model.playback.as_ref() {
            let s = &p.session;
            let sample = StatusSample {
                pos_ms: s.position_ms,
                rate_permille: s.rate_permille,
                playing: p.transport == TransportState::Playing,
                // UI_15: a scan from paused plays audio while the transport stays Paused.
                scanning: s.scan != plippa_core::ScanState::None,
                seek_target_ms: s.seek_target_ms,
                duration_ms: s.duration_ms,
            };
            self.jog_display.on_status(sample, self.now_ms);
        }
        self.sync_jog_view();
    }

    /// Recompute the screen's jog view; a change repaints.
    fn sync_jog_view(&mut self) {
        self.jog_display.tick(self.jog.is_open(), self.now_ms);
        let view = self
            .jog_display
            .view_item(self.jog.live_target(), self.jog.live_item(), self.now_ms);
        if self.model.jog_view != view {
            self.model.jog_view = view;
            self.needs_redraw = true;
        }
        self.sync_playhead();
    }

    /// Update the single playhead used by both the waveform and the footer clock.
    fn sync_playhead(&mut self) {
        if self.model.playback.is_none() {
            self.model.playhead_ms = 0;
            return;
        }
        // AUDIO_05: the only place the visual offset is applied. This moves the picture and
        // the footer clock together; every seek / jog computation keeps using the un-offset
        // `playhead_ms_at`, so a visual compensation can never move a seek target.
        self.model.playhead_ms = self
            .playhead_ms_at(self.now_ms)
            .saturating_sub(self.vis_offset_ms());
    }

    /// The UI-only picture delay (AUDIO_05): the slowest effective output's added delay when
    /// the owner turned the row on, else 0. `AudioRoute::vis_delay_ms` is already 0 with one
    /// output (and the engine reports 0 in Auto mode), so no special-casing is needed here.
    pub fn vis_offset_ms(&self) -> u64 {
        if self.audio.settings.waveform_vis_delay {
            u64::from(self.audio_state.route.vis_delay_ms())
        } else {
            0
        }
    }

    /// The playhead the picture and the clock both use (WAVE_02).
    ///
    /// 1. an open jog gesture: the live target; a target in a neighbouring item (WARP_19)
    ///    clamps to this file's start / end, because the band only shows this file;
    /// 2. a **running** seek (one the engine has not landed yet): its target, so the centre
    ///    stays still until the seek lands (no snap back on release);
    /// 3. otherwise the dead-reckoned actual position — `JogDisplay::actual_ms` already
    ///    applies `rate_permille` (scan / tempo / warp), the forward-only floor and clamping.
    ///
    /// Note that "a seek is running" is the display model's committed state, **not** "a
    /// `JogView` is visible": after landing the view stays alive for `JOG_LANDED_HOLD_MS`
    /// (1.5 s) so the target marker and readout can fade out, and pinning the playhead to
    /// its target for that hold would freeze the picture and the clock while audio plays on,
    /// then jump forward when it expires. The landed hold is a readout-only state.
    pub fn playhead_ms_at(&self, now_ms: u64) -> u64 {
        if let Some(target) = self.jog.live_target() {
            return match self.jog.live_item() {
                d if d < 0 => 0,
                d if d > 0 => self.wave_duration_ms(),
                _ => target,
            };
        }
        if let Some((target, delta)) = self.jog_display.running_seek() {
            return match delta {
                d if d < 0 => 0,
                d if d > 0 => self.wave_duration_ms(),
                _ => target,
            };
        }
        self.jog_display.actual_ms(now_ms).unwrap_or(0)
    }

    /// Duration for waveform clamping: the decoded `.wfm` length when loaded, else the
    /// session's length (before the sidecar arrives).
    fn wave_duration_ms(&self) -> u64 {
        if self.model.wave.duration_ms > 0 {
            return self.model.wave.duration_ms;
        }
        self.model
            .playback
            .as_ref()
            .and_then(|p| p.session.duration_ms)
            .unwrap_or(0)
    }

    /// Follow `SessionState.waveform_path`. A changed path clears the peaks and the failure
    /// fingerprint *before* reading; an unchanged path is a no-op.
    fn sync_wave_path(&mut self) {
        let path = self
            .model
            .playback
            .as_ref()
            .and_then(|p| p.session.waveform_path.clone());
        if self.model.wave.path == path {
            return;
        }
        self.model.wave = WaveView {
            path,
            state: WaveState::Pending,
            ..WaveView::default()
        };
        self.wave_fail_stamp = None;
        if self.load_wave() {
            self.needs_redraw = true;
        }
    }

    /// Read + decode the current sidecar. Returns `true` when the visible state changed.
    ///
    /// The file is untrusted removable media: the length is checked before the read, and
    /// `plippa_core::waveform::decode` validates magic/version/bucket range/length/FNV.
    fn load_wave(&mut self) -> bool {
        let Some(path) = self.model.wave.path.clone() else {
            return false;
        };
        let before = self.model.wave.state;
        self.wave_probe_at = self.now_ms;
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Queued or still being written: stay Pending and retry at most once a second.
                self.model.wave.state = WaveState::Pending;
                self.model.wave.peaks.clear();
                return before != self.model.wave.state;
            }
            Err(_) => {
                self.wave_failed(None);
                return before != self.model.wave.state;
            }
        };
        let stamp = (meta.len(), file_mtime_ns(&meta));
        if meta.len() > MAX_WFM_BYTES {
            self.wave_failed(Some(stamp));
            return before != self.model.wave.state;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.model.wave.state = WaveState::Pending;
                self.model.wave.peaks.clear();
                return before != self.model.wave.state;
            }
            Err(_) => {
                self.wave_failed(Some(stamp));
                return before != self.model.wave.state;
            }
        };
        match plippa_core::waveform::decode(&bytes) {
            Ok(wave) if wave.is_complete() => {
                self.model.wave.state = WaveState::Ready;
                self.model.wave.bucket_ms = wave.bucket_ms;
                self.model.wave.duration_ms = wave.duration_ms;
                self.model.wave.peaks = wave.peaks;
                self.wave_fail_stamp = None;
            }
            // A partial header (no COMPLETE flag): the analyser has not finished.
            Ok(_) => {
                self.model.wave.state = WaveState::Pending;
                self.model.wave.peaks.clear();
            }
            Err(_) => self.wave_failed(Some(stamp)),
        }
        before != self.model.wave.state
    }

    fn wave_failed(&mut self, stamp: Option<(u64, i64)>) {
        self.model.wave.state = WaveState::Failed;
        self.model.wave.peaks.clear();
        self.model.wave.bucket_ms = 0;
        self.model.wave.duration_ms = 0;
        self.wave_fail_stamp = stamp;
    }

    /// Bounded sidecar retry: once a second while Pending; on a changed file only when Failed.
    fn tick_wave(&mut self, now_ms: u64) {
        if self.model.playback.is_none() {
            return;
        }
        match self.model.wave.state {
            WaveState::Pending => {
                if self.model.wave.path.is_some()
                    && now_ms.saturating_sub(self.wave_probe_at) >= WAVE_RETRY_MS
                    && self.load_wave()
                {
                    self.needs_redraw = true;
                }
            }
            WaveState::Failed => {
                // No retry until the file changes — and a `stat` on the stick at most once a
                // second: ticks arrive at 10–30 Hz and a failed sidecar must stay cheap.
                if now_ms.saturating_sub(self.wave_probe_at) < WAVE_RETRY_MS {
                    return;
                }
                let (Some(path), Some(prev)) =
                    (self.model.wave.path.clone(), self.wave_fail_stamp)
                else {
                    return;
                };
                if std::fs::metadata(&path)
                    .is_ok_and(|m| (m.len(), file_mtime_ns(&m)) != prev)
                    && self.load_wave()
                {
                    self.needs_redraw = true;
                }
            }
            WaveState::Ready => {}
        }
    }

    /// The Now Playing cover is hidden while the waveform shows a target other than the actual
    /// playhead: an FF/RW scan (button down, or the engine still reports it) or a jog (the
    /// gesture is open, or its seek has not landed). These are the predicates
    /// [`Self::playhead_ms_at`] uses to place the target playhead. Adjusting a ring parameter
    /// is not a trigger.
    fn art_hide_trigger(&self) -> bool {
        self.scan_held() || self.jog.is_open() || self.jog_display.running_seek().is_some()
    }

    /// Collect a finished cover load and advance the fade (D10). A picture that is mid-fade
    /// is repainted at the waveform rate by [`Self::tick_waveform`]; the frame at rest (fully
    /// shown or fully hidden) is requested here, so the last step of a fade is never skipped.
    fn tick_art(&mut self, now_ms: u64) {
        let mut changed = self.art.poll();
        if changed {
            self.model.np_art = self.art.image().cloned();
        }
        if self.cover_viewer.poll() {
            self.apply_cover_view();
        }
        self.art_fade.tick(now_ms, self.art_hide_trigger());
        let alpha = self.art_fade.alpha(now_ms);
        if alpha != self.model.np_art_alpha {
            self.model.np_art_alpha = alpha;
            changed |= self.model.np_art.is_some() && !self.art_fade.is_animating();
        }
        if changed && *self.nav.current() == Route::Main {
            self.needs_redraw = true;
        }
    }

    /// Resolve the playing item's cover list and start loading its picture when the playing
    /// item changed (or the library did). Reads the index and the stick, so the loop runner
    /// calls it next to [`Self::service_browse`]; painting never does.
    pub fn service_art(&mut self, src: &LibrarySource) {
        let subject = self.model.playback.as_ref().and_then(|p| {
            let playable_id = p.session.current_item?.0;
            let item_position = self.model.current_track_entry().map_or(0, |e| e.item_position);
            Some(ArtSubject {
                playable_id,
                item_position,
            })
        });
        if self.art.service(src, subject, crate::screens::NP_ART_SIDE) {
            self.model.np_art = self.art.image().cloned();
            self.needs_redraw = true;
        }
        self.service_cover_view(src);
    }

    /// What the NFC ring should draw (ART_10), or `None` when it is not the record's: the
    /// `Art LEDs` setting is Off, nothing is loaded or the transport is stopped, the playing
    /// item has no resolved primary picture, or the device is shutting down. Playing and
    /// Paused both qualify (Paused is a frozen ring, not a blank one). The picture is the one
    /// Now Playing already holds, and the position is the waveform's dead-reckoned playhead
    /// (tempo, scan and warp included), so there is no second clock. Cheap: no IO.
    pub fn record_led_input(&self) -> Option<RecordInput> {
        if !self.play_mode.settings.resolve_art_leds() || self.shutdown_pending() {
            return None;
        }
        let p = self.model.playback.as_ref()?;
        if !matches!(p.transport, TransportState::Playing | TransportState::Paused) {
            return None;
        }
        let art = self.model.np_art.as_ref()?;
        let position_ms = self
            .playhead_ms_at(self.now_ms)
            .saturating_sub(self.vis_offset_ms());
        Some(RecordInput {
            art: Arc::clone(art),
            position_ms: i64::try_from(position_ms).unwrap_or(i64::MAX),
        })
    }

    /// Whether the playing item has cover art to show (the resolved list, which includes the
    /// sibling-file fallback and drops missing files, is non-empty). Cheap: no IO.
    pub fn playing_item_has_art(&self) -> bool {
        self.art.resolution().has_art()
    }

    /// What the cover viewer should be showing for the current route (ART_09): the primary
    /// image at the detail art box size on the detail screen, the whole set at the viewer size
    /// on the cover screen, nothing elsewhere.
    fn cover_target<'a>(
        model: &'a ScreenModel,
        route: &Route,
        cover_index: usize,
    ) -> Option<CoverTarget<'a>> {
        let covers = model.current_detail_covers()?;
        match route {
            Route::LibraryDetail => Some(CoverTarget {
                root_idx: covers.root_idx,
                covers: &covers.covers[..covers.covers.len().min(1)],
                index: 0,
                px: crate::screens::DETAIL_ART_SIDE,
            }),
            Route::LibraryCover => Some(CoverTarget {
                root_idx: covers.root_idx,
                covers: &covers.covers,
                index: cover_index,
                px: crate::screens::COVER_VIEW_SIDE,
            }),
            _ => None,
        }
    }

    /// Point the cover viewer at the current route's images (reads file stamps; loop only).
    fn service_cover_view(&mut self, src: &LibrarySource) {
        let target = Self::cover_target(&self.model, self.nav.current(), self.cover_index);
        if self.cover_viewer.sync(src, target) {
            self.apply_cover_view();
        }
    }

    /// Copy what the viewer shows into the model and request a frame if a cover route is up.
    fn apply_cover_view(&mut self) {
        let v = &self.cover_viewer;
        let view = crate::screens::CoverView {
            index: v.index(),
            count: v.count(),
            title: v.title(),
            image: v.image().cloned(),
            failed: v.failed(),
        };
        self.model.cover_view = view;
        if matches!(
            self.nav.current(),
            Route::LibraryDetail | Route::LibraryCover
        ) {
            self.needs_redraw = true;
        }
    }

    /// ~30 FPS waveform animation while the picture moves (WAVE_02). The overlay covers the
    /// band, so it does not force repaints; a paused screen with no gesture does not either.
    ///
    /// Without bars (`Pending` / `Failed`) the band is a static flat line, but the footer clock
    /// can still move: a frame unit changes up to 75 times a second and decimal tenths 10
    /// times. The tick therefore drops to [`WAVE_CLOCK_MS`] only when the timecode unit is
    /// `Off` (plain `MM:SS`); frames repaint at [`WAVE_FRAME_MS`] and decimal at
    /// [`WAVE_DECIMAL_MS`] instead of composing and diffing the 77 k pixels for an identical
    /// screen.
    fn tick_waveform(&mut self, now_ms: u64) {
        if *self.nav.current() != Route::Main {
            return;
        }
        let Some(p) = self.model.playback.as_ref() else {
            return;
        };
        // A cover fade needs frames at the animation rate even over a static band.
        let fading = self.model.np_art.is_some() && self.art_fade.is_animating();
        let moving = p.transport == TransportState::Playing
            || p.session.scan != plippa_core::ScanState::None
            || p.session.seek_target_ms.is_some()
            || self.jog.is_open()
            || fading;
        let interval = if self.model.wave.has_bars() || fading {
            WAVE_FRAME_MS
        } else {
            match self.model.play_mode.timecode_unit {
                TimecodeUnit::Off => WAVE_CLOCK_MS,
                TimecodeUnit::DecTenths => WAVE_DECIMAL_MS,
                // Frame rates and hundredths both repaint at the 30 FPS frame tick: the panel
                // cannot show more than that, and the frame diff drops unchanged screens.
                _ => WAVE_FRAME_MS,
            }
        };
        if moving
            && !self.model.up_next_visible()
            && now_ms.saturating_sub(self.last_wave_paint) >= interval
        {
            self.needs_redraw = true;
            self.last_wave_paint = now_ms;
        }
    }

    fn on_menu_enter(&mut self, wifi: &mut dyn WifiBackend) {
        // Enter leaves the current ring entry (or the implicit jog): commit what was turned.
        self.flush_jog();
        if self.compositor.main_menu_visible() {
            let action = self.nav.enter_focused_menu_item();
            self.compositor.hide_main_menu();
            if matches!(action, NavAction::Entered(Route::Main)) {
                self.model.np_focus.reset();
            }
            if matches!(action, NavAction::Entered(Route::Wifi)) && self.model.wifi.is_none() {
                self.init_wifi(wifi);
            }
            self.on_entered_library_root(&action);
            return;
        }
        match self.nav.current().clone() {
            Route::Main => {
                // Main menu is opened by Back. With a list panel showing, push acts on the
                // selected target (NB_15); otherwise it is a no-op on home.
                if !self.model.list_panel_visible() {
                    // Single track: Enter walks the parameter ring from the implicit jog.
                    self.model.np_focus.push_single();
                } else {
                    // UI_13: Enter on an auto-opened, unscrolled overlay only closes it —
                    // the cursor is on the row that is already playing, so jumping would
                    // restart/seek that row by accident.
                    if self.model.np_focus.mode == NpMode::List && !self.list_overlay_scrolled {
                        self.model.np_focus.back();
                        self.clear_list_overlay();
                        self.needs_redraw = true;
                    } else {
                        if let NpPush::Jump(index) = self.model.np_focus.push() {
                            // The engine owns the list: no local change, the new highlight
                            // and any cue-row shift come back as status / list updates.
                            self.outgoing.push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(
                                PlaybackCommand::JumpToEntry { index: index as u32 },
                            )));
                            // Owner decision 4: a push on a playlist row closes the overlay.
                            self.model.np_focus.back();
                            self.clear_list_overlay();
                            self.needs_redraw = true;
                        }
                    }
                }
            }
            Route::Menu => {
                let action = self.nav.enter_focused_menu_item();
                if matches!(action, NavAction::Entered(Route::Wifi)) && self.model.wifi.is_none() {
                    self.init_wifi(wifi);
                }
                self.on_entered_library_root(&action);
            }
            Route::System => {
                let action = self.nav.enter_focused_system_item();
                if matches!(action, NavAction::Entered(Route::SystemNetwork)) {
                    self.begin_network(wifi);
                } else if matches!(action, NavAction::Entered(Route::SystemStores)) {
                    // The menu already pushed the route; load the status behind it.
                    self.begin_stores();
                }
            }
            Route::Debug => {
                self.controls.enter();
            }
            Route::Wifi => {
                if let Some(form) = self.model.wifi.as_mut() {
                    form.handle(WifiFormEvent::Enter, wifi);
                    self.sync_network_from_wifi(wifi);
                    self.sync_keyboard_a_override();
                }
            }
            Route::SystemNetwork => self.network_enter(wifi),
            Route::SystemNetworkPin => self.network_pin_enter(),
            Route::SystemIp => {
                // Scroll-only list; Enter is a no-op.
            }
            Route::SystemDebugAudio => {
                // Scroll-only report; Enter is a no-op.
            }
            Route::LibraryStoreTap => self.store_enter(),
            Route::SystemStores | Route::LibraryStores => self.stores_enter(),
            Route::StoreConnect => self.connect_enter(),
            Route::StoreReleases => self.releases_enter(),
            Route::StoreRelease => self.release_enter(),
            Route::LibraryRemove => self.remove_enter(),
            Route::SystemPlayMode => self.play_mode_enter(),
            Route::SystemAudio => self.audio_enter(),
            Route::SystemAudioOutputs => self.audio_outputs_enter(),
            Route::SystemAudioDelay => self.audio_delay_enter(),
            Route::SystemAudioAccessories => self.accessories_enter(),
            Route::SystemAudioAccessory => self.accessory_detail_enter(),
            Route::SystemAudioPairNew => self.accessory_pair_enter(),
            Route::SystemDebug => self.debug_enter(),
            Route::Library => {
                // Enter on a category row → its A-Z list.
                if !self.model.storage_present {
                    return;
                }
                let kinds = self.model.library_root_kinds();
                if self.nav.focus >= kinds.len() {
                    // BCS_08: the row after the categories is Stores.
                    self.model.browse_kind = None;
                    self.open_stores(Route::LibraryStores);
                    return;
                }
                if let Some(kind) = kinds.get(self.nav.focus).copied() {
                    self.model.browse_kind = Some(kind);
                    self.model.library.clear();
                    self.model.detail_playable_id = None;
                    self.model.detail_track_pos = None;
                    self.model.detail_info = None;
                    self.request_browse(BrowseRequest::Category(kind));
                    let _ = self.nav.push(Route::LibraryCategory);
                }
            }
            Route::LibraryCategory => {
                // Enter on list → detail (play is on the detail Play action).
                if let Some(item) = self.model.library.get(self.nav.focus).cloned() {
                    self.model.detail_playable_id = Some(item.id);
                    self.model.detail_track_pos = item.item_position;
                    self.model.detail_info = None;
                    self.request_browse(BrowseRequest::Detail {
                        item,
                        kind: self.model.browse_kind,
                    });
                    let _ = self.nav.push(Route::LibraryDetail);
                }
            }
            Route::LibraryDetail => {
                let actions = self.model.detail_actions();
                let action = actions[self.nav.focus % actions.len()];
                match action {
                    LibraryDetailAction::Play => {
                        if let Some(item) = self.model.detail_item().cloned() {
                            // One track of a multi-track release (Tracks / Mixes lists):
                            // play the release from that track.
                            let from_track = item.item_position.filter(|_| {
                                self.model
                                    .current_detail_info()
                                    .is_some_and(|d| d.tracks.len() > 1)
                            });
                            if let Some(pos) = from_track {
                                self.play_from(pos, item.title.clone(), None);
                                return;
                            }
                            if let Some(root) = item.library_root.as_ref() {
                                let id =
                                    PlayableItemId::new(encode_playable_id(root, item.id));
                                // Collections replace the `nowPlayingList` (persisted resume
                                // honoured); single tracks keep the plain LoadAndPlay path:
                                // engine Load alone forces Stopped and would race an
                                // optimistic Playing LED before Play arrives.
                                let cmd = match self.model.browse_kind {
                                    None | Some(LibraryKind::Track) => {
                                        PlaybackCommand::LoadAndPlay(id)
                                    }
                                    Some(_) => PlaybackCommand::LoadCollection { playable_id: id },
                                };
                                self.pending_jump = None;
                                self.outgoing
                                    .push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(cmd)));
                                self.compositor
                                    .show_status(&format!("Playing {}", item.title), self.now_ms);
                            } else {
                                self.compositor
                                    .show_status("Library root missing", self.now_ms);
                            }
                        }
                    }
                    LibraryDetailAction::BrowseTracks => {
                        let _ = self.nav.push(Route::LibraryTracks);
                    }
                    LibraryDetailAction::BrowseChapters => {
                        let _ = self.nav.push(Route::LibraryChapters);
                    }
                    LibraryDetailAction::CoverArt => {
                        self.cover_index = 0;
                        let _ = self.nav.push(Route::LibraryCover);
                    }
                    LibraryDetailAction::Remove => self.open_remove_for_detail(),
                    LibraryDetailAction::Associate => {
                        if let Some(item) = self.model.detail_item().cloned() {
                            self.outgoing.push(UiOutgoing::AssociateSelected {
                                playable_id: item.id,
                                item: None,
                                title: item.title.clone(),
                            });
                            let _ = self.nav.push(Route::LibraryAssociate);
                            self.compositor.show_status("tap a noubin", self.now_ms);
                        }
                    }
                }
            }
            Route::LibraryTracks => {
                // A press opens what can be done with the track; nothing plays until Play.
                let pos = self
                    .model
                    .current_detail_info()
                    .and_then(|d| d.tracks.get(self.nav.focus))
                    .map(|t| t.position);
                if let Some(pos) = pos {
                    self.model.track_menu_pos = Some(pos);
                    let _ = self.nav.push(Route::LibraryTrackActions);
                }
            }
            Route::LibraryTrackActions => self.track_actions_enter(),
            Route::LibraryTrackAnalysis => self.track_analysis_enter(),
            Route::LibraryChapters => {
                let row = self
                    .model
                    .current_detail_info()
                    .and_then(|d| d.chapters.get(self.nav.focus))
                    .map(|c| (c.item_position, c.title.clone(), c.start_ms));
                if let Some((pos, title, start)) = row {
                    self.play_from(pos, title, Some(start));
                }
            }
            Route::LibraryAssociate => {
                // Wait for NFC; Enter is a no-op.
            }
            _ => {}
        }
    }

    /// Press on the track action menu (TRACK_01).
    fn track_actions_enter(&mut self) {
        let Some(track) = self.model.track_menu_row().cloned() else {
            // The list no longer holds this track (stick swapped under the menu): Press, like
            // Back, goes back to the list.
            self.model.track_menu_pos = None;
            let _ = self.nav.back();
            self.needs_redraw = true;
            return;
        };
        let action = TrackAction::ALL[self.nav.focus.min(TrackAction::ALL.len() - 1)];
        match action {
            TrackAction::Play => {
                // The same checks the list made when a press played: a missing or unsupported
                // file says why instead of starting. The menu stays; playing shows in Now
                // Playing, as it does from a collection.
                match ScreenModel::track_unplayable_reason(&track) {
                    Some(reason) => self.compositor.show_status(&reason, self.now_ms),
                    None => self.play_from(track.position, track.title.clone(), None),
                }
            }
            TrackAction::CoverArt => {
                if self
                    .model
                    .current_detail_covers()
                    .is_some_and(|c| !c.covers.is_empty())
                {
                    self.cover_index = 0;
                    let _ = self.nav.push(Route::LibraryCover);
                } else {
                    // Nothing to show: the same answer the detail screen gives by not offering
                    // the action, as a popup rather than an empty screen.
                    self.compositor.show_status("No cover art", self.now_ms);
                }
            }
            TrackAction::Associate => {
                if let Some(item) = self.model.detail_item().cloned() {
                    // NB_18: the track's own position and title, not the release's. The menu
                    // only opens for a track that is in the list (`track` above), so this is
                    // never a track-less action.
                    self.outgoing.push(UiOutgoing::AssociateSelected {
                        playable_id: item.id,
                        item: Some(track.position),
                        title: track.title.clone(),
                    });
                    let _ = self.nav.push(Route::LibraryAssociate);
                    self.compositor.show_status("tap a noubin", self.now_ms);
                }
            }
            TrackAction::Analysis => self.open_track_analysis(&track.title, track.position),
        }
        self.needs_redraw = true;
    }

    /// Open the analysis screen for `position` of the collection on the detail screen.
    fn open_track_analysis(&mut self, title: &str, position: u32) {
        let Some(root) = self
            .model
            .detail_item()
            .and_then(|item| item.library_root.clone().map(|root| (root, item.id)))
        else {
            self.compositor
                .show_status("Library root missing", self.now_ms);
            return;
        };
        let playable_id = encode_playable_id(&root.0, root.1);
        self.model.track_analysis = Some(AnalysisView::new(
            playable_id,
            position,
            screen_text(title),
            self.now_ms,
        ));
        let _ = self.nav.push(Route::LibraryTrackAnalysis);
        self.send_analysis_status(playable_id, position, self.now_ms);
    }

    /// Ask for the track's analysis status. A dead or absent store thread ends the wait with an
    /// error the screen shows, never a spinner.
    fn send_analysis_status(&mut self, playable_id: u64, item: u32, now_ms: u64) {
        if self.store_send(StoreRequest::AnalysisStatus { playable_id, item }) {
            return;
        }
        let error = self.store_unavailable();
        if let Some(view) = self.model.track_analysis.as_mut() {
            view.apply_status(Err(error), now_ms);
        }
    }

    /// Press on the analysis screen: re-analyse, or read the status again.
    fn track_analysis_enter(&mut self) {
        let now_ms = self.now_ms;
        let Some(view) = self.model.track_analysis.as_mut() else {
            return;
        };
        let Some(action) = view.action_at(self.nav.focus) else {
            return;
        };
        let (playable_id, item) = (view.playable_id, view.item);
        match action {
            AnalysisAction::Retry => {
                view.restart(now_ms);
                self.send_analysis_status(playable_id, item, now_ms);
            }
            AnalysisAction::ReanalyseAll | AnalysisAction::ReanalyseKind(_) => {
                let kind = match action {
                    AnalysisAction::ReanalyseKind(kind) => Some(kind),
                    _ => None,
                };
                // A file known to be missing is not worth a request: say so.
                let missing = match kind {
                    Some(kind) => view
                        .kinds
                        .iter()
                        .any(|s| s.kind == kind && s.state == crate::analysis::AnalysisState::Unavailable),
                    None => view.waveform_state() == Some(crate::analysis::AnalysisState::Unavailable),
                };
                if missing {
                    self.compositor
                        .show_status("Media file missing", now_ms);
                } else if view.waiting {
                    // The previous call is still out; the worker answers in order, so a
                    // second press would only queue a duplicate.
                } else {
                    view.begin_call(now_ms);
                    let request = StoreRequest::Reanalyse {
                        playable_id,
                        item,
                        kind,
                    };
                    if !self.store_send(request) {
                        let error = self.store_unavailable();
                        if let Some(view) = self.model.track_analysis.as_mut() {
                            view.apply_reanalyse(kind, Err(error), now_ms);
                        }
                    }
                }
            }
        }
        self.needs_redraw = true;
    }

    /// Entering the Library from the main menu shows fresh category counts.
    fn on_entered_library_root(&mut self, action: &NavAction) {
        if matches!(action, NavAction::Entered(Route::Library)) {
            self.model.browse_kind = None;
            self.request_browse(BrowseRequest::Counts);
        }
    }

    /// Play the selected collection from entry `track_pos` (0-based `mediaList` position),
    /// optionally from the chapter starting at `cue_start_ms` inside it. Sends only engine
    /// commands: the collection is loaded with `LoadCollection` when it is not the loaded
    /// one, then `JumpToEntry` for the row of the published list (see [`Self::resolve_jump`]).
    fn play_from(&mut self, track_pos: u32, title: String, cue_start_ms: Option<u64>) {
        let Some(item) = self.model.detail_item().cloned() else {
            return;
        };
        let Some(root) = item.library_root.as_ref() else {
            self.compositor
                .show_status("Library root missing", self.now_ms);
            return;
        };
        let target = PlayableItemId::new(encode_playable_id(root, item.id));
        let loaded = self
            .model
            .playback
            .as_ref()
            .is_some_and(|p| p.session.current_item == Some(target))
            && self.model.now_playing.is_some();
        self.pending_jump = Some(PendingJump {
            target,
            track_pos,
            title: title.clone(),
            cue_start_ms,
            jumped_track: false,
            tries: 0,
        });
        if !loaded {
            self.outgoing.push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(
                PlaybackCommand::LoadCollection { playable_id: target },
            )));
        }
        self.compositor
            .show_status(&format!("Playing {title}"), self.now_ms);
        self.resolve_jump();
    }

    /// Turn a pending "play from here" into `JumpToEntry` once the engine's published list
    /// belongs to the target collection. The list only carries cue rows for the *current*
    /// track, so a chapter of another track takes two jumps (track row, then cue row).
    fn resolve_jump(&mut self) {
        let Some(mut pj) = self.pending_jump.take() else {
            return;
        };
        let current = self
            .model
            .playback
            .as_ref()
            .and_then(|p| p.session.current_item);
        let Some(list) = self.model.now_playing.as_ref() else {
            self.pending_jump = Some(pj);
            return;
        };
        if current != Some(pj.target) {
            // Old collection still loaded (LoadCollection not applied yet).
            self.pending_jump = Some(pj);
            return;
        }
        // Flat index of the wanted Track row: same position and title, else same title
        // (shuffled list), else same position.
        let track_rows: Vec<(usize, &NowPlayingEntry)> = list
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == NowPlayingEntryKind::Track)
            .collect();
        let track_flat = track_rows
            .iter()
            .find(|(_, e)| e.track_index == pj.track_pos && e.title == pj.title)
            .or_else(|| track_rows.iter().find(|(_, e)| e.title == pj.title))
            .or_else(|| track_rows.iter().find(|(_, e)| e.track_index == pj.track_pos))
            .map(|(i, e)| (*i, e.track_index));
        let Some((track_flat, track_index)) = track_flat else {
            pj.tries += 1;
            if pj.tries < PENDING_JUMP_MAX_TRIES {
                self.pending_jump = Some(pj);
            }
            return;
        };
        let is_current_track = track_flat == list.current_index as usize;
        let index = match pj.cue_start_ms {
            None => Some(track_flat),
            Some(_) if !is_current_track && !pj.jumped_track => Some(track_flat),
            Some(start) if is_current_track => list
                .entries
                .iter()
                .enumerate()
                .find(|(_, e)| {
                    e.kind == NowPlayingEntryKind::Cue
                        && e.track_index == track_index
                        && e.start_ms == start
                })
                .map(|(i, _)| i),
            Some(_) => None, // track jump sent, engine has not moved yet
        };
        let Some(index) = index else {
            pj.tries += 1;
            if pj.tries < PENDING_JUMP_MAX_TRIES {
                self.pending_jump = Some(pj);
            }
            return;
        };
        self.outgoing.push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(
            PlaybackCommand::JumpToEntry { index: index as u32 },
        )));
        let cue_pending = pj.cue_start_ms.is_some() && !is_current_track;
        if cue_pending {
            pj.jumped_track = true;
            pj.tries = 0;
            self.pending_jump = Some(pj);
        }
        // A jump keeps the transport state; "play from here" must end up playing.
        let playing = matches!(
            self.model.playback.as_ref().map(|p| p.transport),
            Some(TransportState::Playing)
        );
        if !playing && !cue_pending {
            self.outgoing
                .push(UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play)));
        }
    }

    /// Compose screen + optional overlays into the framebuffer (no present).
    fn compose(&mut self) {
        // Legacy Route::Menu: show overlay strip without pushing OpenMainMenu.
        if *self.nav.current() == Route::Menu && !self.compositor.main_menu_visible() {
            self.compositor.show_main_menu(self.now_ms);
        }
        let nav = &self.nav;
        let model = &self.model;
        self.compositor.compose_into(
            &mut self.frame,
            BG,
            |fb| model.render_screen(nav, fb),
            |fb| model.render_main_menu_overlay(nav, fb),
        );
    }

    /// Tests: compose the screen and return its pixels without touching a display.
    ///
    /// `present` correctly skips an unchanged screen, so tests that assert on pixels after a
    /// no-op input must read the composition instead of a `MockDisplay`.
    pub fn composed_pixels(&mut self) -> Vec<u16> {
        self.compose();
        self.frame.pixels.clone()
    }

    /// Compose and present to `display`.
    ///
    /// WAVE_02: only the bounding box of pixels that changed is sent. A full frame is used
    /// when there is no previous frame or when the change reaches most of the pixels
    /// **outside** the waveform band (see [`diff_needs_full_frame`]); an unchanged frame
    /// sends nothing at all. Every present path goes through here (no direct
    /// `display.present` elsewhere) so `prev_frame` can never go stale.
    pub fn present(&mut self, display: &mut dyn DisplayBackend) {
        self.compose();
        let Some(prev) = self.prev_frame.as_ref() else {
            display.present(&self.frame);
            self.prev_frame = Some(self.frame.clone());
            return;
        };
        let rect = frame_diff(prev, &self.frame);
        if rect.is_empty() {
            return;
        }
        if diff_needs_full_frame(rect, self.frame.width, self.frame.height) {
            display.present(&self.frame);
        } else {
            display.present_rect(&self.frame, rect);
        }
        self.prev_frame = Some(self.frame.clone());
    }

    /// Compose and present the **whole** frame (used by [`Self::pump`] so frame-count
    /// assertions in existing tests stay meaningful).
    fn present_full(&mut self, display: &mut dyn DisplayBackend) {
        self.compose();
        display.present(&self.frame);
        self.prev_frame = Some(self.frame.clone());
    }

    /// Convenience: apply scripted inputs then present (tests).
    pub fn pump(
        &mut self,
        events: impl IntoIterator<Item = InputEvent>,
        wifi: &mut dyn WifiBackend,
        display: &mut dyn DisplayBackend,
    ) {
        for ev in events {
            self.handle_input(ev, wifi);
        }
        self.present_full(display);
    }

    /// Tests: apply inputs then present through the real diff path (WAVE_02).
    pub fn pump_diff(
        &mut self,
        events: impl IntoIterator<Item = InputEvent>,
        wifi: &mut dyn WifiBackend,
        display: &mut dyn DisplayBackend,
    ) {
        for ev in events {
            self.handle_input(ev, wifi);
        }
        self.present(display);
    }
}

/// Bounding box of the pixels that differ between two composed frames (`EMPTY` when equal).
///
/// A size mismatch (never expected in the loop) is the whole frame, so no pixels are missed.
fn frame_diff(prev: &FrameBuffer, next: &FrameBuffer) -> Rect {
    if prev.width != next.width || prev.height != next.height {
        return Rect::new(0, 0, next.width, next.height);
    }
    let w = next.width as usize;
    let (mut min_x, mut min_y) = (u16::MAX, u16::MAX);
    let (mut max_x, mut max_y) = (0u16, 0u16);
    let mut changed = false;
    for y in 0..next.height {
        let row = y as usize * w;
        for x in 0..next.width {
            if prev.pixels[row + x as usize] != next.pixels[row + x as usize] {
                changed = true;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    if !changed {
        return Rect::EMPTY;
    }
    Rect::new(
        min_x,
        min_y,
        max_x - min_x + 1,
        max_y - min_y + 1,
    )
}

/// Whether a changed bounding box should be presented as a whole frame.
///
/// The spec's "≥ 60 % of the screen" cannot be applied to the screen itself once the waveform
/// band is most of the content box: `WAVE_BAND_H` is `NP_CONTENT_BOTTOM − NP_CONTENT_TOP` rows
/// tall (145 of 240 today), so a loud track whose 30 s window sits inside the file changes
/// ~60 % of the screen on every scroll — a screen-relative guard would send a full frame every
/// 33 ms and defeat the optimisation that is the point of WAVE_02.
///
/// The guard is therefore applied to the pixels the band does **not** cover (header, footer,
/// popup rows): `full − band` is the denominator and the part of the box outside the band the
/// numerator. A band-only change has no pixels there — zero, whatever the tokens say — so it
/// can never trip the guard; a change that repaints most of the header and footer (a new
/// track, a route change, an overlay) still pays a single full frame. Everything is derived
/// from the band rect in [`crate::screens`], never from a hard-coded row count.
fn diff_needs_full_frame(rect: Rect, width: u16, height: u16) -> bool {
    let screen = u32::from(width) * u32::from(height);
    let band_y0 = u32::from(crate::screens::WAVE_BAND_Y.min(height));
    let band_y1 = band_y0
        .saturating_add(u32::from(crate::screens::WAVE_BAND_H))
        .min(u32::from(height));
    let outside = screen.saturating_sub(u32::from(width) * (band_y1 - band_y0));
    if outside == 0 {
        // The band is the whole screen: nothing outside it can change.
        return false;
    }
    // The band is a full-width strip, so the part of the box that lies outside it is exact:
    // the same columns, only the rows of the box above / below the band.
    let rect_y1 = u32::from(rect.y).saturating_add(u32::from(rect.h));
    let overlap_h = rect_y1.min(band_y1).saturating_sub(u32::from(rect.y).max(band_y0));
    let inside_band = u32::from(rect.w).saturating_mul(overlap_h);
    let outside_changed = rect.area().saturating_sub(inside_band);
    outside_changed * 100 >= outside * PRESENT_FULL_PERCENT
}

/// `mtime` in ns since the Unix epoch (`-1` when the filesystem cannot report one).
fn file_mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(-1)
}

/// Default mock wifi for desktop / CI.
pub fn default_mock_wifi() -> MockWifi {
    MockWifi::default()
}

/// The audit line for a tag-page lookup that went ahead without the confirmation screen
/// (BCS_11). Names the host only: a lookup's URL (path, query, anything credential-shaped) never
/// reaches the log, and a host with anything but host characters is reported as unknown.
pub fn consent_skipped_log_line(host: &str) -> String {
    let clean = !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    let host = if clean { host } else { "an unknown host" };
    format!("store: contacted {host} without confirmation (setting on)")
}

fn log_ndef_tap(tap_id: u64, uid: &[u8], ndef: &[u8]) {
    let uid_hex = hex_bytes(uid);
    let ndef_hex = hex_bytes(ndef);
    let key = extract_noubin_key(ndef)
        .map(|k| format!("{k:?}"))
        .unwrap_or_else(|e| format!("(extract failed: {e})"));
    let utf8_preview = String::from_utf8_lossy(ndef);
    println!(
        "plippa-ui: tap #{tap_id} uid={uid_hex} ndef_len={} key={key}",
        ndef.len()
    );
    println!("plippa-ui:   ndef hex: {ndef_hex}");
    println!("plippa-ui:   ndef utf8 preview: {utf8_preview}");
}

fn hex_bytes(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn truncate_popup_line(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// One `Display` line of tag payload for the status popup's second line.
///
/// The popup caps at two lines, so this must never contain a newline: controls (including the
/// newlines in a text record) become `·`. Anything longer than one popup line keeps its first
/// 28 glyphs and ends in `..`, the same ellipsis the text layer uses, so a long payload is
/// shortened visibly instead of losing a silently-dropped third line.
fn ndef_payload_snippet(ndef: &[u8]) -> String {
    /// `Display` glyphs that fit the 304 px popup text box (`304 / 10`).
    const LINE_CHARS: usize = 30;
    let raw = String::from_utf8_lossy(ndef);
    let mut cleaned = raw
        .chars()
        .map(|c| if c.is_control() { '·' } else { c });
    let head: String = cleaned.by_ref().take(LINE_CHARS - 2).collect();
    if cleaned.next().is_some() {
        format!("{head}..")
    } else if head.is_empty() {
        format!("({} bytes)", ndef.len())
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::{MAIN_MENU_H, SCREEN_H, SCREEN_W};
    use crate::control::{InteractionMode, NowPlayingFocus, NpTarget};
    use crate::display::MockDisplay;
    use crate::screens::{
        UP_NEXT_HEADER_Y, UP_NEXT_ROWS, UP_NEXT_ROWS_Y, UP_NEXT_TOP, WAVE_BAND_H, WAVE_BAND_Y,
        WAVE_PLAYHEAD_X,
    };

    use crate::ui::tokens::{
        ACCENT, ACCENT_2, ACCENT_DIM, BG, BG_ALT, BORDER_SOFT, CONTENT_BOTTOM, CONTENT_TOP, FG,
        FG_DIM, FG_MUTED, MARGIN_X, NP_CONTENT_BOTTOM, NP_CONTENT_TOP, ROW_H, SURFACE,
        SURFACE_ALT, WARN,
    };
    use plippa_core::{PlayableItemId, SessionState, TransportState};
    use plippa_ipc_proto::{NoticeSeverity, PlaybackStatus, NOTICE_LIBRARY_MISS};

    #[test]
    fn bg_is_tokens_bg() {
        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
        app.present(&mut display);
        let fb = display.last.as_ref().unwrap();
        // The Now Playing chrome owns the top (`SURFACE` header band) and the bottom (30 px
        // footer); the waveform's `BG` ground shows through between them.
        assert_eq!(fb.pixels[0], SURFACE, "top-left is the Now Playing header band");
        let w = SCREEN_W as usize;
        assert_eq!(fb.pixels[NP_CONTENT_TOP as usize * w], BG, "band ground");
        assert_eq!(fb.pixels[fb.pixels.len() - 1], BG_ALT, "bottom-right is the footer ground");
    }

    #[test]
    fn routes_render_without_panic() {
        let mut app = UiApp::new(UiConfig::default()).with_system(SystemStatus {
            time_hhmm: "15:30".into(),
            temperature_c: Some(48),
            fan_status: "pwm 40%".into(),
            portal_url: Some("http://192.168.1.42:8787".into()),
            ip: Some("192.168.1.42".into()),
            ips: vec!["192.168.1.42".into(), "10.0.0.5".into()],
        });
        app.set_library(vec![LibraryItem {
            id: 1,
            title: "Tone".into(),
            associated: true,
            noudata_relpath: "_tone.noudata".into(),
            library_root: None,
            artist: Some("Artist".into()),
            duration_ms: Some(180_000),
            art_path: None,
            item_position: None,
        }]);
        let mut wifi = MockWifi::default();
        app.init_wifi(&mut wifi);
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);

        let routes = [
            Route::Main,
            Route::Menu,
            Route::System,
            Route::SystemIp,
            Route::Warnings,
            Route::Wifi,
            Route::NetworkInfo,
            Route::Library,
            Route::LibraryDetail,
            Route::LibraryAssociate,
            Route::Debug,
        ];
        for route in &routes {
            app.nav = NavStack::new();
            app.compositor.hide_main_menu();
            if *route != Route::Main {
                app.nav.force_push(route.clone());
            }
            if *route == Route::LibraryDetail || *route == Route::LibraryAssociate {
                app.model.detail_playable_id = Some(1);
            }
            app.present(&mut display);
        }
        assert!(display.frames >= routes.len());
    }

    #[test]
    fn library_detail_play_emits_load_and_play() {
        let root = PathBuf::from("/tmp/plippa-p6-lib");
        let expected_id =
            PlayableItemId::new(encode_playable_id(&root, 2));
        let mut app = UiApp::new(UiConfig::default());
        app.set_library(vec![
            LibraryItem {
                id: 2,
                title: "Beta".into(),
                associated: false,
                noudata_relpath: "_b.noudata".into(),
                library_root: Some(root.clone()),
                artist: Some("B Artist".into()),
                duration_ms: Some(60_000),
                art_path: None,
                item_position: None,
            },
            LibraryItem {
                id: 1,
                title: "Alpha".into(),
                associated: false,
                noudata_relpath: "_a.noudata".into(),
                library_root: Some(root),
                artist: None,
                duration_ms: None,
                art_path: None,
                item_position: None,
            },
        ]);
        // Caller supplies sorted list (LibrarySource sorts); keep as-is for focus.
        let mut wifi = MockWifi::default();
        // NB_12: Route::Library is the category root; the item list is LibraryCategory.
        app.nav.force_push(Route::LibraryCategory);
        app.nav.focus = 0;
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), Route::LibraryDetail);
        assert_eq!(app.model.detail_playable_id, Some(2));
        // Play is focus 0.
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(
            app.pending_load_and_play(),
            "library Play must queue LoadAndPlay for optimistic LED"
        );
        let out = app.take_outgoing();
        assert_eq!(
            out.iter()
                .filter(|o| matches!(
                    o,
                    UiOutgoing::Ipc(IpcMessage::PlaybackCommand(_))
                ))
                .count(),
            1,
            "expected a single playback command, got {out:?}"
        );
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::LoadAndPlay(id)))
                    if *id == expected_id
            )),
            "expected LoadAndPlay({expected_id:?}), got {out:?}"
        );
    }

    #[test]
    fn library_detail_play_without_root_shows_status() {
        let mut app = UiApp::new(UiConfig::default());
        app.set_library(vec![LibraryItem {
            id: 2,
            title: "Beta".into(),
            associated: false,
            noudata_relpath: "_b.noudata".into(),
            library_root: None,
            artist: None,
            duration_ms: None,
            art_path: None,
            item_position: None,
        }]);
        let mut wifi = MockWifi::default();
        app.model.detail_playable_id = Some(2);
        app.nav.force_push(Route::Library);
        app.nav.force_push(Route::LibraryDetail);
        app.nav.focus = 0;
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        let out = app.take_outgoing();
        assert!(
            out.is_empty(),
            "missing library_root must not emit LoadAndPlay, got {out:?}"
        );
        assert!(app
            .compositor
            .status()
            .is_some_and(|s| s.lines().iter().any(|l| l.contains("root missing"))));
    }

    #[test]
    fn release_associate_sends_none() {
        let mut app = UiApp::new(UiConfig::default());
        app.set_library(vec![LibraryItem {
            id: 7,
            title: "Tone".into(),
            associated: false,
            noudata_relpath: "_tone.noudata".into(),
            library_root: None,
            artist: None,
            duration_ms: None,
            art_path: None,
            item_position: None,
        }]);
        let mut wifi = MockWifi::default();
        app.model.detail_playable_id = Some(7);
        app.nav.force_push(Route::Library);
        app.nav.force_push(Route::LibraryDetail);
        app.nav.focus = 1; // Associate
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), Route::LibraryAssociate);
        let out = app.take_outgoing();
        assert!(
            matches!(
                &out[0],
                UiOutgoing::AssociateSelected {
                    playable_id: 7,
                    item: None,
                    title
                } if title == "Tone"
            ),
            "expected AssociateSelected, got {out:?}"
        );
        assert!(app
            .compositor
            .status()
            .is_some_and(|s| s.lines().iter().any(|l| l.contains("noubin"))));
    }

    #[test]
    fn the_network_rows_switch_https_and_regenerate_only_on_a_second_press() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let mut settings = PortalSettingsInfo {
            available: true,
            enabled: true,
            ..PortalSettingsInfo::default()
        };
        settings.fingerprint = Some("0123456789abcdef".repeat(4));
        let mut view = NetworkView::looking(0);
        view.apply(Ok(settings));
        app.model.network = Some(view);
        app.nav.force_push(Route::SystemNetwork);
        fn at(app: &UiApp, row: NetworkRow) -> usize {
            app.network_rows().iter().position(|r| *r == row).expect("the row")
        }

        // The HTTPS row asks for the opposite of what the server reports.
        app.nav.focus = at(&app, NetworkRow::Https);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        // This test has no store worker, so the call fails; the screen says so and keeps the
        // state it knows instead of pretending the switch moved.
        let view = app.model.network.as_ref().expect("network view");
        assert!(!view.busy);
        assert_eq!(view.row_text(NetworkRow::Https, None, None, false).1, "On");
        assert!(view.notice.is_some());

        // Regenerate asks first: the first press only arms the row.
        app.nav.focus = at(&app, NetworkRow::Regenerate);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        let view = app.model.network.as_ref().expect("network view");
        assert!(view.regenerate_armed());
        assert_eq!(
            view.row_text(NetworkRow::Regenerate, None, None, false).1,
            "Push again"
        );
        // The second press makes the call and disarms the row.
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(!app.model.network.as_ref().expect("network view").regenerate_armed());
    }

    #[test]
    fn library_detail_missing_art_placeholder_paints() {
        let mut app = UiApp::new(UiConfig::default());
        app.set_library(vec![LibraryItem {
            id: 1,
            title: "Tone".into(),
            associated: false,
            noudata_relpath: "_tone.noudata".into(),
            library_root: None,
            artist: Some("A".into()),
            duration_ms: Some(1_000),
            art_path: None,
            item_position: None,
        }]);
        app.model.detail_playable_id = Some(1);
        app.nav.force_push(Route::LibraryDetail);
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
        app.present(&mut display);
        let fb = display.last.as_ref().unwrap();
        // The placeholder artefact specifically: the 72 px art box at the left margin.
        let art_x = MARGIN_X;
        let art_y = CONTENT_TOP;
        let art_size = 72u16;
        let px = |x: u16, y: u16| fb.pixels[y as usize * SCREEN_W as usize + x as usize];
        assert_eq!(px(art_x, art_y), WARN, "missing-art border at the top-left corner");
        assert_eq!(
            px(art_x + art_size - 1, art_y + art_size - 1),
            WARN,
            "missing-art border at the bottom-right corner"
        );
        let ground = (art_y + 1..art_y + art_size - 1)
            .flat_map(|y| (art_x + 1..art_x + art_size - 1).map(move |x| (x, y)))
            .filter(|&(x, y)| px(x, y) == BG_ALT)
            .count();
        assert!(ground > 1_000, "placeholder ground, got {ground}");
        let label = (art_y..art_y + art_size)
            .flat_map(|y| (art_x..art_x + art_size).map(move |x| (x, y)))
            .filter(|&(x, y)| px(x, y) == FG_MUTED)
            .count();
        assert!(label > 0, "the placeholder label is painted inside the box");
    }

    #[test]
    fn back_from_now_playing_shows_menu_overlay_timeout() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);

        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
        app.handle_input(InputEvent::Back, &mut wifi);
        assert!(app.compositor.main_menu_visible());
        assert_eq!(*app.nav.current(), Route::Main);
        app.present(&mut display);
        let fb = display.last.as_ref().unwrap();
        // Overlay band was painted (not full-screen menu title bar pattern).
        let overlay_px = fb.pixels[..(MAIN_MENU_H as usize * SCREEN_W as usize)]
            .iter()
            .filter(|&&p| p != BG && p != SURFACE)
            .count();
        assert!(overlay_px > 0, "expected menu glyphs in top {MAIN_MENU_H}px");

        // Idle almost 3s — still open.
        app.handle_input(InputEvent::Tick { now_ms: 2_900 }, &mut wifi);
        assert!(app.compositor.main_menu_visible());
        // Activity resets timer.
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 5_800 }, &mut wifi);
        assert!(app.compositor.main_menu_visible());
        // 3s after last activity (2900 was overwritten by rotate at ~2900... actually
        // note_activity uses now_ms which is still 2900 until next Tick).
        // Rotate happened with now_ms=2900, so hide at 2900+3000=5900.
        app.handle_input(InputEvent::Tick { now_ms: 5_900 }, &mut wifi);
        assert!(!app.compositor.main_menu_visible());
        assert_eq!(*app.nav.current(), Route::Main);
    }

    #[test]
    fn main_menu_strip_follows_raw_encoder_direction() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
        app.handle_input(InputEvent::Back, &mut wifi);
        assert!(app.compositor.main_menu_visible());
        assert_eq!(app.nav.focus, 0, "opens on Now Playing");
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.nav.focus, 1, "right = Library");
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.nav.focus, 2, "right again = System");
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.nav.focus, 1, "left goes back");
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.nav.focus, 2, "left from Now Playing wraps to System");
    }

    #[test]
    fn vertical_lists_keep_the_inverted_direction() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.nav.force_push(Route::System);
        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
        assert!(!app.compositor.main_menu_visible());
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.nav.focus, 1, "left moves down the System list");
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.nav.focus, 0, "right moves back up");
    }

    #[test]
    fn back_from_library_shows_menu_enter_now_playing() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.nav.force_push(Route::Library);
        assert_eq!(*app.nav.current(), Route::Library);

        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(*app.nav.current(), Route::Main);
        assert!(app.compositor.main_menu_visible());
        assert_eq!(app.nav.focus, 0); // Now Playing

        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), Route::Main);
        assert_eq!(app.nav.depth(), 1);
        assert!(!app.compositor.main_menu_visible());
    }

    #[test]
    fn navigate_edit_back_via_controls() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.nav.force_push(Route::Debug);
        assert_eq!(app.controls.mode(), InteractionMode::Navigate);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(app.controls.mode(), InteractionMode::Edit);
        let before = app.controls.focused_value().unwrap();
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.controls.focused_value(), Some(before + 1));
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(app.controls.mode(), InteractionMode::Navigate);
        assert_eq!(*app.nav.current(), Route::Debug);
    }

    #[test]
    fn status_popup_show_clear_in_composed_frame() {
        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
        app.compositor.show_main_menu(0);
        app.compositor.show_status("one\ntwo\nthree\nfour", 0);
        let h = app.compositor.status().unwrap().height_px();
        assert_eq!(h, 60, "two Display rows");
        assert_eq!(app.compositor.status().unwrap().lines().len(), 2);
        app.present(&mut display);
        let fb = display.last.as_ref().unwrap();
        let y0 = (SCREEN_H - h) as usize;
        assert_ne!(fb.pixels[y0 * SCREEN_W as usize], 0);
        app.compositor.clear_status();
        assert!(app.compositor.status().is_none());
    }

    #[test]
    fn hold_hooks_menu_enter_and_assignable() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
        app.handle_input(InputEvent::MenuEnterButton { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 800 }, &mut wifi);
        app.handle_input(InputEvent::AssignableEncoderPush { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 1_600 }, &mut wifi);
        let holds = app.take_hold_events();
        assert!(holds.iter().any(|h| h.target == HoldTarget::MenuEnter));
        assert!(holds
            .iter()
            .any(|h| h.target == HoldTarget::AssignableEncoder));
    }

    #[test]
    fn playback_status_updates_main_screen() {
        let mut app = UiApp::new(UiConfig::default());
        let mut session = SessionState::default();
        session.transport = TransportState::Playing;
        session.current_item = Some(PlayableItemId::new(7));
        session.volume = Volume::new(40);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            session,
        )));
        assert_eq!(app.model.transport_label(), "Playing");
        assert_eq!(app.model.volume.percent(), 40);
    }

    fn track_row(position: u32, available: bool, supported: bool, format: &str) -> crate::screens::TrackRow {
        crate::screens::TrackRow {
            position,
            title: format!("T{position}"),
            artist: None,
            duration_ms: Some(1_000),
            available,
            supported,
            format: format.into(),
        }
    }

    /// A browsed collection whose track list is `tracks`, with the tracks screen open.
    fn tracks_app(tracks: Vec<crate::screens::TrackRow>) -> UiApp {
        let mut app = UiApp::new(UiConfig::default());
        app.set_library(vec![LibraryItem {
            id: 1,
            title: "Release".into(),
            associated: false,
            noudata_relpath: "_r.noudata".into(),
            library_root: Some(PathBuf::from("/media/noubin")),
            artist: None,
            duration_ms: None,
            art_path: None,
            item_position: None,
        }]);
        app.model.detail_playable_id = Some(1);
        app.model.detail_info = Some(crate::screens::DetailInfo {
            playable_id: 1,
            kind: None,
            tracks,
            chapters: vec![],
            media_missing: false,
        });
        app.nav.force_push(Route::Library);
        app.nav.force_push(Route::LibraryTracks);
        app
    }

    #[test]
    fn play_on_an_unplayable_track_says_which_format() {
        let mut app = tracks_app(vec![
            track_row(0, true, false, "M4A/AAC"),
            track_row(1, false, true, "WAV"),
        ]);
        let mut wifi = MockWifi::default();
        app.nav.focus = 0;
        // TRACK_01: a press opens the track menu, which opens for any track; Play (its first
        // row) is where the reason is given, exactly as the list gave it before.
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), Route::LibraryTrackActions);
        assert!(app.take_outgoing().is_empty());
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(app.take_outgoing().is_empty(), "an unplayable track must not start playback");
        let status = app.compositor.status().expect("status popup");
        assert!(status.lines().iter().any(|l| l.contains("M4A/AAC is not supported")), "{:?}", status.lines());
        assert!(!status.lines().iter().any(|l| l.contains("missing")));

        // A genuinely missing file keeps its own message.
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(*app.nav.current(), Route::LibraryTracks);
        app.nav.focus = 1;
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        let status = app.compositor.status().expect("status popup");
        assert!(status.lines().iter().any(|l| l.contains("Media file missing")), "{:?}", status.lines());
        assert!(app.take_outgoing().is_empty());
    }

    #[test]
    fn an_unplayable_notice_is_a_two_line_popup_not_a_missing_file() {
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(IpcMessage::UserNotice(UserNotice {
            severity: NoticeSeverity::Error,
            code: plippa_ipc_proto::NOTICE_MEDIA_UNPLAYABLE.into(),
            message: "M4A/AAC is not supported by this player (plippa-D-poc2)".into(),
        }));
        let status = app.compositor.status().expect("popup");
        assert_eq!(status.lines()[0], "Can't play this file");
        assert!(status.lines()[1].contains("M4A/AAC"));
        assert!(status.lines().len() <= 2);
    }

    #[test]
    fn miss_error_surfaces_on_ui() {
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(IpcMessage::UserNotice(UserNotice {
            severity: NoticeSeverity::Error,
            code: NOTICE_LIBRARY_MISS.into(),
            message: "No local media for this Noubin".into(),
        }));
        assert!(app
            .model
            .banner
            .as_ref()
            .unwrap()
            .contains("No local media"));
        assert_eq!(app.model.notices[0].code, NOTICE_LIBRARY_MISS);
        let status = app.compositor.status().expect("miss status popup");
        assert!(status.lines().iter().any(|l| l.contains("Tag not recognised")));
        assert!(status.lines().iter().any(|l| l.contains("No local media")));
        assert!(status.lines().len() <= 2);
        assert!(status.height_px() <= 60);
    }

    #[test]
    fn unmatched_official_noubin_points_to_the_web_portal() {
        use plippa_ipc_proto::UnmatchedNoubin;
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(IpcMessage::UnmatchedNoubin(UnmatchedNoubin {
            noubin_key: "https://artist.example/x/\u{202E}evil".into(),
            normalised_key: "artist.example/x/".into(),
            seq: 1,
            candidate_urls: Vec::new(),
        }));
        let status = app.compositor.status().expect("status popup");
        assert!(status.lines().iter().any(|l| l.contains("web portal")));
        assert!(status.lines().iter().all(|l| !l.contains("artist.example") && !l.contains('\u{202E}')));
        assert!(status.lines().len() <= 2);
        assert!(status.height_px() <= 60);
    }

    #[test]
    fn store_tap_clears_the_miss_popup() {
        use plippa_ipc_proto::UnmatchedNoubin;
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(IpcMessage::UserNotice(UserNotice {
            severity: NoticeSeverity::Error,
            code: NOTICE_LIBRARY_MISS.into(),
            message: "No local media found for this Noubin".into(),
        }));
        assert!(app.compositor.status().is_some(), "miss popup first");
        app.handle_ipc(IpcMessage::UnmatchedNoubin(UnmatchedNoubin {
            noubin_key: "https://noubin.com/n/ori-moto/ep-01/E1O6E5KB/".into(),
            normalised_key: "noubin.com/n/ori-moto/ep-01/E1O6E5KB/".into(),
            seq: 2,
            candidate_urls: vec!["https://noubin.com/n/ori-moto/ep-01/E1O6E5KB/".into()],
        }));
        assert!(app.model.store_tap.is_some(), "store screen opens");
        assert!(
            app.compositor.status().is_none(),
            "the miss popup must not cover the URL-access prompt"
        );
    }

    #[test]
    fn a_uid_tag_taps_like_a_key_and_a_random_uid_does_not() {
        use plippa_hw::{NfcEvent, TagInfo, TagType};
        let info = TagInfo { uid: vec![1, 2, 3, 4], tag_type: TagType::NonNdef, ndef_capacity: None };
        let mut app = UiApp::new(UiConfig::default());
        app.handle_nfc_event(NfcEvent::UidTag { info: info.clone(), key: Ok("uid:01020304".into()) });
        let sent: Vec<_> = app.take_outgoing();
        assert!(sent.iter().any(|o| matches!(
            o,
            UiOutgoing::Ipc(IpcMessage::TapKey(k)) if k.noubin_key == "uid:01020304"
        )));
        assert!(sent.iter().any(|o| matches!(o, UiOutgoing::TapFeedback { .. })));

        let mut app = UiApp::new(UiConfig::default());
        app.handle_nfc_event(NfcEvent::UidTag {
            info,
            key: Err(plippa_noubin::UidKeyError::RandomUid),
        });
        assert!(app.take_outgoing().is_empty(), "a random UID never taps");
        assert!(app.compositor.status().is_some(), "and says why");
    }

    #[test]
    fn nfc_tap_recognised_and_unknown_status_popup() {
        use plippa_hw::{NfcEvent, TagInfo, TagType};
        use plippa_noubin::{encode_text_record, DEFAULT_TEXT_LANG};

        let mut app = UiApp::new(UiConfig::default());
        let info = TagInfo {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type4,
            ndef_capacity: Some(128),
        };

        // Unknown / empty — no URI/Text record.
        app.handle_nfc_event(NfcEvent::ReadComplete {
            ndef: vec![0x00, 0x01, 0x02],
            info: info.clone(),
        });
        let status = app.compositor.status().expect("unknown popup");
        assert!(status.lines()[0].contains("Tag not recognised"));
        assert_eq!(
            status.lines().len(),
            2,
            "the payload snippet is one line: {status:?}"
        );
        assert!(status.height_px() <= 60);
        let _ = app.take_outgoing();

        // Recognised noubin text record → Playing popup.
        let ndef = encode_text_record("demo-key-abc", DEFAULT_TEXT_LANG).expect("encode");
        app.handle_nfc_event(NfcEvent::ReadComplete { ndef, info });
        let status = app.compositor.status().expect("playing popup");
        assert!(
            status.lines()[0].contains("Playing"),
            "got {:?}",
            status.lines()
        );
        assert!(status.lines().len() <= 2);
        assert!(status.height_px() <= 60);
    }

    #[test]
    fn non_speed_popups_still_appear_on_now_playing() {
        // UI_14 §5 suppresses only speed status popups on Main. The error / NFC paths must
        // keep their popup, so a miss or an unknown tag is never silent on the home screen.
        use plippa_hw::{NfcEvent, TagInfo, TagType};

        let mut app = UiApp::new(UiConfig::default());
        assert_eq!(*app.nav.current(), Route::Main);

        // The NFC prompt path (unknown tag) still pops up.
        app.handle_nfc_event(NfcEvent::ReadComplete {
            ndef: vec![0x00, 0x01, 0x02],
            info: TagInfo {
                uid: vec![1, 2, 3],
                tag_type: TagType::Type4,
                ndef_capacity: Some(128),
            },
        });
        let status = app.compositor.status().expect("NFC popup on Main");
        assert!(status.lines()[0].contains("Tag not recognised"));

        // The speed family is the only thing suppressed here.
        app.compositor.clear_status();
        app.show_speed_status("1.50x");
        assert!(
            app.compositor.status().is_none(),
            "the suppression still covers the speed family on Main"
        );

        // An error notice (library miss) is not a speed popup and still appears.
        app.handle_ipc(IpcMessage::UserNotice(UserNotice {
            severity: NoticeSeverity::Error,
            code: NOTICE_LIBRARY_MISS.into(),
            message: "No local media for this Noubin".into(),
        }));
        let status = app.compositor.status().expect("error popup on Main");
        assert!(status.lines().iter().any(|l| l.contains("Tag not recognised")));

        // An unmatched official Noubin keeps its web-portal prompt too.
        app.compositor.clear_status();
        app.handle_ipc(IpcMessage::UnmatchedNoubin(plippa_ipc_proto::UnmatchedNoubin {
            noubin_key: "https://artist.example/x".into(),
            normalised_key: "artist.example/x/".into(),
            seq: 1,
            candidate_urls: Vec::new(),
        }));
        assert!(app.compositor.status().is_some(), "portal prompt on Main");
    }

    #[test]
    fn ndef_payload_snippet_is_one_popup_line() {
        assert_eq!(ndef_payload_snippet(b""), "(0 bytes)");
        assert_eq!(ndef_payload_snippet(b"abc"), "abc");
        // Newlines (a text record) cannot create a third popup line.
        assert_eq!(ndef_payload_snippet(b"a\nb"), "a·b");
        // A long payload is visibly shortened to one `Display` line: 28 glyphs + `..`.
        let long = vec![b'a'; 100];
        let snippet = ndef_payload_snippet(&long);
        assert!(!snippet.contains('\n'));
        assert_eq!(snippet.chars().count(), 30);
        assert!(snippet.ends_with(".."), "got {snippet:?}");
    }

    #[test]
    fn keyboard_mode_a_region_jump_restores_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assignable_a.json");
        let mut wifi = MockWifi::default();
        let mut app = UiApp::new(UiConfig {
            assign_path: Some(path),
            ..UiConfig::default()
        });
        app.assign.set_encoder_a(Some(AssignableId::volume()));
        app.init_wifi(&mut wifi);
        app.nav.force_push(Route::Wifi);
        // Current → Scan → Keyboard
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(app.model.wifi.as_ref().unwrap().in_keyboard());
        assert!(app.assign.keyboard_override_active());
        let before_region = app.model.wifi.as_ref().unwrap().keyboard.region();
        app.handle_input(InputEvent::AssignableARotate(1), &mut wifi);
        let after_region = app.model.wifi.as_ref().unwrap().keyboard.region();
        assert_ne!(before_region, after_region);
        // Volume must not change while keyboard owns A.
        assert_eq!(app.model.volume.percent(), Volume::DEFAULT_PERCENT);
        // Leave keyboard via Back.
        app.handle_input(InputEvent::Back, &mut wifi);
        assert!(!app.model.wifi.as_ref().unwrap().in_keyboard());
        assert!(!app.assign.keyboard_override_active());
        assert_eq!(
            app.assign.encoder_a().map(AssignableId::as_str),
            Some("volume")
        );
        app.handle_input(InputEvent::AssignableARotate(1), &mut wifi);
        assert_eq!(app.model.volume.percent(), Volume::DEFAULT_PERCENT + 2); // default step 2
    }

    #[test]
    fn volume_encoder_emits_ipc() {
        let mut app = UiApp::new(UiConfig {
            volume_step: 5,
            ..UiConfig::default()
        });
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::VolumeRotate(1), &mut wifi);
        let out = app.take_outgoing();
        assert!(matches!(
            out[0],
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(v)))
                if v.percent() == Volume::DEFAULT_PERCENT + 5
        ));
        let status = app.compositor.status().expect("volume status popup");
        assert_eq!(
            status.lines()[0],
            format!("volume {}%", Volume::DEFAULT_PERCENT + 5)
        );
    }

    #[test]
    fn now_playing_shows_title_artist_timecodes() {
        let mut app = UiApp::new(UiConfig::default());
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
        let mut session = SessionState::default();
        session.transport = TransportState::Playing;
        session.title = "Tone".into();
        session.artist = "Artist".into();
        session.position_ms = 65_000; // 1:05
        session.duration_ms = Some(180_000); // 3:00
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            session,
        )));
        app.present(&mut display);
        let fb = display.last.as_ref().unwrap();
        // Discriminate ink from the painted grounds (`present` clears to `BG`, so "non-zero"
        // is not a test). UI_14: row 0 is title (`ACCENT`) + artist (`FG_DIM`); the footer
        // `POSITION / LENGTH` clocks are `FG` `Display` at 75 fps.
        let ink = |y0: u16, y1: u16, colour: u16| {
            (y0 as usize..y1 as usize)
                .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
                .filter(|&(x, y)| fb.pixels[y * SCREEN_W as usize + x] == colour)
                .count()
        };
        let title = ink(0, 30, ACCENT);
        assert!(title > 20, "expected `ACCENT` title glyphs, got {title}");
        let artist = ink(0, 30, FG_DIM);
        assert!(artist > 20, "expected `FG_DIM` artist glyphs, got {artist}");
        let times = ink(NP_CONTENT_BOTTOM, SCREEN_H, FG);
        assert!(times > 20, "expected `FG` footer glyphs, got {times}");
        assert_eq!(app.model.footer_left(), "01:05:00 / 03:00:00");
        // No list: title and artist fall back to the session fields.
        assert_eq!(
            app.model.np_header_text(),
            (
                "Tone".to_string(),
                "Artist".to_string(),
                String::new(),
                String::new()
            )
        );
        // The fixed playhead is on screen under it, independent of position.
        assert_eq!(
            fb.pixels[(WAVE_BAND_Y + 5) as usize * SCREEN_W as usize + WAVE_PLAYHEAD_X as usize],
            FG
        );
        assert_eq!(
            app.model.playback.as_ref().map(|p| p.session.title.as_str()),
            Some("Tone")
        );
        assert_eq!(
            app.model
                .playback
                .as_ref()
                .map(|p| p.session.position_ms),
            Some(65_000)
        );
    }

    #[test]
    fn now_playing_list_message_is_kept_for_screens() {
        use plippa_core::{NowPlayingEntry, NowPlayingEntryKind, NowPlayingListKind};
        let mut app = UiApp::new(UiConfig::default());
        assert!(app.model.now_playing.is_none());
        let list = plippa_ipc_proto::NowPlayingListChanged {
            entries: vec![NowPlayingEntry {
                kind: NowPlayingEntryKind::Track,
                title: "One".into(),
                artist: String::new(),
                start_ms: 0,
                end_ms: None,
                duration_ms: Some(1_000),
                track_index: 0,
                playable_id: None,
                available: true,
                item_position: 0,
            }],
            current_index: 0,
            list_kind: NowPlayingListKind::Single,
            list_id: 1,
        };
        app.handle_ipc(IpcMessage::NowPlayingListChanged(list.clone()));
        assert_eq!(app.model.now_playing, Some(list));
    }

    #[test]
    fn menu_rotate_on_now_playing_sends_one_seek_to_after_the_idle_time() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let mut session = SessionState::default();
        session.transport = TransportState::Paused;
        session.position_ms = 10_000;
        session.duration_ms = Some(120_000);
        session.title = "Tone".into();
        session.current_item = Some(plippa_core::PlayableItemId::new(1));
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        assert_eq!(*app.nav.current(), Route::Main);
        app.handle_input(InputEvent::Tick { now_ms: 1_000 }, &mut wifi);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        // Nothing is sent while the knob turns.
        assert!(app.take_outgoing().is_empty());
        app.handle_input(InputEvent::Tick { now_ms: 1_000 + crate::warp_ui::JOG_COMMIT_IDLE_MS - 1 }, &mut wifi);
        assert!(app.take_outgoing().is_empty());
        app.handle_input(InputEvent::Tick { now_ms: 1_000 + crate::warp_ui::JOG_COMMIT_IDLE_MS }, &mut wifi);
        let out = app.take_outgoing();
        assert_eq!(out.len(), 1, "{out:?}");
        // One slow click = the finest step of the default Fine time: one frame at 75 fps,
        // `1000 / 75` rounded down (UI_16 §5 derives it; there is no Jog step setting).
        assert!(matches!(
            out[0],
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekTo { target_ms }))
                if target_ms == 10_013
        ), "{out:?}");
        // Backward jog clamps at 0 (swallowed clicks), still one command.
        for k in 0..3 {
            app.handle_input(InputEvent::Tick { now_ms: 2_000 + k * 10 }, &mut wifi);
            app.handle_input(InputEvent::MenuRotate(-30), &mut wifi);
        }
        app.handle_input(InputEvent::Tick { now_ms: 3_000 }, &mut wifi);
        let out = app.take_outgoing();
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(
            out[0],
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekTo { target_ms: 0 }))
        ), "{out:?}");
    }

    #[test]
    fn play_stop_nothing_loaded_shows_status() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();

        app.handle_input(InputEvent::PlayStop, &mut wifi);
        let out = app.take_outgoing();
        assert!(
            out.is_empty(),
            "empty session must not send Play (engine no-op), got {out:?}"
        );
        let status = app.compositor.status().expect("status popup");
        assert!(
            status.lines().iter().any(|l| l.contains("Nothing loaded")),
            "expected Nothing loaded popup, got {:?}",
            status.lines()
        );
    }

    #[test]
    fn prev_next_control_roles_map_to_commands_only_when_available() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let press = |app: &mut UiApp, wifi: &mut MockWifi, r| {
            app.handle_input(InputEvent::ControlPressed(r), wifi);
            (app.take_outgoing(), app.take_led_flashes())
        };
        // Nothing loaded / no status: no command, no flash, no error.
        let (out, fl) = press(&mut app, &mut wifi, ControlRole::PrevTrack);
        assert!(out.is_empty() && fl.is_empty());
        assert_eq!(app.nav_availability(), NavAvailability::default());

        let session = SessionState {
            current_item: Some(PlayableItemId::new(1)),
            can_prev: true,
            can_next: false,
            ..SessionState::default()
        };
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        let (out, fl) = press(&mut app, &mut wifi, ControlRole::NextTrack);
        assert!(out.is_empty() && fl.is_empty(), "Next unlit: nothing happens");
        let (out, fl) = press(&mut app, &mut wifi, ControlRole::PrevTrack);
        assert!(matches!(
            out[..],
            [UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Previous))]
        ));
        assert_eq!(fl, vec![LedRole::PrevTrackButton]);
    }

    #[test]
    fn play_stop_toggles_ipc() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();

        let mut session = SessionState::default();
        session.transport = TransportState::Stopped;
        session.current_item = Some(PlayableItemId::new(1));
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        app.handle_input(InputEvent::PlayStop, &mut wifi);
        let out = app.take_outgoing();
        assert!(matches!(
            out[0],
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play))
        ));

        let mut session = SessionState::default();
        session.transport = TransportState::Playing;
        session.current_item = Some(PlayableItemId::new(1));
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 2,
        )));
        app.handle_input(InputEvent::PlayStop, &mut wifi);
        let out = app.take_outgoing();
        assert!(matches!(
            out[0],
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause))
        ));
    }

    #[test]
    fn play_stop_resume_from_paused() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let mut session = SessionState::default();
        session.transport = TransportState::Paused;
        session.current_item = Some(PlayableItemId::new(1));
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        app.handle_input(InputEvent::PlayStop, &mut wifi);
        let out = app.take_outgoing();
        assert!(
            matches!(
                out[0],
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play))
            ),
            "paused → PlayStop must send Play, got {out:?}"
        );
        assert_eq!(
            app.model.playback.as_ref().map(|p| p.transport),
            Some(TransportState::Paused)
        );
    }

    #[test]
    fn older_status_revision_is_dropped() {
        let mut app = UiApp::new(UiConfig::default());
        let mut session = SessionState::default();
        session.transport = TransportState::Playing;
        session.current_item = Some(PlayableItemId::new(1));
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session.clone(),
            5,
        )));
        assert_eq!(
            app.model.playback.as_ref().map(|p| p.transport),
            Some(TransportState::Playing)
        );
        assert_eq!(
            app.play_button_led(),
            crate::led::PlayButtonLed::Playing
        );

        session.transport = TransportState::Paused;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session.clone(),
            6,
        )));
        assert_eq!(
            app.model.playback.as_ref().map(|p| p.transport),
            Some(TransportState::Paused)
        );
        assert_eq!(
            app.play_button_led(),
            crate::led::PlayButtonLed::Standby
        );

        session.transport = TransportState::Playing;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 5,
        )));
        assert_eq!(
            app.model.playback.as_ref().map(|p| p.transport),
            Some(TransportState::Paused),
            "older revision must not clobber newer Pause"
        );
        assert_eq!(
            app.play_button_led(),
            crate::led::PlayButtonLed::Standby,
            "LED must stay in sync with model"
        );

        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::PlayStop, &mut wifi);
        let out = app.take_outgoing();
        assert!(
            out.iter().any(|o| matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play))
            )),
            "expected Play after Paused model, got {out:?}"
        );
    }

    #[test]
    fn assignable_a_hold_assign_persist_reload_and_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assignable_a.json");
        let mut wifi = MockWifi::default();
        let mut app = UiApp::new(UiConfig {
            assign_path: Some(path.clone()),
            ..UiConfig::default()
        });
        app.nav.force_push(Route::Debug);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(app.controls.mode(), InteractionMode::Edit);
        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
        app.handle_input(InputEvent::AssignableEncoderPush { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 800 }, &mut wifi);
        assert_eq!(
            app.assign.encoder_a().map(AssignableId::as_str),
            Some("dummy_a")
        );
        let status = app.compositor.status().expect("assign status popup");
        assert!(
            status.lines()[0].starts_with("Assigned"),
            "got {:?}",
            status.lines()
        );

        // Restart model from the same file.
        let mut app2 = UiApp::new(UiConfig {
            assign_path: Some(path),
            ..UiConfig::default()
        });
        assert_eq!(
            app2.assign.encoder_a().map(AssignableId::as_str),
            Some("dummy_a")
        );
        let before = app2.controls.value_of("dummy_a").unwrap();
        app2.handle_input(InputEvent::AssignableARotate(1), &mut wifi);
        assert_eq!(app2.controls.value_of("dummy_a"), Some(before + 1));
    }

    #[test]
    fn assignable_short_click_does_not_assign() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assignable_a.json");
        let mut wifi = MockWifi::default();
        let mut app = UiApp::new(UiConfig {
            assign_path: Some(path),
            ..UiConfig::default()
        });
        app.nav.force_push(Route::Debug);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
        app.handle_input(InputEvent::AssignableEncoderPush { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 400 }, &mut wifi);
        app.handle_input(InputEvent::AssignableEncoderPush { down: false }, &mut wifi);
        assert!(app.assign.encoder_a().is_none());
        assert!(app.take_hold_events().is_empty());
    }

    #[test]
    fn unbound_assignable_a_rotate_is_noop() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let vol_before = app.model.volume.percent();
        let dummy_before = app.controls.value_of("dummy_a").unwrap();
        app.handle_input(InputEvent::AssignableARotate(1), &mut wifi);
        assert_eq!(app.model.volume.percent(), vol_before);
        assert_eq!(app.controls.value_of("dummy_a"), Some(dummy_before));
        assert!(app.take_outgoing().is_empty());
    }

    #[test]
    fn assignable_a_volume_binding_applies_rotate() {
        let mut app = UiApp::new(UiConfig {
            volume_step: 5,
            ..UiConfig::default()
        });
        let mut wifi = MockWifi::default();
        app.assign.set_encoder_a(Some(AssignableId::volume()));
        app.handle_input(InputEvent::AssignableARotate(1), &mut wifi);
        assert_eq!(app.model.volume.percent(), Volume::DEFAULT_PERCENT + 5);
        let out = app.take_outgoing();
        assert!(matches!(
            out[0],
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(_)))
        ));
    }

    // ---- NB_12 ----

    fn np_entry(kind: NowPlayingEntryKind, title: &str, track_index: u32, start_ms: u64) -> NowPlayingEntry {
        NowPlayingEntry {
            kind,
            title: title.into(),
            artist: String::new(),
            start_ms,
            end_ms: None,
            duration_ms: None,
            track_index,
            playable_id: None,
            available: true,
            item_position: 0,
        }
    }

    fn list_msg(entries: Vec<NowPlayingEntry>, current_index: u32) -> IpcMessage {
        list_msg_with_id(entries, current_index, 1)
    }

    fn list_msg_with_id(
        entries: Vec<NowPlayingEntry>,
        current_index: u32,
        list_id: u64,
    ) -> IpcMessage {
        IpcMessage::NowPlayingListChanged(plippa_ipc_proto::NowPlayingListChanged {
            entries,
            current_index,
            list_kind: plippa_core::NowPlayingListKind::Album,
            list_id,
        })
    }

    fn status_with_item(item: PlayableItemId, transport: TransportState, rev: u64) -> IpcMessage {
        let mut session = plippa_core::SessionState::default();
        session.current_item = Some(item);
        session.transport = transport;
        let mut st = plippa_ipc_proto::PlaybackStatus::from_session(session);
        st.revision = rev;
        IpcMessage::PlaybackStatus(st)
    }

    fn jumps(out: Vec<UiOutgoing>) -> Vec<PlaybackCommand> {
        out.into_iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn app_on_tracks(root: &std::path::Path) -> UiApp {
        let mut app = UiApp::new(UiConfig::default());
        app.set_library(vec![LibraryItem {
            id: 4,
            title: "Album".into(),
            associated: false,
            noudata_relpath: "_a.noudata".into(),
            library_root: Some(root.to_path_buf()),
            artist: None,
            duration_ms: None,
            art_path: None,
            item_position: None,
        }]);
        app.model.detail_playable_id = Some(4);
        app
    }

    #[test]
    fn play_from_waits_for_the_target_list_then_jumps_and_plays() {
        let root = PathBuf::from("/tmp/nb12-root");
        let target = PlayableItemId::new(encode_playable_id(&root, 4));
        let mut app = app_on_tracks(&root);
        app.play_from(1, "Two".into(), None);
        assert_eq!(
            jumps(app.take_outgoing()),
            vec![PlaybackCommand::LoadCollection { playable_id: target }]
        );
        let entries = || {
            vec![
                np_entry(NowPlayingEntryKind::Track, "One", 0, 0),
                np_entry(NowPlayingEntryKind::Track, "Two", 1, 0),
            ]
        };
        // Old collection's list arrives first: nothing to send.
        app.handle_ipc(list_msg(entries(), 0));
        assert!(jumps(app.take_outgoing()).is_empty());
        // Engine now reports the target as loaded (Stopped -> the jump is followed by Play).
        app.handle_ipc(status_with_item(target, TransportState::Stopped, 1));
        assert_eq!(
            jumps(app.take_outgoing()),
            vec![PlaybackCommand::JumpToEntry { index: 1 }, PlaybackCommand::Play]
        );
        // One-shot: a further list does not jump again.
        app.handle_ipc(list_msg(entries(), 1));
        assert!(jumps(app.take_outgoing()).is_empty());
    }

    #[test]
    fn play_from_on_the_loaded_collection_jumps_without_reloading() {
        let root = PathBuf::from("/tmp/nb12-root");
        let target = PlayableItemId::new(encode_playable_id(&root, 4));
        let mut app = app_on_tracks(&root);
        app.handle_ipc(status_with_item(target, TransportState::Playing, 1));
        app.handle_ipc(list_msg(
            vec![
                np_entry(NowPlayingEntryKind::Track, "One", 0, 0),
                np_entry(NowPlayingEntryKind::Cue, "c1", 0, 0),
                np_entry(NowPlayingEntryKind::Cue, "c2", 0, 30_000),
                np_entry(NowPlayingEntryKind::Track, "Two", 1, 0),
            ],
            0,
        ));
        // "Two" sits behind the current track's cue rows: flat index 3, not 1.
        app.play_from(1, "Two".into(), None);
        assert_eq!(
            jumps(app.take_outgoing()),
            vec![PlaybackCommand::JumpToEntry { index: 3 }]
        );
        // A chapter of the current track is its cue row.
        app.play_from(0, "One".into(), Some(30_000));
        assert_eq!(
            jumps(app.take_outgoing()),
            vec![PlaybackCommand::JumpToEntry { index: 2 }]
        );
    }

    #[test]
    fn chapter_of_another_track_takes_a_track_jump_then_a_cue_jump() {
        let root = PathBuf::from("/tmp/nb12-root");
        let target = PlayableItemId::new(encode_playable_id(&root, 4));
        let mut app = app_on_tracks(&root);
        app.handle_ipc(status_with_item(target, TransportState::Playing, 1));
        let two = || np_entry(NowPlayingEntryKind::Track, "Two", 1, 0);
        app.handle_ipc(list_msg(
            vec![np_entry(NowPlayingEntryKind::Track, "One", 0, 0), two()],
            0,
        ));
        app.play_from(1, "Two".into(), Some(10_000));
        assert_eq!(
            jumps(app.take_outgoing()),
            vec![PlaybackCommand::JumpToEntry { index: 1 }]
        );
        // Same list again (engine has not moved yet): no second jump.
        app.handle_ipc(list_msg(
            vec![np_entry(NowPlayingEntryKind::Track, "One", 0, 0), two()],
            0,
        ));
        assert!(jumps(app.take_outgoing()).is_empty());
        // Engine moved: track "Two" is current and now carries cue rows.
        app.handle_ipc(list_msg(
            vec![
                np_entry(NowPlayingEntryKind::Track, "One", 0, 0),
                two(),
                np_entry(NowPlayingEntryKind::Cue, "a", 1, 0),
                np_entry(NowPlayingEntryKind::Cue, "b", 1, 10_000),
            ],
            1,
        ));
        assert_eq!(
            jumps(app.take_outgoing()),
            vec![PlaybackCommand::JumpToEntry { index: 3 }]
        );
    }

    #[test]
    fn pending_jump_is_dropped_when_the_row_never_shows_up() {
        let root = PathBuf::from("/tmp/nb12-root");
        let target = PlayableItemId::new(encode_playable_id(&root, 4));
        let mut app = app_on_tracks(&root);
        app.handle_ipc(status_with_item(target, TransportState::Playing, 1));
        app.play_from(7, "Nope".into(), None);
        for _ in 0..PENDING_JUMP_MAX_TRIES + 2 {
            app.handle_ipc(list_msg(vec![np_entry(NowPlayingEntryKind::Track, "One", 0, 0)], 0));
        }
        assert!(app.pending_jump.is_none());
        // Title miss falls back to position (e.g. the engine renamed an untitled item).
        app.play_from(0, "Other name".into(), None);
        assert!(!jumps(app.take_outgoing()).is_empty());
    }

    #[test]
    fn detail_play_uses_load_collection_for_collections_and_load_and_play_for_tracks() {
        let root = PathBuf::from("/tmp/nb12-root");
        let mut app = app_on_tracks(&root);
        let mut wifi = MockWifi::default();
        app.nav.force_push(Route::LibraryDetail);
        app.model.browse_kind = Some(LibraryKind::Album);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(matches!(
            jumps(app.take_outgoing()).as_slice(),
            [PlaybackCommand::LoadCollection { .. }]
        ));
        app.model.browse_kind = Some(LibraryKind::Track);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(matches!(
            jumps(app.take_outgoing()).as_slice(),
            [PlaybackCommand::LoadAndPlay(_)]
        ));
        // The optimistic-LED hook sees both.
        app.model.browse_kind = Some(LibraryKind::Mix);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(app.pending_load_and_play());
    }

    #[test]
    fn entering_category_and_detail_queue_lookups_without_touching_the_index() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.model.kind_counts = vec![(LibraryKind::Album, 1)];
        app.nav.force_push(Route::Library);
        app.nav.focus = 1; // Albums
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(*app.nav.current(), Route::LibraryCategory);
        assert_eq!(app.model.browse_kind, Some(LibraryKind::Album));
        assert_eq!(
            app.take_browse_requests(),
            vec![BrowseRequest::Category(LibraryKind::Album)]
        );
        // Focus a listed item and open it.
        app.set_library(vec![LibraryItem {
            id: 9,
            title: "X".into(),
            associated: false,
            noudata_relpath: "x".into(),
            library_root: None,
            artist: None,
            duration_ms: None,
            art_path: None,
            item_position: None,
        }]);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(matches!(
            app.take_browse_requests().as_slice(),
            [BrowseRequest::Detail { kind: Some(LibraryKind::Album), .. }]
        ));
        // Back to the root asks for fresh counts and forgets the category.
        app.handle_input(InputEvent::Back, &mut wifi);
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(*app.nav.current(), Route::Library);
        assert_eq!(app.model.browse_kind, None);
        assert_eq!(app.take_browse_requests(), vec![BrowseRequest::Counts]);
    }

    #[test]
    fn hostile_now_playing_entries_and_popups_are_sanitised() {
        let mut app = UiApp::new(UiConfig::default());
        let evil = "Ev\u{202E}il\u{200B}\u{0007}\nx";
        let mut e = np_entry(NowPlayingEntryKind::Cue, evil, 0, 0);
        e.artist = evil.into();
        app.handle_ipc(list_msg(vec![e], 0));
        let got = &app.model.now_playing.as_ref().unwrap().entries[0];
        assert_eq!(got.title, "Evilx");
        assert_eq!(got.artist, "Evilx");
        // Status popups quote tag keys / titles: the popup lines carry no invisible characters.
        app.compositor
            .show_status("Playing …\nEv\u{202E}il\u{200B}\u{0007}x\u{202E}", 0);
        let lines = app.compositor.status().unwrap().lines().to_vec();
        assert_eq!(lines, vec!["Playing …".to_string(), "Evilx".to_string()]);
    }

    // ---- NB_15 ----

    /// Album list of `n` tracks with `cur` current, plus a status carrying `cue` (row shift).
    fn np_album(app: &mut UiApp, n: u32, cur: u32, transport: TransportState, rev: u64) {
        let entries = (0..n)
            .map(|i| np_entry(NowPlayingEntryKind::Track, &format!("T{}", i + 1), i, 0))
            .collect();
        app.handle_ipc(list_msg(entries, cur));
        let mut session = SessionState::default();
        session.transport = transport;
        session.current_item = Some(PlayableItemId::new(1));
        session.current_entry_index = cur;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, rev,
        )));
    }

    fn cmds(app: &mut UiApp) -> Vec<PlaybackCommand> {
        jumps(app.take_outgoing())
    }

    #[test]
    fn single_track_keeps_scrub_on_rotate_and_shows_no_panel() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.handle_ipc(list_msg(
            vec![np_entry(NowPlayingEntryKind::Track, "One", 0, 0)],
            0,
        ));
        assert!(!app.model.list_panel_visible());
        // Nothing loaded: the jog does nothing.
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 1_000 }, &mut wifi);
        assert!(cmds(&mut app).is_empty());
        let mut session = SessionState::default();
        session.current_item = Some(PlayableItemId::new(1));
        session.duration_ms = Some(100_000);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 5,
        )));
        app.handle_input(InputEvent::Tick { now_ms: 2_000 }, &mut wifi);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(cmds(&mut app).is_empty(), "nothing is sent while the knob turns");
        app.handle_input(InputEvent::Tick { now_ms: 2_200 }, &mut wifi);
        assert!(matches!(cmds(&mut app).as_slice(), [PlaybackCommand::SeekTo { .. }]));
        // Push is a no-op on home.
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(cmds(&mut app).is_empty());
    }

    #[test]
    fn collection_rotate_opens_list_and_scrub_needs_jog() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 5, 0, TransportState::Playing, 1);
        assert!(app.model.list_panel_visible());
        // UI_13: one turn either way opens the overlay on the playing row; it never scrubs.
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(cmds(&mut app).is_empty());
        assert_eq!(app.model.np_focus.mode, NpMode::List);
        assert_eq!(app.model.np_focus.target, NpTarget::List);
        assert_eq!(app.model.np_focus.cursor, 1, "playing row + one");
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.model.np_focus.cursor, 0);
        // Back dismisses the overlay and does not open the main menu.
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(app.model.np_focus.mode, NpMode::Select);
        assert!(!app.compositor.main_menu_visible());
        // Push on current time -> jog: rotate now scrubs.
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(app.model.np_focus.mode, NpMode::Adjust(AdjustParam::Jog));
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(cmds(&mut app).is_empty(), "nothing is sent while the knob turns");
        // WARP_07: push cycles the ring (Jog -> Tempo) and commits the turn at once (WARP_15);
        // Back returns to Select.
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(matches!(cmds(&mut app).as_slice(), [PlaybackCommand::SeekTo { .. }]));
        assert_eq!(app.model.np_focus.mode, NpMode::Adjust(AdjustParam::Tempo));
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(app.model.np_focus.mode, NpMode::Select);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(cmds(&mut app).is_empty());
        assert_eq!(app.model.np_focus.mode, NpMode::List, "a turn opens the list again");
    }

    #[test]
    fn jump_closes_overlay() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 6, 1, TransportState::Playing, 1);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi); // open on row 1, move to 2
        assert_eq!((app.model.np_focus.mode, app.model.np_focus.cursor), (NpMode::List, 2));
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.np_focus.cursor, 3);
        assert!(cmds(&mut app).is_empty(), "browsing sends nothing");
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(cmds(&mut app), vec![PlaybackCommand::JumpToEntry { index: 3 }]);
        // Owner decision 4: the overlay closes and returns to the waveform.
        assert_eq!(app.model.np_focus.mode, NpMode::Select);
        assert!(!app.model.up_next_visible());
        // Back from the waveform opens the main menu (no extra level to unwind).
        app.handle_input(InputEvent::Back, &mut wifi);
        assert!(app.compositor.main_menu_visible());
    }

    #[test]
    fn back_closes_overlay_and_is_consumed() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 4, 0, TransportState::Playing, 1);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(app.model.up_next_visible());
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(*app.nav.current(), Route::Main, "Back stays on Now Playing");
        assert_eq!(app.model.np_focus.mode, NpMode::Select);
        assert!(!app.model.up_next_visible());
        assert_eq!(app.list_overlay_until_ms, None);
        assert!(!app.compositor.main_menu_visible(), "the press is consumed");
    }

    #[test]
    fn list_overlay_times_out_after_four_seconds() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 4, 0, TransportState::Playing, 1);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.list_overlay_until_ms, Some(4_000));
        app.handle_input(InputEvent::Tick { now_ms: 3_999 }, &mut wifi);
        assert!(app.model.up_next_visible(), "still open at 3 999 ms");
        app.needs_redraw = false;
        app.handle_input(InputEvent::Tick { now_ms: 4_000 }, &mut wifi);
        assert!(!app.model.up_next_visible(), "closed at 4 000 ms");
        assert_eq!(app.model.np_focus.mode, NpMode::Select);
        assert_eq!(app.list_overlay_until_ms, None);
        assert!(app.needs_redraw, "closing the overlay requests a redraw");
    }

    #[test]
    fn list_overlay_scroll_refreshes_timeout() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 5, 0, TransportState::Playing, 1);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 3_900 }, &mut wifi);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.list_overlay_until_ms, Some(7_900));
        app.handle_input(InputEvent::Tick { now_ms: 7_899 }, &mut wifi);
        assert!(app.model.up_next_visible());
        app.handle_input(InputEvent::Tick { now_ms: 7_900 }, &mut wifi);
        assert!(!app.model.up_next_visible());
        assert_eq!(app.list_overlay_until_ms, None);
    }

    /// Next / Previous reveal the overlay for four seconds, seeded on the playing row.
    #[test]
    fn next_prev_open_overlay_and_follow_the_playing_row() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let entries: Vec<NowPlayingEntry> = (0..4)
            .map(|i| np_entry(NowPlayingEntryKind::Track, &format!("T{}", i + 1), i, 0))
            .collect();
        app.handle_ipc(list_msg(entries.clone(), 0));
        let mut session = SessionState::default();
        session.current_item = Some(PlayableItemId::new(1));
        session.current_entry_index = 0;
        session.can_next = true;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        assert!(matches!(
            app.take_outgoing().as_slice(),
            [UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Next))]
        ));
        assert_eq!(app.list_overlay_until_ms, Some(4_000));
        assert!(app.model.up_next_visible());
        assert_eq!(app.model.np_focus.cursor, 0);
        // The engine moves on; while unscrolled the cursor follows the new playing row.
        app.handle_ipc(list_msg(entries, 2));
        let mut session = SessionState::default();
        session.current_item = Some(PlayableItemId::new(1));
        session.current_entry_index = 2;
        session.can_next = true;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 2,
        )));
        assert!(app.model.up_next_visible());
        assert_eq!(app.model.np_focus.cursor, 2);
    }

    #[test]
    fn unavailable_next_does_not_open_overlay() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 4, 0, TransportState::Playing, 1);
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        assert!(app.take_outgoing().is_empty(), "no command");
        assert!(app.take_led_flashes().is_empty(), "no flash");
        assert!(!app.model.up_next_visible());
        assert_eq!(app.list_overlay_until_ms, None);
    }

    // ---- UI_CONTROL_01: the encoder highlight follows the physical Next / Previous ----

    fn np_entries(n: u32) -> Vec<NowPlayingEntry> {
        (0..n)
            .map(|i| np_entry(NowPlayingEntryKind::Track, &format!("T{}", i + 1), i, 0))
            .collect()
    }

    fn np_status_at(app: &mut UiApp, cur: u32, rev: u64) {
        let mut session = SessionState::default();
        session.current_item = Some(PlayableItemId::new(1));
        session.current_entry_index = cur;
        session.can_next = true;
        session.can_prev = true;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, rev,
        )));
    }

    /// A four-row list playing row 1, with the overlay opened and scrolled to row 3.
    fn scrolled_overlay(app: &mut UiApp, wifi: &mut MockWifi) {
        app.handle_ipc(list_msg(np_entries(4), 1));
        np_status_at(app, 1, 1);
        app.handle_input(InputEvent::MenuRotate(1), wifi);
        app.handle_input(InputEvent::MenuRotate(1), wifi);
        app.handle_input(InputEvent::MenuRotate(1), wifi);
        assert_eq!(app.model.np_focus.mode, NpMode::List);
        assert_eq!(app.model.np_focus.cursor, 3, "scrolled away from the playing row");
        assert!(app.list_overlay_scrolled);
        app.take_outgoing();
    }

    #[test]
    fn next_moves_the_scrolled_cursor_to_the_new_row_list_then_status() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        scrolled_overlay(&mut app, &mut wifi);
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        assert!(matches!(
            app.take_outgoing().as_slice(),
            [UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Next))]
        ));
        assert!(!app.list_overlay_scrolled, "a physical press resets the scroll");
        app.handle_ipc(list_msg(np_entries(4), 2));
        np_status_at(&mut app, 2, 2);
        assert_eq!(app.model.now_playing_row(), Some(2));
        assert_eq!(app.model.np_focus.cursor, 2);
        assert!(!app.np_follow_pending);
    }

    #[test]
    fn next_moves_the_scrolled_cursor_to_the_new_row_status_then_list() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        scrolled_overlay(&mut app, &mut wifi);
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        app.take_outgoing();
        np_status_at(&mut app, 2, 2);
        app.handle_ipc(list_msg(np_entries(4), 2));
        assert_eq!(app.model.np_focus.cursor, 2);
    }

    #[test]
    fn previous_that_restarts_leaves_the_highlight_on_the_playing_row() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        scrolled_overlay(&mut app, &mut wifi);
        app.handle_input(InputEvent::ControlPressed(ControlRole::PrevTrack), &mut wifi);
        // The engine restarts the entry: same row, a fresh status only.
        np_status_at(&mut app, 1, 2);
        assert_eq!(app.model.np_focus.cursor, 1);
    }

    #[test]
    fn a_later_auto_advance_does_not_yank_a_cursor_the_person_scrolled() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        scrolled_overlay(&mut app, &mut wifi);
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        app.handle_ipc(list_msg(np_entries(4), 2));
        assert_eq!(app.model.np_focus.cursor, 2);
        // The person scrolls on: their cursor wins over the track that ends later.
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.np_focus.cursor, 3);
        assert!(!app.np_follow_pending);
        app.handle_ipc(list_msg(np_entries(4), 0));
        np_status_at(&mut app, 0, 3);
        assert_eq!(app.model.np_focus.cursor, 3);
    }

    #[test]
    fn next_while_adjusting_tempo_leaves_the_encoder_alone() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.handle_ipc(list_msg(np_entries(4), 1));
        np_status_at(&mut app, 1, 1);
        app.model.np_focus.mode = NpMode::Adjust(AdjustParam::Tempo);
        let before = app.model.np_focus.clone();
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        assert!(matches!(
            app.take_outgoing().as_slice(),
            [UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Next))]
        ));
        assert_eq!(app.model.np_focus, before);
        assert_eq!(app.list_overlay_until_ms, None);
        assert!(!app.np_follow_pending);
        app.handle_ipc(list_msg(np_entries(4), 2));
        np_status_at(&mut app, 2, 2);
        assert_eq!(app.model.np_focus, before, "the answer does not move it either");
    }

    #[test]
    fn next_on_another_route_leaves_np_focus_alone() {
        for route in [Route::Library, Route::System] {
            let mut app = UiApp::new(UiConfig::default());
            let mut wifi = MockWifi::default();
            app.handle_ipc(list_msg(np_entries(4), 1));
            np_status_at(&mut app, 1, 1);
            app.nav.force_push(route.clone());
            let before = app.model.np_focus.clone();
            app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
            assert_eq!(app.model.np_focus, before, "{route:?}");
            assert_eq!(app.list_overlay_until_ms, None, "{route:?}");
            assert!(!app.np_follow_pending, "{route:?}");
        }
    }

    #[test]
    fn unavailable_next_changes_nothing_at_all() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 4, 3, TransportState::Playing, 1);
        let before = app.model.np_focus.clone();
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        assert!(app.take_outgoing().is_empty());
        assert!(app.take_led_flashes().is_empty());
        assert_eq!(app.model.np_focus, before);
        assert_eq!(app.list_overlay_until_ms, None);
        assert!(!app.np_follow_pending);
    }

    #[test]
    fn accessory_transport_next_follows_like_the_hardware_button() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        scrolled_overlay(&mut app, &mut wifi);
        app.apply_accessory_event(AccessoryEvent::Transport(TransportAction::Next), 100);
        assert!(matches!(
            app.take_outgoing().as_slice(),
            [UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Next))]
        ));
        app.handle_ipc(list_msg(np_entries(4), 2));
        np_status_at(&mut app, 2, 2);
        assert_eq!(app.model.np_focus.cursor, 2);
        let _ = &mut wifi;
    }

    #[test]
    fn a_stale_follow_flag_is_cleared_with_the_overlay() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        scrolled_overlay(&mut app, &mut wifi);
        app.handle_input(InputEvent::ControlPressed(ControlRole::PrevTrack), &mut wifi);
        assert!(app.np_follow_pending);
        app.handle_input(InputEvent::Tick { now_ms: LIST_OVERLAY_HOLD_MS + 1 }, &mut wifi);
        assert_eq!(app.list_overlay_until_ms, None);
        assert!(!app.np_follow_pending);
    }

    /// Enter while the overlay was auto-opened (Next) and not scrolled closes it without a
    /// `JumpToEntry`: the cursor sits on the row that is already playing.
    #[test]
    fn enter_while_auto_opened_does_not_jump() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let entries: Vec<NowPlayingEntry> = (0..4)
            .map(|i| np_entry(NowPlayingEntryKind::Track, &format!("T{}", i + 1), i, 0))
            .collect();
        app.handle_ipc(list_msg(entries, 0));
        let mut session = SessionState::default();
        session.current_item = Some(PlayableItemId::new(1));
        session.current_entry_index = 0;
        session.can_next = true;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 1,
        )));
        app.handle_input(InputEvent::ControlPressed(ControlRole::NextTrack), &mut wifi);
        app.take_outgoing();
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert!(
            !app.take_outgoing()
                .iter()
                .any(|o| matches!(o, UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::JumpToEntry { .. })))),
            "an unscrolled auto-opened overlay must not jump"
        );
        assert_eq!(app.model.np_focus.mode, NpMode::Select);
        assert!(!app.model.up_next_visible());
        assert_eq!(app.list_overlay_until_ms, None);
    }

    #[test]
    fn zoom_setting_does_not_move_the_session_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("play_mode.json");
        let cfg = UiConfig {
            play_mode_path: Some(path.clone()),
            ..UiConfig::default()
        };
        let mut app = UiApp::new(cfg);
        assert_eq!(app.model.wave_window_ms, crate::play_mode::ZOOM_DEFAULT_MS);
        app.nav.push(Route::SystemPlayMode);
        app.nav.focus = PLAYBACK_SETTINGS_ROWS
            .iter()
            .position(|r| r.setting() == Some(PlayModeRow::WaveformZoom))
            .unwrap();
        // UI_16 §1: Enter enters editing without changing the value; a rotation steps the
        // stored default along the printed ladder (right = up = a shorter window).
        app.handle_input(InputEvent::MenuEnter, &mut MockWifi::default());
        assert_eq!(app.play_mode.settings.waveform_zoom_ms, None);
        app.handle_input(InputEvent::MenuRotate(1), &mut MockWifi::default());
        let next = crate::play_mode::zoom_step(crate::play_mode::ZOOM_DEFAULT_MS, -1);
        assert_ne!(next, crate::play_mode::ZOOM_DEFAULT_MS);
        assert_eq!(app.play_mode.settings.waveform_zoom_ms, Some(next));
        // Persist and report on exit.
        app.handle_input(InputEvent::MenuEnter, &mut MockWifi::default());
        assert_eq!(
            app.model.wave_window_ms,
            crate::play_mode::ZOOM_DEFAULT_MS,
            "the live session zoom does not move with the startup default"
        );
        let text = app.compositor.status().unwrap().lines().join(" ");
        assert_eq!(
            text,
            format!("Default zoom: {}", crate::play_mode::zoom_text(next))
        );
        // A restart re-reads the persisted default.
        drop(app);
        let restarted = UiApp::new(UiConfig {
            play_mode_path: Some(path),
            ..UiConfig::default()
        });
        assert_eq!(restarted.model.wave_window_ms, next);
    }

    #[test]
    fn zoom_ring_moves_the_session_window_and_nothing_else() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 3, 0, TransportState::Playing, 1);
        // Select -> Adjust(Jog) -> Adjust(Tempo) -> Adjust(Zoom).
        for _ in 0..3 {
            app.handle_input(InputEvent::MenuEnter, &mut wifi);
        }
        assert_eq!(app.model.np_focus.mode, NpMode::Adjust(AdjustParam::Zoom));
        let persisted = app.play_mode.settings.waveform_zoom_ms;
        let _ = app.take_outgoing();

        // One detent is exactly one ladder step, in the turn's direction.
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.wave_window_ms, 40_000);
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.model.wave_window_ms, 30_000);
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.model.wave_window_ms, 20_000);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.wave_window_ms, crate::play_mode::ZOOM_DEFAULT_MS);

        // 19 further detents reach and then clamp at the 600 s ladder top (no wrap).
        for _ in 0..19 {
            app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        }
        assert_eq!(app.model.wave_window_ms, 600_000);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.wave_window_ms, 600_000, "clamped at the top");

        // Reversing from the bottom: 19 detents reach 1 s, and one more stays there.
        for _ in 0..19 {
            app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        }
        assert_eq!(app.model.wave_window_ms, 1_000);
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.model.wave_window_ms, 1_000, "clamped at the bottom");

        // Session-only: the persisted default is untouched and the engine hears nothing.
        assert_eq!(app.play_mode.settings.waveform_zoom_ms, persisted);
        assert!(persisted.is_none(), "nothing was persisted before the turns");
        assert!(
            app.take_outgoing().is_empty(),
            "a ZOOM turn must not queue any outgoing IPC"
        );
    }

    #[test]
    fn back_leaves_jog_before_the_menu_opens() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 3, 0, TransportState::Playing, 1);
        app.handle_input(InputEvent::MenuEnter, &mut wifi);
        assert_eq!(app.model.np_focus.mode, NpMode::Adjust(AdjustParam::Jog));
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(app.model.np_focus.mode, NpMode::Select);
        assert!(!app.compositor.main_menu_visible());
    }

    #[test]
    fn playing_row_follows_engine_cue_index_not_a_ui_clock() {
        let mut app = UiApp::new(UiConfig::default());
        let entries = vec![
            np_entry(NowPlayingEntryKind::Track, "Mix", 0, 0),
            np_entry(NowPlayingEntryKind::Cue, "A", 0, 0),
            np_entry(NowPlayingEntryKind::Cue, "B", 0, 10_000),
            np_entry(NowPlayingEntryKind::Cue, "C", 0, 20_000),
        ];
        app.handle_ipc(list_msg(entries, 0));
        assert_eq!(app.model.now_playing_row(), Some(0), "no cue reported yet");
        for (rev, (cue, row)) in [(Some(0u32), 1usize), (Some(2), 3), (None, 0)]
            .into_iter()
            .enumerate()
        {
            let mut session = SessionState::default();
            session.current_cue_index = cue;
            app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
                session,
                rev as u64 + 1,
            )));
            assert_eq!(app.model.now_playing_row(), Some(row));
        }
        // A stale cue index that points past the list falls back to the track row.
        let mut session = SessionState::default();
        session.current_cue_index = Some(9);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            session, 10,
        )));
        assert_eq!(app.model.now_playing_row(), Some(0));
    }

    #[test]
    fn replaced_list_resets_cursor_but_track_change_keeps_it() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 6, 0, TransportState::Playing, 1);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        for _ in 0..4 {
            app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        }
        assert_eq!(app.model.np_focus.cursor, 5);
        // Same collection, current track moved (rows identical): the scrolled cursor stays.
        let entries = (0..6)
            .map(|i| np_entry(NowPlayingEntryKind::Track, &format!("T{}", i + 1), i, 0))
            .collect();
        app.handle_ipc(list_msg(entries, 2));
        assert_eq!(app.model.np_focus.cursor, 5);
        // A different collection replaces the list: focus resets to the waveform and the
        // cursor is seeded on the playing row for the next push.
        let other = (0..3)
            .map(|i| np_entry(NowPlayingEntryKind::Track, &format!("X{}", i + 1), i, 0))
            .collect();
        app.handle_ipc(list_msg_with_id(other, 1, 2));
        assert_eq!(app.model.np_focus.cursor, 1);
        assert_eq!(
            (app.model.np_focus.mode, app.model.np_focus.target),
            (NpMode::Select, NpTarget::CurrentTime),
            "a new collection returns to the waveform"
        );
        // Shrinking to a single track drops back to plain Now Playing.
        app.handle_ipc(list_msg(
            vec![np_entry(NowPlayingEntryKind::Track, "Solo", 0, 0)],
            0,
        ));
        assert_eq!(app.model.np_focus, NowPlayingFocus::default());
    }

    /// WAVE_02: the list is an overlay reached by navigation, not a permanent bottom panel.
    #[test]
    fn up_next_overlay_appears_only_when_navigated_to() {
        let mut wifi = MockWifi::default();
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(list_msg(
            vec![np_entry(NowPlayingEntryKind::Track, "One", 0, 0)],
            0,
        ));
        // Single track: rotate has no list target, so nothing covers the waveform.
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.np_focus.target, NpTarget::CurrentTime);
        assert!(!app.model.up_next_visible());

        np_album(&mut app, 5, 0, TransportState::Playing, 1);
        assert!(!app.model.up_next_visible(), "the waveform fills the screen");
        assert_eq!(overlay_marks(&mut app), (0, 0));
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(app.model.up_next_visible(), "one turn either way reveals Up Next");
        // The cursor opens on the playing row and moves with the turn, so the ring and the
        // playing marker are on different rows here.
        assert_eq!(app.model.np_focus.cursor, 1);
        let (accent, accent2) = overlay_marks(&mut app);
        assert!(accent > 1_000, "cursor row paints its focus ring: {accent}");
        assert!(accent2 > 0, "the playing row carries its marker");
        // Further turns move the ring, never send a command.
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.np_focus.mode, NpMode::List);
        assert_eq!(app.model.np_focus.cursor, 2);
        let (accent, _) = overlay_marks(&mut app);
        assert!(accent > 1_000, "cursor row paints its focus ring: {accent}");
        // Back returns to the waveform and hides the overlay in one press.
        app.handle_input(InputEvent::Back, &mut wifi);
        assert_eq!(app.model.np_focus.target, NpTarget::CurrentTime);
        assert!(!app.model.up_next_visible());
        assert!(!app.compositor.main_menu_visible(), "Back dismisses, never opens the menu");
    }

    /// (cursor-ring `ACCENT` pixels, playing-marker `ACCENT_2` pixels) inside the row band.
    fn overlay_marks(app: &mut UiApp) -> (usize, usize) {
        let fb = app.composed_pixels();
        let start = UP_NEXT_ROWS_Y as usize * SCREEN_W as usize;
        let end = (UP_NEXT_ROWS_Y + ROW_H * UP_NEXT_ROWS) as usize * SCREEN_W as usize;
        let mut ring = 0;
        let mut marker = 0;
        for &p in &fb[start..end] {
            if p == ACCENT {
                ring += 1;
            } else if p == ACCENT_2 {
                marker += 1;
            }
        }
        (ring, marker)
    }

    #[test]
    fn up_next_overlay_paints_its_header_and_top_edge() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 12, 2, TransportState::Playing, 1);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(app.model.up_next_visible());
        let fb = app.composed_pixels();
        // 1 px accent top edge across the full width.
        for x in [0, SCREEN_W / 2, SCREEN_W - 1] {
            assert_eq!(
                fb[UP_NEXT_TOP as usize * SCREEN_W as usize + x as usize],
                ACCENT
            );
        }
        // Header text: `UP NEXT` left and the position right, both FG.
        let header_ink = (UP_NEXT_HEADER_Y..UP_NEXT_HEADER_Y + ROW_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|&(x, y)| fb[y as usize * SCREEN_W as usize + x as usize] == FG)
            .count();
        assert!(header_ink > 40, "header glyphs: {header_ink}");
        // Six rows now fit between the header and the Now Playing footer (UI_14 §3).
        assert_eq!(UP_NEXT_ROWS, 6);
        assert!(UP_NEXT_ROWS_Y + ROW_H * UP_NEXT_ROWS <= NP_CONTENT_BOTTOM);
    }

    /// UI_10 review: cue rows were indented by prepending a literal space (9 px, and the
    /// widget's `indent: 8` was ignored). The overlay now passes `Row::indent` through `List`.
    #[test]
    fn cue_rows_use_the_widget_indent_px_in_the_overlay() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        let entries = vec![
            np_entry(NowPlayingEntryKind::Track, "One", 0, 0),
            np_entry(NowPlayingEntryKind::Cue, "Part", 0, 5_000),
            np_entry(NowPlayingEntryKind::Track, "Two", 1, 0),
        ];
        app.handle_ipc(list_msg(entries, 0));
        // A loaded item is needed for the knob to have a Now Playing target at all.
        let mut session = SessionState::default();
        session.current_item = Some(PlayableItemId::new(1));
        session.duration_ms = Some(60_000);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            session,
        )));
        // Reveal the overlay with a left turn: the cursor stays on the playing row (row 0) and
        // its ring is away from the cue inset.
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert_eq!(app.model.np_focus.target, NpTarget::List);
        assert_eq!(app.model.np_focus.cursor, 0);
        assert!(app.model.up_next_visible());
        let fb = app.composed_pixels();
        let ink_in_inset = |row_y: u16| {
            (MARGIN_X..MARGIN_X + 8).any(|x| {
                (row_y..row_y + ROW_H).any(|y| {
                    !matches!(
                        fb[y as usize * SCREEN_W as usize + x as usize],
                        BG | SURFACE | SURFACE_ALT | BORDER_SOFT
                    )
                })
            })
        };
        let cue_row = UP_NEXT_ROWS_Y + ROW_H;
        let track_row = UP_NEXT_ROWS_Y + ROW_H * 2;
        assert!(!ink_in_inset(cue_row), "cue label is inset by `Row::indent`");
        assert!(ink_in_inset(track_row), "track label starts at the margin");
    }

    /// UI_13: one turn opens the overlay directly (there is no longer a Select→List step and
    /// no time-block ring — the waveform is the default focus and the overlay is the list's
    /// focus surface).
    #[test]
    fn selecting_the_list_target_reveals_the_overlay() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        np_album(&mut app, 5, 0, TransportState::Playing, 1);
        let frame = |app: &mut UiApp| -> Vec<u16> { app.composed_pixels() };
        let top_edge = |v: &[u16]| v[UP_NEXT_TOP as usize * SCREEN_W as usize];
        // Default: the waveform owns the screen; the overlay band is untouched by the list.
        let time_focused = frame(&mut app);
        assert_eq!(app.model.np_focus.target, NpTarget::CurrentTime);
        assert_ne!(top_edge(&time_focused), ACCENT, "no overlay top edge");

        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert_eq!(app.model.np_focus.target, NpTarget::List);
        assert_eq!(app.model.np_focus.mode, NpMode::List);
        let list_focused = frame(&mut app);
        assert_eq!(top_edge(&list_focused), ACCENT, "overlay top edge is ACCENT");
        let (ring, _) = overlay_marks(&mut app);
        assert!(ring > 1_000, "cursor row paints its 2 px ring: {ring}");
    }

    /// NB_VERIFY NB_15: "entries whose media is unavailable render dimmed" had no test.
    #[test]
    fn unavailable_entries_are_dimmed_in_the_overlay() {
        let frame = |gone: bool| -> Vec<u16> {
            let mut wifi = MockWifi::default();
            let mut app = UiApp::new(UiConfig::default());
            let mut entries: Vec<NowPlayingEntry> = (0..4)
                .map(|i| np_entry(NowPlayingEntryKind::Track, &format!("Song {i}"), i, 0))
                .collect();
            entries[1].available = !gone;
            app.handle_ipc(list_msg(entries, 0));
            let mut session = SessionState::default();
            session.current_item = Some(PlayableItemId::new(1));
            session.duration_ms = Some(60_000);
            app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
                session,
            )));
            app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
            // Move off the unavailable row so its Disabled style (not the focus ring) is what
            // the comparison sees.
            app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
            app.composed_pixels()
        };
        let (ok, dim) = (frame(false), frame(true));
        assert_ne!(ok, dim, "an unavailable row must look different");
        // Same glyph shapes, different colour: the ink count is unchanged.
        let ink = |v: &[u16]| {
            v.iter()
                .filter(|&&p| p != BG && p != SURFACE_ALT && p != SURFACE)
                .count()
        };
        assert_eq!(ink(&ok), ink(&dim));
        let colours = |v: &[u16]| {
            let mut c: Vec<u16> = v
                .iter()
                .copied()
                .filter(|&p| p != BG && p != SURFACE_ALT && p != SURFACE)
                .collect();
            c.sort_unstable();
            c.dedup();
            c
        };
        assert_ne!(colours(&ok), colours(&dim));
        assert!(colours(&dim).contains(&crate::ui::tokens::FG_MUTED));
    }

    #[test]
    fn hostile_list_text_is_stripped_before_the_panel_draws_it() {
        let mut app = UiApp::new(UiConfig::default());
        let evil = "Ev\u{202E}il\u{200B}\u{0007}\u{001B}[31m";
        let mut a = np_entry(NowPlayingEntryKind::Track, evil, 0, 0);
        a.artist = evil.into();
        let b = np_entry(NowPlayingEntryKind::Cue, evil, 0, 5_000);
        app.handle_ipc(list_msg(vec![a, b], 0));
        let l = app.model.now_playing.as_ref().unwrap();
        for e in &l.entries {
            assert!(
                !e.title.chars().any(|c| c.is_control() || "\u{202E}\u{200B}".contains(c)),
                "{:?}",
                e.title
            );
        }
        assert_eq!(
            crate::screens::np_row_text(
                &l.entries[1],
                crate::screens::NpRowNumbering::Sequence,
                true
            ),
            "0:05 Evil[31m"
        );
    }

    /// WARP_06: re-queued / repeated warp commands are replaced, not appended.
    #[test]
    fn push_outgoing_coalesces_warp_commands() {
        let mut app = UiApp::new(UiConfig::default());
        let seek = |t| UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekTo {
            target_ms: t,
        }));
        for t in [1, 2, 3] {
            app.push_outgoing(seek(t));
        }
        app.queue_cmd(PlaybackCommand::SetRate { permille: 1250 });
        app.queue_cmd(PlaybackCommand::SetRate { permille: 1500 });
        let out = app.take_outgoing();
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], seek(3));
    }

    // ---- WAVE_02: playhead, sidecar states, partial present ----

    fn wave_session(path: Option<String>, transport: TransportState, pos_ms: u64, rate: i32) -> SessionState {
        let mut s = SessionState::default();
        s.transport = transport;
        s.title = "Tone".into();
        s.artist = "Artist".into();
        s.position_ms = pos_ms;
        s.rate_permille = rate;
        s.duration_ms = Some(180_000);
        s.current_item = Some(PlayableItemId::new(1));
        s.waveform_path = path;
        s
    }

    /// A loaded, playing single-track session (no cues) sampled at `now_ms == 0`.
    fn wave_app(pos_ms: u64, rate: i32) -> UiApp {
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            wave_session(None, TransportState::Playing, pos_ms, rate),
        )));
        app
    }

    fn wfm_bytes(peaks: &[u8], bucket_ms: u16, duration_ms: u64) -> Vec<u8> {
        plippa_core::waveform::encode_to_vec(&plippa_core::waveform::Waveform::new_complete(
            bucket_ms,
            duration_ms,
            44_100,
            2,
            1_234,
            99,
            peaks.to_vec(),
        ))
    }

    /// Loads `app`'s sidecar path through the normal IPC path (main screen, playing).
    fn send_wave_path(app: &mut UiApp, path: &std::path::Path) {
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            wave_session(
                Some(path.to_string_lossy().into_owned()),
                TransportState::Playing,
                0,
                1000,
            ),
        )));
    }

    /// Count of pixels of `colour` in rows `y0..y1` of a composited screen.
    fn ink(pixels: &[u16], y0: u16, y1: u16, colour: u16) -> usize {
        let w = usize::from(SCREEN_W);
        let mut n = 0;
        for y in y0..y1 {
            for x in 0..w {
                if pixels[y as usize * w + x] == colour {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn playhead_dead_reckon_follows_the_effective_rate() {
        // The engine pushes status ~10 Hz; the UI interpolates between samples (never beyond
        // `JOG_DEADRECKON_MAX_AGE_MS`, and never backwards on a forward rate).
        for (rate, per_100ms) in [
            (1000i32, 100i64),
            (1500, 150),
            (4000, 400),
            (-750, -75),
        ] {
            let mut app = wave_app(65_000, rate);
            assert_eq!(app.playhead_ms_at(0), 65_000, "rate {rate}: at the sample");
            let mid = app.playhead_ms_at(300) as i64;
            assert_eq!(mid, 65_000 + 3 * per_100ms, "rate {rate}: interpolated");
            app.handle_input(InputEvent::Tick { now_ms: 300 }, &mut MockWifi::default());
            app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
                wave_session(None, TransportState::Playing, mid as u64, rate),
                2,
            )));
            assert_eq!(
                app.playhead_ms_at(500) as i64,
                mid + 2 * per_100ms,
                "rate {rate}: keeps running from the fresh sample"
            );
        }
        // Clamped at the file end, never past it.
        let app = wave_app(179_900, 4000);
        assert_eq!(app.playhead_ms_at(300), 180_000);
        // Paused: the clock stands still whatever the rate and however stale the sample.
        let mut app = wave_app(65_000, 1000);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            wave_session(None, TransportState::Paused, 65_000, 1000),
        )));
        assert_eq!(app.playhead_ms_at(5_000), 65_000);
        // Backwards 1x clamps at zero rather than wrapping.
        let app = wave_app(200, -1000);
        assert_eq!(app.playhead_ms_at(300), 0);
    }

    /// UI_15 §2: a scan from **paused** plays audio while the engine keeps `transport =
    /// Paused`. The picture must follow the scan rate (it used to freeze, then jump forward on
    /// the next status), and on release it must sit exactly on the settled position.
    #[test]
    fn playhead_dead_reckons_through_a_paused_scan_and_settles_on_the_release() {
        let mut app = UiApp::new(UiConfig::default());
        let mut scanning = wave_session(None, TransportState::Paused, 65_000, 4000);
        scanning.scan = plippa_core::ScanState::Forward;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            scanning,
        )));
        // 300 ms at 4x: the picture advances, even though the transport says Paused.
        assert_eq!(app.playhead_ms_at(0), 65_000);
        app.handle_input(InputEvent::Tick { now_ms: 300 }, &mut MockWifi::default());
        assert_eq!(app.playhead_ms_at(300), 66_200);
        app.handle_input(InputEvent::Tick { now_ms: 500 }, &mut MockWifi::default());
        assert_eq!(app.playhead_ms_at(500), 67_000);
        // A status one fast window behind the drawn value does not pull it back (the sample was
        // taken ~100 ms ago at 4x = 400 ms of playhead).
        let mut behind = wave_session(None, TransportState::Paused, 66_600, 4000);
        behind.scan = plippa_core::ScanState::Forward;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            behind, 2,
        )));
        assert_eq!(app.playhead_ms_at(500), 67_000, "no backward jump on a stale sample");
        app.handle_input(InputEvent::Tick { now_ms: 600 }, &mut MockWifi::default());
        assert!(app.playhead_ms_at(600) >= 67_000, "the picture never steps back");
        // Release: the engine clears the scan and publishes the position it settled on. The
        // picture lands exactly there and stays there (nothing moves once released).
        let mut settled = wave_session(None, TransportState::Paused, 67_050, 1000);
        settled.scan = plippa_core::ScanState::None;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            settled, 3,
        )));
        assert_eq!(app.playhead_ms_at(600), 67_050, "exactly the reported position");
        assert_eq!(app.playhead_ms_at(5_000), 67_050, "and it does not drift");
    }

    /// UI_15 §2: the same for a scan that started from Playing — the release restores the set
    /// speed, and the picture continues from the reported position at that rate (no jump).
    #[test]
    fn playhead_release_restores_the_set_speed_without_a_jump() {
        let mut app = UiApp::new(UiConfig::default());
        let mut scanning = wave_session(None, TransportState::Playing, 65_000, 8000);
        scanning.scan = plippa_core::ScanState::Forward;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            scanning,
        )));
        assert_eq!(app.playhead_ms_at(200), 66_600);
        // The engine's one settled status: scan off, rate back at the 150 % set speed.
        app.handle_input(InputEvent::Tick { now_ms: 200 }, &mut MockWifi::default());
        let mut settled = wave_session(None, TransportState::Playing, 66_700, 1500);
        settled.scan = plippa_core::ScanState::None;
        settled.target_rate_permille = 1500;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            settled, 2,
        )));
        assert_eq!(app.playhead_ms_at(200), 66_700);
        // Continues at 1.5x from there.
        assert_eq!(app.playhead_ms_at(500), 67_150);
    }

    #[test]
    fn playhead_jog_target_holds_the_centre_until_the_seek_lands() {
        let mut app = UiApp::new(UiConfig::default());
        // Paused audio: dead reckoning does not catch up with the target for the 120 ms
        // commit-idle, so this exercises the running seek rather than the anti-stuck landing
        // heuristic. UI_16 §5: the finest click comes from Fine time (Dec 1/10 = 100 ms).
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            wave_session(None, TransportState::Paused, 65_000, 1000),
        )));
        app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        let target = app.jog.live_target().expect("jog gesture open");
        assert_eq!(target, 65_100, "one click is one 100 ms fine step");
        assert_eq!(app.playhead_ms_at(150), target, "centre follows the gesture");

        // Commit on idle: one `SeekTo`, and the centre stays on the target (no snap back).
        let released_at = crate::warp_ui::JOG_COMMIT_IDLE_MS;
        app.handle_input(InputEvent::Tick { now_ms: released_at }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: released_at + 1 }, &mut wifi);
        let seeks: Vec<u64> = app
            .take_outgoing()
            .into_iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekTo {
                    target_ms,
                })) => Some(target_ms),
                _ => None,
            })
            .collect();
        assert_eq!(seeks, vec![target], "one commit on release");
        assert_eq!(
            app.playhead_ms_at(released_at + 1),
            target,
            "no snap back while the engine lands"
        );


        // The engine acks the seek (target still pending): the centre is still the target.
        let mut s = wave_session(None, TransportState::Paused, 65_000, 1000);
        s.seek_target_ms = Some(target);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(s, 2)));
        assert_eq!(app.playhead_ms_at(released_at + 60), target);

        // Landed: the actual position is the target, so the picture does not jump.
        app.handle_input(InputEvent::Tick { now_ms: released_at + 100 }, &mut wifi);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            wave_session(None, TransportState::Paused, target, 1000),
            3,
        )));
        assert_eq!(app.playhead_ms_at(released_at + 100), target);
    }

    #[test]
    fn playhead_and_clock_follow_the_audio_after_the_seek_lands() {
        // Regression (WAVE_02 review): `playhead_ms_at` returned the `JogView` target whenever
        // a view was visible. The view lives for `JOG_LANDED_HOLD_MS` (1.5 s) after landing so
        // the marker and readout can fade, so the picture and the footer clock froze for up
        // to 1.5 s after every jog / chase / resume / cue seek and then jumped forward. The
        // landed hold must be readout-only: only a seek the engine has not landed yet pins
        // the centre.
        let mut app = UiApp::new(UiConfig::default());
        // Paused for the jog so the dead-reckoned sample cannot overtake the 100 ms fine step
        // (UI_16 §5: Fine time gives the click size); the landing status is where audio
        // resumes, and from there the follow-the-audio assertions below apply.
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            wave_session(None, TransportState::Paused, 65_000, 1000),
        )));
        app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        let target = app.jog.live_target().expect("jog gesture open");

        // Release: commit, the engine acks the target, then reports it as the position.
        let released_at = crate::warp_ui::JOG_COMMIT_IDLE_MS;
        app.handle_input(InputEvent::Tick { now_ms: released_at }, &mut wifi);
        let mut s = wave_session(None, TransportState::Paused, 65_000, 1000);
        s.seek_target_ms = Some(target);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(s, 2)));
        let landed_at = released_at + 100;
        app.handle_input(InputEvent::Tick { now_ms: landed_at }, &mut wifi);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            wave_session(None, TransportState::Playing, target, 1000),
            3,
        )));
        assert_eq!(app.playhead_ms_at(landed_at), target, "landed on the target");
        let view = app.model.jog_view.expect("landed marker still visible");
        assert!(view.readout, "the readout keeps the target during the hold");

        // The audio plays on; the engine pushes a status ~10 Hz. At every step the picture
        // and the clock must be where the audio is, not on the frozen target...
        let mut rev = 4;
        for step in [100u64, 300, 1_000, 1_400, 1_600, 2_000] {
            let now = landed_at + step;
            let pos = target + step;
            // The loop ticks before draining IPC, so the status is stamped at `now`.
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
                wave_session(None, TransportState::Playing, pos, 1000),
                rev,
            )));
            rev += 1;
            assert_eq!(app.playhead_ms_at(now), pos, "picture at +{step} ms");
            assert_eq!(app.model.playhead_ms, pos, "footer clock at +{step} ms");
        }
        // ... and there is no jump when the landed hold expires (it ends at 1.5 s): the view
        // is gone at 1.6 s while the playhead stays exactly continuous.
        assert!(
            app.model.jog_view.is_none(),
            "the landed hold has expired; only the playhead remains"
        );
        let now = landed_at + 2_100;
        let pos = target + 2_100;
        app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            wave_session(None, TransportState::Playing, pos, 1000),
            rev,
        )));
        assert_eq!(app.playhead_ms_at(now), pos);
    }

    #[test]
    fn playhead_jog_target_clamps_to_the_loaded_file() {
        let mut app = UiApp::new(UiConfig::default());
        app.play_mode.settings.seek_outside_track = Some(true);
        // UI_16 §5: the finest click is 100 ms (Fine time Dec 1/10), so start 50 ms in and one
        // click left runs past this file's start.
        app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
        let mut s = wave_session(None, TransportState::Playing, 50, 1000);
        s.seek_outside_track = true;
        s.prev_duration_ms = Some(60_000);
        s.next_duration_ms = Some(90_000);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(s)));
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::MenuRotate(-1), &mut wifi);
        assert!(app.jog.live_item() < 0, "gesture entered the previous track");
        assert_eq!(app.playhead_ms_at(0), 0, "clamped to this file's start");

        // The band only shows this file, so a target that runs past its end pins to the end.
        let mut app = wave_app(179_950, 1000);
        app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
        let mut s = wave_session(None, TransportState::Playing, 179_950, 1000);
        s.seek_outside_track = true;
        s.prev_duration_ms = Some(60_000);
        s.next_duration_ms = Some(90_000);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(s)));
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(app.jog.live_item() > 0, "gesture entered the next track");
        assert_eq!(app.playhead_ms_at(0), 180_000, "clamped to this file's end");
    }

    #[test]
    fn wave_load_reads_the_sidecar_and_clears_on_a_path_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wfm");
        let peaks = vec![0u8, 200, 255, 0];
        std::fs::write(&path, wfm_bytes(&peaks, 20, 80)).unwrap();
        let mut app = UiApp::new(UiConfig::default());
        send_wave_path(&mut app, &path);
        assert_eq!(app.model.wave.state, WaveState::Ready);
        assert_eq!(app.model.wave.peaks, peaks);
        assert_eq!(app.model.wave.bucket_ms, 20, "geometry uses the header");
        assert_eq!(app.model.wave.duration_ms, 80, "geometry uses the header");

        // A new path with no file yet: Pending with the old peaks cleared, then a retry
        // picks the file up once it lands — without a remount.
        let b = dir.path().join("b.wfm");
        send_wave_path(&mut app, &b);
        assert_eq!(app.model.wave.state, WaveState::Pending);
        assert!(app.model.wave.peaks.is_empty());
        app.handle_input(InputEvent::Tick { now_ms: 10 }, &mut MockWifi::default());
        assert_eq!(app.model.wave.state, WaveState::Pending, "retry is rate limited");
        std::fs::write(&b, wfm_bytes(&[255], 20, 20)).unwrap();
        app.handle_input(
            InputEvent::Tick {
                now_ms: WAVE_RETRY_MS,
            },
            &mut MockWifi::default(),
        );
        assert_eq!(app.model.wave.state, WaveState::Ready);
        assert_eq!(app.model.wave.peaks, vec![255]);
    }

    #[test]
    fn waveform_states_text_is_exact_and_never_no_waveform() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wfm");
        let mut app = UiApp::new(UiConfig::default());
        // Missing sidecar: ANALYSING, flat band, no accent ink.
        send_wave_path(&mut app, &path);
        assert_eq!(app.model.wave.state, WaveState::Pending);
        assert_eq!(app.model.footer_centre(), "ANALYSING");
        let fb = app.composed_pixels();
        assert_eq!(ink(&fb, WAVE_BAND_Y, WAVE_BAND_Y + 140, ACCENT), 0);
        assert!(ink(&fb, WAVE_BAND_Y, WAVE_BAND_Y + 140, BORDER_SOFT) > 0, "flat centre line");

        // Arrival: bars appear in the band, the footer text is gone. Two seconds of full-scale
        // peaks, so bars extend right of the playhead (the first column under it is overdrawn).
        std::fs::write(&path, wfm_bytes(&[255; 100], 20, 2_000)).unwrap();
        app.handle_input(
            InputEvent::Tick {
                now_ms: WAVE_RETRY_MS,
            },
            &mut MockWifi::default(),
        );
        assert_eq!(app.model.wave.state, WaveState::Ready);
        assert_eq!(app.model.footer_centre(), "");
        let fb = app.composed_pixels();
        assert!(ink(&fb, WAVE_BAND_Y, WAVE_BAND_Y + 140, ACCENT) > 0, "bars drawn");

        // Corrupt sidecar: flat band, and no text at all (never "NO WAVEFORM").
        let bad = dir.path().join("bad.wfm");
        std::fs::write(&bad, b"definitely not a .wfm").unwrap();
        send_wave_path(&mut app, &bad);
        assert_eq!(app.model.wave.state, WaveState::Failed);
        assert_eq!(app.model.footer_centre(), "");
        let fb = app.composed_pixels();
        assert_eq!(ink(&fb, WAVE_BAND_Y, WAVE_BAND_Y + 140, ACCENT), 0);
        assert_eq!(ink(&fb, WAVE_BAND_Y, WAVE_BAND_Y + 140, ACCENT_DIM), 0);
    }

    #[test]
    fn present_sends_only_the_changed_bounding_box() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quiet.wfm");
        // Quiet bars: a scroll repaints a thin strip around the centre, so the diff box stays
        // well under the spec's 60 % full-frame guard.
        let peaks: Vec<u8> = (0..3_000).map(|i| 20 + (i % 20) as u8).collect();
        std::fs::write(&path, wfm_bytes(&peaks, 20, 60_000)).unwrap();
        let mut app = UiApp::new(UiConfig::default());
        send_wave_path(&mut app, &path);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            wave_session(
                Some(path.to_string_lossy().into_owned()),
                TransportState::Playing,
                10_100,
                1000,
            ),
        )));
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
        let mut wifi = MockWifi::default();
        let full_rect = crate::display::Rect::new(0, 0, SCREEN_W, SCREEN_H);
        let full = u32::from(SCREEN_W) * u32::from(SCREEN_H);
        app.present(&mut display);
        assert_eq!(display.frames, 1);
        assert_eq!(*display.rects.last().unwrap(), full_rect);

        // Unchanged frame: nothing is sent.
        app.present(&mut display);
        assert_eq!(display.frames, 1, "no present when nothing changed");

        // 300 ms of playback (a fresh sample keeps the dead reckoning alive): one rect inside
        // the band, never the whole frame.
        app.handle_input(InputEvent::Tick { now_ms: 300 }, &mut wifi);
        app.present(&mut display);
        assert_eq!(display.frames, 2);
        let r = *display.rects.last().unwrap();
        assert!(r.area() < full, "partial, not full frame: {r:?}");
        // UI_14: the frames clock in the footer ticks with the band, so a band scroll may
        // include the footer rows. It still starts at the band and never repaints the header.
        assert!(r.y >= WAVE_BAND_Y && r.y + r.h <= SCREEN_H, "band+footer only: {r:?}");

        // A footer-only change (remaining time), playhead unchanged: the rect is in the footer.
        let frames = display.frames;
        let mut s = wave_session(
            Some(path.to_string_lossy().into_owned()),
            TransportState::Paused,
            10_400,
            1000,
        );
        s.duration_ms = Some(181_000);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(s)));
        app.present(&mut display);
        assert_eq!(display.frames, frames + 1, "footer text change is presented");
        let r = *display.rects.last().unwrap();
        assert!(r.y >= CONTENT_BOTTOM, "footer only: {r:?}");

        // A whole-screen change is presented in full.
        let _ = app.nav.push(Route::Library);
        app.present(&mut display);
        assert_eq!(*display.rects.last().unwrap(), full_rect);
        // ... and the composite always matches the app's own frame.
        assert_eq!(display.screen().pixels, app.composed_pixels());
    }

    #[test]
    fn loud_waveform_scroll_never_presents_a_full_frame() {
        // Loud peaks (200..255, full scale in every window) with the playhead well inside the
        // file: the bars fill the band edge to edge, so the changed box is the whole band —
        // `WAVE_BAND_H` = 145 of 240 rows = ~60 % of the screen. The guard is measured against
        // the pixels outside the band, so this must stay a rect present of the band; a
        // screen-relative threshold sends a full frame every 33 ms.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loud.wfm");
        let peaks: Vec<u8> = (0..3_000).map(|i| 200 + ((i * 37) % 56) as u8).collect();
        std::fs::write(&path, wfm_bytes(&peaks, 20, 60_000)).unwrap();
        let full = Rect::new(0, 0, SCREEN_W, SCREEN_H);
        let mut wifi = MockWifi::default();

        for pos_ms in [20_500u64, 40_500] {
            let mut app = UiApp::new(UiConfig::default());
            send_wave_path(&mut app, &path);
            app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
                wave_session(
                    Some(path.to_string_lossy().into_owned()),
                    TransportState::Playing,
                    pos_ms,
                    1000,
                ),
            )));
            let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
            app.present(&mut display);
            assert_eq!(display.frames, 1);
            assert_eq!(*display.rects.last().unwrap(), full, "first frame is full");

            let mut now = 0;
            // Half a second of 30 FPS-ish ticks, with fresh status samples like the engine.
            for _ in 0..15 {
                now += WAVE_FRAME_MS;
                app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
                app.present(&mut display);
            }
            assert!(display.frames > 1, "the band is animating at {pos_ms}");
            for r in &display.rects[1..] {
                assert_ne!(*r, full, "no full-frame present for a waveform frame");
                // UI_14: the band is 145 px and the frames clock in the footer ticks with it,
                // so the box starts at the band top and ends inside the screen (band+footer).
                assert_eq!(
                    r.y, WAVE_BAND_Y,
                    "the change starts at the band top at {pos_ms} ms: {r:?}"
                );
                assert!(
                    r.y + r.h <= SCREEN_H,
                    "the change never reaches past the footer at {pos_ms} ms: {r:?}"
                );
                assert!(
                    r.h >= WAVE_BAND_H,
                    "the change covers the whole band at {pos_ms} ms: {r:?}"
                );
                // The band-relative guard ignores band pixels, so even a box covering the band
                // and the footer rows is never promoted to a full frame.
                assert!(
                    r.area() * 100 >= full.area() * 55,
                    "the regression case must be a box covering the whole band: {r:?}"
                );
                // A column whose bar height happens to be unchanged does not grow the box,
                // so it can be a few px narrower than the band — never a thin strip.
                assert!(
                    r.w * 100 >= SCREEN_W * 85,
                    "the rect is the band, not a strip: {r:?}"
                );
            }
            // ... and the composite still matches the app's own frame.
            assert_eq!(display.screen().pixels, app.composed_pixels());
        }
    }

    #[test]
    fn paused_ticks_present_nothing_and_the_composite_stays_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.wfm");
        let peaks: Vec<u8> = (0..3_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, wfm_bytes(&peaks, 20, 60_000)).unwrap();
        let mut app = UiApp::new(UiConfig::default());
        send_wave_path(&mut app, &path);
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session(
            wave_session(
                Some(path.to_string_lossy().into_owned()),
                TransportState::Paused,
                10_000,
                1000,
            ),
        )));
        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
        let mut wifi = MockWifi::default();
        app.present(&mut display);
        let frames = display.frames;
        for now in [100u64, 1_000, 5_000, 30_000] {
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            app.present(&mut display);
        }
        assert_eq!(display.frames, frames, "paused: no periodic presents");

        // A long sequence of mixed input: the composite of every rect present must equal the
        // app's own framebuffer after each step (the "never goes stale" invariant).
        let mut now = 30_000;
        let steps = [
            InputEvent::MenuRotate(2),
            InputEvent::MenuRotate(-1),
            InputEvent::MenuRotate(1),
        ];
        for ev in steps {
            now += 40;
            let label = format!("{ev:?}");
            app.handle_input(ev, &mut wifi);
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            app.present(&mut display);
            assert_eq!(display.screen().pixels, app.composed_pixels(), "after {label}");
        }
        // Open Up Next, scroll it, then Back to the waveform.
        for ev in [
            InputEvent::MenuRotate(3),
            InputEvent::MenuEnter,
            InputEvent::MenuRotate(1),
            InputEvent::Back,
        ] {
            now += 40;
            let label = format!("{ev:?}");
            app.handle_input(ev, &mut wifi);
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            app.present(&mut display);
            assert_eq!(display.screen().pixels, app.composed_pixels(), "after {label}");
        }
    }

    #[test]
    fn a_flat_band_ticks_the_clock_at_the_timecode_unit_rate() {
        // Pending / Failed sidecar: the band is a static flat line, but a sub-second footer
        // clock still changes. The old 1 s tick froze the frames digit for a second and then
        // jumped; frames need the 30 FPS tick, decimal tenths 10 Hz, and plain `MM:SS` 1 Hz.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("later.wfm");
        let mut app = UiApp::new(UiConfig::default());
        send_wave_path(&mut app, &path);
        assert_eq!(app.model.wave.state, WaveState::Pending);
        let mut wifi = MockWifi::default();
        let _ = app.take_needs_redraw();
        assert_eq!(
            app.model.play_mode.timecode_unit,
            TimecodeUnit::Frames75,
            "the default unit carries frames"
        );

        // Frames: every 33 ms tick can change the digits, so it repaints.
        for now in [WAVE_FRAME_MS, WAVE_FRAME_MS * 2, WAVE_FRAME_MS * 3] {
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            assert!(
                app.take_needs_redraw(),
                "the frames clock repaints at {now} ms"
            );
        }

        // Decimal shows tenths: 10 Hz is enough, so a 33 ms tick must not force a repaint.
        app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
        app.play_mode_changed();
        let _ = app.take_needs_redraw();
        app.handle_input(InputEvent::Tick { now_ms: 198 }, &mut wifi);
        assert!(
            !app.take_needs_redraw(),
            "no repaint 99 ms after a decimal tick"
        );
        app.handle_input(InputEvent::Tick { now_ms: 199 }, &mut wifi);
        assert!(
            app.take_needs_redraw(),
            "a tenth changed 100 ms after the last repaint"
        );

        // Off: plain `MM:SS`, so the 1 s clock is the only thing that can change.
        app.play_mode.settings.timecode_unit = Some(TimecodeUnit::Off);
        app.play_mode_changed();
        let _ = app.take_needs_redraw();
        for now in [232u64, 265, 298] {
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            assert!(!app.take_needs_redraw(), "no repaint for MM:SS at {now} ms");
        }
        app.handle_input(InputEvent::Tick { now_ms: 1_199 }, &mut wifi);
        assert!(
            app.take_needs_redraw(),
            "the 1 s clock still refreshes with the unit off"
        );

        // The sidecar arrives: bars are back, so is the 30 FPS tick even with the unit off.
        // The pending probe at 1 199 ms gates the next read, so 2 199 ms is the first retry.
        std::fs::write(&path, wfm_bytes(&[255; 8], 20, 160)).unwrap();
        app.handle_input(InputEvent::Tick { now_ms: 2_199 }, &mut wifi);
        assert_eq!(app.model.wave.state, WaveState::Ready);
        assert!(app.take_needs_redraw(), "the first bars repaint");
        app.handle_input(
            InputEvent::Tick {
                now_ms: 2_199 + WAVE_FRAME_MS,
            },
            &mut wifi,
        );
        assert!(app.take_needs_redraw(), "30 FPS once the picture can move");
    }

    #[test]
    fn a_failed_sidecar_is_probed_at_most_once_a_second() {
        // `Failed` only retries when the file changes, but the old code stat'ed the stick on
        // every tick (10–30 Hz on USB). A rewrite of the file inside the retry window must
        // not be picked up; the next probe (1 s later) must load it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.wfm");
        std::fs::write(&path, b"definitely not a .wfm").unwrap();
        let mut app = UiApp::new(UiConfig::default());
        send_wave_path(&mut app, &path);
        assert_eq!(app.model.wave.state, WaveState::Failed);
        let mut wifi = MockWifi::default();

        // The analyser finishes and rewrites the sidecar 100 ms after the failure.
        std::fs::write(&path, wfm_bytes(&[255; 8], 20, 160)).unwrap();
        for now in [100u64, 999] {
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            assert_eq!(
                app.model.wave.state,
                WaveState::Failed,
                "no probe inside the 1 s window (tick {now})"
            );
        }
        app.handle_input(InputEvent::Tick { now_ms: 1_000 }, &mut wifi);
        assert_eq!(
            app.model.wave.state,
            WaveState::Ready,
            "the changed file is retried on the next one-second probe"
        );
    }

    /// PWRSW_01: a fresh app with the button up (the loop's first release arms the tracker).
    fn power_app(poweroff_armed: bool) -> UiApp {
        let mut app = UiApp::new(UiConfig {
            poweroff_armed,
            ..UiConfig::default()
        });
        app.handle_input(InputEvent::PowerButton { down: false }, &mut MockWifi::default());
        app
    }

    fn popup_text(app: &UiApp) -> Option<String> {
        app.compositor
            .status()
            .map(|p| p.lines().join(" "))
    }

    fn pause_sent(out: &[UiOutgoing]) -> bool {
        out.iter().any(|o| {
            matches!(
                o,
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause))
            )
        })
    }

    fn poweroff_requested(out: &[UiOutgoing]) -> Option<bool> {
        out.iter().find_map(|o| match o {
            UiOutgoing::ShutdownRequest { power_off } => Some(*power_off),
            _ => None,
        })
    }

    #[test]
    fn power_hold_warns_at_one_second_and_cancels_on_release() {
        let mut app = power_app(true);
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::PowerButton { down: true }, &mut wifi);

        app.handle_input(InputEvent::Tick { now_ms: 999 }, &mut wifi);
        assert!(popup_text(&app).is_none(), "no warning before 1 s");
        assert!(!app.power_warning_active());

        app.handle_input(InputEvent::Tick { now_ms: 1_000 }, &mut wifi);
        assert_eq!(
            popup_text(&app).as_deref(),
            Some("Keep holding to power off")
        );
        assert!(app.power_warning_active());

        // The sticky popup must still be up just before the shutdown threshold.
        app.handle_input(InputEvent::Tick { now_ms: 3_999 }, &mut wifi);
        assert!(popup_text(&app).is_some(), "sticky across the whole hold");
        assert!(!app.shutdown_pending());
        assert!(app.take_outgoing().is_empty());

        app.handle_input(InputEvent::PowerButton { down: false }, &mut wifi);
        assert!(popup_text(&app).is_none(), "release clears the warning");
        assert!(!app.power_warning_active());
        assert!(app.take_outgoing().is_empty());
    }

    #[test]
    fn power_hold_at_four_seconds_pauses_acks_then_requests() {
        let mut app = power_app(true);
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::PowerButton { down: true }, &mut wifi);

        // A late tick past both thresholds warns first, then fires on the next tick.
        app.handle_input(InputEvent::Tick { now_ms: 4_000 }, &mut wifi);
        assert_eq!(
            popup_text(&app).as_deref(),
            Some("Keep holding to power off")
        );
        app.handle_input(InputEvent::Tick { now_ms: 4_033 }, &mut wifi);

        let out = app.take_outgoing();
        assert!(pause_sent(&out), "audio pauses at the threshold: {out:?}");
        assert_eq!(
            poweroff_requested(&out),
            None,
            "the request waits for the acknowledgement"
        );
        assert_eq!(popup_text(&app).as_deref(), Some("Powering off…"));
        assert!(app.shutdown_pending());

        // 700 ms acknowledgement: nothing before the deadline, exactly once at it.
        app.handle_input(InputEvent::Tick { now_ms: 4_732 }, &mut wifi);
        assert!(app.take_outgoing().is_empty());
        app.handle_input(InputEvent::Tick { now_ms: 4_733 }, &mut wifi);
        assert_eq!(poweroff_requested(&app.take_outgoing()), Some(true));
        app.handle_input(InputEvent::Tick { now_ms: 5_000 }, &mut wifi);
        assert_eq!(poweroff_requested(&app.take_outgoing()), None, "once only");
    }

    #[test]
    fn unarmed_hold_stops_the_app_without_powering_off() {
        // Dev / CI: the 4 s mechanism still works but must not ask the host to halt.
        let mut app = power_app(false);
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::PowerButton { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 4_000 }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 4_100 }, &mut wifi);
        app.take_outgoing();
        assert_eq!(popup_text(&app).as_deref(), Some("Shutting down…"));
        app.handle_input(InputEvent::Tick { now_ms: 4_800 }, &mut wifi);
        assert_eq!(poweroff_requested(&app.take_outgoing()), Some(false));
    }

    #[test]
    fn input_is_ignored_after_the_shutdown_request() {
        let mut app = power_app(true);
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::PowerButton { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 4_000 }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 4_100 }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 4_800 }, &mut wifi);
        assert_eq!(poweroff_requested(&app.take_outgoing()), Some(true));

        app.handle_input(InputEvent::VolumeRotate(1), &mut wifi);
        app.handle_input(InputEvent::Back, &mut wifi);
        app.handle_input(InputEvent::PowerButton { down: false }, &mut wifi);
        assert!(
            app.take_outgoing().is_empty(),
            "knob, buttons and edges are dropped while powering off"
        );
        assert_eq!(popup_text(&app).as_deref(), Some("Powering off…"));
    }

    #[test]
    fn press_already_down_at_startup_does_nothing_until_released() {
        // Powering on by holding the button must not immediately power off again.
        let mut app = UiApp::new(UiConfig {
            poweroff_armed: true,
            ..UiConfig::default()
        });
        let mut wifi = MockWifi::default();
        app.handle_input(InputEvent::PowerButton { down: true }, &mut wifi);
        for now in [500u64, 1_000, 4_000, 9_000] {
            app.handle_input(InputEvent::Tick { now_ms: now }, &mut wifi);
            assert!(popup_text(&app).is_none(), "unarmed press at {now}");
            assert!(app.take_outgoing().is_empty());
        }
        app.handle_input(InputEvent::PowerButton { down: false }, &mut wifi);
        assert!(popup_text(&app).is_none());

        // The next press counts normally.
        app.handle_input(InputEvent::PowerButton { down: true }, &mut wifi);
        app.handle_input(InputEvent::Tick { now_ms: 10_000 }, &mut wifi);
        assert_eq!(
            popup_text(&app).as_deref(),
            Some("Keep holding to power off")
        );
    }

    // ---- PLAY_04: SW2 tap cycles the five play-mode rungs -------------------------------

    use plippa_core::{OrderMode, PlayModeState, RepeatMode};

    fn sw2_edge(app: &mut UiApp, wifi: &mut MockWifi, down: bool, at_ms: u64) {
        app.handle_input(
            InputEvent::ControlButton {
                role: ControlRole::PlayMode,
                down,
                at_ms,
            },
            wifi,
        );
    }

    /// Press and release well inside [`PLAYMODE_HOLD_MS`]: one `Tap`.
    fn sw2_tap(app: &mut UiApp, wifi: &mut MockWifi, at_ms: u64) {
        sw2_edge(app, wifi, true, at_ms);
        sw2_edge(app, wifi, false, at_ms + 80);
    }

    /// The `SetOrderMode` / `SetRepeatMode` commands in `out`, in send order.
    fn mode_commands(out: &[UiOutgoing]) -> Vec<PlaybackCommand> {
        out.iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => match c {
                    PlaybackCommand::SetOrderMode(_) | PlaybackCommand::SetRepeatMode(_) => {
                        Some(c.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn rung(order: OrderMode, repeat: RepeatMode) -> PlayModeState {
        PlayModeState { order, repeat }
    }

    #[test]
    fn sw2_tap_cycles_the_five_rungs() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        assert_eq!(app.play_mode_state(), rung(OrderMode::Normal, RepeatMode::Off));

        let expected = [
            (OrderMode::Shuffle, RepeatMode::Off, "Shuffle"),
            (OrderMode::Normal, RepeatMode::Queue, "Repeat all"),
            (OrderMode::Shuffle, RepeatMode::Queue, "Shuffle + repeat"),
            (OrderMode::Normal, RepeatMode::Item, "Repeat one"),
            // The fifth rung wraps back to the default, including the order.
            (OrderMode::Normal, RepeatMode::Off, "Normal"),
        ];
        for (i, (order, repeat, text)) in expected.iter().enumerate() {
            sw2_tap(&mut app, &mut wifi, i as u64 * 1_000);
            assert_eq!(
                app.play_mode_state(),
                rung(*order, *repeat),
                "tap {} should be {text}",
                i + 1
            );
            assert_eq!(
                popup_text(&app),
                Some(format!("Play mode {text}")),
                "tap {} popup",
                i + 1
            );
            let _ = app.take_outgoing();
        }
        assert_eq!(app.play_mode_state(), rung(OrderMode::Normal, RepeatMode::Off));
    }

    #[test]
    fn sw2_tap_sends_only_the_changed_fields() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();

        // Normal/Off -> Shuffle/Off: only the order changes.
        sw2_tap(&mut app, &mut wifi, 0);
        assert_eq!(
            mode_commands(&app.take_outgoing()),
            vec![PlaybackCommand::SetOrderMode(OrderMode::Shuffle)]
        );

        // Shuffle/Queue -> Repeat one: order and repeat both change, order first.
        app.model.order_mode = OrderMode::Shuffle;
        app.model.repeat_mode = RepeatMode::Queue;
        sw2_tap(&mut app, &mut wifi, 1_000);
        assert_eq!(
            mode_commands(&app.take_outgoing()),
            vec![
                PlaybackCommand::SetOrderMode(OrderMode::Normal),
                PlaybackCommand::SetRepeatMode(RepeatMode::Item),
            ]
        );

        // Normal/Queue -> Shuffle/Queue: only the order changes again.
        app.model.order_mode = OrderMode::Normal;
        app.model.repeat_mode = RepeatMode::Queue;
        sw2_tap(&mut app, &mut wifi, 2_000);
        assert_eq!(
            mode_commands(&app.take_outgoing()),
            vec![PlaybackCommand::SetOrderMode(OrderMode::Shuffle)]
        );
    }

    #[test]
    fn sw2_tap_from_repeat_item_returns_to_normal_off() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        app.model.order_mode = OrderMode::Normal;
        app.model.repeat_mode = RepeatMode::Item;

        sw2_tap(&mut app, &mut wifi, 0);
        assert_eq!(app.play_mode_state(), rung(OrderMode::Normal, RepeatMode::Off));
        assert_eq!(
            mode_commands(&app.take_outgoing()),
            vec![PlaybackCommand::SetRepeatMode(RepeatMode::Off)],
            "the order was already Normal: one command, no re-send"
        );
        assert_eq!(popup_text(&app), Some("Play mode Normal".to_string()));
    }

    #[test]
    fn sw2_hold_still_opens_playback_settings() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        sw2_edge(&mut app, &mut wifi, true, 0);
        app.handle_input(InputEvent::Tick { now_ms: PLAYMODE_HOLD_MS }, &mut wifi);
        assert_eq!(*app.nav.current(), Route::SystemPlayMode);
        assert_eq!(
            app.play_mode_state(),
            rung(OrderMode::Normal, RepeatMode::Off),
            "a hold never changes the mode"
        );
        assert!(
            mode_commands(&app.take_outgoing()).is_empty(),
            "a hold sends no mode command"
        );
        // Releasing after a hold reports HoldEnd: still nothing.
        sw2_edge(&mut app, &mut wifi, false, PLAYMODE_HOLD_MS + 100);
        assert_eq!(*app.nav.current(), Route::SystemPlayMode);
        assert!(mode_commands(&app.take_outgoing()).is_empty());
    }

    #[test]
    fn sw2_tap_with_nothing_loaded_still_sends() {
        let mut app = UiApp::new(UiConfig::default());
        let mut wifi = MockWifi::default();
        assert!(app.model.playback.is_none(), "nothing loaded");
        sw2_tap(&mut app, &mut wifi, 0);
        assert_eq!(
            mode_commands(&app.take_outgoing()),
            vec![PlaybackCommand::SetOrderMode(OrderMode::Shuffle)],
            "the mode applies to the next load"
        );
    }

    // ---- ART_08: the cover fades while a scan or jog is under way ---------------------------

    fn cover() -> std::sync::Arc<crate::art::ArtImage> {
        let side = crate::screens::NP_ART_SIDE;
        std::sync::Arc::new(crate::art::ArtImage {
            side,
            rgb565: vec![0xF81F; usize::from(side) * usize::from(side)],
            rgb_side: 64,
            rgb: vec![0; 64 * 64 * 3],
        })
    }

    /// A paused session over a static band (no sidecar, plain `MM:SS`), so only a fade can
    /// ask for frames faster than once a second.
    fn quiet_art_app() -> UiApp {
        let mut app = UiApp::new(UiConfig::default());
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
            wave_session(None, TransportState::Paused, 10_000, 1000),
            1,
        )));
        app.play_mode.settings.timecode_unit = Some(TimecodeUnit::Off);
        app.play_mode_changed();
        app.model.np_art = Some(cover());
        let _ = app.take_needs_redraw();
        app
    }

    fn scan_status(app: &mut UiApp, scan: plippa_core::ScanState, rev: u64) {
        let mut s = wave_session(None, TransportState::Paused, 10_000, 1000);
        s.scan = scan;
        app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(s, rev)));
        let _ = app.take_needs_redraw();
    }

    fn tick_at(app: &mut UiApp, wifi: &mut MockWifi, now_ms: u64) -> bool {
        app.handle_input(InputEvent::Tick { now_ms }, wifi);
        app.take_needs_redraw()
    }

    #[test]
    fn scan_fades_the_cover_out_in_120_ms_and_back_in_after_800_ms_quiet() {
        let mut app = quiet_art_app();
        let mut wifi = MockWifi::default();
        tick_at(&mut app, &mut wifi, 1_000);
        assert_eq!(app.model.np_art_alpha, 100);

        scan_status(&mut app, plippa_core::ScanState::Forward, 2);
        tick_at(&mut app, &mut wifi, 1_033);
        let mid = {
            tick_at(&mut app, &mut wifi, 1_093);
            app.model.np_art_alpha
        };
        assert!(mid > 0 && mid < 100, "part way at 60 ms: {mid}");
        tick_at(&mut app, &mut wifi, 1_153);
        assert_eq!(app.model.np_art_alpha, 0, "gone 120 ms after the scan began");
        // Held scan: stays gone.
        tick_at(&mut app, &mut wifi, 5_000);
        assert_eq!(app.model.np_art_alpha, 0);

        // Release: the quiet period is measured from the last tick that still saw the scan.
        scan_status(&mut app, plippa_core::ScanState::None, 3);
        let released = 5_000;
        for dt in [33u64, 400, 799] {
            tick_at(&mut app, &mut wifi, released + dt);
            assert_eq!(app.model.np_art_alpha, 0, "still hidden {dt} ms after release");
        }
        tick_at(&mut app, &mut wifi, released + 950);
        let rising = app.model.np_art_alpha;
        assert!(rising > 0 && rising < 100, "fading in at 950 ms: {rising}");
        tick_at(&mut app, &mut wifi, released + 1_100);
        assert_eq!(app.model.np_art_alpha, 100, "fully back 1100 ms after release");
    }

    #[test]
    fn a_new_scan_during_the_fade_in_reverses_without_a_snap() {
        let mut app = quiet_art_app();
        let mut wifi = MockWifi::default();
        tick_at(&mut app, &mut wifi, 0);
        scan_status(&mut app, plippa_core::ScanState::Forward, 2);
        tick_at(&mut app, &mut wifi, 100);
        tick_at(&mut app, &mut wifi, 300);
        scan_status(&mut app, plippa_core::ScanState::None, 3);
        tick_at(&mut app, &mut wifi, 301);
        tick_at(&mut app, &mut wifi, 1_100 + 150);
        let before = app.model.np_art_alpha;
        assert!(before > 0 && before < 100, "mid fade-in: {before}");
        scan_status(&mut app, plippa_core::ScanState::Backward, 4);
        tick_at(&mut app, &mut wifi, 1_250 + 1);
        let after = app.model.np_art_alpha;
        assert!(after <= before && before - after <= 5, "continues from {before}, not a snap: {after}");
    }

    #[test]
    fn a_jog_gesture_hides_the_cover_too() {
        let mut app = quiet_art_app();
        let mut wifi = MockWifi::default();
        tick_at(&mut app, &mut wifi, 1_000);
        app.handle_input(InputEvent::MenuRotate(1), &mut wifi);
        assert!(app.jog.is_open());
        tick_at(&mut app, &mut wifi, 1_033);
        tick_at(&mut app, &mut wifi, 1_153);
        assert_eq!(app.model.np_art_alpha, 0);
    }

    #[test]
    fn adjusting_a_ring_parameter_is_not_a_trigger() {
        let mut app = quiet_art_app();
        let mut wifi = MockWifi::default();
        app.model.np_focus.mode = NpMode::Adjust(AdjustParam::Tempo);
        for now in [100u64, 500, 2_000] {
            tick_at(&mut app, &mut wifi, now);
            assert_eq!(app.model.np_art_alpha, 100);
        }
    }

    #[test]
    fn fades_redraw_at_the_frame_rate_over_a_static_paused_band_and_only_then() {
        let mut app = quiet_art_app();
        let mut wifi = MockWifi::default();
        // Visible and quiet: nothing asks for frames faster than the 1 s clock.
        for now in [33u64, 66, 99] {
            assert!(!tick_at(&mut app, &mut wifi, now), "visible, static: no frame at {now}");
        }
        scan_status(&mut app, plippa_core::ScanState::Forward, 2);
        // Fade-out: every 33 ms tick repaints, including the one that lands on alpha 0.
        let mut now = 99;
        loop {
            now += WAVE_FRAME_MS;
            let drew = tick_at(&mut app, &mut wifi, now);
            assert!(drew, "frame at {now} (alpha {})", app.model.np_art_alpha);
            if app.model.np_art_alpha == 0 {
                break;
            }
            assert!(now < 99 + 8 * WAVE_FRAME_MS, "the fade-out must end");
        }
        // Hidden while the scan runs: no 33 ms ticks ...
        for step in 1..=5u64 {
            assert!(!tick_at(&mut app, &mut wifi, now + step * WAVE_FRAME_MS), "hidden at step {step}");
        }
        // ... and none during the 800 ms hold after release.
        scan_status(&mut app, plippa_core::ScanState::None, 3);
        let released = now + 5 * WAVE_FRAME_MS;
        tick_at(&mut app, &mut wifi, released);
        let mut t = released;
        while t + WAVE_FRAME_MS < released + 790 {
            t += WAVE_FRAME_MS;
            assert!(!tick_at(&mut app, &mut wifi, t), "hidden, holding: no frame at {t}");
        }
        // Fade-in: a frame on every tick, including the first one with a nonzero alpha.
        let mut frames = 0;
        while t < released + 1_150 {
            t += WAVE_FRAME_MS;
            frames += usize::from(tick_at(&mut app, &mut wifi, t));
        }
        assert_eq!(app.model.np_art_alpha, 100);
        assert!(frames >= 8, "about ten frames over 300 ms, got {frames}");
        // Back at rest: the cadence returns to the slow clock.
        for step in 1..=5u64 {
            assert!(!tick_at(&mut app, &mut wifi, t + step * WAVE_FRAME_MS), "visible again");
        }
    }
}
