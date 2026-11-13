//! WARP_15 / WARP_16: jog gesture (live target, one `SeekTo` on idle) and the target/actual
//! display. Mock input + injected `Tick` clock + frame buffer; no engine, no wall-clock time.

use plippa_board::ControlRole;
use plippa_core::{PlayableItemId, SessionState, TransportState};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand, PlaybackStatus};
use plippa_ui::ui::tokens::FG;
use plippa_ui::{
    InputEvent, MockWifi, TimecodeUnit, UiApp, UiConfig, UiOutgoing, JOG_COMMIT_IDLE_MS,
    SCREEN_W, WAVE_BAND_Y,
};

const DUR: u64 = 180_000;

struct Rig {
    app: UiApp,
    wifi: MockWifi,
    rev: u64,
    playing: bool,
}

impl Rig {
    fn new(playing: bool) -> Self {
        let mut r = Rig {
            app: UiApp::new(UiConfig::default()),
            wifi: MockWifi::default(),
            rev: 0,
            playing,
        };
        r.status(65_000, |_| {});
        r.tick(1_000);
        r
    }
    fn status(&mut self, pos: u64, f: impl FnOnce(&mut SessionState)) {
        let mut s = SessionState::default();
        s.transport = if self.playing {
            TransportState::Playing
        } else {
            TransportState::Paused
        };
        s.title = "Tone".into();
        s.artist = "Artist".into();
        s.position_ms = pos;
        s.rate_permille = 1000;
        s.duration_ms = Some(DUR);
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
    fn seeks(&mut self) -> Vec<u64> {
        let mut all = vec![];
        for o in self.app.take_outgoing() {
            if let UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) = o {
                match c {
                    PlaybackCommand::SeekTo { target_ms } => all.push(target_ms),
                    // WARP_18: a held FF/RW legitimately sends the scan commands.
                    PlaybackCommand::ScanStart { .. } | PlaybackCommand::ScanStop => {}
                    other => panic!("unexpected command while jogging: {other:?}"),
                }
            }
        }
        all
    }
    fn frame(&mut self) -> Vec<u16> {
        // Compose directly: `present` correctly skips an unchanged screen, and these tests
        // need the pixels in both cases.
        self.app.composed_pixels()
    }
}

#[test]
fn one_slow_click_moves_the_target_by_the_fine_time_step() {
    // UI_16 §5: there is no `Jog step` setting; the finest click is derived from Fine time.
    // Frame units give `1000 / fps` whole ms (rounded down — a ms step cannot land exactly on
    // every frame boundary), decimal tenths 100 ms, hundredths 10 ms, `Off` 100 ms.
    for (unit, step) in [
        (TimecodeUnit::Off, 100u32),
        (TimecodeUnit::DecTenths, 100),
        (TimecodeUnit::Frames24, 41),
        (TimecodeUnit::Frames25, 40),
        (TimecodeUnit::Frames30, 33),
        (TimecodeUnit::Frames50, 20),
        (TimecodeUnit::Frames60, 16),
        (TimecodeUnit::Frames75, 13),
        (TimecodeUnit::DecHundredths, 10),
    ] {
        let mut r = Rig::new(false);
        r.app.play_mode.settings.timecode_unit = Some(unit);
        r.input(InputEvent::MenuRotate(1));
        assert!(r.seeks().is_empty());
        r.tick(1_000 + JOG_COMMIT_IDLE_MS);
        assert_eq!(r.seeks(), vec![65_000 + u64::from(step)], "{unit:?}");
    }
}

#[test]
fn the_menu_inversion_does_not_reach_the_jog_gesture() {
    // UI_16 §7: the main menu, System, Playback settings and the library read top-down, but
    // the jog ring is not a list: a right detent is still forward, a left detent backward.
    let mut r = Rig::new(false);
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(
        r.app.model.jog_view.expect("live view").target_ms,
        65_100,
        "right = forward, unchanged by the menu inversion"
    );
    let mut r = Rig::new(false);
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(
        r.app.model.jog_view.expect("live view").target_ms,
        64_900,
        "left = backward, unchanged by the menu inversion"
    );
}

#[test]
fn changing_fine_time_changes_the_finest_click_at_once() {
    // UI_16 §5: the jog path reads the derived step live, so a menu change applies to the very
    // next click without a restart.
    let mut r = Rig::new(false);
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::Frames24);
    r.input(InputEvent::MenuRotate(1));
    r.tick(1_000 + JOG_COMMIT_IDLE_MS);
    assert_eq!(r.seeks(), vec![65_041], "24 fps = 41 ms");
    r.status(65_041, |_| {});
    // Idle past the velocity reset so the second click is a first click again (1x step).
    r.tick(2_000);
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecHundredths);
    r.input(InputEvent::MenuRotate(1));
    r.tick(2_000 + JOG_COMMIT_IDLE_MS);
    assert_eq!(r.seeks(), vec![65_051], "hundredths = 10 ms");
}

