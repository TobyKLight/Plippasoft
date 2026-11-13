//! WARP_07 (and later WARP controls): mock-input + frame-buffer tests of the Now Playing
//! parameter ring, tempo knob and readout. No engine, no hardware, no wall-clock time: the
//! app clock is the injected `Tick { now_ms }`.

use plippa_core::{
    NowPlayingEntry, NowPlayingEntryKind, NowPlayingListKind, PlayableItemId, ScanState,
    SessionState, TransportState, WarpMode,
};
use plippa_ipc_proto::{IpcMessage, NowPlayingListChanged, PlaybackCommand, PlaybackStatus};
use plippa_ui::ui::tokens::NP_CONTENT_BOTTOM;
use plippa_ui::{
    AdjustParam, InputEvent, MockDisplay, MockWifi, NpMode, PlayButtonLed, Rect, Route, UiApp,
    UiConfig, UiOutgoing, PARAM_RING, SCREEN_W, WAVE_BAND_BOTTOM,
};

struct Rig {
    app: UiApp,
    wifi: MockWifi,
    rev: u64,
}

fn entry(i: u32) -> NowPlayingEntry {
    NowPlayingEntry {
        kind: NowPlayingEntryKind::Track,
        title: format!("T{i}"),
        artist: "A".into(),
        start_ms: 0,
        end_ms: None,
        duration_ms: Some(60_000),
        track_index: i,
        playable_id: None,
        available: true,
        // PLAY_02: source (`mediaList`) position for a Track entry (cues use `start_ms`);
        // the UI fixture is never reordered.
        item_position: 0,
    }
}

impl Rig {
    /// `tracks` = 1 gives the plain single-track layout, > 1 the list panel.
    fn new(tracks: u32) -> Self {
        let mut r = Rig {
            app: UiApp::new(UiConfig::default()),
            wifi: MockWifi::default(),
            rev: 0,
        };
        r.status(|_| {});
        let entries: Vec<_> = (0..tracks).map(entry).collect();
        r.app
            .handle_ipc(IpcMessage::NowPlayingListChanged(NowPlayingListChanged {
                entries,
                current_index: 0,
                list_kind: NowPlayingListKind::Album,
                // PLAY_02: fixture list identity (one per load); the UI only uses it to
                // keep an overlay open across a reorder.
                list_id: 1,
            }));
        r
    }

    fn status(&mut self, f: impl FnOnce(&mut SessionState)) {
        let mut s = SessionState::default();
        s.transport = TransportState::Playing;
        s.title = "Tone".into();
        s.artist = "Artist".into();
        s.position_ms = 65_000;
        s.duration_ms = Some(180_000);
        s.current_item = Some(PlayableItemId::new(1));
        f(&mut s);
        self.rev += 1;
        self.app.handle_ipc(IpcMessage::PlaybackStatus(
            PlaybackStatus::from_session_rev(s, self.rev),
        ));
    }

    fn tick(&mut self, ms: u64) {
        self.app
            .handle_input(InputEvent::Tick { now_ms: ms }, &mut self.wifi);
    }
    fn input(&mut self, ev: InputEvent) {
        self.app.handle_input(ev, &mut self.wifi);
    }
    fn cmds(&mut self) -> Vec<PlaybackCommand> {
        self.app
            .take_outgoing()
            .into_iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
                _ => None,
            })
            .collect()
    }
    fn rates(&mut self) -> Vec<i32> {
        self.cmds()
            .into_iter()
            .filter_map(|c| match c {
                PlaybackCommand::SetRate { permille } => Some(permille),
                _ => None,
            })
            .collect()
    }
    fn frame(&mut self) -> Vec<u16> {
        // Compose directly: `present` only sends the changed bounding box (WAVE_02), so a
        // fresh mock display would hold just that patch.
        self.app.composed_pixels()
    }
    /// Enter the Tempo parameter (either layout).
    fn enter_tempo(&mut self) {
        self.input(InputEvent::MenuEnter); // panel: Jog; single: Tempo
        if self.app.model.np_focus.param() != Some(AdjustParam::Tempo) {
            self.input(InputEvent::MenuEnter);
        }
        assert_eq!(self.app.model.np_focus.param(), Some(AdjustParam::Tempo));
    }
    /// One slow detent (gap far above the 350 ms velocity reset): always the 1% step.
    fn slow_click(&mut self, t: &mut u64, dir: i8) -> Vec<i32> {
        *t += 1_000;
        self.tick(*t);
        self.input(InputEvent::MenuRotate(dir));
        let rates = self.rates();
        // The engine acknowledges: status follows the last rate set.
        if let Some(last) = rates.last().copied() {
            self.status(|s| s.target_rate_permille = last);
        }
        rates
    }
    /// Scripted click train: `n` detents `gap_ms` apart from `t0`; returns the rates sent.
    fn train(&mut self, t0: u64, gap_ms: u64, n: u64, dir: i8) -> Vec<i32> {
        let mut seq = vec![];
        for i in 0..n {
            self.tick(t0 + i * gap_ms);
            self.input(InputEvent::MenuRotate(dir));
            seq.extend(self.rates());
        }
        seq
    }
}

