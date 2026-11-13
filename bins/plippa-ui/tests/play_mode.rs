//! WARP_10/19, UI_16, PLAY_04: the merged System > Playback settings screen (the four session
//! mode rows on top, then the settings), System > Debug (one row), persistence, reconcile, the
//! SW2 tap cycle and hold. Mock input, injected clock (`Tick { now_ms }`), frame-buffer checks;
//! no engine, no wall-clock time.

use plippa_board::ControlRole;
use plippa_core::{PlayableItemId, SessionState, TransportState, WarpMode};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand, PlaybackStatus};
use plippa_ui::ui::tokens::{
    ACCENT, ACCENT_2, BG, CONTENT_BOTTOM, CONTENT_TOP, FG_DIM, ROW_H, SPACE_1, SPACE_2,
};
use plippa_ui::{
    speed_percent_text, PlayModeRow, PlayModeValues, PlayModeSettings, PlayModeStore, PlayModesRow,
    Route, InputEvent, MockWifi, NavStack, SettingsRow, TimecodeUnit, UiApp, UiConfig, UiOutgoing,
    PLAYMODE_HOLD_MS, PLAYBACK_SETTINGS_ROWS, PLAY_MODE_ROWS, PLAY_MODES_ROWS,
    PLAY_MODE_VISIBLE_ROWS, SCREEN_H, SCREEN_W, UP_NEXT_ROWS_Y,
};
use std::path::PathBuf;

struct Rig {
    app: UiApp,
    wifi: MockWifi,
    path: PathBuf,
    rev: u64,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("play_mode.json");
        Self::with_dir(dir, path)
    }

    fn with_dir(dir: tempfile::TempDir, path: PathBuf) -> Self {
        Rig {
            app: UiApp::new(UiConfig {
                play_mode_path: Some(path.clone()),
                ..UiConfig::default()
            }),
            wifi: MockWifi::default(),
            path,
            rev: 0,
            _dir: dir,
        }
    }

    /// Restart: a new app on the same file.
    fn restart(self) -> Self {
        let Rig { path, _dir, .. } = self;
        Self::with_dir(_dir, path)
    }

    fn input(&mut self, ev: InputEvent) {
        self.app.handle_input(ev, &mut self.wifi);
    }
    fn tick(&mut self, ms: u64) {
        self.input(InputEvent::Tick { now_ms: ms });
    }
    fn button(&mut self, down: bool, at_ms: u64) {
        self.input(InputEvent::ControlButton {
            role: ControlRole::PlayMode,
            down,
            at_ms,
        });
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
    fn status(&mut self, speed: WarpMode, seek: WarpMode) {
        self.status_with(|s| {
            s.speed_warp_mode = speed;
            s.seek_warp_mode = seek;
        });
    }
    /// Engine status carrying the PLAY_01 session modes.
    fn status_with(&mut self, edit: impl FnOnce(&mut SessionState)) {
        let mut s = SessionState::default();
        s.transport = TransportState::Playing;
        s.current_item = Some(PlayableItemId::new(1));
        edit(&mut s);
        self.rev += 1;
        self.app.handle_ipc(IpcMessage::PlaybackStatus(
            PlaybackStatus::from_session_rev(s, self.rev),
        ));
    }
    fn frame(&mut self) -> Vec<u16> {
        // Compose directly: `present` only sends the changed bounding box (WAVE_02), so a
        // fresh mock display would hold just that patch.
        self.app.composed_pixels()
    }
    /// Walk System -> Playback settings with the normal menu path.
    fn open_via_system(&mut self) {
        self.app.nav.force_push(Route::System);
        self.app.nav.focus = 1; // Network is row 0 (SECFIX_02)
        self.input(InputEvent::MenuEnter);
        assert_eq!(*self.app.nav.current(), Route::SystemPlayMode);
    }
    fn enter_row(&mut self, row: usize) {
        self.app.nav.focus = row;
        self.input(InputEvent::MenuEnter);
    }
    fn file(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }
}

fn count_colour(px: &[u16], y0: usize, y1: usize, colour: u16) -> usize {
    (y0..y1.min(SCREEN_H as usize))
        .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
        .filter(|&(x, y)| px[y * SCREEN_W as usize + x] == colour)
        .count()
}

/// Index of a persisted settings row on the merged Playback settings screen.
fn setting_row(row: PlayModeRow) -> usize {
    PLAYBACK_SETTINGS_ROWS
        .iter()
        .position(|r| r.setting() == Some(row))
        .unwrap_or_else(|| panic!("{row:?} is not on the merged screen"))
}

/// Index of a session-mode row on the merged Playback settings screen.
fn mode_row(row: PlayModesRow) -> usize {
    PLAYBACK_SETTINGS_ROWS
        .iter()
        .position(|r| r.mode() == Some(row))
        .unwrap_or_else(|| panic!("{row:?} is not on the merged screen"))
}

#[test]
fn system_menu_has_no_separate_play_modes_screen_and_the_merged_list_is_modes_first() {
    let nav = NavStack::new();
    assert_eq!(
        nav.system_items(),
        &[
            Route::SystemNetwork,
            Route::SystemPlayMode,
            Route::SystemStores,
            Route::SystemAudio,
            Route::SystemDebug
        ],
        "the session modes are the top rows of Playback settings, not a second System screen"
    );
    assert_eq!(Route::SystemPlayMode.title(), "Playback settings");
    assert_eq!(Route::SystemAudio.title(), "Audio");
    assert_eq!(Route::SystemDebug.title(), "Debug");
    assert_eq!(
        PLAY_MODE_ROWS,
        [
            PlayModeRow::PlaybackSpeed,
            PlayModeRow::SpeedWarp,
            PlayModeRow::SeekWarp,
            PlayModeRow::SeekOutsideTrack,
            PlayModeRow::WaveformZoom,
            PlayModeRow::FineTime,
            PlayModeRow::ArtLeds,
        ],
        "UI_16 §5 removed Jog step; BCS_11 moved the tap consent to Network"
    );
    assert_eq!(
        PLAY_MODE_ROWS
            .iter()
            .filter(|r| r.edits_by_scroll())
            .copied()
            .collect::<Vec<_>>(),
        vec![
            PlayModeRow::PlaybackSpeed,
            PlayModeRow::WaveformZoom,
            PlayModeRow::FineTime
        ],
        "only the >3-option rows enter an edit state"
    );
    assert_eq!(
        PLAYBACK_SETTINGS_ROWS,
        [
            SettingsRow::Mode(PlayModesRow::Shuffle),
            SettingsRow::Mode(PlayModesRow::Repeat),
            SettingsRow::Mode(PlayModesRow::PauseAfterEach),
            SettingsRow::Mode(PlayModesRow::RepeatReshuffles),
            SettingsRow::Setting(PlayModeRow::PlaybackSpeed),
            SettingsRow::Setting(PlayModeRow::SpeedWarp),
            SettingsRow::Setting(PlayModeRow::SeekWarp),
            SettingsRow::Setting(PlayModeRow::SeekOutsideTrack),
            SettingsRow::Setting(PlayModeRow::WaveformZoom),
            SettingsRow::Setting(PlayModeRow::FineTime),
            SettingsRow::Setting(PlayModeRow::ArtLeds),
        ],
        "the four session rows come first, then the settings"
    );
    assert_eq!(mode_row(PlayModesRow::Shuffle), 0);
    assert_eq!(mode_row(PlayModesRow::RepeatReshuffles), 3);
    assert_eq!(setting_row(PlayModeRow::PlaybackSpeed), 4);
}

