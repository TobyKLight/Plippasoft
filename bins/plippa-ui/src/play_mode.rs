//! System > Playback settings (WARP_10): the persisted UI settings behind the Playback
//! settings and Debug screens, their option lists, texts and `play_mode.json`.
//!
//! File (`PLIPPA_STATE_DIR/play_mode.json`, else `XDG_STATE_HOME/plippa/`, default
//! `~/.local/state/plippa/`), written atomically, **only values the user explicitly chose**:
//! `{"speed_warp_mode":"pitch_correct","seek_warp_mode":"pitch_shift"}`. A missing key, an
//! unknown value or a garbage file follows the default (so the WARP_09 default flip reaches
//! devices that never opened the menu). Every key is judged on its own, so an old file that
//! still carries the removed `jog_step_ms` loads normally.
//!
//! PLAY_04 added order, repeat, pause-after-each and repeat-reshuffle ([`PlayModesRow`]): they
//! are **session state mirrored from the engine** and are never written here. They now sit at
//! the top of the merged System > Playback settings screen ([`SettingsRow`]), together with the
//! live "Playback speed" row, also session state. That keeps the engine the single owner of
//! those five values. BCS_11's tap consent lives on System > Network instead.
//!
//! UI_16: the finest jog click is no longer a setting. It is derived from the resolved Fine
//! time unit ([`TimecodeUnit::finest_jog_step_ms`]), read live by the jog path. UI_16 also adds
//! the Debug screen's one setting (`rec_enables_nfc_poll`) to this same file: it is a UI
//! setting like the rest, and the loop gates its NFC poll on it.

use crate::assign::state_file_path;
use plippa_core::{
    OrderMode, RepeatMode, SourceOrder, WarpMode, WarpModeSettings, RATE_MAX_ABS_PERMILLE,
    RATE_MIN_ABS_PERMILLE, RATE_UNITY_PERMILLE, SEEK_WARP_DEFAULT, SPEED_WARP_DEFAULT,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// File name inside the state directory.
pub const PLAY_MODE_FILE_NAME: &str = "play_mode.json";

/// Waveform time window ladder on Now Playing (UI_13, owner decision): 20 steps from 1 s to
/// 10 min. The ring moves one step per detent and clamps at both ends (no wrap). The `.wfm`
/// analysis bucket size is unchanged; only the displayed window moves.
pub const ZOOM_LADDER_MS: [u64; 20] = [
    1_000, 2_000, 4_000, 8_000, 12_000, 16_000, 20_000, 30_000, 40_000, 50_000, 60_000, 90_000,
    120_000, 150_000, 200_000, 250_000, 300_000, 400_000, 500_000, 600_000,
];
/// Startup default for the waveform window (30 s).
pub const ZOOM_DEFAULT_MS: u64 = 30_000;

/// Sub-second unit of the Now Playing footer clocks (UI_14 §7, extended by UI_16 §4): two
/// decimal modes and the common video frame rates, plus `Off` for plain `MM:SS`. 75 fps (CDJ
/// style) is the default; a missing or unparseable stored value resolves to it, never to `Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimecodeUnit {
    /// No sub-second unit: the footer clocks are plain `MM:SS`.
    Off,
    /// Tenths of a second: `MM:SS.t`.
    DecTenths,
    Frames24,
    Frames25,
    Frames30,
    Frames50,
    Frames60,
    #[default]
    Frames75,
    /// Hundredths of a second: `MM:SS.tt`.
    DecHundredths,
}

/// Fine time choices in display order (owner order, UI_16 §4): `Off`, decimal tenths, the
/// frame rates **low to high**, then decimal hundredths. The row is a bounded ladder edited by
/// scrolling (clamped, no wrap), not a click-to-cycle list.
pub const TIMECODE_UNITS: [TimecodeUnit; 9] = [
    TimecodeUnit::Off,
    TimecodeUnit::DecTenths,
    TimecodeUnit::Frames24,
    TimecodeUnit::Frames25,
    TimecodeUnit::Frames30,
    TimecodeUnit::Frames50,
    TimecodeUnit::Frames60,
    TimecodeUnit::Frames75,
    TimecodeUnit::DecHundredths,
];

impl TimecodeUnit {
    /// Frame rate for a frame unit, `None` for a decimal mode or off.
    pub const fn frames_per_second(self) -> Option<u32> {
        match self {
            Self::Off | Self::DecTenths | Self::DecHundredths => None,
            Self::Frames75 => Some(75),
            Self::Frames60 => Some(60),
            Self::Frames50 => Some(50),
            Self::Frames30 => Some(30),
            Self::Frames25 => Some(25),
            Self::Frames24 => Some(24),
        }
    }

    /// Stored value of the unit (`timecode_unit` in `play_mode.json`). `DecTenths` keeps the
    /// pre-UI_16 `decimal` key so an existing settings file keeps its decimal mode.
    fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DecTenths => "decimal",
            Self::Frames24 => "frames_24",
            Self::Frames25 => "frames_25",
            Self::Frames30 => "frames_30",
            Self::Frames50 => "frames_50",
            Self::Frames60 => "frames_60",
            Self::Frames75 => "frames_75",
            Self::DecHundredths => "decimal_100",
        }
    }

    fn from_key(k: &str) -> Option<Self> {
        match k {
            "off" => Some(Self::Off),
            // UI_16: the pre-UI_16 decimal mode is tenths.
            "decimal" => Some(Self::DecTenths),
            "frames_24" => Some(Self::Frames24),
            "frames_25" => Some(Self::Frames25),
            "frames_30" => Some(Self::Frames30),
            "frames_50" => Some(Self::Frames50),
            "frames_60" => Some(Self::Frames60),
            "frames_75" => Some(Self::Frames75),
            "decimal_100" => Some(Self::DecHundredths),
            _ => None,
        }
    }

    /// Value text on the Fine time row (`Off`, `Dec .1`, `24 fps`, `Dec .01`).
    pub const fn text(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::DecTenths => "Dec .1",
            Self::Frames24 => "24 fps",
            Self::Frames25 => "25 fps",
            Self::Frames30 => "30 fps",
            Self::Frames50 => "50 fps",
            Self::Frames60 => "60 fps",
            Self::Frames75 => "75 fps",
            Self::DecHundredths => "Dec .01",
        }
    }

    /// Ladder position of the unit (it is always on [`TIMECODE_UNITS`]).
    fn index(self) -> usize {
        TIMECODE_UNITS
            .iter()
            .position(|u| *u == self)
            .unwrap_or(TIMECODE_UNITS.len() - 1)
    }

    /// One scroll step along the Fine time ladder: clamped at both ends, never wrapped (a
    /// bounded list, unlike the menu).
    pub fn step(self, delta: i8) -> Self {
        let last = TIMECODE_UNITS.len() as i32 - 1;
        let idx = (self.index() as i32 + i32::from(delta)).clamp(0, last);
        TIMECODE_UNITS[idx as usize]
    }

    /// The finest jog click this unit implies, in ms (UI_16 §5; the owner removed the separate
    /// `Jog step` setting). A frame unit gives one frame per slow click; decimal tenths gives
    /// 100 ms, hundredths 10 ms, and `Off` 100 ms ("1/10 second when off", owner).
    ///
    /// Frame rates are rounded down to a whole millisecond (`1000 / fps`: 75 → 13, 60 → 16,
    /// 50 → 20, 30 → 33, 25 → 40, 24 → 41). A millisecond-granular step cannot land exactly on
    /// every frame boundary; the owner accepted "as close as possible".
    pub const fn finest_jog_step_ms(self) -> u32 {
        match self.frames_per_second() {
            Some(fps) => 1_000 / fps,
            None => match self {
                Self::DecHundredths => 10,
                // Decimal tenths, and the owner's "1/10 seconds when off".
                _ => 100,
            },
        }
    }
}