fn hash(px: &[u16]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for p in px {
        h ^= *p as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Golden-frame guard for the pure Now Playing layouts. WAVE_02 replaced the UI_10 screen
/// (header + full-screen waveform + footer), so the baselines are new: any unintended pixel
/// drift in the plain layouts still fails here.
#[test]
fn pure_layouts_render_a_stable_frame() {
    let mut single = Rig::new(1);
    assert_eq!(hash(&single.frame()), 3969682878042673437);
    let mut panel = Rig::new(3);
    assert_eq!(hash(&panel.frame()), 15197018127544576637);
    let selecting = panel.frame();
    panel.input(InputEvent::MenuEnter); // old "jog mode" = Adjust(Jog)
    assert_eq!(
        panel.app.model.np_focus.mode,
        NpMode::Adjust(AdjustParam::Jog)
    );
    // UI_12 (owner decision): the transport / tempo row comes back as an overlay band over
    // the waveform, so the frame now differs from Select mode.
    assert!(!panel.app.model.up_next_visible());
    let adjusting = panel.frame();
    assert_ne!(
        hash(&adjusting),
        hash(&selecting),
        "the Adjust overlay must be painted"
    );
    // Leaving Adjust repaints the band away: the Select frame returns pixel for pixel, which
    // is what the `prev_frame` diff presents (the band's rect is inside the changed bbox).
    panel.input(InputEvent::Back);
    assert_eq!(panel.app.model.np_focus.mode, NpMode::Select);
    assert_eq!(
        panel.frame(),
        selecting,
        "the overlay's disappearance must repaint the waveform underneath"
    );
    // Entering Tempo never changes the rate and sends nothing.
    panel.input(InputEvent::MenuEnter);
    assert!(panel.cmds().is_empty());
}

/// UI_12 review: the Adjust band's enter/leave must work through the *real* present path, not
/// only at composition level. `warp_controls` above compares `composed_pixels()`; this drives
/// `UiApp::present` into a `MockDisplay` and asserts what the display actually receives:
/// entering Adjust presents a rect that covers the band (not a full frame), leaving presents a
/// rect that repaints the band away, and the composite never keeps a stale band.
#[test]
fn adjust_band_presents_a_band_rect_and_repaints_it_away() {
    use plippa_ui::ui::tokens::{ROW_H_DISPLAY, SPACE_1};
    // `screens::ADJUST_BAND_Y` / `ADJUST_BAND_H` are private, so derive the band from the
    // public Now Playing tokens: `WAVE_BAND_BOTTOM` (210) − 30 − 4 = 176. The concrete
    // assertions below (176 / 30) make any later geometry change fail loudly here.
    let band_h = ROW_H_DISPLAY;
    let band_y = WAVE_BAND_BOTTOM - band_h - SPACE_1;
    assert_eq!(band_y, 176, "ADJUST_BAND_Y is 210 - 30 - 4");
    assert_eq!(band_h, 30, "ADJUST_BAND_H is ROW_H_DISPLAY");
    let full = Rect::new(0, 0, SCREEN_W, 240);
    let covers_band = |r: Rect| r.y <= band_y && r.y + r.h >= band_y + band_h;

    let mut r = Rig::new(3);
    let mut d = MockDisplay::new(SCREEN_W, 240);
    // Boot: the first present has no `prev_frame`, so it is a full frame by design.
    r.app.present(&mut d);
    assert_eq!(d.rects, vec![full]);
    let select = r.app.composed_pixels();
    assert_eq!(d.screen().pixels, select);

    r.input(InputEvent::MenuEnter);
    assert_eq!(r.app.model.np_focus.mode, NpMode::Adjust(AdjustParam::Jog));
    r.app.present(&mut d);
    assert_eq!(d.rects.len(), 2, "entering Adjust presents exactly one patch");
    let rect = *d.rects.last().unwrap();
    assert_ne!(rect, full, "a band-only change must not present a full frame");
    assert!(covers_band(rect), "{rect:?} does not cover the band at {band_y}..{}", band_y + band_h);
    assert_eq!(
        d.screen().pixels,
        r.app.composed_pixels(),
        "the patched screen must hold the composed Adjust frame"
    );
    assert_ne!(d.screen().pixels, select, "the band is visibly painted");

    r.input(InputEvent::Back);
    assert_eq!(r.app.model.np_focus.mode, NpMode::Select);
    r.app.present(&mut d);
    assert_eq!(d.rects.len(), 3, "leaving Adjust presents exactly one patch");
    let rect = *d.rects.last().unwrap();
    assert_ne!(rect, full, "the band repaint must not be a full frame");
    assert!(covers_band(rect), "{rect:?} does not repaint the band away");
    assert_eq!(
        r.app.composed_pixels(),
        select,
        "leaving Adjust restores the Select frame"
    );
    assert_eq!(
        d.screen().pixels,
        select,
        "no stale band is left on the display after the repaint"
    );
}

#[test]
fn enter_cycles_the_ring_and_back_returns_to_select_on_both_layouts() {
    // Panel: Select -> Enter x N walks PARAM_RING in order and wraps; Back -> Select.
    let mut r = Rig::new(3);
    for lap in 0..2 {
        for p in PARAM_RING {
            r.input(InputEvent::MenuEnter);
            assert_eq!(r.app.model.np_focus.mode, NpMode::Adjust(*p), "lap {lap}");
        }
    }
    r.input(InputEvent::Back);
    assert_eq!(r.app.model.np_focus.mode, NpMode::Select);
    assert!(
        !r.app.compositor.main_menu_visible(),
        "Back from a ring param never leaves"
    );
    r.input(InputEvent::MenuEnter);
    r.input(InputEvent::MenuEnter);
    r.input(InputEvent::Back);
    assert_eq!(r.app.model.np_focus.mode, NpMode::Select);
    r.input(InputEvent::Back);
    assert!(
        r.app.compositor.main_menu_visible(),
        "Back at Select keeps the old behaviour"
    );

    // Single track: implicit jog; Enter goes to the next entry (Tempo), and around.
    let mut r = Rig::new(1);
    let mut seen = vec![];
    for _ in 0..PARAM_RING.len() * 2 {
        r.input(InputEvent::MenuEnter);
        seen.push(r.app.model.np_focus.param().unwrap());
    }
    assert_eq!(seen[0], PARAM_RING[0].next());
    assert_eq!(seen[PARAM_RING.len() - 1], PARAM_RING[0]);
    r.input(InputEvent::Back);
    assert_eq!(r.app.model.np_focus.param(), None);
    assert!(!r.app.compositor.main_menu_visible());
    r.input(InputEvent::Back);
    assert!(r.app.compositor.main_menu_visible());
}

#[test]
fn tempo_slow_clicks_are_one_percent_and_sticky_at_100() {
    for tracks in [1, 3] {
        let mut r = Rig::new(tracks);
        r.status(|s| s.target_rate_permille = 980);
        r.enter_tempo();
        let mut t = 0;
        assert_eq!(r.slow_click(&mut t, 1), vec![990]);
        assert_eq!(
            r.slow_click(&mut t, 1),
            vec![1000],
            "reaching 100% sends SetRate 1000"
        );
        // Sticky: same direction within 300 ms is swallowed ...
        r.tick(t + 100);
        r.input(InputEvent::MenuRotate(1));
        assert!(r.rates().is_empty());
        r.tick(t + 250);
        r.input(InputEvent::MenuRotate(1));
        assert!(r.rates().is_empty());
        // ... and leaves after that.
        t += 1_000;
        r.tick(t);
        r.input(InputEvent::MenuRotate(1));
        assert_eq!(r.rates(), vec![1010]);
        // The optimistic speed drives the next step even though status is stale.
        r.tick(t + 400);
        r.input(InputEvent::MenuRotate(-1));
        assert_eq!(r.rates(), vec![1000]);
    }
}

#[test]
fn tempo_fast_turn_snaps_and_clamps_and_keeps_sign() {
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1100);
    r.enter_tempo();
    // Click trains of 8 / 17 / 33 clicks per second: steps 2% / 5% / 10%, snapped.
    let mut seqs = vec![];
    for gap in [120u64, 60, 30] {
        let mut r = Rig::new(1);
        r.status(|s| s.target_rate_permille = 1100);
        r.enter_tempo();
        seqs.push(r.train(10_000, gap, 8, 1));
    }
    assert_eq!(
        seqs[0],
        vec![1110, 1120, 1140, 1160, 1180, 1200, 1220, 1240],
        "{seqs:?}"
    );
    assert_eq!(
        seqs[1],
        vec![1110, 1150, 1200, 1250, 1300, 1350, 1400, 1450],
        "{seqs:?}"
    );
    assert_eq!(
        seqs[2],
        vec![1110, 1200, 1300, 1400, 1500, 1600, 1700, 1800],
        "{seqs:?}"
    );
    // Limits.
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1990);
    r.enter_tempo();
    let mut t = 0;
    assert_eq!(r.slow_click(&mut t, 1), vec![2000]);
    assert!(r.slow_click(&mut t, 1).is_empty(), "clamped at 200%");
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 110);
    r.enter_tempo();
    let mut t = 0;
    assert_eq!(r.slow_click(&mut t, -1), vec![100]);
    assert!(r.slow_click(&mut t, -1).is_empty(), "clamped at 10%");
    // Sign preserved: a set negative speed edits its magnitude.
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = -750);
    r.enter_tempo();
    let mut t = 0;
    assert_eq!(r.slow_click(&mut t, 1), vec![-760]);
    assert_eq!(r.slow_click(&mut t, -1), vec![-750]);
}