#[test]
fn fresh_device_shows_defaults_cursor_and_description() {
    let mut r = Rig::new();
    r.open_via_system();
    let v = r.app.model.play_mode;
    assert_eq!(v.value_text(PlayModeRow::SpeedWarp), "Correct");
    assert_eq!(v.value_text(PlayModeRow::SeekWarp), "Shift");
    let f0 = r.frame();
    // UI_10 geometry: rows start at `CONTENT_TOP` and step by `ROW_H`; the focus cue is the
    // 2 px `ACCENT` ring inside the row.
    let band = |row: usize| {
        let y = CONTENT_TOP as usize + row * ROW_H as usize;
        (y, y + ROW_H as usize)
    };
    assert_eq!(v.value_text(PlayModeRow::SeekOutsideTrack), "Off");
    assert_eq!(v.value_text(PlayModeRow::WaveformZoom), "30s");
    assert_eq!(v.value_text(PlayModeRow::FineTime), "75 fps");
    assert_eq!(v.value_text(PlayModeRow::PlaybackSpeed), "100%");
    // The window starts at the top, so the first eight rows are the ones painted.
    for row in 0..PLAY_MODE_VISIBLE_ROWS {
        let (a, b) = band(row);
        assert_eq!(
            count_colour(&f0, a, b, ACCENT) > 0,
            row == 0,
            "row {row} cursor"
        );
    }
    // The description band starts under the rule: the visible rows, a 2 px gap, the rule, then
    // the description. It holds `FG_DIM` `Small` text down to `CONTENT_BOTTOM`.
    let desc_top = CONTENT_TOP + PLAY_MODE_VISIBLE_ROWS as u16 * ROW_H + 2;
    let desc = |f: &[u16]| count_colour(f, desc_top as usize, CONTENT_BOTTOM as usize, FG_DIM);
    assert!(desc(&f0) > 200, "description line painted in FG_DIM");
    // UI_16 §6/§7: the menu wraps at both ends and the encoder is inverted (left = down).
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.app.nav.focus, 1, "left moves down the list");
    let f1 = r.frame();
    assert_ne!(f0, f1, "the cursor and the description follow the focus");
    assert!(count_colour(&f1, band(1).0, band(1).1, ACCENT) > 0);
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.app.nav.focus, 0, "right moves back up");
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(
        r.app.nav.focus,
        PLAYBACK_SETTINGS_ROWS.len() - 1,
        "up from the first row wraps to the last"
    );
    let f2 = r.frame();
    assert_ne!(f0, f2, "wrapping to the last row repaints");
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.app.nav.focus, 0, "down from the last row wraps to the first");
    // Nothing was stored just by looking.
    assert_eq!(r.file(), "");
    assert!(r.cmds().is_empty());
}

#[test]
fn enter_toggles_warp_rows_stores_only_that_key_and_sends_the_command() {
    let mut r = Rig::new();
    r.open_via_system();
    r.enter_row(setting_row(PlayModeRow::SpeedWarp));
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchShift)]
    );
    let text = r.file();
    assert!(
        text.contains("\"speed_warp_mode\": \"pitch_shift\""),
        "{text}"
    );
    assert!(
        !text.contains("seek_warp_mode") && !text.contains("jog_step_ms"),
        "{text}"
    );
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Speed: pitch shift"]
    );
    r.enter_row(setting_row(PlayModeRow::SeekWarp));
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect)]
    );
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Seek: pitch correct"]
    );
    // Survives a restart.
    let r = r.restart();
    assert_eq!(r.app.model.play_mode.speed, WarpMode::PitchShift);
    assert_eq!(r.app.model.play_mode.seek, WarpMode::PitchCorrect);
    assert_eq!(
        PlayModeStore::load_or_default(&r.path).settings.warp.speed,
        Some(WarpMode::PitchShift)
    );
}

#[test]
fn fine_time_derives_the_finest_click_and_an_old_jog_key_is_ignored() {
    // UI_16 §5: the `Jog step` row and its setting are gone; the finest click always follows
    // Fine time. An existing `play_mode.json` that still has `jog_step_ms` must keep loading.
    let mut r = Rig::new();
    r.open_via_system();
    assert!(
        !PLAY_MODE_ROWS.iter().any(|row| row.label().contains("Jog")),
        "the Jog step row is deleted"
    );
    std::fs::write(&r.path, r#"{"jog_step_ms": 500}"#).unwrap();
    let mut r = r.restart();
    assert_eq!(
        r.app.model.play_mode.timecode_unit,
        TimecodeUnit::Frames75,
        "an ignored jog key leaves the Fine time default in place"
    );
    // Saving another setting drops the dead key rather than carrying it forward.
    r.open_via_system();
    r.enter_row(setting_row(PlayModeRow::SpeedWarp));
    let text = r.file();
    assert!(text.contains("speed_warp_mode"), "{text}");
    assert!(!text.contains("jog_step_ms"), "{text}");
    // Every Fine time choice round-trips through the file and sets the derived click.
    for (unit, step) in [
        (TimecodeUnit::Off, 100),
        (TimecodeUnit::DecTenths, 100),
        (TimecodeUnit::Frames24, 41),
        (TimecodeUnit::Frames25, 40),
        (TimecodeUnit::Frames30, 33),
        (TimecodeUnit::Frames50, 20),
        (TimecodeUnit::Frames60, 16),
        (TimecodeUnit::Frames75, 13),
        (TimecodeUnit::DecHundredths, 10),
    ] {
        let settings = PlayModeSettings {
            timecode_unit: Some(unit),
            ..Default::default()
        };
        std::fs::write(&r.path, settings.to_json()).unwrap();
        let loaded = PlayModeStore::load_or_default(&r.path);
        assert_eq!(loaded.settings.timecode_unit, Some(unit), "{unit:?}");
        assert_eq!(loaded.settings.resolve_jog_step(), step, "{unit:?}");
    }
}

#[test]
fn reconcile_sends_each_mode_once_per_status_revision_and_on_connect() {
    let r = Rig::new();
    std::fs::write(
        &r.path,
        r#"{"speed_warp_mode":"pitch_shift","seek_warp_mode":"pitch_correct"}"#,
    )
    .unwrap();
    let mut r = r.restart();
    // Engine reports the defaults: both differ from the stored choices.
    r.status(WarpMode::PitchCorrect, WarpMode::PitchShift);
    let sent = r.cmds();
    assert_eq!(
        sent,
        vec![
            PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchShift),
            PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect)
        ]
    );
    // Same revision again: nothing more (idempotent).
    let rev = r.rev;
    r.app.handle_ipc(IpcMessage::PlaybackStatus(
        PlaybackStatus::from_session_rev(
            {
                let mut s = SessionState::default();
                s.current_item = Some(PlayableItemId::new(1));
                s
            },
            rev,
        ),
    ));
    assert!(r.cmds().is_empty(), "once per revision per setting");
    // A newer status that still differs: one more each.
    r.status(WarpMode::PitchCorrect, WarpMode::PitchShift);
    assert_eq!(r.cmds().len(), 2);
    // Engine now matches: silence.
    r.status(WarpMode::PitchShift, WarpMode::PitchCorrect);
    assert!(r.cmds().is_empty());
    // Connection lost and re-established: both are re-sent once, unconditionally.
    r.app.set_engine_connected(false);
    r.app.set_engine_connected(true);
    assert_eq!(
        r.cmds(),
        vec![
            PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchShift),
            PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect)
        ]
    );
    // A restarted engine starts its revisions over: its first (low) revision is still applied.
    r.rev = 0;
    r.status(WarpMode::PitchShift, WarpMode::PitchCorrect);
    assert!(r.cmds().is_empty());
    assert!(r
        .app
        .model
        .playback
        .as_ref()
        .is_some_and(|p| p.revision == 1));
}

