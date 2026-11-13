//! WARP series wire/domain types: the two warp-mode settings, scan state, voice kind and the
//! validation maths shared by the engine and the UI (WARP_01).
//!
//! Two control families (see the WARP README): the **speed family** (`SetRate`, taps, tempo)
//! edits the session-wide set speed; the **seek family** (hold scan, jog `SeekTo`) moves the
//! playhead. Both use a [`WarpMode`] setting, kept separately (`speed_warp_mode`,
//! `seek_warp_mode`). Nothing here touches audio.

use serde::{Deserialize, Serialize};

/// How a family warps time: varispeed (pitch follows speed) or time-stretch (pitch kept).
/// Used for **both** settings (speed warp mode and seek warp mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WarpMode {
    /// Varispeed: pitch follows speed (the cassette sound).
    PitchShift,
    /// Time-stretch: pitch is kept (forward travel only; reverse is always pitch-warped).
    PitchCorrect,
}

/// Direction of a hold scan (FF/RW hold).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScanDir {
    Forward,
    Backward,
}

/// Published scan state (`None` unless FF/RW is held).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ScanState {
    #[default]
    None,
    Forward,
    Backward,
}

impl From<ScanDir> for ScanState {
    fn from(d: ScanDir) -> Self {
        match d {
            ScanDir::Forward => ScanState::Forward,
            ScanDir::Backward => ScanState::Backward,
        }
    }
}

/// Which voice the speed family uses. **Diagnostic only** (tests, `plippa-np`); the UI never
/// shows it as an indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum VoiceKind {
    /// Exactly 100 % and no seek: the untouched pure path.
    #[default]
    Bypass,
    Varispeed,
    Stretch,
}

/// Default speed warp mode. **One named constant**: `PitchCorrect` since WARP_09 (the
/// owner-decided default; set back to `PitchShift` if the CM4 cannot afford the stretcher).
pub const SPEED_WARP_DEFAULT: WarpMode = WarpMode::PitchCorrect;
/// Default seek warp mode (the cassette sound).
pub const SEEK_WARP_DEFAULT: WarpMode = WarpMode::PitchShift;

/// Neutral set speed (1.0x) in permille.
pub const RATE_UNITY_PERMILLE: i32 = 1000;
/// Smallest accepted `|SetRate|` (10 %).
pub const RATE_MIN_ABS_PERMILLE: i32 = 100;
/// Largest accepted `|SetRate|` (200 %).
pub const RATE_MAX_ABS_PERMILLE: i32 = 2000;
/// A seek target is never closer than this to the end of the track, so a jog can never
/// trigger end-of-track.
pub const SEEK_END_GUARD_MS: u64 = 250;

/// `SetRate` validity: `100..=2000` or `-2000..=-100` permille (0 and out of range rejected).
pub fn rate_permille_valid(permille: i32) -> bool {
    (RATE_MIN_ABS_PERMILLE as u32..=RATE_MAX_ABS_PERMILLE as u32).contains(&permille.unsigned_abs())
}

/// Clamp a `SeekTo` target to `[0, duration - SEEK_END_GUARD_MS]` (unknown duration: lower
/// clamp only, i.e. unchanged). A track shorter than the guard clamps to 0.
pub fn clamp_seek_target_ms(target_ms: u64, duration_ms: Option<u64>) -> u64 {
    match duration_ms {
        Some(d) if d > 0 => target_ms.min(d.saturating_sub(SEEK_END_GUARD_MS)),
        _ => target_ms,
    }
}

/// Tempo range in permille (`TEMPO_MIN_PERMILLE ..= TEMPO_MAX_PERMILLE`, 10 %-200 %) for both
/// speed modes (T4); the UI can clamp with [`tempo_range`].
pub const TEMPO_MIN_PERMILLE: i32 = RATE_MIN_ABS_PERMILLE;
pub const TEMPO_MAX_PERMILLE: i32 = RATE_MAX_ABS_PERMILLE;

/// Allowed `|rate|` range in permille for a speed mode (full range for both initially).
pub fn tempo_range(_mode: WarpMode) -> std::ops::RangeInclusive<i32> {
    TEMPO_MIN_PERMILLE..=TEMPO_MAX_PERMILLE
}