#[test]
fn nothing_loaded_tempo_has_no_effect_but_ring_still_cycles() {
    let mut r = Rig::new(1);
    r.status(|s| s.current_item = None);
    r.input(InputEvent::MenuEnter);
    assert_eq!(r.app.model.np_focus.param(), Some(AdjustParam::Tempo));
    r.tick(1_000);
    r.input(InputEvent::MenuRotate(1));
    assert!(r.cmds().is_empty());
    r.input(InputEvent::MenuEnter);
    assert_eq!(r.app.model.np_focus.param(), Some(AdjustParam::Zoom));
}

fn diff(a: &[u16], b: &[u16]) -> Vec<(u16, u16)> {
    let w = SCREEN_W as usize;
    (0..a.len())
        .filter(|i| a[*i] != b[*i])
        .map(|i| ((i % w) as u16, (i / w) as u16))
        .collect()
}

#[test]
fn set_speed_text_says_rev_or_corr_in_the_footer() {
    for tracks in [1u32, 3] {
        let mut r = Rig::new(tracks);
        let pure = r.frame();
        // -75% rev: same pixels in both speed warp modes (no asterisk, no fallback marker).
        r.status(|s| {
            s.target_rate_permille = -750;
            s.speed_warp_mode = WarpMode::PitchCorrect;
        });
        assert_eq!(r.app.model.footer_right(), "-75% rev");
        assert_eq!(r.app.model.footer_centre(), "");
        let rev = r.frame();
        r.status(|s| {
            s.target_rate_permille = -750;
            s.speed_warp_mode = WarpMode::PitchShift;
        });
        assert_eq!(rev, r.frame(), "reverse wording ignores the warp mode");
        assert_ne!(rev, pure);
        diff_only_in_footer(&pure, &rev);

        // Wording differs between correct / shift for a positive speed.
        r.status(|s| {
            s.target_rate_permille = 1250;
            s.speed_warp_mode = WarpMode::PitchCorrect;
        });
        assert_eq!(r.app.model.footer_right(), "125% corr");
        assert_eq!(r.app.model.footer_centre(), "");
        let corr = r.frame();
        diff_only_in_footer(&pure, &corr);
        r.status(|s| {
            s.target_rate_permille = 1250;
            s.speed_warp_mode = WarpMode::PitchShift;
        });
        assert_eq!(r.app.model.footer_right(), "125% shift");
        let shift = r.frame();
        assert_ne!(corr, shift);
        diff_only_in_footer(&pure, &shift);

        // Back at 100% the centre is silent and the frame is exactly the pure one.
        r.status(|s| {
            s.target_rate_permille = 1000;
            s.speed_warp_mode = WarpMode::PitchCorrect;
        });
        assert_eq!(r.app.model.footer_centre(), "");
        assert_eq!(r.frame(), pure);
    }
}

/// WAVE_02 layout: set-speed text lives in the footer band only; the waveform band above it
/// is untouched while the picture is at rest.
fn diff_only_in_footer(a: &[u16], b: &[u16]) {
    let d = diff(a, b);
    assert!(!d.is_empty(), "expected the footer text to change");
    assert!(
        d.iter().all(|(_, y)| *y >= NP_CONTENT_BOTTOM),
        "ink changed outside the footer: {d:?}"
    );
}

#[test]
fn a_scan_does_not_change_the_painted_set_speed() {
    let mut r = Rig::new(1);
    r.status(|s| {
        s.target_rate_permille = 1250;
        s.scan = ScanState::None;
    });
    let warped = r.frame();
    r.status(|s| {
        s.target_rate_permille = 1250;
        s.scan = ScanState::Forward;
        s.rate_permille = 4000;
    });
    let scanning = r.frame();
    // WAVE_02 removed the transport row and its scan arrows: the picture is the scan
    // indicator and the footer keeps the *set* speed, not the instantaneous rate.
    assert_eq!(warped, scanning);
    let mut pure_rig = Rig::new(1);
    let pure = pure_rig.frame();
    assert_ne!(warped, pure);
    assert_eq!(r.app.model.footer_right(), "125% corr");
}

#[test]
fn offline_readout_follows_desired_and_reconnect_status_wins_without_resend() {
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1000);
    r.enter_tempo();
    r.app.set_engine_connected(false);
    let t = 1_000;
    r.tick(t);
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.rates(), vec![1010], "no engine ack while offline");
    // Far beyond 600 ms offline: still showing the set speed.
    r.tick(t + 5_000);
    assert_eq!(r.app.model.set_speed_permille(), 1010);
    // Reconnect: nothing is re-sent, and once the grace passes the engine status wins.
    r.app.set_engine_connected(true);
    r.tick(t + 5_100);
    // (WARP_10: the two warp-mode commands are re-sent on connect; the tempo never is.)
    assert!(r.rates().is_empty());
    r.tick(t + 5_100 + 700);
    assert_eq!(
        r.app.model.set_speed_permille(),
        1000,
        "engine restarted at 100%"
    );
    assert!(r.cmds().is_empty());
}