#[test]
fn default_flip_with_no_stored_value_matches_a_default_engine() {
    let mut r = Rig::new();
    r.status(WarpMode::PitchCorrect, WarpMode::PitchShift);
    assert!(
        r.cmds().is_empty(),
        "nothing chosen: follow the engine default, send nothing"
    );
}

#[test]
fn sw2_tap_cycles_one_rung_and_699ms_hold_is_still_a_tap() {
    // PLAY_04 §1: a release before the hold threshold is a tap and advances one rung — from
    // anywhere, without opening a route. 699 ms is still a tap.
    let mut r = Rig::new();
    r.tick(0);
    r.button(true, 100);
    r.tick(300);
    r.tick(799); // 699 ms down
    r.button(false, 799);
    assert_eq!(*r.app.nav.current(), Route::Main);
    assert!(!r.app.compositor.main_menu_visible());
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Shuffle)]
    );
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Play mode", "Shuffle"]
    );
    // A quick tap advances again, still without opening a route.
    r.button(true, 1_000);
    r.button(false, 1_050);
    assert_eq!(*r.app.nav.current(), Route::Main);
    // Shuffle/Off -> Normal/Queue changes both fields (order first).
    assert_eq!(
        r.cmds(),
        vec![
            PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Normal),
            PlaybackCommand::SetRepeatMode(plippa_core::RepeatMode::Queue),
        ]
    );
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Play mode", "Repeat all"]
    );
}

#[test]
fn sw2_tap_cycles_the_five_rungs() {
    // PLAY_04 §1 + owner decision 5/6: five taps from the default return to it, in order.
    let mut r = Rig::new();
    r.tick(0);
    let cases: [(&str, Vec<PlaybackCommand>); 5] = [
        (
            "Shuffle",
            vec![PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Shuffle)],
        ),
        (
            "Repeat all",
            vec![
                PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Normal),
                PlaybackCommand::SetRepeatMode(plippa_core::RepeatMode::Queue),
            ],
        ),
        (
            "Shuffle + repeat",
            vec![PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Shuffle)],
        ),
        (
            "Repeat one",
            vec![
                PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Normal),
                PlaybackCommand::SetRepeatMode(plippa_core::RepeatMode::Item),
            ],
        ),
        (
            "Normal",
            vec![PlaybackCommand::SetRepeatMode(plippa_core::RepeatMode::Off)],
        ),
    ];
    let mut at = 10;
    for (i, (label, cmds)) in cases.iter().enumerate() {
        r.button(true, at);
        r.button(false, at + 20);
        assert_eq!(&r.cmds(), cmds, "tap {i} sends only what changed");
        assert_eq!(
            r.app.compositor.status().unwrap().lines(),
            ["Play mode", *label],
            "tap {i} names the rung"
        );
        assert!(!r.app.compositor.main_menu_visible());
        assert_eq!(*r.app.nav.current(), Route::Main);
        at += 100;
    }
    assert_eq!(r.app.model.play_mode_state(), plippa_core::PLAY_MODE_CYCLE[0]);
}

#[test]
fn sw2_tap_sends_only_the_changed_fields() {
    // Normal/Off -> Shuffle/Off changes the order only: one command, not two.
    let mut r = Rig::new();
    r.tick(0);
    r.button(true, 10);
    r.button(false, 30);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Shuffle)]
    );
    // Shuffle/Queue -> Repeat one changes both fields, so both commands go out.
    let mut r = Rig::new();
    r.status_with(|s| {
        s.order_mode = plippa_core::OrderMode::Shuffle;
        s.repeat_mode = plippa_core::RepeatMode::Queue;
    });
    r.tick(0);
    r.button(true, 10);
    r.button(false, 30);
    assert_eq!(
        r.cmds(),
        vec![
            PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Normal),
            PlaybackCommand::SetRepeatMode(plippa_core::RepeatMode::Item),
        ]
    );
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Play mode", "Repeat one"]
    );
}

#[test]
fn sw2_tap_from_repeat_item_returns_to_normal_off() {
    // Rung 5 (Normal + repeat one) is a real engine state, not just a UI mirror.
    let mut r = Rig::new();
    r.status_with(|s| {
        s.order_mode = plippa_core::OrderMode::Normal;
        s.repeat_mode = plippa_core::RepeatMode::Item;
    });
    r.tick(0);
    assert_eq!(r.app.model.play_mode_state().repeat, plippa_core::RepeatMode::Item);
    r.button(true, 10);
    r.button(false, 30);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetRepeatMode(plippa_core::RepeatMode::Off)]
    );
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Play mode", "Normal"]
    );
}

#[test]
fn sw2_hold_still_opens_playback_settings() {
    // The hold threshold is unchanged by PLAY_04, and a hold never sends a mode command or a
    // SetRate: it is navigation only.
    let mut r = Rig::new();
    r.tick(0);
    r.button(true, 0);
    r.tick(PLAYMODE_HOLD_MS);
    assert_eq!(*r.app.nav.current(), Route::SystemPlayMode);
    r.button(false, PLAYMODE_HOLD_MS + 100);
    assert!(r.cmds().is_empty(), "hold sends no command");
    assert_eq!(r.app.model.play_mode_state(), plippa_core::PLAY_MODE_CYCLE[0]);
}

#[test]
fn sw2_tap_with_nothing_loaded_still_sends() {
    // No session yet (fresh device): the mode is engine state and the command still goes.
    let mut r = Rig::new();
    r.tick(0);
    r.button(true, 10);
    r.button(false, 30);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetOrderMode(plippa_core::OrderMode::Shuffle)]
    );
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Play mode", "Shuffle"]
    );
}

#[test]
fn sw2_hold_opens_the_menu_once_and_back_returns_without_overlay() {
    let mut r = Rig::new();
    r.tick(0);
    r.button(true, 0);
    r.tick(PLAYMODE_HOLD_MS - 1);
    assert_eq!(*r.app.nav.current(), Route::Main);
    r.tick(PLAYMODE_HOLD_MS);
    assert_eq!(*r.app.nav.current(), Route::SystemPlayMode);
    let depth = r.app.nav.depth();
    r.tick(PLAYMODE_HOLD_MS + 500);
    r.button(false, PLAYMODE_HOLD_MS + 600);
    assert_eq!(r.app.nav.depth(), depth, "once; the release is ignored");
    // Back: origin was Now Playing, no main-menu overlay.
    r.input(InputEvent::Back);
    assert_eq!(*r.app.nav.current(), Route::Main);
    assert!(!r.app.compositor.main_menu_visible());

    // From Library and from System: back returns to that screen.
    for origin in [Route::Library, Route::System] {
        let mut r = Rig::new();
        r.app.nav.force_push(origin.clone());
        r.tick(0);
        r.button(true, 0);
        r.tick(PLAYMODE_HOLD_MS);
        assert_eq!(*r.app.nav.current(), Route::SystemPlayMode);
        r.button(false, 1_000);
        r.input(InputEvent::Back);
        assert_eq!(*r.app.nav.current(), origin);
        assert!(!r.app.compositor.main_menu_visible());
    }
}