/// `8s` below a minute, `1m00s` / `2m30s` / `10m00s` at and above it. Worst case is six
/// glyphs, which fits the Adjust band next to the transport label at `Display`.
pub fn zoom_text(ms: u64) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1_000)
    } else {
        let secs = ms / 1_000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Exact ladder position of `ms`, else the default's (30 s). A stored garbage value therefore
/// resolves like the default rather than pinning an arbitrary step.
pub fn zoom_index(ms: u64) -> usize {
    ZOOM_LADDER_MS
        .iter()
        .position(|v| *v == ms)
        .unwrap_or_else(|| {
            ZOOM_LADDER_MS
                .iter()
                .position(|v| *v == ZOOM_DEFAULT_MS)
                .unwrap_or(0)
        })
}

/// One ring detent from `ms`: clamped at both ladder ends, never wrapped.
pub fn zoom_step(ms: u64, delta: i8) -> u64 {
    let last = ZOOM_LADDER_MS.len() as i32 - 1;
    let idx = (zoom_index(ms) as i32 + i32::from(delta)).clamp(0, last);
    ZOOM_LADDER_MS[idx as usize]
}

/// One detent of the Playback speed row: 5 % of the engine's tempo range.
pub const SPEED_ROW_STEP_PERMILLE: i32 = 50;

/// Number of ladder entries on each side of 0 (10 %..200 % in 5 % steps).
const SPEED_ROW_STEPS_PER_SIDE: i32 = (RATE_MAX_ABS_PERMILLE - RATE_MIN_ABS_PERMILLE)
    / SPEED_ROW_STEP_PERMILLE
    + 1;

/// The Playback speed ladder: `-2000, -1950, … -100, 100, 150, … 2000` permille, ascending.
///
/// The engine's validity range is `±RATE_MIN_ABS_PERMILLE..=±RATE_MAX_ABS_PERMILLE` with 0
/// rejected (`plippa_core::rate_permille_valid`); 0 is not a speed and is not a rung. 5 %
/// steps because 125 % / 150 % are the speeds people use for podcasts — a 10 % ladder cannot
/// reach 125 %. The TEMPO ring stays the fine / accelerating control.
pub fn speed_row_ladder() -> impl DoubleEndedIterator<Item = i32> {
    (0..SPEED_ROW_STEPS_PER_SIDE)
        .map(|i| -RATE_MAX_ABS_PERMILLE + i * SPEED_ROW_STEP_PERMILLE)
        .chain(
            (0..SPEED_ROW_STEPS_PER_SIDE)
                .map(|i| RATE_MIN_ABS_PERMILLE + i * SPEED_ROW_STEP_PERMILLE),
        )
}

/// Nearest ladder entry at or beyond `current` in `delta`'s direction, clamped at both ends
/// (never wrapped, never 0). A value that is not on the ladder snaps in the turn's direction
/// (`1023` steps up to `1050` and down to `1000`).
///
/// **The two sides do not cross zero**: `+100` is the bottom of the forward ladder and `-100`
/// the top of the reverse one (owner test: `1000 → 950 → … → 100 → 100`). 0 is not a speed, so
/// there is no rung to land on; the reverse range is reached by the set speed already being
/// negative (FF/RW reverse), and this row then walks the negative ladder.
pub fn speed_row_step(current: i32, delta: i8) -> i32 {
    let dir = delta.signum();
    if dir == 0 {
        return current;
    }
    let mut cur = current;
    let mut strict = speed_row_ladder().any(|v| v == current);
    for _ in 0..delta.unsigned_abs() {
        // One ordered ladder across both directions: -200% … -10%, +10% … +200%. Reverse is
        // reached by turning past the smallest forward rung (there is no 0%).
        let next = if dir > 0 {
            speed_row_ladder().find(|v| if strict { *v > cur } else { *v >= cur })
        } else {
            speed_row_ladder()
                .rev()
                .find(|v| if strict { *v < cur } else { *v <= cur })
        };
        match next {
            Some(v) => {
                cur = v;
                strict = true;
            }
            // At the end of the ladder: stay put (bounded, not wrapped).
            None => break,
        }
    }
    cur
}

/// `100%`, `125%`, `-75%`. Every rung is a multiple of 5 %, so one decimal is never needed.
pub fn speed_percent_text(permille: i32) -> String {
    format!("{}%", permille / 10)
}

/// Rows of the persisted part of Playback settings, in order. Later rows append here without
/// redesign.
///
/// The order, repeat, pause-after-each and repeat-reshuffle rows are **not** here: they are
/// session state ([`PlayModesRow`]) and appear above these on the same screen. The tap consent
/// moved to System > Network (`NetworkRow::SkipInternetAsk`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayModeRow {
    /// PLAY_04: live set speed (the same value as the Now Playing `TEMPO` ring), session-only.
    PlaybackSpeed,
    SpeedWarp,
    SeekWarp,
    /// WARP_19: scan and jog may cross into the neighbouring track.
    SeekOutsideTrack,
    /// UI_13: startup waveform time window on Now Playing.
    WaveformZoom,
    /// UI_14/UI_16: sub-second unit of the Now Playing position / length clocks and the
    /// finest jog click.
    FineTime,
    /// ART_10: the NFC ring plays the cover colours like a record. Persisted.
    ArtLeds,
}

/// One const list drives the screen, the cursor wrap and Enter.
pub const PLAY_MODE_ROWS: [PlayModeRow; 7] = [
    PlayModeRow::PlaybackSpeed,
    PlayModeRow::SpeedWarp,
    PlayModeRow::SeekWarp,
    PlayModeRow::SeekOutsideTrack,
    PlayModeRow::WaveformZoom,
    PlayModeRow::FineTime,
    PlayModeRow::ArtLeds,
];

impl PlayModeRow {
    pub fn label(self) -> &'static str {
        match self {
            PlayModeRow::PlaybackSpeed => "Playback speed",
            PlayModeRow::SpeedWarp => "Speed warp",
            PlayModeRow::SeekWarp => "Seek warp",
            PlayModeRow::SeekOutsideTrack => "Seek outside track",
            PlayModeRow::WaveformZoom => "Default zoom",
            PlayModeRow::FineTime => "Fine time",
            PlayModeRow::ArtLeds => "Art LEDs",
        }
    }

    /// Label as drawn on the 20-glyph row: `Seek outside track` plus its value does not fit,
    /// so that row shows `Seek outside` (the description and popup carry the full wording).
    /// `Default zoom` + `10m00s` (18 glyphs) does fit, so it keeps the full label.
    pub fn row_label(self) -> &'static str {
        match self {
            PlayModeRow::SeekOutsideTrack => "Seek outside",
            other => other.label(),
        }
    }

    /// The description line shown under the list while the cursor is on this row.
    pub fn description(self) -> &'static str {
        match self {
            PlayModeRow::PlaybackSpeed => "Tempo: 100% is normal. TEMPO knob and FF/RW taps share it. Resets on Unload.",
            PlayModeRow::SpeedWarp => "Tempo and FF/RW taps. Correct keeps pitch; shift changes it like tape.",
            PlayModeRow::SeekWarp => "Jog jump and FF/RW hold. Shift = cassette sound; correct keeps pitch.",
            PlayModeRow::SeekOutsideTrack => "On: scan and jog may run into the next or previous track.",
            PlayModeRow::WaveformZoom => "Waveform window at startup. The ZOOM knob changes it for this session.",
            PlayModeRow::FineTime => "Sub-second unit of the clocks and the finest jog click. Enter, then turn.",
            PlayModeRow::ArtLeds => "Rotate the cover colours on the ring while playing.",
        }
    }

    /// True for the rows with more than three choices: Enter starts editing the value and
    /// scrolling steps it, Enter / Back leaves (UI_16 §1). The two-option rows still toggle on
    /// a single click. PLAY_04: the speed row is a 5 % ladder and is never persisted.
    pub const fn edits_by_scroll(self) -> bool {
        matches!(
            self,
            PlayModeRow::PlaybackSpeed | PlayModeRow::WaveformZoom | PlayModeRow::FineTime
        )
    }
}