#[test]
fn new_load_while_warped_shows_popup_and_tempo_is_kept_across_tracks() {
    let mut r = Rig::new(1);
    r.status(|s| {
        s.target_rate_permille = 1250;
        s.speed_warp_mode = WarpMode::PitchCorrect;
        s.current_item = Some(PlayableItemId::new(1));
    });
    r.app.compositor.clear_status();
    r.status(|s| {
        s.target_rate_permille = 1250;
        s.speed_warp_mode = WarpMode::PitchCorrect;
        s.current_item = Some(PlayableItemId::new(2));
    });
    let popup = r
        .app
        .compositor
        .status()
        .expect("popup on new load while warped");
    assert!(
        popup.lines().iter().any(|l| l == "125% corr"),
        "{:?}",
        popup.lines()
    );
    // Kept across the change: the UI sends nothing and still shows 125%.
    assert!(r.cmds().is_empty());
    assert_eq!(r.app.model.set_speed_permille(), 1250);
    // Pure load: no popup.
    let mut r = Rig::new(1);
    r.status(|s| s.current_item = Some(PlayableItemId::new(2)));
    assert!(r.app.compositor.status().is_none());
}

// ---------------------------------------------------------------------------------------
// WARP_08: Play button timing and the 800 ms reset hold
// ---------------------------------------------------------------------------------------

use plippa_board::{ControlRole, LedRole};

impl Rig {
    fn play_edge(&mut self, down: bool, at_ms: u64) {
        self.tick(at_ms);
        self.input(InputEvent::ControlButton {
            role: ControlRole::Play,
            down,
            at_ms,
        });
    }
}

#[test]
fn play_immediate_toggles_on_press_and_hold_sends_nothing_more() {
    let mut r = Rig::new(1);
    r.status(|s| s.transport = TransportState::Paused);
    r.play_edge(true, 1_000);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::Play],
        "toggle on press at 100%"
    );
    for t in [1_500, 1_799, 1_800, 3_000] {
        r.tick(t);
    }
    r.play_edge(false, 3_000);
    assert!(
        r.cmds().is_empty(),
        "a long hold at 100% does nothing (already pure)"
    );
    assert!(r.app.take_led_flashes().is_empty());

    // Playing -> Pause, and "Nothing loaded" is unchanged.
    let mut r = Rig::new(1);
    r.play_edge(true, 0);
    assert_eq!(r.cmds(), vec![PlaybackCommand::Pause]);
    let mut r = Rig::new(1);
    r.status(|s| {
        s.transport = TransportState::Stopped;
        s.current_item = None;
    });
    r.play_edge(true, 0);
    assert!(r.cmds().is_empty());
    assert!(r
        .app
        .compositor
        .status()
        .is_some_and(|p| p.lines().iter().any(|l| l == "Nothing loaded")));
}

#[test]
fn play_deferred_toggles_on_release_before_800_ms() {
    for warp in [
        |s: &mut SessionState| s.target_rate_permille = 1250,
        |s: &mut SessionState| s.seek_target_ms = Some(42_000),
        |s: &mut SessionState| s.warp_active = true,
    ] {
        let mut r = Rig::new(1);
        r.status(warp);
        r.play_edge(true, 1_000);
        assert!(
            r.cmds().is_empty(),
            "nothing on press while warped / seeking"
        );
        r.tick(1_799);
        assert!(r.cmds().is_empty());
        r.play_edge(false, 1_799);
        assert_eq!(
            r.cmds(),
            vec![PlaybackCommand::Pause],
            "toggle on release (799 ms)"
        );
    }
}

#[test]
fn play_during_a_held_scan_toggles_on_press_and_never_resets_the_set_speed() {
    let mut r = Rig::new(1);
    r.status(|s| {
        s.transport = TransportState::Paused;
        s.target_rate_permille = 1250;
        s.speed_warp_mode = WarpMode::PitchCorrect;
        s.scan = ScanState::Forward;
        s.rate_permille = 4000;
    });
    // The scan wins over the warp: Play is a plain toggle, so it is not deferred and the
    // WARP_21 long-press reset can never arm — the set speed (1250) must survive.
    r.play_edge(true, 1_000);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::Play],
        "toggle on the press edge, not deferred"
    );
    r.tick(1_799);
    r.tick(1_800);
    r.tick(2_500);
    assert!(
        r.cmds().is_empty(),
        "held past 800 ms: no ResetRate, no repeat"
    );
    // The optimistic readout stays on the owner's set speed (no popup, no reset flash).
    assert!(r.app.compositor.status().is_none(), "no speed popup on Main");
    assert!(r.app.take_led_flashes().is_empty(), "no reset flash");
    // Release is inert: exactly one toggle per press, and the scan is left alone.
    r.play_edge(false, 2_600);
    assert!(r.cmds().is_empty());
    assert_eq!(
        r.app.model.playback.as_ref().unwrap().session.scan,
        ScanState::Forward,
        "the scan is not cancelled by Play"
    );
}

#[test]
fn play_press_while_a_scan_runs_without_the_button_is_also_immediate() {
    // The engine still reports the scan for a moment after the button is released; Play must
    // stay a plain toggle there too, not silently fall back to the deferred reset.
    let mut r = Rig::new(1);
    r.status(|s| {
        s.transport = TransportState::Playing;
        s.target_rate_permille = 500;
        s.scan = ScanState::Backward;
    });
    r.play_edge(true, 0);
    assert_eq!(r.cmds(), vec![PlaybackCommand::Pause]);
    r.tick(900);
    r.tick(1_000);
    assert!(r.cmds().is_empty(), "no ResetRate while the scan is reported");
}

#[test]
fn play_reset_boundary_release_at_exactly_800_is_one_reset_and_no_toggle() {
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 750);
    r.play_edge(true, 0);
    // No tick between: the release itself carries the 800 ms.
    r.input(InputEvent::ControlButton {
        role: ControlRole::Play,
        down: false,
        at_ms: 800,
    });
    assert_eq!(r.cmds(), vec![PlaybackCommand::ResetRate]);
}