#[test]
fn sw2_hold_is_ignored_in_associate_wait_and_when_already_open() {
    let mut r = Rig::new();
    r.app.nav.force_push(Route::LibraryAssociate);
    r.tick(0);
    r.button(true, 0);
    r.tick(PLAYMODE_HOLD_MS + 10);
    assert_eq!(*r.app.nav.current(), Route::LibraryAssociate);
    r.button(false, 900);

    let mut r = Rig::new();
    r.open_via_system();
    let depth = r.app.nav.depth();
    r.tick(0);
    r.button(true, 0);
    r.tick(PLAYMODE_HOLD_MS + 10);
    assert_eq!(r.app.nav.depth(), depth, "already open: no second push");
}

#[test]
fn sw2_hold_hides_an_open_main_menu_overlay_and_opens_the_screen() {
    let mut r = Rig::new();
    r.input(InputEvent::Back); // overlay
    assert!(r.app.compositor.main_menu_visible());
    r.tick(0);
    r.button(true, 0);
    r.tick(PLAYMODE_HOLD_MS);
    assert!(!r.app.compositor.main_menu_visible());
    assert_eq!(*r.app.nav.current(), Route::SystemPlayMode);
}

#[test]
fn fine_time_enters_scrolls_clamps_and_persists_on_exit() {
    // UI_16 §1/§3/§4: `Fine time` (last row) is a bounded ladder edited by enter → scroll →
    // exit. Enter never changes the value, scrolling applies it live to the footer clocks,
    // the ends clamp, and the value is persisted (with one popup) only on exit.
    let mut r = Rig::new();
    r.open_via_system();
    // A loaded session so the footer draws its clocks.
    let mut s = SessionState::default();
    s.transport = TransportState::Playing;
    s.current_item = Some(PlayableItemId::new(1));
    s.position_ms = 65_432;
    s.duration_ms = Some(180_000);
    r.rev += 1;
    r.app.handle_ipc(IpcMessage::PlaybackStatus(
        PlaybackStatus::from_session_rev(s, r.rev),
    ));
    assert_eq!(r.app.model.footer_left(), "01:05:32 / 03:00:00");
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::FineTime),
        "75 fps"
    );

    let row = setting_row(PlayModeRow::FineTime);
    assert_eq!(
        row,
        PLAYBACK_SETTINGS_ROWS.len() - 2,
        "the second-to-last row, before Art LEDs"
    );
    // Enter only arms the row: no value change, no popup, nothing stored.
    r.enter_row(row);
    assert_eq!(r.app.model.play_mode.editing, Some(PlayModeRow::FineTime));
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::FineTime),
        "75 fps"
    );
    assert_eq!(r.app.model.footer_left(), "01:05:32 / 03:00:00");
    assert_eq!(r.file(), "");
    assert!(r.app.compositor.status().is_none(), "no popup while editing");

    // Scroll: 75 -> 60 -> 50 -> 30 -> 25 -> 24 -> Dec .1 -> Off (clamped there).
    let mut values = Vec::new();
    for _ in 0..9 {
        r.input(InputEvent::MenuRotate(1));
        values.push(r.app.model.play_mode.value_text(PlayModeRow::FineTime));
    }
    assert_eq!(
        values,
        [
            "60 fps", "50 fps", "30 fps", "25 fps", "24 fps", "Dec .1", "Off", "Off", "Off"
        ],
        "the top of the ladder clamps, no wrap"
    );
    assert_eq!(
        r.app.model.footer_left(),
        "1:05 / 3:00",
        "off is plain MM:SS for both clocks"
    );
    assert!(r.cmds().is_empty(), "UI-only, like before");
    assert_eq!(r.file(), "", "not persisted while the row is still open");

    // Back leaves editing only (the route stays) and names the value it kept.
    r.input(InputEvent::Back);
    assert_eq!(*r.app.nav.current(), Route::SystemPlayMode);
    assert_eq!(r.app.model.play_mode.editing, None);
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::FineTime),
        "Off",
        "Back keeps the value the scroll reached"
    );
    assert!(r.file().contains("\"timecode_unit\": \"off\""), "{}", r.file());
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Fine time: Off"]
    );

    // Down the ladder again: Dec .1, 24 fps, ... , Dec .01 (clamped at the bottom).
    r.enter_row(row);
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::FineTime),
        "Dec .1"
    );
    assert_eq!(
        r.app.model.footer_left(),
        "01:05.4 / 03:00.0",
        "tenths: truncated, one digit"
    );
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::FineTime),
        "24 fps"
    );
    assert_eq!(r.app.model.footer_left(), "01:05:10 / 03:00:00");
    let mut last = String::new();
    for _ in 0..6 {
        r.input(InputEvent::MenuRotate(-1));
        last = r.app.model.play_mode.value_text(PlayModeRow::FineTime).to_string();
    }
    assert_eq!(last, "Dec .01", "the bottom of the ladder");
    assert_eq!(
        r.app.model.footer_left(),
        "01:05.43 / 03:00.00",
        "hundredths: truncated, two digits"
    );
    // Enter leaves the edit state, persists once and names the value.
    r.input(InputEvent::MenuEnter);
    assert_eq!(r.app.model.play_mode.editing, None);
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Fine time: Dec .01"]
    );
    assert!(
        r.file().contains("\"timecode_unit\": \"decimal_100\""),
        "{}",
        r.file()
    );
    // One Back now leaves the screen as before.
    r.input(InputEvent::Back);
    assert_eq!(*r.app.nav.current(), Route::System);

    // Persisted across a restart, still live on the footer unit.
    let r = r.restart();
    assert_eq!(
        PlayModeStore::load_or_default(&r.path).settings.resolve_timecode_unit().text(),
        "Dec .01"
    );
    assert_eq!(r.app.model.play_mode.timecode_unit.text(), "Dec .01");
}