/// Seek-voice stretch caps (conservative starting values, tuned from the eval app `cpu` and
/// listening results): smoothed chase and hold-scan with `seek_warp_mode = PitchCorrect`.
pub const MAX_SEEK_STRETCH_CHASE: f64 = 2.0;
pub const MAX_SEEK_STRETCH_SCAN: f64 = 3.0;
/// Varispeed seek caps (eval-app defaults `R_MAX_VARI` 4, `MAX_SCAN_VARI` 8).
pub const MAX_SEEK_VARI_CHASE: f64 = 4.0;
pub const MAX_SEEK_VARI_SCAN: f64 = 8.0;

/// Which seek driver a cap applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekDriver {
    /// Chase toward a jog target (WARP_14).
    Chase,
    /// FF/RW hold scan (WARP_17).
    Scan,
}

/// Voice of the seek family for `mode` and travel direction: `Stretch` only for
/// `PitchCorrect` **forward** travel; backward travel is always varispeed.
pub fn seek_voice(mode: WarpMode, dir: ScanDir) -> VoiceKind {
    match (mode, dir) {
        (WarpMode::PitchCorrect, ScanDir::Forward) => VoiceKind::Stretch,
        _ => VoiceKind::Varispeed,
    }
}

/// Largest `|rate|` the seek family may use for `driver` in `mode`/`dir` (the drivers apply it
/// as their clamp; the stretch caps are lower than the varispeed ones).
pub fn max_rate(mode: WarpMode, dir: ScanDir, driver: SeekDriver) -> f64 {
    match (seek_voice(mode, dir), driver) {
        (VoiceKind::Stretch, SeekDriver::Chase) => MAX_SEEK_STRETCH_CHASE,
        (VoiceKind::Stretch, SeekDriver::Scan) => MAX_SEEK_STRETCH_SCAN,
        (_, SeekDriver::Chase) => MAX_SEEK_VARI_CHASE,
        (_, SeekDriver::Scan) => MAX_SEEK_VARI_SCAN,
    }
}

// ---- UI_15: the FF/RW-hold scan speed (encoder ladder) ----

/// Scan speeds the encoder steps through **while FF/RW is held**, in permille of the profile's
/// usual cassette scan speed (`ScanProfile::base` while held, `boost` after 2 s): `1000` = the
/// usual scan, `10` = 1 % of it (far below 1x playback), `4000` = 4x it. One list for the UI
/// (which steps it) and the engine (which validates the wire value), so the two cannot drift.
pub const SCAN_SPEED_LADDER_PERMILLE: [i32; 9] = [10, 50, 100, 250, 500, 1000, 1500, 2000, 4000];
/// Ladder entry in force at the start of every hold (100 % = the usual scan speed).
pub const SCAN_SPEED_DEFAULT_PERMILLE: i32 = 1000;
/// Lowest / highest ladder entry (both ends clamp: the selection never wraps).
pub const SCAN_SPEED_MIN_PERMILLE: i32 = SCAN_SPEED_LADDER_PERMILLE[0];
pub const SCAN_SPEED_MAX_PERMILLE: i32 = SCAN_SPEED_LADDER_PERMILLE[8];

/// A wire `ScanRate` value: one of the ladder entries.
pub fn scan_speed_valid(permille: i32) -> bool {
    SCAN_SPEED_LADDER_PERMILLE.contains(&permille)
}

/// `delta` encoder detents through the ladder from `current`, clamped at both ends (never
/// wrapped, never zero). A value that is not exactly on the ladder steps to the next entry
/// past it in the turn's direction, so the result is always a ladder entry.
pub fn scan_speed_step(current: i32, delta: i8) -> i32 {
    let mut v = current;
    for _ in 0..delta.unsigned_abs() {
        v = if delta > 0 {
            SCAN_SPEED_LADDER_PERMILLE
                .iter()
                .copied()
                .find(|x| *x > v)
                .unwrap_or(SCAN_SPEED_MAX_PERMILLE)
        } else if delta < 0 {
            SCAN_SPEED_LADDER_PERMILLE
                .iter()
                .copied()
                .rev()
                .find(|x| *x < v)
                .unwrap_or(SCAN_SPEED_MIN_PERMILLE)
        } else {
            v
        };
    }
    v
}