#[test]
fn play_optimistic_led_query_matches_the_toggle_rule() {
    let mut r = Rig::new(1);
    let down = InputEvent::ControlButton {
        role: ControlRole::Play,
        down: true,
        at_ms: 0,
    };
    let up = InputEvent::ControlButton {
        role: ControlRole::Play,
        down: false,
        at_ms: 100,
    };
    assert!(r.app.event_toggles_play(&InputEvent::PlayStop));
    assert!(r.app.event_toggles_play(&down), "immediate: press toggles");
    r.status(|s| s.target_rate_permille = 1250);
    assert!(!r.app.event_toggles_play(&down), "deferred: press does not");
    r.input(down.clone());
    assert!(
        r.app.event_toggles_play(&up),
        "deferred: release before 800 ms toggles"
    );
}

// ---- WARP_13: FF/RW tap ladder ------------------------------------------------------------

impl Rig {
    /// One press/release of a transport button as the hardware delivers it: the `ControlPressed`
    /// role event and the two timestamped edges.
    fn button(&mut self, role: ControlRole, down_at: u64, up_at: u64) {
        self.button_down(role, down_at);
        self.button_up(role, up_at);
    }
    fn button_down(&mut self, role: ControlRole, at_ms: u64) {
        self.tick(at_ms);
        self.input(InputEvent::ControlPressed(role));
        self.input(InputEvent::ControlButton { role, down: true, at_ms });
    }
    fn button_up(&mut self, role: ControlRole, at_ms: u64) {
        self.tick(at_ms);
        self.input(InputEvent::ControlButton { role, down: false, at_ms });
    }
    fn ff(&mut self, at: u64) {
        self.button(ControlRole::FastForward, at, at + 40);
    }
    fn rw(&mut self, at: u64) {
        self.button(ControlRole::Rewind, at, at + 40);
    }
    /// The engine acknowledges the last rate set.
    fn ack(&mut self, permille: i32) {
        self.status(|s| s.target_rate_permille = permille);
    }
    fn popup(&self) -> Option<String> {
        self.app.compositor.status().map(|p| p.lines().join("|"))
    }
}

#[test]
fn ff_taps_step_up_from_the_current_speed_with_no_tap_window() {
    let mut r = Rig::new(1);
    // Two taps 100 ms apart, no status in between: 1.25 then 1.5 (optimistic override).
    r.ff(1_000);
    assert_eq!(r.rates(), vec![1250]);
    assert_eq!(r.popup(), None, "UI_14 §5: speed tap popups stay off Now Playing");
    r.ff(1_100);
    assert_eq!(r.rates(), vec![1500]);
    assert_eq!(r.popup(), None);
    // A stale status (still the old rate) arrives between taps: the next tap still steps
    // from the optimistic value, never repeating a rung.
    r.ack(1000);
    r.ff(1_300);
    assert_eq!(r.rates(), vec![1750]);
    // The engine catches up; a single tap at 1.75 gives 2.0, the next is the end.
    r.ack(1750);
    r.ff(5_000);
    assert_eq!(r.rates(), vec![2000]);
    r.ack(2000);
    r.ff(6_000);
    assert!(r.cmds().is_empty(), "FF at +2.0 is ignored");
    assert_eq!(r.popup(), None, "the end popup is a speed popup: off on Main");
}

#[test]
fn rw_taps_slow_down_through_zero_into_reverse_down_to_the_end() {
    let mut r = Rig::new(1);
    let mut t = 1_000;
    let mut seq = vec![];
    // From pure 1.0 all the way down: 0.75 ... 0.25, -0.25 ... -2.0.
    for _ in 0..11 {
        r.rw(t);
        let rates = r.rates();
        assert_eq!(rates.len(), 1);
        seq.push(rates[0]);
        r.ack(rates[0]);
        t += 1_000;
    }
    assert_eq!(
        seq,
        vec![750, 500, 250, -250, -500, -750, -1000, -1250, -1500, -1750, -2000]
    );
    // No reverse indicator, asterisk or one-time popup anywhere on the way; on Now Playing
    // even the ordinary speed popup is suppressed (UI_14 §5).
    assert_eq!(r.popup(), None);
    r.rw(t);
    assert!(r.cmds().is_empty(), "RW at -2.0 is ignored");
    assert_eq!(r.popup(), None, "the end popup is a speed popup: off on Main");
    // And back up through zero: FF from -0.25 lands on +0.25.
    r.ack(-250);
    r.ff(t + 1_000);
    assert_eq!(r.rates(), vec![250]);
}

#[test]
fn between_rung_values_step_to_the_next_rung_and_zero_is_never_a_rung() {
    for (cur, ff, rw) in [(870, 1000, 750), (1200, 1250, 1000), (150, 250, -250), (-100, 250, -250)] {
        let mut r = Rig::new(1);
        r.status(|s| s.target_rate_permille = cur);
        r.ff(1_000);
        assert_eq!(r.rates(), vec![ff], "FF from {cur}");
        let mut r = Rig::new(1);
        r.status(|s| s.target_rate_permille = cur);
        r.rw(1_000);
        assert_eq!(r.rates(), vec![rw], "RW from {cur}");
    }
}

#[test]
fn tap_boundary_299_ms_is_a_tap_301_ms_is_not() {
    let mut r = Rig::new(1);
    r.button(ControlRole::FastForward, 1_000, 1_299);
    assert_eq!(r.rates(), vec![1250], "299 ms press is a tap");
    let mut r = Rig::new(1);
    r.button(ControlRole::FastForward, 1_000, 1_301);
    assert!(r.cmds().is_empty(), "301 ms is a hold: nothing until WARP_18");
    assert!(r.popup().is_none());
}

#[test]
fn tap_while_the_other_button_is_held_is_ignored_and_taps_leave_everything_else_alone() {
    let mut r = Rig::new(1);
    r.button_down(ControlRole::FastForward, 1_000);
    r.button(ControlRole::Rewind, 1_050, 1_090);
    assert!(r.cmds().is_empty(), "RW tap during an FF hold is ignored");
    r.button_up(ControlRole::FastForward, 1_500);
    assert!(r.cmds().is_empty(), "the FF hold itself does nothing yet");
    // Taps only ever send SetRate: no SeekMs/SeekTo, no warp-mode commands, no Previous.
    let mut r = Rig::new(3);
    for (i, t) in [1_000u64, 2_000, 3_000, 4_000].into_iter().enumerate() {
        if i % 2 == 0 {
            r.rw(t);
        } else {
            r.ff(t);
        }
    }
    let cmds = r.cmds();
    assert!(!cmds.is_empty());
    assert!(cmds
        .iter()
        .all(|c| matches!(c, PlaybackCommand::SetRate { .. })), "{cmds:?}");
}