#[test]
fn legacy_decimal_key_still_loads_as_tenths() {
    // UI_16 §4: the pre-UI_16 `decimal` key keeps working (it is the 1/10 variant now).
    let mut r = Rig::new();
    std::fs::write(&r.path, r#"{"timecode_unit": "decimal"}"#).unwrap();
    r = r.restart();
    assert_eq!(
        r.app.model.play_mode.timecode_unit,
        TimecodeUnit::DecTenths
    );
    // Garbage falls back to the 75 fps default (UI_14 owner decision, unchanged).
    std::fs::write(&r.path, r#"{"timecode_unit": "nonsense"}"#).unwrap();
    r = r.restart();
    assert_eq!(
        r.app.model.play_mode.timecode_unit,
        TimecodeUnit::Frames75
    );
    // `off` stays explicit.
    std::fs::write(&r.path, r#"{"timecode_unit": "off"}"#).unwrap();
    r = r.restart();
    assert_eq!(r.app.model.play_mode.timecode_unit, TimecodeUnit::Off);
}

#[test]
fn play_mode_row_texts_fit_the_screen() {
    // Every description wraps into the space under the rows (no line cut off the frame).
    let mut r = Rig::new();
    r.open_via_system();
    for row in 0..PLAYBACK_SETTINGS_ROWS.len() {
        r.app.nav.focus = row;
        let f = r.frame();
        // The frame is cleared to the `BG` ground, so "empty" means "no ink above it".
        let last_band = &f[(SCREEN_H as usize - 6) * SCREEN_W as usize..];
        assert!(
            last_band.iter().all(|p| *p == BG),
            "row {row}: text reaches the bottom edge"
        );
    }
    // PLAY_04: the setting is "Pause after each"; the retired "stop" wording must not come back
    // into any user-visible row label or description.
    for row in PLAY_MODES_ROWS {
        for text in [row.label(), row.row_label(), row.description()] {
            assert!(
                !text.to_ascii_lowercase().contains("stop"),
                "Play modes row {row:?} still uses the retired stop wording: {text}"
            );
        }
    }
    for row in PLAY_MODE_ROWS {
        for text in [row.label(), row.row_label(), row.description()] {
            assert!(
                !text.to_ascii_lowercase().contains("stop"),
                "Playback settings row {row:?} still uses the retired stop wording: {text}"
            );
        }
    }
}

#[test]
fn the_merged_rows_and_description_fit_the_panel() {
    // The window (eight rows), the rule and the description of the focused row must sit inside
    // `CONTENT_BOTTOM`; the list is longer than the window, so the cursor drives the window.
    let mut r = Rig::new();
    open_play_modes(&mut r);
    let rows_bottom = CONTENT_TOP + PLAY_MODE_VISIBLE_ROWS as u16 * ROW_H;
    let desc_top = rows_bottom + 2;
    assert!(
        rows_bottom <= CONTENT_BOTTOM,
        "the visible rows end above CONTENT_BOTTOM"
    );
    assert!(
        desc_top + 2 * 13 <= CONTENT_BOTTOM,
        "two description lines fit above CONTENT_BOTTOM"
    );
    for focus in 0..PLAYBACK_SETTINGS_ROWS.len() {
        r.app.nav.focus = focus;
        let f = r.frame();
        // The focused row is always inside the window: find which band carries the ring.
        let first = focus.saturating_add(1).saturating_sub(PLAY_MODE_VISIBLE_ROWS);
        for row in 0..PLAY_MODE_VISIBLE_ROWS {
            let y0 = (CONTENT_TOP + row as u16 * ROW_H) as usize;
            let y1 = y0 + ROW_H as usize;
            assert_eq!(
                count_colour(&f, y0, y1, ACCENT) > 0,
                first + row == focus,
                "focus {focus}: ring on window row {row} (list row {})",
                first + row
            );
        }
        // The description is painted in its own band under the rows and never overlaps them.
        assert!(
            count_colour(&f, desc_top as usize, CONTENT_BOTTOM as usize, FG_DIM) > 100,
            "focus {focus}: description painted under the rows"
        );
        let last_band = &f[(SCREEN_H as usize - 6) * SCREEN_W as usize..];
        assert!(
            last_band.iter().all(|p| *p == BG),
            "focus {focus}: description reaches the bottom edge"
        );
    }
    assert_eq!(PLAYBACK_SETTINGS_ROWS.len(), 11);
}

fn status_outside(r: &mut Rig, outside: bool) {
    let mut s = SessionState::default();
    s.transport = TransportState::Playing;
    s.current_item = Some(PlayableItemId::new(1));
    s.seek_outside_track = outside;
    r.rev += 1;
    r.app.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
        s, r.rev,
    )));
}

/// WARP_19: the new row toggles, persists explicitly, sends the command, is re-sent on connect
/// (only when On) and on a status mismatch (once per revision); a fresh device is Off.
#[test]
fn seek_outside_track_row_toggles_persists_and_reconciles() {
    let mut r = Rig::new();
    r.open_via_system();
    assert!(!r.app.model.play_mode.seek_outside_track, "fresh device = Off");
    let outside_row = setting_row(PlayModeRow::SeekOutsideTrack);
    r.enter_row(outside_row);
    assert_eq!(r.cmds(), vec![PlaybackCommand::SetSeekOutsideTrack(true)]);
    assert!(r.file().contains("\"seek_outside_track\": true"), "{}", r.file());
    assert_eq!(r.app.model.play_mode.value_text(PlayModeRow::SeekOutsideTrack), "On");
    r.enter_row(outside_row);
    assert_eq!(r.cmds(), vec![PlaybackCommand::SetSeekOutsideTrack(false)]);
    assert!(r.file().contains("\"seek_outside_track\": false"));
    r.enter_row(outside_row);
    let _ = r.cmds();
    // Restart with On stored: connect re-sends it; an engine already On needs nothing more.
    let mut r = r.restart();
    assert!(r.app.model.play_mode.seek_outside_track);
    r.app.set_engine_connected(false);
    r.app.set_engine_connected(true);
    let sent = r.cmds();
    assert!(sent.contains(&PlaybackCommand::SetSeekOutsideTrack(true)), "{sent:?}");
    status_outside(&mut r, true);
    assert!(!r
        .cmds()
        .contains(&PlaybackCommand::SetSeekOutsideTrack(true)));
    // The engine lost it (restart): the status mismatch re-sends once per revision.
    status_outside(&mut r, false);
    assert_eq!(
        r.cmds()
            .iter()
            .filter(|c| **c == PlaybackCommand::SetSeekOutsideTrack(true))
            .count(),
        1
    );
    // A fresh device never sends it on connect.
    let mut fresh = Rig::new();
    fresh.app.set_engine_connected(false);
    fresh.app.set_engine_connected(true);
    assert!(!fresh
        .cmds()
        .iter()
        .any(|c| matches!(c, PlaybackCommand::SetSeekOutsideTrack(_))));
}

/// ART_10: the last row toggles on the encoder click, names the new value in the popup, is
/// written to `play_mode.json`, comes back after a restart and persists.
#[test]
fn art_leds_row_toggles_pops_up_persists_and_applies_immediately() {
    let mut r = Rig::new();
    r.open_via_system();
    assert_eq!(PLAY_MODE_ROWS.len(), 7);
    assert_eq!(PLAY_MODE_ROWS[6], PlayModeRow::ArtLeds, "last settings row");
    assert!(r.app.model.play_mode.art_leds, "fresh device = On");
    assert!(!r.file().contains("art_leds"), "nothing written until chosen");
    assert_eq!(r.app.model.play_mode.value_text(PlayModeRow::ArtLeds), "On");

    let row = setting_row(PlayModeRow::ArtLeds);
    r.enter_row(row);
    assert!(!r.app.model.play_mode.art_leds);
    assert_eq!(r.app.model.play_mode.value_text(PlayModeRow::ArtLeds), "Off");
    assert_eq!(r.app.compositor.status().unwrap().lines(), ["Art LEDs: Off"]);
    assert!(r.file().contains("\"art_leds\": false"), "{}", r.file());
    assert!(r.cmds().is_empty(), "UI-only: nothing goes to the engine");

    let mut r = r.restart();
    assert!(!r.app.model.play_mode.art_leds, "Off survives a restart");
    r.open_via_system();
    r.enter_row(row);
    assert!(r.app.model.play_mode.art_leds);
    assert_eq!(r.app.compositor.status().unwrap().lines(), ["Art LEDs: On"]);
    assert!(r.file().contains("\"art_leds\": true"), "{}", r.file());
}