#[test]
fn a_burst_is_one_seek_to_and_two_bursts_are_two() {
    let mut r = Rig::new(true);
    let mut t = 2_000;
    for _ in 0..30 {
        t += 100;
        r.tick(t);
        r.input(InputEvent::MenuRotate(1));
        assert!(r.seeks().is_empty(), "nothing is sent while the knob turns");
    }
    let live = r.app.model.jog_view.expect("live view").target_ms;
    // UI_16 §5: the finest step is one frame (13 ms at the default 75 fps); 30 rapid clicks
    // land well beyond 30 unaccelerated clicks.
    assert!(live > 65_000 + 30 * 13, "accelerated: {live}");
    r.tick(t + JOG_COMMIT_IDLE_MS - 1);
    assert!(r.seeks().is_empty());
    r.tick(t + JOG_COMMIT_IDLE_MS);
    assert_eq!(r.seeks(), vec![live]);
    // The engine reports the running jump; a second burst continues from its target.
    r.status(66_000, |s| s.seek_target_ms = Some(live));
    t += 200;
    r.tick(t);
    r.input(InputEvent::MenuRotate(1));
    r.tick(t + JOG_COMMIT_IDLE_MS);
    let second = r.seeks();
    assert_eq!(second.len(), 1);
    assert!(
        second[0] > live && second[0] < live + 1_000,
        "continues from the running target"
    );
}

#[test]
fn leaving_jog_commits_at_once_and_sends_nothing_else() {
    for leave in [InputEvent::MenuEnter, InputEvent::Back] {
        let mut r = Rig::new(true);
        r.input(InputEvent::MenuRotate(3));
        r.input(InputEvent::MenuRotate(1));
        assert!(r.seeks().is_empty());
        r.input(leave);
        assert_eq!(r.seeks().len(), 1);
        r.tick(5_000);
        assert!(r.seeks().is_empty(), "exactly one SeekTo per gesture");
    }
}

#[test]
fn engine_offline_discards_the_gesture() {
    let mut r = Rig::new(true);
    r.app.set_engine_connected(false);
    r.app.take_outgoing();
    r.input(InputEvent::MenuRotate(1));
    r.tick(1_000 + JOG_COMMIT_IDLE_MS);
    assert!(r.seeks().is_empty());
    r.app.set_engine_connected(true);
    r.app.take_outgoing();
    r.tick(9_000);
    assert!(r.seeks().is_empty(), "nothing queued for later");
}

#[test]
fn jog_during_a_held_ffrw_sends_nothing() {
    let mut r = Rig::new(true);
    r.input(InputEvent::ControlButton {
        role: ControlRole::FastForward,
        down: true,
        at_ms: 1_000,
    });
    r.app.take_outgoing();
    r.input(InputEvent::MenuRotate(1));
    r.tick(1_300);
    assert!(r.seeks().is_empty());
    assert!(r.app.model.jog_view.is_none());
}

#[test]
fn target_clamps_at_both_ends_and_commits_the_clamped_value() {
    let mut r = Rig::new(true);
    r.input(InputEvent::MenuRotate(-100));
    r.tick(1_200);
    assert_eq!(r.seeks(), vec![0]);
    let mut r = Rig::new(true);
    let mut t = 1_000;
    for _ in 0..200 {
        t += 5;
        r.tick(t);
        r.input(InputEvent::MenuRotate(1));
    }
    r.tick(t + 200);
    assert_eq!(r.seeks(), vec![DUR - 250]);
}