#[test]
fn nothing_loaded_taps_show_a_popup_and_send_nothing() {
    let mut r = Rig::new(1);
    r.status(|s| {
        s.transport = TransportState::Stopped;
        s.current_item = None;
    });
    r.ff(1_000);
    r.rw(2_000);
    assert!(r.cmds().is_empty());
    assert_eq!(r.popup().as_deref(), Some("Nothing loaded"));
}

#[test]
fn paused_taps_are_stored_like_any_other_and_both_warp_modes_behave_the_same() {
    for mode in [WarpMode::PitchCorrect, WarpMode::PitchShift] {
        let mut r = Rig::new(1);
        r.status(|s| {
            s.transport = TransportState::Paused;
            s.speed_warp_mode = mode;
        });
        r.rw(1_000);
        r.ack(750);
        r.rw(2_000);
        assert_eq!(r.rates(), vec![500], "SetRate while paused ({mode:?})");
        r.ack(500);
        r.rw(3_000);
        assert_eq!(r.rates(), vec![250]);
        // Through zero in either mode: only the plain signed speed is ever announced.
        r.ack(250);
        r.rw(4_000);
        assert_eq!(r.rates(), vec![-250]);
        assert_eq!(r.popup(), None, "no speed popup on Main ({mode:?})");
        // No warp-mode command was ever produced by a tap.
        assert!(r.cmds().is_empty());
    }
}

#[test]
fn negative_rungs_are_gated_on_long_reverse() {
    use plippa_ui::{ladder_step_enabled, ladder_step_with, LONG_REVERSE_AVAILABLE};
    // This build advertises long reverse (WARP_11): a partial revert that flips the const
    // must fail here.
    assert!(LONG_REVERSE_AVAILABLE);
    assert_eq!(ladder_step_enabled(250, -1), Some(-250));
    // Without long reverse the ladder stops at the lowest positive rung.
    assert_eq!(ladder_step_with(250, -1, false), None);
    assert_eq!(ladder_step_with(1000, -1, false), Some(750));
    assert_eq!(ladder_step_with(-500, 1, false), None);
    assert_eq!(ladder_step_with(1000, 1, false), Some(1250));
}

// ---- WARP_18: FF/RW hold = cassette scan ------------------------------------------------

use plippa_core::ScanDir;