/// BCS_11: the tap consent lives on System > Network (`Skip internet ask on tap`) now, not on
/// Playback settings. Its persistence is unchanged: `play_mode.json`, explicit choices only.
#[test]
fn the_tap_consent_is_not_a_playback_settings_row_any_more() {
    assert!(
        !PLAY_MODE_ROWS
            .iter()
            .any(|row| row.label().to_ascii_lowercase().contains("internet")),
        "the row moved to Network"
    );
    assert!(
        !PLAYBACK_SETTINGS_ROWS.iter().any(|row| {
            row.setting()
                .is_some_and(|r| r.label().to_ascii_lowercase().contains("internet"))
        }),
        "and it is not on the merged screen either"
    );
    // The setting still loads and resolves from the same file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("play_mode.json");
    std::fs::write(&path, r#"{"skip_internet_consent": true}"#).unwrap();
    let store = PlayModeStore::load_or_default(&path);
    assert!(store.settings.resolve_skip_internet_consent());
}

/// UI_16 §8: System > Debug holds the `Rec enables NFC poll` setting, followed by the AUDIO_05
/// `Audio info` navigation row. The setting toggles on the click, persists, stays silent (no
/// popup, no notice) and a fresh device is Off.
#[test]
fn debug_screen_toggles_the_nfc_poll_setting_silently_and_persists_it() {
    let mut r = Rig::new();
    // System (index 4 = Debug, last after Network, Playback settings, Stores and Audio).
    r.app.nav.force_push(Route::System);
    r.app.nav.focus = 4;
    r.input(InputEvent::MenuEnter);
    assert_eq!(*r.app.nav.current(), Route::SystemDebug);
    assert_eq!(Route::SystemDebug.title(), "Debug");
    assert!(!r.app.model.play_mode.rec_enables_nfc_poll, "fresh device = Off");
    assert!(!r.app.nfc_rec_gate(), "Off: the loop polls continuously");
    // Toggle On: stored, no command, no popup.
    r.input(InputEvent::MenuEnter);
    assert!(r.app.model.play_mode.rec_enables_nfc_poll);
    assert!(r.app.nfc_rec_gate());
    assert!(r.file().contains("\"rec_enables_nfc_poll\": true"), "{}", r.file());
    assert!(r.app.compositor.status().is_none(), "no popup explaining the gate");
    assert!(r.cmds().is_empty(), "UI-only: nothing is sent to the engine");
    // The list now has two rows: rotating moves between them and stays on the screen.
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.app.nav.focus, 1, "Audio info follows the setting");
    assert_eq!(*r.app.nav.current(), Route::SystemDebug);
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.app.nav.focus, 0);
    assert_eq!(*r.app.nav.current(), Route::SystemDebug);
    // Restart: the setting (and the loop gate) come back from the same file.
    let mut r = r.restart();
    assert!(r.app.model.play_mode.rec_enables_nfc_poll);
    assert!(r.app.nfc_rec_gate());
    // Back leaves the screen; toggling Off is persisted explicitly (not "unset").
    r.app.nav.force_push(Route::System);
    r.app.nav.focus = 5; // Debug is still the last System row.
    r.input(InputEvent::MenuEnter);
    r.input(InputEvent::MenuEnter);
    assert!(!r.app.model.play_mode.rec_enables_nfc_poll);
    assert!(r.file().contains("\"rec_enables_nfc_poll\": false"), "{}", r.file());
    assert!(r.app.compositor.status().is_none());
    assert!(!r.restart().app.nfc_rec_gate());
}

/// UI_16 §7 follow-up: the Wi-Fi SSID list is a menu list, so it follows the encoder
/// inversion (left = down); the password keyboard is a character grid and keeps the raw
/// detent. Both are on the same route, so this pins the split.
#[test]
fn the_wifi_ssid_list_follows_the_menu_inversion_but_the_keyboard_does_not() {
    let mut r = Rig::new();
    r.app.nav.force_push(Route::Wifi);
    r.wifi.networks = vec!["HomeLAN".into(), "CafeGuest".into(), "Studio".into()];
    r.app.model.wifi = Some(plippa_ui::WifiForm::new(vec![
        "HomeLAN".into(),
        "CafeGuest".into(),
        "Studio".into(),
    ]));
    // Current → Scan list (three SSIDs, focus on the first). Enter re-syncs from the backend.
    r.input(InputEvent::MenuEnter);
    {
        let form = r.app.model.wifi.as_ref().unwrap();
        assert_eq!(form.phase, plippa_ui::WifiPhase::Scan);
        assert_eq!(form.focus, 0);
    }
    // Left = down the list, right = up, exactly like every other menu list.
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.app.model.wifi.as_ref().unwrap().focus, 1, "left is down");
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(r.app.model.wifi.as_ref().unwrap().focus, 0, "right is up");
    // A further left from the top wraps (the list was already infinite).
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.app.model.wifi.as_ref().unwrap().focus, 1);
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.app.model.wifi.as_ref().unwrap().focus, 2);
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(r.app.model.wifi.as_ref().unwrap().focus, 0, "wraps back to the top");

    // Unknown network → the password keyboard, where the detent stays un-inverted.
    r.input(InputEvent::MenuEnter);
    {
        let form = r.app.model.wifi.as_ref().unwrap();
        assert_eq!(form.phase, plippa_ui::WifiPhase::Keyboard);
    }
    let mut direct = r.app.model.wifi.clone().unwrap();
    let mut wifi = MockWifi::default();
    direct.handle(plippa_ui::WifiFormEvent::Rotate(1), &mut wifi);
    r.input(InputEvent::MenuRotate(1));
    assert_eq!(
        r.app
            .model
            .wifi
            .as_ref()
            .unwrap()
            .keyboard
            .charset_index(),
        direct.keyboard.charset_index(),
        "the keyboard must see the raw detent"
    );
}

// ---- PLAY_04: the session modes on the merged Playback settings screen --------------------
// (They used to have their own System screen; they are the top four rows now.)

use plippa_core::{OrderMode, RepeatMode, SourceOrder};
use plippa_ipc_proto::{NoticeSeverity, UserNotice, NOTICE_NO_FIXED_ORDER};
use plippa_ui::AdjustParam;

fn status_mode(
    r: &mut Rig,
    order: OrderMode,
    repeat: RepeatMode,
    pause_after_each: bool,
    reshuffle_on_repeat: bool,
    source_order: SourceOrder,
) {
    let mut s = SessionState::default();
    s.transport = TransportState::Playing;
    s.current_item = Some(PlayableItemId::new(1));
    s.order_mode = order;
    s.repeat_mode = repeat;
    s.pause_after_each = pause_after_each;
    s.reshuffle_on_repeat = reshuffle_on_repeat;
    s.source_order = source_order;
    r.rev += 1;
    r.app.handle_ipc(IpcMessage::PlaybackStatus(
        PlaybackStatus::from_session_rev(s, r.rev),
    ));
}

/// Open the merged screen through the System menu (Playback settings is the second entry).
fn open_play_modes(r: &mut Rig) {
    r.open_via_system();
}

#[test]
fn play_modes_rows_show_the_engine_values() {
    let mut r = Rig::new();
    open_play_modes(&mut r);
    assert_eq!(Route::SystemPlayMode.title(), "Playback settings");
    // The session rows are the first four; each reads the engine mirror.
    assert_eq!(mode_row(PlayModesRow::Shuffle), 0);
    assert_eq!(mode_row(PlayModesRow::Repeat), 1);
    assert_eq!(mode_row(PlayModesRow::PauseAfterEach), 2);
    assert_eq!(mode_row(PlayModesRow::RepeatReshuffles), 3);
    // Fresh engine defaults, straight from the model.
    let d = r.app.model.play_mode_values();
    assert_eq!(d, PlayModeValues::default());
    let f0 = r.frame();

    status_mode(
        &mut r,
        OrderMode::Shuffle,
        RepeatMode::Item,
        true,
        false,
        SourceOrder::Dynamic,
    );
    let v = r.app.model.play_mode_values();
    assert_eq!(v.order, OrderMode::Shuffle);
    assert_eq!(v.repeat, RepeatMode::Item);
    assert!(v.pause_after_each);
    assert!(!v.reshuffle_on_repeat);
    assert_eq!(v.source_order, SourceOrder::Dynamic);
    assert_eq!(PlayModesRow::Shuffle.value_text(&v), "On");
    assert_eq!(PlayModesRow::Repeat.value_text(&v), "One");
    assert_eq!(PlayModesRow::PauseAfterEach.value_text(&v), "On");
    assert_eq!(PlayModesRow::RepeatReshuffles.value_text(&v), "Off");
    assert_ne!(f0, r.frame(), "the value column follows the status");

    // Back returns to the System menu (no editor state on this screen).
    r.input(InputEvent::Back);
    assert_eq!(*r.app.nav.current(), Route::System);
}