/// Rows of the session-mode section of the merged System > Playback settings screen (PLAY_04),
/// in order. The values are **session-only** mirrors of the engine's `SessionState`: nothing
/// here is written to `play_mode.json` and every one of them resets on a device reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayModesRow {
    /// `OrderMode`: `On` = shuffle.
    Shuffle,
    /// `RepeatMode`, cycling `Off → Queue → One → Off`.
    Repeat,
    /// `pause_after_each`: pause between items with the next one ready at 0:00.
    PauseAfterEach,
    /// `reshuffle_on_repeat`: repeat-queue on a shuffled list starts a fresh shuffle.
    RepeatReshuffles,
}

/// One const list drives the Play modes screen, its cursor wrap and Enter.
pub const PLAY_MODES_ROWS: [PlayModesRow; 4] = [
    PlayModesRow::Shuffle,
    PlayModesRow::Repeat,
    PlayModesRow::PauseAfterEach,
    PlayModesRow::RepeatReshuffles,
];

impl PlayModesRow {
    pub fn label(self) -> &'static str {
        match self {
            PlayModesRow::Shuffle => "Shuffle",
            PlayModesRow::Repeat => "Repeat",
            PlayModesRow::PauseAfterEach => "Pause after each",
            PlayModesRow::RepeatReshuffles => "Repeat reshuffles",
        }
    }

    pub fn row_label(self) -> &'static str {
        self.label()
    }

    /// The description line under the list. Every row says the value is session state, so
    /// nobody looks for it in a settings file after a restart (owner decision 4).
    pub fn description(self) -> &'static str {
        match self {
            PlayModesRow::Shuffle => {
                "Resets on device reset. Shuffle plays the collection in a random order."
            }
            PlayModesRow::Repeat => {
                "Resets on device reset. Queue repeats the collection; One repeats the current item."
            }
            PlayModesRow::PauseAfterEach => {
                "Resets on device reset. Pauses after each item with the next one ready at 0:00."
            }
            PlayModesRow::RepeatReshuffles => {
                "Resets on device reset. Repeat queue on a shuffled list starts a fresh shuffle."
            }
        }
    }

    /// Value text from the engine mirrors ([`PlayModeValues`]), never from
    /// [`PlayModeSettings`]. Nothing here is persisted, so a settings file cannot disagree.
    pub fn value_text(self, values: &PlayModeValues) -> &'static str {
        match self {
            PlayModesRow::Shuffle => on_off(values.order == OrderMode::Shuffle),
            PlayModesRow::Repeat => match values.repeat {
                RepeatMode::Off => "Off",
                RepeatMode::Queue => "Queue",
                RepeatMode::Item => "One",
            },
            PlayModesRow::PauseAfterEach => on_off(values.pause_after_each),
            PlayModesRow::RepeatReshuffles => on_off(values.reshuffle_on_repeat),
        }
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

/// One row of the merged System > Playback settings screen. The old second System screen,
/// "Play modes", is folded into it: its four session-mode rows come first, then the settings
/// rows. One list means one cursor, one wrap rule and one description band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    /// A session mode mirrored from the engine; Enter toggles or cycles it, nothing is stored.
    Mode(PlayModesRow),
    /// A Playback settings row (persisted, or the session-only Playback speed).
    Setting(PlayModeRow),
}

/// One const list drives the merged screen, its cursor wrap and Enter. The session rows are
/// first, in [`PLAY_MODES_ROWS`] order; the settings rows follow in [`PLAY_MODE_ROWS`] order.
pub const PLAYBACK_SETTINGS_ROWS: [SettingsRow; 11] = [
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
];

impl SettingsRow {
    /// The row's full label (the popup and the description use it).
    pub fn label(self) -> &'static str {
        match self {
            Self::Mode(row) => row.label(),
            Self::Setting(row) => row.label(),
        }
    }

    /// The label as drawn on the 320 px row, which may be shortened.
    pub fn row_label(self) -> &'static str {
        match self {
            Self::Mode(row) => row.row_label(),
            Self::Setting(row) => row.row_label(),
        }
    }

    /// The description line shown under the list while the cursor is on this row.
    pub fn description(self) -> &'static str {
        match self {
            Self::Mode(row) => row.description(),
            Self::Setting(row) => row.description(),
        }
    }

    /// Value text of the row: session modes read [`PlayModeValues`] (the engine mirrors), the
    /// settings read [`PlayModeView`]. Two sources on one screen, never mixed for one row.
    pub fn value_text(self, view: &PlayModeView, values: &PlayModeValues) -> String {
        match self {
            Self::Mode(row) => row.value_text(values).to_string(),
            Self::Setting(row) => view.value_text(row),
        }
    }

    /// The settings row inside, `None` for a session mode.
    pub const fn setting(self) -> Option<PlayModeRow> {
        match self {
            Self::Setting(row) => Some(row),
            Self::Mode(_) => None,
        }
    }

    /// The session mode inside, `None` for a settings row.
    pub const fn mode(self) -> Option<PlayModesRow> {
        match self {
            Self::Mode(row) => Some(row),
            Self::Setting(_) => None,
        }
    }

    /// True for the settings rows with more than three choices (Enter starts editing the
    /// value). Session modes never edit by scrolling: they toggle or cycle on one press.
    pub const fn edits_by_scroll(self) -> bool {
        match self {
            Self::Setting(row) => row.edits_by_scroll(),
            Self::Mode(_) => false,
        }
    }
}

/// The engine's play-mode state as the "Play modes" screen shows it (`SessionState`), copied
/// into [`crate::screens::ScreenModel`] by `apply_playback`. Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayModeValues {
    pub order: OrderMode,
    pub repeat: RepeatMode,
    pub pause_after_each: bool,
    pub reshuffle_on_repeat: bool,
    pub source_order: SourceOrder,
}

impl Default for PlayModeValues {
    fn default() -> Self {
        Self {
            order: OrderMode::Normal,
            repeat: RepeatMode::Off,
            pause_after_each: false,
            // Owner decision 5: repeat-queue reshuffles by default, and the engine owns it.
            reshuffle_on_repeat: true,
            source_order: SourceOrder::Fixed,
        }
    }
}

/// Rows of the System > Debug screen (UI_16 §8, AUDIO_05), in order. Two-state settings and
/// navigation rows share the list; [`Self::opens_screen`] tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugRow {
    /// `On`: REC starts and stops the NFC poll; a played tap also stops it.
    RecEnablesNfcPoll,
    /// AUDIO_05: opens `Route::SystemDebugAudio`, a read-only report.
    AudioInfo,
}

/// One const list drives the Debug screen, its cursor and Enter.
pub const DEBUG_ROWS: [DebugRow; 2] = [DebugRow::RecEnablesNfcPoll, DebugRow::AudioInfo];

impl DebugRow {
    pub fn label(self) -> &'static str {
        match self {
            DebugRow::RecEnablesNfcPoll => "Rec enables NFC poll",
            DebugRow::AudioInfo => "Audio info",
        }
    }

    pub fn row_label(self) -> &'static str {
        self.label()
    }

    /// The description line under the list while the cursor is on this row. The setting has no
    /// popup and no on-screen explanation of the REC gesture beyond this line.
    pub fn description(self) -> &'static str {
        match self {
            DebugRow::RecEnablesNfcPoll => {
                "On: REC starts and stops the NFC poll; a successful tap also stops it. Off: poll always."
            }
            DebugRow::AudioInfo => {
                "What the engine and the radio report right now. Enter shows it and writes it to the log."
            }
        }
    }

    /// Value text derived from the resolved settings. Navigation rows have no value column.
    pub fn value_text(self, view: &PlayModeView) -> &'static str {
        match self {
            DebugRow::RecEnablesNfcPoll => {
                if view.rec_enables_nfc_poll {
                    "On"
                } else {
                    "Off"
                }
            }
            DebugRow::AudioInfo => "",
        }
    }

    /// True for rows that push a screen instead of toggling a value on Enter (AUDIO_05).
    pub const fn opens_screen(self) -> bool {
        matches!(self, Self::AudioInfo)
    }
}

/// `Pitch correct` / `Pitch shift` (popup and spoken wording).
pub fn warp_mode_text(m: WarpMode) -> &'static str {
    match m {
        WarpMode::PitchCorrect => "Pitch correct",
        WarpMode::PitchShift => "Pitch shift",
    }
}