fn scan_cmds(cmds: &[PlaybackCommand]) -> Vec<String> {
    cmds.iter()
        .map(|c| match c {
            PlaybackCommand::ScanStart { dir: ScanDir::Forward } => "FF".to_string(),
            PlaybackCommand::ScanStart { dir: ScanDir::Backward } => "RW".to_string(),
            PlaybackCommand::ScanStop => "stop".to_string(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn hold_ff_300_ms_starts_one_scan_and_release_stops_it() {
    let mut r = Rig::new(1);
    r.button_down(ControlRole::FastForward, 1_000);
    assert!(r.cmds().is_empty(), "nothing before the threshold");
    r.tick(1_299);
    assert!(r.cmds().is_empty());
    r.tick(1_300);
    assert_eq!(scan_cmds(&r.cmds()), ["FF"]);
    r.tick(2_000);
    r.tick(2_500);
    assert!(r.cmds().is_empty(), "exactly one ScanStart per hold");
    r.button_up(ControlRole::FastForward, 3_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
}

#[test]
fn ff_held_then_rw_pressed_the_latest_wins_and_its_release_stops() {
    let mut r = Rig::new(1);
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    assert_eq!(scan_cmds(&r.cmds()), ["FF"]);
    r.button_down(ControlRole::Rewind, 1_500);
    r.tick(1_800);
    assert_eq!(scan_cmds(&r.cmds()), ["RW"], "the later press takes over");
    // The earlier button's release is ignored; the winner's release stops.
    r.button_up(ControlRole::FastForward, 2_000);
    assert!(r.cmds().is_empty());
    r.button_up(ControlRole::Rewind, 2_500);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
    // Both pressed 50 ms apart: the earlier (FF) is ignored, only RW scans.
    let mut r = Rig::new(1);
    r.button_down(ControlRole::FastForward, 1_000);
    r.button_down(ControlRole::Rewind, 1_050);
    r.tick(1_300);
    r.tick(1_350);
    assert_eq!(scan_cmds(&r.cmds()), ["RW"]);
}

#[test]
fn a_250_ms_press_is_a_tap_and_a_hold_never_touches_the_set_speed() {
    let mut r = Rig::new(1);
    r.button(ControlRole::FastForward, 1_000, 1_250);
    assert_eq!(r.rates(), vec![1250], "a tap, no scan");
    r.ack(1500);
    r.button_down(ControlRole::FastForward, 5_000);
    r.tick(5_400);
    r.button_up(ControlRole::FastForward, 6_000);
    let cmds = r.cmds();
    assert!(cmds.iter().all(|c| !matches!(c, PlaybackCommand::SetRate { .. })), "{cmds:?}");
}

#[test]
fn hold_with_nothing_loaded_is_a_popup_and_sends_nothing() {
    let mut r = Rig::new(1);
    r.status(|s| {
        s.transport = TransportState::Stopped;
        s.current_item = None;
    });
    r.button_down(ControlRole::Rewind, 1_000);
    r.tick(1_300);
    assert!(r.cmds().is_empty());
    assert_eq!(r.popup().as_deref(), Some("Nothing loaded"));
    r.button_up(ControlRole::Rewind, 1_500);
    assert!(r.cmds().is_empty(), "no ScanStop for a scan that never started");
}

// ---- UI_15: Play while FF/RW is held is an ordinary pause/play toggle again --------------
//
// UI_14 turned a Play tap during an FF/RW hold into "snap to 100 % and lock the held button".
// UI_15 disables that gesture (the mechanism is kept in `ffrw_play_reset`, unused), so these
// tests assert the restored behaviour: a normal toggle, the set speed untouched, and the held
// button's own release still ends its scan once.

#[test]
fn play_tap_while_ff_held_toggles_pause_and_never_resets_the_speed() {
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1250);
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    assert_eq!(scan_cmds(&r.cmds()), ["FF"], "the hold scan is running");
    // Play tapped while FF is held: a plain toggle (deferred to the release while a scan runs).
    r.play_edge(true, 1_400);
    r.play_edge(false, 1_450);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::Pause],
        "Playing → Pause; no ResetRateInstant, no scan stop from the Play key"
    );
    assert_eq!(r.app.model.set_speed_permille(), 1250, "the set speed is untouched");
    // The held button's own release still ends its scan, exactly once.
    r.button_up(ControlRole::FastForward, 3_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
}

#[test]
fn play_tap_before_the_hold_threshold_toggles_and_the_hold_still_scans() {
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1250);
    r.button_down(ControlRole::FastForward, 1_000);
    r.play_edge(true, 1_100);
    r.play_edge(false, 1_150);
    assert_eq!(r.cmds(), vec![PlaybackCommand::Pause], "a normal toggle, no reset");
    assert_eq!(r.app.model.set_speed_permille(), 1250);
    // The hold threshold passes with the button still down: the scan starts as usual.
    r.tick(1_400);
    assert_eq!(scan_cmds(&r.cmds()), ["FF"]);
    // Release ends it once: no tap, no extra toggle.
    r.button_up(ControlRole::FastForward, 1_500);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
    assert!(r.cmds().is_empty());
}

/// UI_15 §4: paused → hold RW → Play. The scan plays audio; Play is a real Play command (not a
/// rate snap), so the transport and the Play LED follow the sound instead of staying paused.
#[test]
fn play_while_rw_held_from_paused_plays_and_the_led_follows() {
    let mut r = Rig::new(1);
    r.status(|s| {
        s.transport = TransportState::Paused;
        s.target_rate_permille = 1000;
    });
    assert_eq!(
        PlayButtonLed::from_transport(TransportState::Paused, true),
        PlayButtonLed::Standby,
        "paused shows the standby LED"
    );
    r.button_down(ControlRole::Rewind, 1_000);
    r.tick(1_300);
    assert_eq!(scan_cmds(&r.cmds()), ["RW"], "the paused scan is running (and audible)");
    // The engine publishes the scan in the session while it runs: the transport still reads
    // Paused (a *paused* scan), which is exactly the state UI_15 §4 is about.
    r.status(|s| {
        s.transport = TransportState::Paused;
        s.scan = ScanState::Backward;
    });
    let down = InputEvent::ControlButton {
        role: ControlRole::Play,
        down: true,
        at_ms: 1_400,
    };
    let up = InputEvent::ControlButton {
        role: ControlRole::Play,
        down: false,
        at_ms: 1_450,
    };
    r.tick(1_400);
    // UI_15 follow-up: with a scan held the press toggles at once, so the loop lights the
    // optimistic LED on the *press* (queried before the event is dispatched, as the loop does).
    assert!(
        r.app.event_toggles_play(&down),
        "the loop lights the optimistic LED on the press"
    );
    r.input(down.clone());
    assert_eq!(r.cmds(), vec![PlaybackCommand::Play], "Paused → Play, never a reset");
    assert!(
        !r.app.event_toggles_play(&up),
        "the release is inert: the press already toggled"
    );
    r.input(up);
    assert!(r.cmds().is_empty(), "exactly one toggle per press");
    // The engine confirms: the LED is blue (playing) and the picture advances.
    r.status(|s| s.transport = TransportState::Playing);
    assert_eq!(
        PlayButtonLed::from_transport(TransportState::Playing, true),
        PlayButtonLed::Playing
    );
    // Release RW: the scan ends where it stands and playback continues.
    r.button_up(ControlRole::Rewind, 2_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
}

/// UI_15 §1/§3: during a scan a Play tap toggles and the FF release still ends only its own
/// scan; the next Play press is a normal toggle (no lock, no deferred leftovers).
#[test]
fn play_during_a_scan_never_touches_the_scan_or_the_speed() {
    let mut r = Rig::new(1);
    r.status(|s| {
        s.transport = TransportState::Paused;
        s.target_rate_permille = 1250;
    });
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    assert_eq!(scan_cmds(&r.cmds()), ["FF"]);
    r.status(|s| {
        s.transport = TransportState::Paused;
        s.scan = ScanState::Forward;
    });
    // UI_15 follow-up: the scan makes the press immediate (no deferral, so no long-press
    // reset can arm); the release is inert.
    r.play_edge(true, 1_400);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::Play],
        "a toggle on the press, and the scan keeps running under the held button"
    );
    r.play_edge(false, 1_450);
    assert!(r.cmds().is_empty(), "the release never toggles twice");
    r.button_up(ControlRole::FastForward, 1_500);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
    // The next Play press pauses normally (the scan has ended, so the transport rules again).
    r.status(|s| s.transport = TransportState::Playing);
    r.play_edge(true, 2_000);
    assert_eq!(r.cmds(), vec![PlaybackCommand::Pause]);
    r.play_edge(false, 2_050);
    assert!(r.cmds().is_empty());
}

// ---- UI_15: the encoder is the scan speed while FF/RW is held ----------------------------

#[test]
fn encoder_steps_the_scan_speed_while_held_and_never_touches_the_set_speed() {
    use plippa_core::SCAN_SPEED_LADDER_PERMILLE;
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1500);
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    assert_eq!(scan_cmds(&r.cmds()), ["FF"]);
    assert_eq!(r.app.scan_speed_permille(), 1000, "a fresh device starts at the default");
    // One detent up: 150 % of the usual scan, and the status line says so.
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.cmds(), vec![PlaybackCommand::ScanRate { permille: 1500 }]);
    assert_eq!(r.app.scan_speed_permille(), 1500);
    assert_eq!(r.popup().as_deref(), Some("SCAN 150%"));
    // Clamped at the top: extra detents send nothing and never wrap.
    for expected in [2000, 4000] {
        r.input(InputEvent::MenuRotate(1));
        assert_eq!(r.cmds(), vec![PlaybackCommand::ScanRate { permille: expected }]);
    }
    for _ in 0..4 {
        r.input(InputEvent::MenuRotate(1));
        assert!(r.cmds().is_empty(), "no wrap past 400 %");
    }
    assert_eq!(r.app.scan_speed_permille(), *SCAN_SPEED_LADDER_PERMILLE.last().unwrap());
    // Down through the default to the bottom, clamped there.
    for expected in [2000, 1500, 1000, 500, 250, 100, 50, 10] {
        r.input(InputEvent::MenuRotate(-1));
        assert_eq!(r.cmds(), vec![PlaybackCommand::ScanRate { permille: expected }]);
    }
    r.input(InputEvent::MenuRotate(-1));
    assert!(r.cmds().is_empty(), "no wrap past 1 %");
    assert_eq!(r.app.scan_speed_permille(), SCAN_SPEED_LADDER_PERMILLE[0]);
    // The scan speed never became a set speed: no SetRate, and the painted set speed is 150 %.
    assert_eq!(r.app.model.set_speed_permille(), 1500);
    // Release: the scan stops and nothing is re-sent for the set speed.
    r.button_up(ControlRole::FastForward, 2_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
    // A later tap steps the *set* speed ladder from 150 %, untouched by the scan speed.
    r.ack(1500);
    r.ff(3_000);
    assert_eq!(r.rates(), vec![1750]);
}