#[test]
fn repeat_row_cycles_off_queue_one() {
    let mut r = Rig::new();
    open_play_modes(&mut r);
    let repeat_row = mode_row(PlayModesRow::Repeat);
    r.enter_row(repeat_row);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetRepeatMode(RepeatMode::Queue)]
    );
    assert_eq!(r.app.model.repeat_mode, RepeatMode::Queue);
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Repeat: Queue"]
    );
    r.enter_row(repeat_row);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetRepeatMode(RepeatMode::Item)]
    );
    assert_eq!(PlayModesRow::Repeat.value_text(&r.app.model.play_mode_values()), "One");
    r.enter_row(repeat_row);
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetRepeatMode(RepeatMode::Off)]
    );
    assert_eq!(PlayModesRow::Repeat.value_text(&r.app.model.play_mode_values()), "Off");
    // No edit state: the cursor never leaves the row.
    assert_eq!(r.app.model.play_mode.editing, None);
    assert_eq!(r.app.nav.focus, repeat_row);
}

#[test]
fn the_play_mode_rows_are_never_persisted() {
    let mut r = Rig::new();
    open_play_modes(&mut r);
    for row in PLAY_MODES_ROWS {
        r.enter_row(mode_row(row));
    }
    // Four commands, one per row, and not a single byte in `play_mode.json`.
    assert_eq!(r.cmds().len(), 4);
    assert_eq!(r.file(), "", "session state is not written to the settings file");
    for key in [
        "order_mode",
        "repeat_mode",
        "pause_after_each",
        "reshuffle_on_repeat",
        "source_order",
    ] {
        assert!(!r.file().contains(key), "{key} leaked into the file");
    }
    // A restart starts from the engine's defaults, never from the file.
    let r = r.restart();
    assert_eq!(r.app.model.play_mode_values(), PlayModeValues::default());
}

/// Enter on the first row arms the speed ladder; one detent is exactly one `SetRate`, the value
/// column follows at once, the ends clamp and 0 % is never sent.
#[test]
fn playback_speed_row_sends_one_set_rate_per_detent() {
    let mut r = Rig::new();
    r.open_via_system();
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "100%"
    );
    let speed_row = setting_row(PlayModeRow::PlaybackSpeed);
    assert_eq!(speed_row, 4, "the first settings row, after the four session modes");
    r.enter_row(speed_row);
    assert_eq!(r.app.model.play_mode.editing, Some(PlayModeRow::PlaybackSpeed));
    assert!(r.cmds().is_empty(), "arming the row sends nothing");
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "100%"
    );

    // One detent (left = up the ladder): 100 % -> 105 %, exactly one command.
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetRate { permille: 1050 }],
        "one detent = one SetRate"
    );
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "105%",
        "the value column follows immediately"
    );
    // Ten detents from 100 % reach 150 % (owner acceptance criterion).
    for _ in 0..9 {
        r.input(InputEvent::MenuRotate(-1));
    }
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "150%"
    );
    // The top clamps at +200 % and stops sending.
    for _ in 0..10 {
        r.input(InputEvent::MenuRotate(-1));
    }
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "200%"
    );
    let _ = r.cmds();
    r.input(InputEvent::MenuRotate(-1));
    assert!(r.cmds().is_empty(), "+200 % clamps");
    assert_eq!(r.app.model.set_speed_permille(), 2000);

    // Exit persists nothing and names the value once.
    r.input(InputEvent::MenuEnter);
    assert_eq!(r.app.model.play_mode.editing, None);
    assert_eq!(
        r.app.compositor.status().unwrap().lines(),
        ["Playback speed: 200%"]
    );
    let text = r.file();
    assert!(!text.contains("speed"), "the speed is session state: {text}");

    // Down again from 100 %: 5 % rungs, and there is no 0 % — the smallest forward rung is
    // +10 %, and one more detent crosses to -10 %.
    let mut fresh = Rig::new();
    fresh.open_via_system();
    fresh.enter_row(speed_row);
    for _ in 0..18 {
        fresh.input(InputEvent::MenuRotate(1));
    }
    assert_eq!(
        fresh.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "10%"
    );
    fresh.input(InputEvent::MenuRotate(1));
    assert_eq!(
        fresh.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "-10%",
        "the ladder crosses from forward to reverse without a 0 % rung"
    );
    assert!(!fresh.cmds().is_empty(), "crossing still sends the new speed");
    // Every command ever sent was a valid engine speed.
    assert!(fresh.cmds().iter().all(|c| !matches!(c, PlaybackCommand::SetRate { permille: 0 })));
}

#[test]
fn the_playback_speed_row_is_never_persisted() {
    let mut r = Rig::new();
    r.open_via_system();
    let speed_row = setting_row(PlayModeRow::PlaybackSpeed);
    r.enter_row(speed_row);
    for _ in 0..5 {
        r.input(InputEvent::MenuRotate(-1));
    }
    assert!(r
        .cmds()
        .iter()
        .all(|c| matches!(c, PlaybackCommand::SetRate { .. })));
    assert_eq!(r.file(), "", "no speed key, nothing persisted");
    assert!(!r.file().contains("rate"));

    let mut r = r.restart();
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "100%",
        "a restart starts at unity until the engine reports its speed"
    );
    // An engine that kept its speed (the UI restarted, not the device) shows it again.
    let mut s = SessionState::default();
    s.current_item = Some(PlayableItemId::new(1));
    s.target_rate_permille = 1250;
    r.rev += 1;
    r.app.handle_ipc(IpcMessage::PlaybackStatus(
        PlaybackStatus::from_session_rev(s, r.rev),
    ));
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        "125%"
    );
    assert_eq!(r.file(), "", "still nothing written");
}

#[test]
fn playback_speed_row_works_with_nothing_loaded() {
    let mut r = Rig::new();
    assert!(r.app.model.playback.is_none());
    r.open_via_system();
    let speed_row = setting_row(PlayModeRow::PlaybackSpeed);
    r.enter_row(speed_row);
    r.input(InputEvent::MenuRotate(-1));
    assert_eq!(
        r.cmds(),
        vec![PlaybackCommand::SetRate { permille: 1050 }],
        "the engine stores it for the next start"
    );
}

#[test]
fn playback_speed_row_follows_the_tempo_ring() {
    let mut r = Rig::new();
    r.status(WarpMode::PitchCorrect, WarpMode::PitchShift);
    let _ = r.cmds();
    // On Now Playing a single-track session: Enter arms the ring at TEMPO, rotation edits the
    // set speed through the same `DesiredRate` path.
    r.input(InputEvent::MenuEnter);
    assert_eq!(r.app.model.np_focus.param(), Some(AdjustParam::Tempo));
    r.input(InputEvent::MenuRotate(-1));
    let ring = r.app.model.set_speed_permille();
    assert_ne!(ring, 1000, "the ring moved the speed");
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        speed_percent_text(ring),
        "the settings row reads the live set speed, not a stored one"
    );
    // And the row agrees when the screen is opened afterwards.
    r.open_via_system();
    assert_eq!(
        r.app.model.play_mode.value_text(PlayModeRow::PlaybackSpeed),
        speed_percent_text(ring)
    );
    assert_eq!(r.file(), "");
}