/// Short row value (`Correct` / `Shift`): the 320 px Small-face row has 20 glyphs, a label
/// plus `Pitch correct` does not fit (the description line carries the full wording).
pub fn warp_mode_short(m: WarpMode) -> &'static str {
    match m {
        WarpMode::PitchCorrect => "Correct",
        WarpMode::PitchShift => "Shift",
    }
}

fn mode_key(m: WarpMode) -> &'static str {
    match m {
        WarpMode::PitchCorrect => "pitch_correct",
        WarpMode::PitchShift => "pitch_shift",
    }
}

fn mode_from_key(k: &str) -> Option<WarpMode> {
    match k {
        "pitch_correct" => Some(WarpMode::PitchCorrect),
        "pitch_shift" => Some(WarpMode::PitchShift),
        _ => None,
    }
}

/// The resolved settings the screens show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayModeView {
    pub speed: WarpMode,
    pub seek: WarpMode,
    pub seek_outside_track: bool,
    /// Resolved startup waveform window (UI_13).
    pub waveform_zoom_ms: u64,
    /// Resolved sub-second clock unit (UI_14), read live by the Now Playing footer.
    pub timecode_unit: TimecodeUnit,
    /// UI_16 §1: the row whose value is being edited by scrolling, `None` while the list is
    /// only browsed. Enter/Back clears it; its value column is drawn in `ACCENT` meanwhile.
    pub editing: Option<PlayModeRow>,
    /// UI_16 §8 (Debug): when true, REC gates the NFC poll.
    pub rec_enables_nfc_poll: bool,
    /// ART_10: the NFC ring shows the cover colours while playing (persisted).
    pub art_leds: bool,
    /// BCS_11: a tag-page lookup goes ahead without asking (persisted).
    pub skip_internet_consent: bool,
    /// PLAY_04: the live set speed (status `target_rate_permille`, or the optimistic value
    /// while it is fresh). Session state, filled from the engine — not from
    /// [`PlayModeSettings`] and never persisted.
    pub rate_permille: i32,
}

impl Default for PlayModeView {
    fn default() -> Self {
        PlayModeSettings::default().view()
    }
}

impl PlayModeView {
    /// Value text of `row`.
    pub fn value_text(&self, row: PlayModeRow) -> String {
        match row {
            PlayModeRow::PlaybackSpeed => speed_percent_text(self.rate_permille),
            PlayModeRow::SpeedWarp => warp_mode_short(self.speed).into(),
            PlayModeRow::SeekWarp => warp_mode_short(self.seek).into(),
            PlayModeRow::SeekOutsideTrack => {
                if self.seek_outside_track {
                    "On"
                } else {
                    "Off"
                }
                .into()
            }
            PlayModeRow::WaveformZoom => zoom_text(self.waveform_zoom_ms),
            PlayModeRow::FineTime => self.timecode_unit.text().into(),
            PlayModeRow::ArtLeds => on_off(self.art_leds).into(),
        }
    }

    /// Popup text naming the row's value after a change (`Fine time: 75 fps`,
    /// `Seek outside track: On`, …). One place so the words stay in step with the rows.
    pub fn change_text(&self, row: PlayModeRow) -> String {
        format!("{}: {}", row.label(), self.value_text(row))
    }
}

/// What the user explicitly chose (`None` = follow the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayModeSettings {
    pub warp: WarpModeSettings,
    /// WARP_19: explicit choice only (absent = off).
    pub seek_outside_track: Option<bool>,
    /// UI_13: startup waveform window; off-ladder values follow the default.
    pub waveform_zoom_ms: Option<u64>,
    /// UI_14: sub-second clock unit; unknown values follow the default (75 fps).
    pub timecode_unit: Option<TimecodeUnit>,
    /// UI_16 §8 (Debug screen): explicit choice only (absent = Off, poll always).
    pub rec_enables_nfc_poll: Option<bool>,
    /// ART_10: explicit choice only (absent = On).
    pub art_leds: Option<bool>,
    /// BCS_11: explicit choice only (absent = Off: the tag-page lookup asks first).
    pub skip_internet_consent: Option<bool>,
}

impl PlayModeSettings {
    /// "Seek outside track" with the default (off) applied.
    pub fn resolve_seek_outside(&self) -> bool {
        self.seek_outside_track.unwrap_or(false)
    }

    /// `(speed, seek)` with defaults applied.
    pub fn resolve_warp(&self) -> (WarpMode, WarpMode) {
        (
            self.warp.speed.unwrap_or(SPEED_WARP_DEFAULT),
            self.warp.seek.unwrap_or(SEEK_WARP_DEFAULT),
        )
    }

    /// The startup waveform window: a stored value on the ladder, else 30 s.
    pub fn resolve_waveform_zoom(&self) -> u64 {
        self.waveform_zoom_ms
            .filter(|v| ZOOM_LADDER_MS.contains(v))
            .unwrap_or(ZOOM_DEFAULT_MS)
    }

    /// The sub-second clock unit: the stored choice, else 75 fps.
    pub fn resolve_timecode_unit(&self) -> TimecodeUnit {
        self.timecode_unit.unwrap_or_default()
    }

    /// The finest jog click in ms, derived live from the resolved Fine time unit (UI_16 §5).
    pub fn resolve_jog_step(&self) -> u32 {
        self.resolve_timecode_unit().finest_jog_step_ms()
    }

    /// Debug "Rec enables NFC poll" with the default (Off) applied.
    pub fn resolve_rec_enables_nfc_poll(&self) -> bool {
        self.rec_enables_nfc_poll.unwrap_or(false)
    }

    /// "Art LEDs" with the default (On) applied.
    pub fn resolve_art_leds(&self) -> bool {
        self.art_leds.unwrap_or(true)
    }

    /// "Skip internet ask" with the default (Off) applied.
    pub fn resolve_skip_internet_consent(&self) -> bool {
        self.skip_internet_consent.unwrap_or(false)
    }

    pub fn view(&self) -> PlayModeView {
        let (speed, seek) = self.resolve_warp();
        PlayModeView {
            speed,
            seek,
            seek_outside_track: self.resolve_seek_outside(),
            waveform_zoom_ms: self.resolve_waveform_zoom(),
            timecode_unit: self.resolve_timecode_unit(),
            editing: None,
            rec_enables_nfc_poll: self.resolve_rec_enables_nfc_poll(),
            art_leds: self.resolve_art_leds(),
            skip_internet_consent: self.resolve_skip_internet_consent(),
            // Session state: the caller refreshes it from the engine status (or the optimistic
            // set speed). A fresh view is unity until that arrives.
            rate_permille: RATE_UNITY_PERMILLE,
        }
    }

    /// File text: only explicitly chosen keys.
    pub fn to_json(&self) -> String {
        let mut m = serde_json::Map::new();
        if let Some(v) = self.warp.speed {
            m.insert("speed_warp_mode".into(), mode_key(v).into());
        }
        if let Some(v) = self.warp.seek {
            m.insert("seek_warp_mode".into(), mode_key(v).into());
        }
        if let Some(v) = self.seek_outside_track {
            m.insert("seek_outside_track".into(), v.into());
        }
        if let Some(v) = self.waveform_zoom_ms {
            m.insert("waveform_zoom_ms".into(), v.into());
        }
        if let Some(v) = self.timecode_unit {
            m.insert("timecode_unit".into(), v.key().into());
        }
        if let Some(v) = self.rec_enables_nfc_poll {
            m.insert("rec_enables_nfc_poll".into(), v.into());
        }
        if let Some(v) = self.art_leds {
            m.insert("art_leds".into(), v.into());
        }
        if let Some(v) = self.skip_internet_consent {
            m.insert("skip_internet_consent".into(), v.into());
        }
        serde_json::to_string_pretty(&serde_json::Value::Object(m)).unwrap_or_else(|_| "{}".into())
    }