#[test]
fn display_shows_target_immediately_and_the_actual_keeps_playing() {
    let mut r = Rig::new(true);
    // UI_16 §5: pin Fine time so "every click is the finest step" is a readable 100 ms.
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
    let idle = r.frame();
    for k in 1..=6u64 {
        r.tick(1_000 + k * 400); // slow clicks: every click is the finest step
        r.status(65_000 + k * 400, |_| {}); // the engine keeps reporting the playing position
        r.input(InputEvent::MenuRotate(1));
        let v = r.app.model.jog_view.expect("view");
        assert_eq!(
            v.target_ms,
            65_400 + k * 100,
            "target follows every click at once"
        );
        assert!(!v.paused);
    }
    let v = r.app.model.jog_view.unwrap();
    // The actual playhead keeps playing at the set speed while the target moves.
    assert!(
        v.actual_ms >= 67_300 && v.actual_ms <= 67_500,
        "actual {}",
        v.actual_ms
    );
    assert!(v.actual_ms > v.target_ms + 1_000);
    assert!(v.show_marker);
    // WAVE_02: the picture is the readout. The band's playhead is the live jog target (the
    // old amber bar marker is gone) and the footer clock agrees with it.
    r.tick(3_450);
    assert_eq!(r.app.model.playhead_ms, v.target_ms, "centre is the live target");
    let f = r.frame();
    let w = SCREEN_W as usize;
    let playhead_x = (SCREEN_W / 2) as usize;
    let y = (WAVE_BAND_Y + 5) as usize;
    let px_actual = f[y * w + playhead_x];
    assert!(
        px_actual == FG,
        "fixed playhead column at x={playhead_x}, y={y}: got {px_actual:#06x}, want {FG:#06x}"
    );
    assert_ne!(f, idle, "jogging moves the picture / clock");
}

#[test]
fn landing_collapses_the_display_and_the_screen_returns_to_the_plain_layout() {
    let mut r = Rig::new(true);
    let baseline = r.frame();
    r.input(InputEvent::MenuRotate(1));
    r.tick(1_000 + JOG_COMMIT_IDLE_MS);
    assert_eq!(r.seeks().len(), 1);
    let target = 65_100;
    // Chase running, then landed (status without a target).
    r.status(65_050, |s| s.seek_target_ms = Some(target));
    r.tick(1_300);
    assert!(r.app.model.jog_view.unwrap().readout);
    r.status(65_100, |_| {});
    r.tick(1_400);
    let v = r.app.model.jog_view.expect("landed hold");
    assert!(v.readout && !v.show_now);
    r.tick(1_400 + 1_000); // marker gone, readout still held
    assert!(!r.app.model.jog_view.unwrap().show_marker);
    r.status(66_700, |_| {});
    r.tick(1_400 + 1_600);
    assert!(r.app.model.jog_view.is_none());
    r.status(65_000, |_| {});
    assert_eq!(
        r.frame(),
        baseline,
        "not jogging: identical to the plain layout"
    );
}

#[test]
fn paused_shows_one_marker_and_no_now_line() {
    let mut r = Rig::new(false);
    r.input(InputEvent::MenuRotate(2));
    let v = r.app.model.jog_view.expect("view");
    assert!(v.paused && !v.show_now && !v.show_marker);
    r.tick(1_000 + JOG_COMMIT_IDLE_MS);
    assert_eq!(r.seeks().len(), 1);
    let v = r.app.model.jog_view.expect("view after commit");
    assert!(v.paused && !v.show_now && !v.show_marker);
}

#[test]
fn a_stale_status_never_moves_the_playhead_tick_backwards() {
    let mut r = Rig::new(true);
    r.input(InputEvent::MenuRotate(1));
    r.tick(1_500);
    let before = r.app.model.jog_view.unwrap().actual_ms;
    // Status refresh with the position the engine had 200 ms ago.
    r.status(65_000 + 300, |_| {});
    r.tick(1_501);
    let after = r.app.model.jog_view.unwrap().actual_ms;
    assert!(after + 100 >= before, "{before} -> {after}");
}

// ---- WARP_20: sweep items (acceleration cap, reversal, idle restart) ------------------------

#[test]
fn acceleration_is_capped_per_click_and_a_reversal_or_idle_restarts_at_the_step() {
    let mut r = Rig::new(true); // 180 s item: cap = 5% = 9 s per click
    // UI_16 §5: pin Fine time so "the plain step" is a readable 100 ms.
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
    let mut t = 2_000u64;
    let mut last = 65_000u64;
    let mut max_step = 0u64;
    for _ in 0..60 {
        t += 15;
        r.tick(t);
        r.input(InputEvent::MenuRotate(1));
        let v = r.app.model.jog_view.expect("live").target_ms;
        max_step = max_step.max(v - last);
        last = v;
    }
    assert!(max_step > 1_000, "a fast turn moves more than the step: {max_step}");
    assert!(max_step <= 9_000, "capped at 5% of a 3 min item: {max_step}");
    // Reversal: the first click back is the plain step, whatever the speed was.
    t += 15;
    r.tick(t);
    r.input(InputEvent::MenuRotate(-1));
    let v = r.app.model.jog_view.unwrap().target_ms;
    assert_eq!(last - v, 100, "reversal restarts at the Fine time step");
    // Idle (> 350 ms but inside the commit window is impossible; the gesture commits first):
    // the next click after a pause is again the plain step.
    t += 400;
    r.tick(t);
    let _ = r.seeks();
    r.status(v, |_| {});
    r.input(InputEvent::MenuRotate(1));
    let w = r.app.model.jog_view.unwrap().target_ms;
    assert!(w.abs_diff(v) <= 150, "idle restart: step {} from {v}", w as i64 - v as i64);
}