/// The scan-speed multiplier for a ladder value: `1000` -> 1.0 (the usual scan speed).
pub fn scan_speed_scale(permille: i32) -> f64 {
    f64::from(permille.clamp(SCAN_SPEED_MIN_PERMILLE, SCAN_SPEED_MAX_PERMILLE)) / 1000.0
}

/// Persisted play-mode warp choices (used by WARP_10). `None` means "never chosen: follow the
/// default", so the WARP_09 default flip reaches devices that never opened the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WarpModeSettings {
    #[serde(default)]
    pub speed: Option<WarpMode>,
    #[serde(default)]
    pub seek: Option<WarpMode>,
}

impl WarpModeSettings {
    /// `(speed, seek)` with the defaults applied.
    pub fn resolve(self) -> (WarpMode, WarpMode) {
        (
            self.speed.unwrap_or(SPEED_WARP_DEFAULT),
            self.seek.unwrap_or(SEEK_WARP_DEFAULT),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_validation_bounds() {
        for ok in [100, 101, 1000, 1500, 2000, -100, -750, -2000] {
            assert!(rate_permille_valid(ok), "{ok}");
        }
        for bad in [0, 99, -99, 2001, -2001, i32::MIN, i32::MAX] {
            assert!(!rate_permille_valid(bad), "{bad}");
        }
    }

    #[test]
    fn seek_target_clamp() {
        assert_eq!(clamp_seek_target_ms(5_000, Some(10_000)), 5_000);
        assert_eq!(clamp_seek_target_ms(9_900, Some(10_000)), 9_750);
        assert_eq!(clamp_seek_target_ms(u64::MAX, Some(10_000)), 9_750);
        assert_eq!(
            clamp_seek_target_ms(u64::MAX, None),
            u64::MAX,
            "unknown: no upper clamp"
        );
        assert_eq!(
            clamp_seek_target_ms(7, Some(100)),
            0,
            "shorter than the guard"
        );
        assert_eq!(
            clamp_seek_target_ms(7, Some(0)),
            7,
            "zero duration = unknown"
        );
    }

    #[test]
    fn default_resolves_to_pitch_correct_speed_and_pitch_shift_seek() {
        assert_eq!(
            WarpModeSettings::default().resolve(),
            (WarpMode::PitchCorrect, WarpMode::PitchShift)
        );
    }

    #[test]
    fn seek_voice_and_caps_never_stretch_backward() {
        for dir in [ScanDir::Forward, ScanDir::Backward] {
            assert_eq!(seek_voice(WarpMode::PitchShift, dir), VoiceKind::Varispeed);
        }
        assert_eq!(
            seek_voice(WarpMode::PitchCorrect, ScanDir::Forward),
            VoiceKind::Stretch
        );
        assert_eq!(
            seek_voice(WarpMode::PitchCorrect, ScanDir::Backward),
            VoiceKind::Varispeed
        );
        let m = WarpMode::PitchCorrect;
        assert_eq!(max_rate(m, ScanDir::Forward, SeekDriver::Scan), MAX_SEEK_STRETCH_SCAN);
        assert_eq!(max_rate(m, ScanDir::Forward, SeekDriver::Chase), MAX_SEEK_STRETCH_CHASE);
        assert_eq!(max_rate(m, ScanDir::Backward, SeekDriver::Scan), MAX_SEEK_VARI_SCAN);
        assert_eq!(
            max_rate(WarpMode::PitchShift, ScanDir::Forward, SeekDriver::Chase),
            MAX_SEEK_VARI_CHASE
        );
    }

    #[test]
    fn tempo_range_is_the_full_range_for_both_modes() {
        for m in [WarpMode::PitchShift, WarpMode::PitchCorrect] {
            assert_eq!(tempo_range(m), 100..=2000);
        }
    }

    #[test]
    fn settings_resolve_applies_defaults_independently() {
        assert_eq!(
            WarpModeSettings::default().resolve(),
            (SPEED_WARP_DEFAULT, SEEK_WARP_DEFAULT)
        );
        let s = WarpModeSettings {
            speed: Some(WarpMode::PitchCorrect),
            seek: None,
        };
        assert_eq!(s.resolve(), (WarpMode::PitchCorrect, SEEK_WARP_DEFAULT));
        let s = WarpModeSettings {
            speed: None,
            seek: Some(WarpMode::PitchCorrect),
        };
        assert_eq!(s.resolve(), (SPEED_WARP_DEFAULT, WarpMode::PitchCorrect));
        // Explicit choice that equals today's default is still an explicit choice.
        let s = WarpModeSettings {
            speed: Some(WarpMode::PitchShift),
            seek: Some(WarpMode::PitchShift),
        };
        assert_eq!(s.resolve(), (WarpMode::PitchShift, WarpMode::PitchShift));
    }

    #[test]
    fn settings_json_round_trip_and_missing_fields() {
        let s = WarpModeSettings {
            speed: Some(WarpMode::PitchCorrect),
            seek: None,
        };
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<WarpModeSettings>(&j).unwrap(), s);
        assert_eq!(
            serde_json::from_str::<WarpModeSettings>("{}").unwrap(),
            WarpModeSettings::default()
        );
    }

    #[test]
    fn scan_state_from_dir_and_defaults() {
        assert_eq!(ScanState::from(ScanDir::Forward), ScanState::Forward);
        assert_eq!(ScanState::from(ScanDir::Backward), ScanState::Backward);
        assert_eq!(ScanState::default(), ScanState::None);
        assert_eq!(VoiceKind::default(), VoiceKind::Bypass);
    }

    // ---- UI_15: the held-scan speed ladder ----

    #[test]
    fn scan_speed_ladder_steps_one_entry_per_detent_and_clamps_both_ends() {
        // The published ladder, in the owner's order, nine entries, 100 % in the middle.
        assert_eq!(
            SCAN_SPEED_LADDER_PERMILLE,
            [10, 50, 100, 250, 500, 1000, 1500, 2000, 4000]
        );
        assert_eq!(SCAN_SPEED_LADDER_PERMILLE[5], SCAN_SPEED_DEFAULT_PERMILLE);
        assert!(scan_speed_valid(SCAN_SPEED_DEFAULT_PERMILLE));
        // Up one entry at a time from the default.
        assert_eq!(scan_speed_step(1000, 1), 1500);
        assert_eq!(scan_speed_step(1500, 1), 2000);
        assert_eq!(scan_speed_step(2000, 1), 4000);
        // Down one entry at a time, through the default to the bottom.
        assert_eq!(scan_speed_step(1000, -1), 500);
        assert_eq!(scan_speed_step(500, -1), 250);
        assert_eq!(scan_speed_step(100, -1), 50);
        assert_eq!(scan_speed_step(50, -1), 10);
        // Clamped: no wrap at either end, however many detents arrive.
        assert_eq!(scan_speed_step(4000, 1), 4000);
        assert_eq!(scan_speed_step(4000, 9), 4000);
        assert_eq!(scan_speed_step(10, -1), 10);
        assert_eq!(scan_speed_step(10, -9), 10);
        // Multi-detent turns move that many entries and stop at the ends.
        assert_eq!(scan_speed_step(10, 3), 250);
        assert_eq!(scan_speed_step(1000, 5), 4000);
        assert_eq!(scan_speed_step(1000, -5), 10);
        assert_eq!(scan_speed_step(1000, 0), 1000);
        // An off-ladder value lands on the neighbouring entry in the turn's direction.
        assert_eq!(scan_speed_step(870, 1), 1000);
        assert_eq!(scan_speed_step(870, -1), 500);
        // Every reachable value is a ladder entry and validates for the wire.
        for i in 0..SCAN_SPEED_LADDER_PERMILLE.len() {
            let v = SCAN_SPEED_LADDER_PERMILLE[i];
            assert!(scan_speed_valid(v));
        }
        assert!(!scan_speed_valid(0));
        assert!(!scan_speed_valid(875));
        assert!(!scan_speed_valid(8000));
    }

    #[test]
    fn scan_speed_scale_is_a_multiplier_with_unity_as_the_default() {
        assert_eq!(scan_speed_scale(SCAN_SPEED_DEFAULT_PERMILLE), 1.0);
        assert_eq!(scan_speed_scale(4000), 4.0);
        assert_eq!(scan_speed_scale(10), 0.01);
        // Out-of-range values clamp into the published ladder instead of exploding.
        assert_eq!(scan_speed_scale(0), 0.01);
        assert_eq!(scan_speed_scale(99_999), 4.0);
    }
}