    /// Parse leniently: garbage = defaults, each key is judged on its own. Unknown keys
    /// (including the UI_15-era `jog_step_ms`) are ignored, so an existing `play_mode.json`
    /// keeps loading and its other choices are untouched.
    pub fn from_json(text: &str) -> Self {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return Self::default();
        };
        let mode = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(mode_from_key);
        Self {
            warp: WarpModeSettings {
                speed: mode("speed_warp_mode"),
                seek: mode("seek_warp_mode"),
            },
            seek_outside_track: v.get("seek_outside_track").and_then(|x| x.as_bool()),
            waveform_zoom_ms: v
                .get("waveform_zoom_ms")
                .and_then(|x| x.as_u64())
                .filter(|x| ZOOM_LADDER_MS.contains(x)),
            timecode_unit: v
                .get("timecode_unit")
                .and_then(|x| x.as_str())
                .and_then(TimecodeUnit::from_key),
            rec_enables_nfc_poll: v.get("rec_enables_nfc_poll").and_then(|x| x.as_bool()),
            art_leds: v.get("art_leds").and_then(|x| x.as_bool()),
            skip_internet_consent: v.get("skip_internet_consent").and_then(|x| x.as_bool()),
        }
    }
}

/// Settings + optional persist path (beside `AssignStore`).
#[derive(Debug, Clone, Default)]
pub struct PlayModeStore {
    path: Option<PathBuf>,
    pub settings: PlayModeSettings,
}