// ---- WARP_19: jog into the neighbouring item ------------------------------------------------

fn all_cmds(r: &mut Rig) -> Vec<PlaybackCommand> {
    r.app
        .take_outgoing()
        .into_iter()
        .filter_map(|o| match o {
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
            _ => None,
        })
        .collect()
}

fn outside_status(r: &mut Rig, pos: u64, prev: Option<u64>, next: Option<u64>) {
    r.app.play_mode.settings.seek_outside_track = Some(true);
    r.status(pos, |s| {
        s.seek_outside_track = true;
        s.prev_duration_ms = prev;
        s.next_duration_ms = next;
    });
    let _ = all_cmds(r);
}

#[test]
fn jog_back_past_zero_commits_a_seek_to_item_in_the_previous_item() {
    let mut r = Rig::new(false);
    // UI_16 §5: Fine time Dec 1/10 gives a 100 ms click, so start 50 ms in to overshoot zero.
    outside_status(&mut r, 50, Some(60_000), Some(90_000));
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
    r.input(InputEvent::MenuRotate(-1));
    let v = r.app.model.jog_view.expect("live view");
    assert_eq!(v.item_delta, -1, "the live target is in the previous item");
    assert_eq!(v.target_ms, 0, "the bar shows the clamped end");
    // The 100 ms step overshoots by less than the previous item's end guard (250 ms), so the
    // target lands on that guard.
    let want = 60_000 - plippa_core::SEEK_END_GUARD_MS;
    assert_eq!(v.item_target_ms, want, "prev, clamped to its end guard");
    r.tick(1_000 + JOG_COMMIT_IDLE_MS);
    assert_eq!(
        all_cmds(&mut r),
        vec![PlaybackCommand::SeekToItem { item_delta: -1, target_ms: want }]
    );
}

#[test]
fn jog_past_the_end_commits_into_the_next_item_and_off_stays_a_clamped_seek_to() {
    let mut r = Rig::new(false);
    outside_status(&mut r, DUR - 300, Some(60_000), Some(90_000));
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
    r.input(InputEvent::MenuRotate(1));
    let v = r.app.model.jog_view.expect("live view");
    assert_eq!(
        (v.item_delta, v.item_target_ms),
        (1, 100 - 50),
        "overshoot past the guard"
    );
    r.tick(1_000 + JOG_COMMIT_IDLE_MS);
    assert_eq!(
        all_cmds(&mut r),
        vec![PlaybackCommand::SeekToItem { item_delta: 1, target_ms: 50 }]
    );
    // Setting off (the engine publishes no neighbours): the same clicks clamp inside the item.
    let mut r = Rig::new(false);
    r.status(DUR - 300, |_| {});
    r.app.play_mode.settings.timecode_unit = Some(TimecodeUnit::DecTenths);
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.app.model.jog_view.expect("live view").item_delta, 0);
    r.tick(1_000 + JOG_COMMIT_IDLE_MS);
    assert_eq!(
        all_cmds(&mut r),
        vec![PlaybackCommand::SeekTo { target_ms: DUR - plippa_core::SEEK_END_GUARD_MS }]
    );
}

/// UI_20 (owner bug): the playhead must not inherit the previous item's position across a
/// track change. `actual_ms` extrapolates from the last sample and `on_status` keeps a forward
/// floor while a new sample only trails that extrapolation; both are only meaningful **within**
/// one item, so carried over a tap they held the old item's playhead for a status interval and
/// then snapped to the new item (the "moves once, then jumps back" the owner saw).
#[test]
fn a_new_item_does_not_inherit_the_old_playhead() {
    let mut r = Rig::new(true);
    r.status(65_000, |_| {}); // sampled at the injected clock
    r.tick(1_100);
    let old = r.app.playhead_ms_at(1_100);
    assert_eq!(old, 65_100, "dead-reckoned on the old item");

    // Tap: item 2 reports its own position, just behind the old item's extrapolation (inside
    // the forward-floor tolerance, which is what used to keep the old playhead).
    r.status(65_050, |s| s.current_item = Some(PlayableItemId::new(2)));
    assert_eq!(
        r.app.playhead_ms_at(1_100),
        65_050,
        "the new item's playhead is its own, not the old item's extrapolation"
    );
}