#[test]
fn no_fixed_order_notice_is_a_two_line_popup() {
    let mut r = Rig::new();
    r.app.handle_ipc(IpcMessage::UserNotice(UserNotice {
        severity: NoticeSeverity::Warning,
        code: NOTICE_NO_FIXED_ORDER.into(),
        message: "Dynamic playlist, no fixed order".into(),
    }));
    let popup = r.app.compositor.status().expect("popup");
    assert_eq!(
        popup.lines(),
        ["No fixed order", "Dynamic playlist, no fixed order"]
    );
    assert_eq!(popup.lines().len(), 2, "exactly two Display lines");
    assert!(popup.height_px() <= 60);
    // It is still a notice (the banner list), not only a popup.
    assert!(r
        .app
        .model
        .notices
        .iter()
        .any(|n| n.code == NOTICE_NO_FIXED_ORDER));
    // A long engine-authored message is truncated to one popup line, never a third line.
    r.app.handle_ipc(IpcMessage::UserNotice(UserNotice {
        severity: NoticeSeverity::Warning,
        code: NOTICE_NO_FIXED_ORDER.into(),
        message: "x".repeat(200),
    }));
    let popup = r.app.compositor.status().expect("popup");
    assert_eq!(popup.lines().len(), 2);
    assert!(popup.height_px() <= 60);
}

// ---------------------------------------------------------------- PLAY_04 §5: Up Next identity

fn album_entry(playable: u64, position: u32, title: &str) -> plippa_core::NowPlayingEntry {
    plippa_core::NowPlayingEntry {
        kind: plippa_core::NowPlayingEntryKind::Track,
        title: title.into(),
        artist: String::new(),
        start_ms: 0,
        end_ms: None,
        duration_ms: Some(1_000 + playable),
        track_index: 0,
        playable_id: Some(PlayableItemId::new(playable)),
        available: true,
        item_position: position,
    }
}

/// A five-track album in source order; `order` is the playback order of source positions.
fn send_album(r: &mut Rig, list_id: u64, order: &[u32], current: u32) {
    let entries = order
        .iter()
        .map(|p| album_entry(u64::from(*p) + 1, *p, &format!("T{}", p + 1)))
        .collect();
    r.app.handle_ipc(IpcMessage::NowPlayingListChanged(
        plippa_ipc_proto::NowPlayingListChanged {
            entries,
            current_index: current,
            list_kind: plippa_core::NowPlayingListKind::Album,
            list_id,
        },
    ));
}

#[test]
fn up_next_survives_a_reshuffle_without_resetting_the_cursor() {
    let mut r = Rig::new();
    r.tick(0);
    send_album(&mut r, 7, &[0, 1, 2, 3, 4], 0);
    // Open Up Next and scroll the cursor to T3 (source position 2, playback index 2).
    r.input(InputEvent::MenuRotate(1));
    r.input(InputEvent::MenuRotate(1));
    assert!(r.app.model.up_next_visible());
    assert_eq!(r.app.model.np_focus.cursor, 2);
    // Same list, reshuffled, and playback moved to T5.
    send_album(&mut r, 7, &[4, 2, 0, 3, 1], 0);
    assert!(r.app.model.up_next_visible(), "a reorder is not a new load");
    let list = r.app.model.now_playing.as_ref().expect("list");
    let cursor = r.app.model.np_focus.cursor;
    assert_eq!(
        list.entries[cursor].playable_id,
        Some(PlayableItemId::new(3)),
        "the cursor stays on T3, wherever the shuffle moved it"
    );
    // The 4 s overlay timer is unchanged by the reorder: it does not restart or close here.
    r.tick(1_000);
    assert!(r.app.model.up_next_visible());
}

#[test]
fn up_next_resets_when_the_list_id_changes() {
    let mut r = Rig::new();
    r.tick(0);
    send_album(&mut r, 7, &[0, 1, 2, 3, 4], 0);
    r.input(InputEvent::MenuRotate(1));
    r.input(InputEvent::MenuRotate(1));
    assert!(r.app.model.up_next_visible());
    // A different collection: new list_id, so the overlay resets to the waveform.
    send_album(&mut r, 8, &[0, 1, 2], 0);
    assert!(!r.app.model.up_next_visible());
    assert_eq!(r.app.model.np_focus.target, plippa_ui::NpTarget::CurrentTime);
    assert_eq!(r.app.model.np_focus.cursor, 0);
}

fn cue_entry(start_ms: u64, title: &str) -> plippa_core::NowPlayingEntry {
    plippa_core::NowPlayingEntry {
        kind: plippa_core::NowPlayingEntryKind::Cue,
        title: title.into(),
        artist: String::new(),
        start_ms,
        end_ms: None,
        duration_ms: None,
        track_index: 0,
        playable_id: None,
        available: true,
        item_position: 0,
    }
}

/// PLAY_03/PLAY_04: under cue shuffle the engine publishes the cue rows in play order and
/// reports `current_cue_index` as the **permutation** position. Up Next must list those rows
/// verbatim and highlight the heard cue at `current_index + 1 + current_cue_index`.
#[test]
fn up_next_lists_cues_in_play_order_and_highlights_the_heard_cue() {
    let mut r = Rig::new();
    r.tick(0);
    // Permutation [0, 800, 400]: the second row played is the third chronological segment.
    let entries = vec![
        album_entry(1, 0, "Side A"),
        cue_entry(0, "S1"),
        cue_entry(800, "S2"),
        cue_entry(400, "S3"),
    ];
    r.app.handle_ipc(IpcMessage::NowPlayingListChanged(
        plippa_ipc_proto::NowPlayingListChanged {
            entries,
            current_index: 0,
            list_kind: plippa_core::NowPlayingListKind::Album,
            list_id: 7,
        },
    ));
    r.input(InputEvent::MenuRotate(1));
    assert!(r.app.model.up_next_visible());
    let list = r.app.model.now_playing.as_ref().expect("list");
    let cues: Vec<u64> = list
        .entries
        .iter()
        .filter(|e| e.kind == plippa_core::NowPlayingEntryKind::Cue)
        .map(|e| e.start_ms)
        .collect();
    assert_eq!(
        cues,
        [0, 800, 400],
        "the panel lists the cue rows in play order, not chronologically"
    );
    // Permutation position 2 is the third published cue row (start 400), which is not the
    // chronological next segment (800, row 2).
    r.status_with(|s| s.current_cue_index = Some(2));
    let base = r.app.model.now_playing.as_ref().unwrap().current_index as usize;
    assert_eq!(r.app.model.now_playing_row(), Some(base + 1 + 2));
    let heard = &r.app.model.now_playing.as_ref().unwrap().entries[3];
    assert_eq!(heard.kind, plippa_core::NowPlayingEntryKind::Cue);
    assert_eq!((heard.start_ms, heard.title.as_str()), (400, "S3"));
    assert_eq!(
        r.app.model.now_playing.as_ref().unwrap().entries[2].start_ms,
        800,
        "the highlight follows the permutation, not the clock"
    );
    // The overlay paints the playing marker (`ACCENT_2`) on that row only.
    let f = r.frame();
    let marker = |row: usize| {
        let y0 = (UP_NEXT_ROWS_Y + row as u16 * ROW_H) as usize;
        count_colour(&f, y0, y0 + ROW_H as usize, ACCENT_2)
    };
    assert!(
        marker(3) > 0,
        "the heard cue row carries the playing marker"
    );
    for row in [0, 1, 2] {
        assert_eq!(marker(row), 0, "row {row} is not the heard cue");
    }
}