/// UI_18 (owner): the scan speed chosen with the encoder **sticks while the loaded track is
/// the same one** — it survives the release and the next hold — and a different item puts it
/// back to the default. The audio thread always starts a scan at the default, so a sticky
/// value has to travel with each `ScanStart`.
#[test]
fn the_scan_speed_sticks_across_holds_while_the_same_track_is_loaded() {
    let mut r = Rig::new(1);
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    r.cmds();
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.app.scan_speed_permille(), 1500);
    r.cmds();
    r.button_up(ControlRole::FastForward, 2_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
    // The next hold on the same item: still 150 %, and it is re-sent with the scan so the
    // fresh scan does not spin up at the default.
    r.button_down(ControlRole::FastForward, 3_000);
    r.tick(3_300);
    assert_eq!(r.app.scan_speed_permille(), 1500, "sticky for the loaded track");
    assert_eq!(
        r.cmds(),
        vec![
            PlaybackCommand::ScanStart {
                dir: ScanDir::Forward
            },
            PlaybackCommand::ScanRate { permille: 1500 },
        ],
        "the sticky speed rides with the new scan"
    );
    // RW too: the same track, the same speed.
    r.button_down(ControlRole::Rewind, 4_000);
    r.tick(4_300);
    assert_eq!(r.app.scan_speed_permille(), 1500);
    assert_eq!(
        r.cmds(),
        vec![
            PlaybackCommand::ScanStart {
                dir: ScanDir::Backward
            },
            PlaybackCommand::ScanRate { permille: 1500 },
        ]
    );
    r.button_up(ControlRole::Rewind, 5_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
}

#[test]
fn loading_a_different_track_returns_the_scan_speed_to_the_default() {
    let mut r = Rig::new(1);
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    r.cmds();
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.app.scan_speed_permille(), 1500);
    r.cmds();
    r.button_up(ControlRole::FastForward, 2_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
    // A different item is loaded (the same physical track is not: identity is the id).
    r.status(|s| s.current_item = Some(PlayableItemId::new(2)));
    r.button_down(ControlRole::FastForward, 3_000);
    r.tick(3_300);
    assert_eq!(
        r.app.scan_speed_permille(),
        1000,
        "a new track starts from the default again"
    );
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::ScanStart {
            dir: ScanDir::Forward
        }],
        "the default needs no ScanRate"
    );
    r.button_up(ControlRole::FastForward, 4_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
}

#[test]
fn the_encoder_is_the_scan_speed_on_every_route_while_held() {
    // Held means held: the scan speed is steerable without opening a parameter, and a rotate
    // does not scroll the list overlay.
    let mut r = Rig::new(3);
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    r.cmds();
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.cmds(), vec![PlaybackCommand::ScanRate { permille: 1500 }]);
    r.button_up(ControlRole::FastForward, 2_000);
    assert_eq!(scan_cmds(&r.cmds()), ["stop"]);
    // Released: the encoder behaves as before (a rotation on Now Playing steps the jog).
    r.input(InputEvent::MenuRotate(1));
    let cmds = r.cmds();
    assert!(
        cmds.iter().all(|c| !matches!(c, PlaybackCommand::ScanRate { .. })),
        "{cmds:?}"
    );
}

// ---- UI_16 §7: the menu inversion stops at menu-style lists ------------------------------

/// UI_16 §7 inverted the encoder for menus and the library only. The Adjust band and the
/// held-scan speed ladder keep exactly the direction they had before (tempo: right steps the
/// magnitude down; scan: right climbs the ladder). The jog ring is covered in `tests/jog.rs`.
#[test]
fn menu_inversion_does_not_reach_the_adjust_band_or_the_held_scan() {
    // Adjust band (tempo).
    let mut r = Rig::new(1);
    r.enter_tempo();
    let mut t = 2_000;
    assert_eq!(r.slow_click(&mut t, 1), vec![1010], "tempo: right still steps up");
    assert_eq!(r.slow_click(&mut t, -1), vec![1000], "tempo: left still steps down");

    // Held scan speed (UI_15): right still climbs.
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1500);
    r.button_down(ControlRole::FastForward, 1_000);
    r.tick(1_300);
    r.cmds();
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::ScanRate { permille: 1500 }],
        "scan: right still climbs the ladder"
    );
    assert_eq!(r.app.scan_speed_permille(), 1500);
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.cmds(), vec![PlaybackCommand::ScanRate { permille: 1000 }]);
}

/// UI_14 §5: speed popups are suppressed on Now Playing only. Every other route keeps them.
#[test]
fn speed_popups_are_suppressed_on_main_but_kept_elsewhere() {
    // Long-press Play (ramped ResetRate): no popup on Main, popup on Library.
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1250);
    r.play_edge(true, 0);
    r.tick(800);
    assert_eq!(r.cmds(), vec![PlaybackCommand::ResetRate], "long press stays ramped");
    assert_eq!(r.popup(), None, "Main: no play-reset popup");

    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1250);
    r.app.nav.force_push(Route::Library);
    r.play_edge(true, 0);
    r.tick(800);
    assert_eq!(r.cmds(), vec![PlaybackCommand::ResetRate]);
    assert_eq!(r.popup().as_deref(), Some("100% pure"), "other routes keep it");

    // Tap-rate popup: silent on Main, shown on Library.
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1250);
    r.ff(1_000);
    assert_eq!(r.rates(), vec![1500]);
    assert_eq!(r.popup(), None, "Main: no tap-rate popup");

    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 1250);
    r.app.nav.force_push(Route::Library);
    r.ff(1_000);
    assert_eq!(r.rates(), vec![1500]);
    assert_eq!(r.popup().as_deref(), Some("1.50x"));

    // Tap-end popup at the ladder end: silent on Main, shown on Library.
    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 2000);
    r.ff(1_000);
    assert!(r.rates().is_empty());
    assert_eq!(r.popup(), None, "Main: no tap-end popup");

    let mut r = Rig::new(1);
    r.status(|s| s.target_rate_permille = 2000);
    r.app.nav.force_push(Route::Library);
    r.ff(1_000);
    assert_eq!(r.popup().as_deref(), Some("2.00x max"));
}