impl PlayModeStore {
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            settings: PlayModeSettings::default(),
        }
    }

    /// `PLIPPA_STATE_DIR/play_mode.json`, else the XDG state dir.
    pub fn default_path() -> PathBuf {
        state_file_path(PLAY_MODE_FILE_NAME)
    }

    /// Load `path`; a missing or unreadable file is "nothing chosen".
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let settings = match fs::read_to_string(path) {
            Ok(text) => PlayModeSettings::from_json(&text),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    eprintln!(
                        "plippa-ui: play_mode load `{}`: {e} - defaults",
                        path.display()
                    );
                }
                PlayModeSettings::default()
            }
        };
        Self {
            path: Some(path.to_path_buf()),
            settings,
        }
    }

    pub fn load_default() -> Self {
        Self::load_or_default(Self::default_path())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Atomic write (temp + rename). No-op without a path.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, self.settings.to_json())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_follow_the_constants_and_pitch_correct_is_the_speed_default() {
        let v = PlayModeSettings::default().view();
        assert_eq!(
            v.speed,
            WarpMode::PitchCorrect,
            "WARP_09 default flip reaches unset devices"
        );
        assert_eq!(v.seek, WarpMode::PitchShift);
        assert_eq!(v.timecode_unit, TimecodeUnit::Frames75);
        assert_eq!(v.waveform_zoom_ms, ZOOM_DEFAULT_MS);
        assert!(!v.seek_outside_track);
        assert!(!v.rec_enables_nfc_poll, "debug default: poll always");
        assert_eq!(v.editing, None);
    }

    #[test]
    fn round_trip_writes_only_explicit_choices() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PLAY_MODE_FILE_NAME);
        let mut store = PlayModeStore::with_path(&path);
        store.settings.warp.speed = Some(WarpMode::PitchShift);
        store.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"speed_warp_mode\": \"pitch_shift\""),
            "{text}"
        );
        assert!(
            !text.contains("seek_warp_mode")
                && !text.contains("jog_step_ms")
                && !text.contains("rec_enables_nfc_poll"),
            "{text}"
        );
        let loaded = PlayModeStore::load_or_default(&path);
        assert_eq!(loaded.settings, store.settings);
        store.settings.warp.seek = Some(WarpMode::PitchCorrect);
        store.settings.rec_enables_nfc_poll = Some(true);
        store.save().unwrap();
        let loaded = PlayModeStore::load_or_default(&path);
        assert_eq!(loaded.settings, store.settings);
        assert!(loaded.settings.resolve_rec_enables_nfc_poll());
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file renamed away"
        );
    }

    #[test]
    fn garbage_missing_and_invalid_values_follow_the_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PLAY_MODE_FILE_NAME);
        assert_eq!(
            PlayModeStore::load_or_default(&path).settings,
            PlayModeSettings::default()
        );
        fs::write(&path, "not json {{").unwrap();
        assert_eq!(
            PlayModeStore::load_or_default(&path).settings,
            PlayModeSettings::default()
        );
        let s = PlayModeSettings::from_json(
            r#"{"speed_warp_mode":"bogus","seek_warp_mode":"pitch_correct","rec_enables_nfc_poll":"yes"}"#,
        );
        assert_eq!(s.warp.speed, None);
        assert_eq!(s.warp.seek, Some(WarpMode::PitchCorrect));
        assert_eq!(s.rec_enables_nfc_poll, None, "garbage is not On");
        assert!(!s.resolve_rec_enables_nfc_poll());
    }

    /// UI_16 §5: a settings file written before the Jog step row was removed still loads; its
    /// other choices survive and the stale key is simply dropped on the next save.
    #[test]
    fn an_existing_file_with_jog_step_still_loads_and_keeps_its_choices() {
        let old = r#"{
            "speed_warp_mode": "pitch_shift",
            "jog_step_ms": 250,
            "timecode_unit": "frames_24",
            "seek_outside_track": true
        }"#;
        let s = PlayModeSettings::from_json(old);
        assert_eq!(s.warp.speed, Some(WarpMode::PitchShift));
        assert_eq!(s.timecode_unit, Some(TimecodeUnit::Frames24));
        assert_eq!(s.seek_outside_track, Some(true));
        // The removed key is not written back; the derived finest click comes from Fine time.
        let text = s.to_json();
        assert!(!text.contains("jog_step_ms"), "{text}");
        assert!(text.contains("frames_24"), "{text}");
        assert_eq!(s.resolve_jog_step(), 41, "24 fps finest click");
    }

    #[test]
    fn art_leds_defaults_on_round_trips_and_tolerates_bad_input() {
        assert!(PlayModeSettings::default().resolve_art_leds(), "default On");
        assert!(PlayModeSettings::default().view().art_leds);
        assert!(!PlayModeSettings::default().to_json().contains("art_leds"));
        // Missing key, wrong types, garbage: On.
        for text in [
            "{}",
            r#"{"art_leds": "no"}"#,
            r#"{"art_leds": 0}"#,
            r#"{"art_leds": null}"#,
            "not json",
        ] {
            assert!(PlayModeSettings::from_json(text).resolve_art_leds(), "{text}");
        }
        // Off survives a round trip (explicitly written), and so does an explicit On.
        let off = PlayModeSettings {
            art_leds: Some(false),
            ..PlayModeSettings::default()
        };
        let json = off.to_json();
        assert!(json.contains("\"art_leds\": false"), "{json}");
        let back = PlayModeSettings::from_json(&json);
        assert!(!back.resolve_art_leds());
        assert_eq!(back.view().value_text(PlayModeRow::ArtLeds), "Off");
        let on = PlayModeSettings::from_json(r#"{"art_leds": true}"#);
        assert_eq!(on.view().value_text(PlayModeRow::ArtLeds), "On");
        // The other keys are untouched by it.
        let both = PlayModeSettings::from_json(r#"{"art_leds": false, "seek_outside_track": true}"#);
        assert!(both.resolve_seek_outside() && !both.resolve_art_leds());
        assert_eq!(PlayModeRow::ArtLeds.label(), "Art LEDs");
        assert!(!PlayModeRow::ArtLeds.edits_by_scroll(), "two values: toggles on Enter");
        assert_eq!(off.view().change_text(PlayModeRow::ArtLeds), "Art LEDs: Off");
    }

    /// BCS_11: absent = off, garbage = off (the other keys untouched), and a stored choice
    /// round-trips through `play_mode.json`.
    #[test]
    fn skip_internet_consent_round_trips_and_defaults_off() {
        assert!(!PlayModeSettings::default().resolve_skip_internet_consent());
        assert!(!PlayModeSettings::default().view().skip_internet_consent);
        assert!(
            !PlayModeSettings::default().to_json().contains("skip_internet_consent"),
            "nothing is written until the person chooses"
        );
        for text in [
            "{}",
            "not json",
            r#"{"skip_internet_consent": "yes"}"#,
            r#"{"skip_internet_consent": 1}"#,
            r#"{"skip_internet_consent": null}"#,
        ] {
            assert!(
                !PlayModeSettings::from_json(text).resolve_skip_internet_consent(),
                "{text}"
            );
        }
        // A garbage value leaves the file's other choices alone.
        let mixed = PlayModeSettings::from_json(
            r#"{"skip_internet_consent": "maybe", "seek_outside_track": true, "art_leds": false}"#,
        );
        assert_eq!(mixed.skip_internet_consent, None);
        assert_eq!(mixed.seek_outside_track, Some(true));
        assert_eq!(mixed.art_leds, Some(false));

        let on = PlayModeSettings {
            skip_internet_consent: Some(true),
            ..PlayModeSettings::default()
        };
        let json = on.to_json();
        assert!(json.contains("\"skip_internet_consent\": true"), "{json}");
        let back = PlayModeSettings::from_json(&json);
        assert_eq!(back, on);
        assert!(back.resolve_skip_internet_consent());
        let off = PlayModeSettings {
            skip_internet_consent: Some(false),
            ..PlayModeSettings::default()
        };
        assert_eq!(
            PlayModeSettings::from_json(&off.to_json()).resolve_skip_internet_consent(),
            false
        );
        // It is a Network row now, not a Playback settings row.
        assert!(
            !PLAY_MODE_ROWS
                .iter()
                .any(|row| row.label().to_ascii_lowercase().contains("internet")),
            "the tap consent moved to System > Network"
        );
    }

    #[test]
    fn row_list_is_the_seven_settings_rows_in_order() {
        assert_eq!(
            PLAY_MODE_ROWS,
            [
                PlayModeRow::PlaybackSpeed,
                PlayModeRow::SpeedWarp,
                PlayModeRow::SeekWarp,
                PlayModeRow::SeekOutsideTrack,
                PlayModeRow::WaveformZoom,
                PlayModeRow::FineTime,
                PlayModeRow::ArtLeds
            ]
        );
        let v = PlayModeView::default();
        assert_eq!(v.value_text(PlayModeRow::PlaybackSpeed), "100%");
        assert_eq!(PlayModeRow::PlaybackSpeed.label(), "Playback speed");
        assert_eq!(v.value_text(PlayModeRow::SpeedWarp), "Correct");
        assert_eq!(v.value_text(PlayModeRow::SeekWarp), "Shift");
        assert_eq!(warp_mode_text(v.speed), "Pitch correct");
        assert_eq!(v.value_text(PlayModeRow::SeekOutsideTrack), "Off");
        assert_eq!(PlayModeRow::SeekOutsideTrack.label(), "Seek outside track");
        assert_eq!(v.value_text(PlayModeRow::WaveformZoom), "30s");
        assert_eq!(PlayModeRow::WaveformZoom.label(), "Default zoom");
        assert_eq!(PlayModeRow::WaveformZoom.row_label(), "Default zoom");
        assert_eq!(v.value_text(PlayModeRow::FineTime), "75 fps");
        assert_eq!(PlayModeRow::FineTime.label(), "Fine time");
        assert_eq!(PlayModeRow::FineTime.row_label(), "Fine time");
        assert!(
            PlayModeRow::FineTime.label().chars().count() <= 20,
            "the row label fits the shared settings row"
        );
    }

    /// UI_16 §1: only the rows with more than three choices edit by scrolling.
    #[test]
    fn rows_with_more_than_three_options_edit_by_scroll() {
        for row in PLAY_MODE_ROWS {
            assert_eq!(
                row.edits_by_scroll(),
                matches!(
                    row,
                    PlayModeRow::PlaybackSpeed
                        | PlayModeRow::WaveformZoom
                        | PlayModeRow::FineTime
                ),
                "{row:?}"
            );
        }
    }

    #[test]
    fn speed_row_ladder_is_symmetric_and_skips_zero() {
        let ladder: Vec<i32> = speed_row_ladder().collect();
        assert_eq!(ladder.len(), 78);
        assert_eq!(ladder.first(), Some(&-RATE_MAX_ABS_PERMILLE));
        assert_eq!(ladder.last(), Some(&RATE_MAX_ABS_PERMILLE));
        let gaps: Vec<i32> = ladder.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            gaps.iter().filter(|g| **g != SPEED_ROW_STEP_PERMILLE).count(),
            1,
            "the only gap that is not 5 % is the skipped zero"
        );
        assert!(
            gaps.contains(&(2 * RATE_MIN_ABS_PERMILLE)),
            "the junction is -100 -> 100 (zero is skipped)"
        );
        assert!(ladder.contains(&RATE_UNITY_PERMILLE));
        assert!(ladder.contains(&1250), "125 % is reachable, the reason for 5 %");
        assert!(!ladder.contains(&0), "0 is not a speed");
        // Symmetric: every positive rung has its negative twin.
        for v in ladder.iter().filter(|v| **v > 0) {
            assert!(ladder.contains(&-v), "{v} has no negative twin");
        }
        // Every rung is a valid engine speed.
        assert!(ladder.iter().all(|v| plippa_core::rate_permille_valid(*v)));
    }

    #[test]
    fn speed_row_step_clamps_and_snaps_in_the_turn_direction() {
        // Up the ladder: 5 % per detent, then clamped at +200 %.
        assert_eq!(speed_row_step(1000, 1), 1050);
        let mut v = 1000;
        for _ in 0..20 {
            v = speed_row_step(v, 1);
        }
        assert_eq!(v, 2000);
        assert_eq!(speed_row_step(2000, 1), 2000, "top clamps, no wrap");
        assert_eq!(speed_row_step(2000, 127), 2000);
        // Down: 950, … 100, then straight to -100 (one ladder, no 0% rung).
        assert_eq!(speed_row_step(1000, -1), 950);
        let mut v = 1000;
        for _ in 0..18 {
            v = speed_row_step(v, -1);
        }
        assert_eq!(v, 100, "the smallest forward rung");
        assert_eq!(speed_row_step(100, -1), -100, "crosses to reverse");
        let mut v = 100;
        for _ in 0..39 {
            v = speed_row_step(v, -1);
        }
        assert_eq!(v, -2000, "reverse end clamps, no wrap");
        assert_eq!(speed_row_step(-2000, -1), -2000);
        // Up the negative side reaches 100% and keeps going.
        assert_eq!(speed_row_step(-1000, 1), -950);
        assert_eq!(speed_row_step(-1000, -1), -1050);
        assert_eq!(speed_row_step(-100, 1), 100);
        // Zero snaps away from itself in the turn's direction.
        assert_eq!(speed_row_step(0, 1), 100);
        assert_eq!(speed_row_step(0, -1), -100);
        // An off-ladder value snaps in the turn's direction, then steps strictly.
        assert_eq!(speed_row_step(1023, 1), 1050);
        assert_eq!(speed_row_step(1023, -1), 1000);
        assert_eq!(speed_row_step(1023, 2), 1100, "a two-detent turn keeps stepping");
    }

    #[test]
    fn speed_percent_text_formats() {
        assert_eq!(speed_percent_text(1000), "100%");
        assert_eq!(speed_percent_text(1250), "125%");
        assert_eq!(speed_percent_text(-750), "-75%");
        assert_eq!(speed_percent_text(2000), "200%");
        assert_eq!(speed_percent_text(100), "10%");
        for v in speed_row_ladder() {
            let text = speed_percent_text(v);
            assert!(text.ends_with('%'), "{text}");
            assert!(
                text.chars().count() <= 5,
                "{text} does not fit the value column"
            );
        }
    }

    /// The merged screen is one list: the four session modes first (engine order), then the
    /// settings rows in their documented order, and no row appears twice.
    #[test]
    fn the_merged_settings_list_puts_the_session_modes_first() {
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
            ]
        );
        assert_eq!(PLAYBACK_SETTINGS_ROWS.len(), PLAY_MODES_ROWS.len() + PLAY_MODE_ROWS.len());
        // The session modes are exactly the first four, and both halves keep their own order.
        let modes: Vec<_> = PLAYBACK_SETTINGS_ROWS
            .iter()
            .filter_map(|row| row.mode())
            .collect();
        assert_eq!(modes, PLAY_MODES_ROWS.to_vec());
        let settings: Vec<_> = PLAYBACK_SETTINGS_ROWS
            .iter()
            .filter_map(|row| row.setting())
            .collect();
        assert_eq!(settings, PLAY_MODE_ROWS.to_vec());
        // Values come from the right source and leave out nothing.
        let view = PlayModeSettings::default().view();
        let values = PlayModeValues::default();
        for row in PLAYBACK_SETTINGS_ROWS {
            assert!(!row.value_text(&view, &values).is_empty(), "{row:?}");
            assert!(!row.description().is_empty(), "{row:?}");
            assert!(row.row_label().chars().count() <= 20, "{row:?} fits the row");
        }
        assert_eq!(SettingsRow::Mode(PlayModesRow::Repeat).value_text(&view, &values), "Off");
        assert_eq!(
            SettingsRow::Setting(PlayModeRow::WaveformZoom).value_text(&view, &values),
            "30s"
        );
        // Only the settings rows can enter the edit state; a session mode never does.
        for row in PLAYBACK_SETTINGS_ROWS {
            assert_eq!(
                row.edits_by_scroll(),
                row.setting().is_some_and(|r| r.edits_by_scroll()),
                "{row:?}"
            );
        }
    }

    /// PLAY_04: the four Play modes rows read the engine mirrors and never the settings file.
    #[test]
    fn play_modes_rows_are_the_four_engine_mirrors() {
        assert_eq!(
            PLAY_MODES_ROWS,
            [
                PlayModesRow::Shuffle,
                PlayModesRow::Repeat,
                PlayModesRow::PauseAfterEach,
                PlayModesRow::RepeatReshuffles
            ]
        );
        let defaults = PlayModeValues::default();
        assert_eq!(PlayModesRow::Shuffle.value_text(&defaults), "Off");
        assert_eq!(PlayModesRow::Repeat.value_text(&defaults), "Off");
        assert_eq!(PlayModesRow::PauseAfterEach.value_text(&defaults), "Off");
        assert_eq!(
            PlayModesRow::RepeatReshuffles.value_text(&defaults),
            "On",
            "owner decision 5: reshuffles on by default"
        );
        let all_on = PlayModeValues {
            order: OrderMode::Shuffle,
            repeat: RepeatMode::Item,
            pause_after_each: true,
            reshuffle_on_repeat: false,
            source_order: SourceOrder::Dynamic,
        };
        assert_eq!(PlayModesRow::Shuffle.value_text(&all_on), "On");
        assert_eq!(PlayModesRow::Repeat.value_text(&all_on), "One");
        assert_eq!(PlayModesRow::PauseAfterEach.value_text(&all_on), "On");
        assert_eq!(PlayModesRow::RepeatReshuffles.value_text(&all_on), "Off");
        for row in PLAY_MODES_ROWS {
            assert!(row.description().starts_with("Resets on device reset."), "{row:?}");
            assert!(row.label().chars().count() <= 20, "{row:?} label fits");
        }
    }

    /// PLAY_04: none of the five session settings has a key in `play_mode.json`, and a view
    /// built from a settings file always reports the session defaults, never a stored speed.
    #[test]
    fn play_modes_and_speed_are_never_written_to_the_settings_file() {
        let mut s = PlayModeSettings::default();
        // Choose everything that *is* persisted, so the file is non-empty.
        s.warp.speed = Some(WarpMode::PitchShift);
        s.warp.seek = Some(WarpMode::PitchCorrect);
        s.seek_outside_track = Some(true);
        s.waveform_zoom_ms = Some(150_000);
        s.timecode_unit = Some(TimecodeUnit::Frames24);
        s.rec_enables_nfc_poll = Some(true);
        let json = s.to_json();
        for key in [
            "order_mode",
            "repeat_mode",
            "pause_after_each",
            "reshuffle_on_repeat",
            "source_order",
            "rate_permille",
            "playback_speed",
            "target_rate_permille",
        ] {
            assert!(!json.contains(key), "`{key}` must not be persisted: {json}");
        }
        // A file carrying them anyway (hand-edited, or a future bug) is ignored, not honoured.
        let hostile = r#"{"order_mode":"shuffle","repeat_mode":"item","pause_after_each":true,
            "reshuffle_on_repeat":false,"rate_permille":1500}"#;
        let back = PlayModeSettings::from_json(hostile);
        assert_eq!(back, PlayModeSettings::default());
        assert_eq!(back.view().rate_permille, RATE_UNITY_PERMILLE);
        // And a view of a fully chosen file still starts at unity speed.
        assert_eq!(s.view().rate_permille, RATE_UNITY_PERMILLE);
    }

    #[test]
    fn fine_time_ladder_is_the_owner_order_and_clamps_at_both_ends() {
        assert_eq!(
            TIMECODE_UNITS,
            [
                TimecodeUnit::Off,
                TimecodeUnit::DecTenths,
                TimecodeUnit::Frames24,
                TimecodeUnit::Frames25,
                TimecodeUnit::Frames30,
                TimecodeUnit::Frames50,
                TimecodeUnit::Frames60,
                TimecodeUnit::Frames75,
                TimecodeUnit::DecHundredths,
            ]
        );
        assert_eq!(TimecodeUnit::Off.step(1), TimecodeUnit::DecTenths);
        assert_eq!(TimecodeUnit::Frames24.step(1), TimecodeUnit::Frames25);
        assert_eq!(
            TimecodeUnit::Frames75.step(1),
            TimecodeUnit::DecHundredths,
            "frame rates run low to high"
        );
        // Clamped, not wrapped: these are bounded ladders.
        assert_eq!(TimecodeUnit::Off.step(-1), TimecodeUnit::Off);
        assert_eq!(
            TimecodeUnit::DecHundredths.step(1),
            TimecodeUnit::DecHundredths
        );
        assert_eq!(TimecodeUnit::Frames75.step(-128), TimecodeUnit::Off);
        assert_eq!(
            TimecodeUnit::Frames24.step(2),
            TimecodeUnit::Frames30,
            "multi-detent steps keep the ladder order"
        );
        // Value texts.
        assert_eq!(TimecodeUnit::Off.text(), "Off");
        assert_eq!(TimecodeUnit::DecTenths.text(), "Dec .1");
        assert_eq!(TimecodeUnit::DecHundredths.text(), "Dec .01");
        assert_eq!(TimecodeUnit::Frames24.text(), "24 fps");
        assert_eq!(TimecodeUnit::Frames75.text(), "75 fps");
        for u in TIMECODE_UNITS {
            assert!(
                u.text().chars().count() <= 8,
                "{} does not fit the value column",
                u.text()
            );
        }
    }

    /// UI_16 §5: the finest jog click is derived from the resolved Fine time unit.
    #[test]
    fn finest_jog_step_follows_the_fine_time_unit() {
        let cases = [
            (TimecodeUnit::Off, 100),
            (TimecodeUnit::DecTenths, 100),
            (TimecodeUnit::DecHundredths, 10),
            (TimecodeUnit::Frames24, 41),
            (TimecodeUnit::Frames25, 40),
            (TimecodeUnit::Frames30, 33),
            (TimecodeUnit::Frames50, 20),
            (TimecodeUnit::Frames60, 16),
            (TimecodeUnit::Frames75, 13),
        ];
        for (unit, want) in cases {
            assert_eq!(unit.finest_jog_step_ms(), want, "{unit:?}");
            let s = PlayModeSettings {
                timecode_unit: Some(unit),
                ..Default::default()
            };
            assert_eq!(s.resolve_jog_step(), want, "resolved {unit:?}");
        }
        // Unset follows 75 fps, like the footer.
        assert_eq!(PlayModeSettings::default().resolve_jog_step(), 13);
    }

    #[test]
    fn timecode_unit_persists_each_variant_distinctly_and_unknown_falls_back_to_75() {
        for unit in TIMECODE_UNITS {
            let s = PlayModeSettings {
                timecode_unit: Some(unit),
                ..Default::default()
            };
            let json = s.to_json();
            let back = PlayModeSettings::from_json(&json);
            assert_eq!(back.timecode_unit, Some(unit), "{unit:?} round trip");
            assert_eq!(back.resolve_timecode_unit(), unit);
        }
        // The two decimals are distinct both in the file and in what they render.
        let tenths = PlayModeSettings {
            timecode_unit: Some(TimecodeUnit::DecTenths),
            ..Default::default()
        }
        .to_json();
        let hundredths = PlayModeSettings {
            timecode_unit: Some(TimecodeUnit::DecHundredths),
            ..Default::default()
        }
        .to_json();
        assert_ne!(tenths, hundredths);
        assert!(tenths.contains("\"decimal\""), "{tenths}");
        assert!(hundredths.contains("\"decimal_100\""), "{hundredths}");
        // Off is an explicit choice, persisted distinctly from "unset" — and unset still
        // resolves to 75 fps, never to Off.
        let off = PlayModeSettings {
            timecode_unit: Some(TimecodeUnit::Off),
            ..Default::default()
        };
        let json = off.to_json();
        assert!(json.contains("\"timecode_unit\": \"off\""), "{json}");
        let back = PlayModeSettings::from_json(&json);
        assert_eq!(back.timecode_unit, Some(TimecodeUnit::Off));
        assert_eq!(back.resolve_timecode_unit(), TimecodeUnit::Off);
        assert!(
            !PlayModeSettings::default().to_json().contains("timecode_unit"),
            "unset is not persisted"
        );
        assert_eq!(
            PlayModeSettings::default().resolve_timecode_unit(),
            TimecodeUnit::Frames75
        );
        // A pre-UI_16 file's decimal mode still means tenths.
        assert_eq!(
            PlayModeSettings::from_json(r#"{"timecode_unit":"decimal"}"#).resolve_timecode_unit(),
            TimecodeUnit::DecTenths
        );
        assert_eq!(
            PlayModeSettings::from_json(r#"{"timecode_unit":"smpte_48"}"#).resolve_timecode_unit(),
            TimecodeUnit::Frames75
        );
        assert_eq!(
            PlayModeSettings::from_json(r#"{"timecode_unit":7}"#).resolve_timecode_unit(),
            TimecodeUnit::Frames75
        );
        assert_eq!(
            PlayModeSettings::from_json("not json").resolve_timecode_unit(),
            TimecodeUnit::Frames75
        );
    }

    /// UI_16 §8: the Debug setting is explicit, default Off, and lives in this file.
    #[test]
    fn rec_enables_nfc_poll_persists_only_when_chosen() {
        assert!(!PlayModeSettings::default().resolve_rec_enables_nfc_poll());
        assert!(!PlayModeSettings::default().to_json().contains("rec_enables_nfc_poll"));
        let off = PlayModeSettings {
            rec_enables_nfc_poll: Some(false),
            ..Default::default()
        };
        assert!(!off.resolve_rec_enables_nfc_poll());
        assert!(off.to_json().contains("\"rec_enables_nfc_poll\": false"));
        let on = PlayModeSettings {
            rec_enables_nfc_poll: Some(true),
            ..Default::default()
        };
        let json = on.to_json();
        assert!(json.contains("\"rec_enables_nfc_poll\": true"), "{json}");
        assert_eq!(PlayModeSettings::from_json(&json), on);
        assert!(on.view().rec_enables_nfc_poll);
        assert_eq!(DebugRow::RecEnablesNfcPoll.value_text(&on.view()), "On");
        assert_eq!(
            DebugRow::RecEnablesNfcPoll.value_text(&PlayModeSettings::default().view()),
            "Off"
        );
        assert_eq!(
            DebugRow::RecEnablesNfcPoll.label(),
            "Rec enables NFC poll"
        );
    }

    #[test]
    fn zoom_ladder_is_sorted_and_covers_1s_to_10min() {
        assert_eq!(ZOOM_LADDER_MS.len(), 20);
        assert_eq!(ZOOM_LADDER_MS[0], 1_000);
        assert_eq!(ZOOM_LADDER_MS[19], 600_000);
        assert!(ZOOM_LADDER_MS.contains(&ZOOM_DEFAULT_MS));
        assert!(
            ZOOM_LADDER_MS.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing"
        );
    }

    #[test]
    fn zoom_step_clamps_and_does_not_wrap() {
        // One step in either direction from the default.
        assert_eq!(zoom_step(ZOOM_DEFAULT_MS, 1), 40_000);
        assert_eq!(zoom_step(ZOOM_DEFAULT_MS, -1), 20_000);
        // Both ends clamp, repeated detents stay put.
        assert_eq!(zoom_step(1_000, -1), 1_000);
        assert_eq!(zoom_step(600_000, 1), 600_000);
        assert_eq!(zoom_step(1_000, -128), 1_000);
        assert_eq!(zoom_step(600_000, 127), 600_000);
        // An off-ladder value behaves like the default.
        assert_eq!(zoom_step(33_333, 1), 40_000);
        assert_eq!(zoom_index(33_333), zoom_index(ZOOM_DEFAULT_MS));
    }

    #[test]
    fn zoom_text_formats() {
        assert_eq!(zoom_text(1_000), "1s");
        assert_eq!(zoom_text(30_000), "30s");
        assert_eq!(zoom_text(60_000), "1m00s");
        assert_eq!(zoom_text(150_000), "2m30s");
        assert_eq!(zoom_text(600_000), "10m00s");
        for ms in ZOOM_LADDER_MS {
            assert!(measure_fits(&zoom_text(ms)), "{ms} does not fit the band");
        }
    }

    /// Worst case readout is six glyphs; the Adjust band gives it 60 px at `Display`.
    fn measure_fits(text: &str) -> bool {
        crate::ui::text::measure(text, crate::ui::text::TypeRole::Display) <= 60
    }

    #[test]
    fn waveform_zoom_setting_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PLAY_MODE_FILE_NAME);
        let mut store = PlayModeStore::with_path(&path);
        assert_eq!(store.settings.resolve_waveform_zoom(), ZOOM_DEFAULT_MS);
        assert!(!store.settings.to_json().contains("waveform_zoom_ms"));
        store.settings.waveform_zoom_ms = Some(150_000);
        store.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"waveform_zoom_ms\": 150000"), "{text}");
        let loaded = PlayModeStore::load_or_default(&path);
        assert_eq!(loaded.settings.waveform_zoom_ms, Some(150_000));
        assert_eq!(loaded.settings.resolve_waveform_zoom(), 150_000);
        // Off-ladder and garbage values fall back to 30 s.
        let off = PlayModeSettings::from_json(r#"{"waveform_zoom_ms": 33}"#);
        assert_eq!(off.waveform_zoom_ms, None);
        assert_eq!(off.resolve_waveform_zoom(), ZOOM_DEFAULT_MS);
        let garbage = PlayModeSettings::from_json(r#"{"waveform_zoom_ms": "8s"}"#);
        assert_eq!(garbage.waveform_zoom_ms, None);
        assert_eq!(garbage.resolve_waveform_zoom(), ZOOM_DEFAULT_MS);
    }

    #[test]
    fn seek_outside_track_defaults_off_and_is_written_only_when_chosen() {
        let s = PlayModeSettings::default();
        assert!(!s.resolve_seek_outside());
        assert!(!s.to_json().contains("seek_outside_track"));
        let on = PlayModeSettings {
            seek_outside_track: Some(true),
            ..Default::default()
        };
        assert!(on.to_json().contains("\"seek_outside_track\": true"));
        assert_eq!(PlayModeSettings::from_json(&on.to_json()), on);
        assert_eq!(
            PlayModeSettings::from_json(r#"{"seek_outside_track":"yes"}"#).seek_outside_track,
            None
        );
    }
}
