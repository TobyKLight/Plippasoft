//! Cassette-style scan profile (WARP_17): the speed of an FF/RW **hold** as a function of the
//! hold time, with the spin-up lag. Pure functions of simulated time: the audio thread calls
//! [`ScanProfile::scan_target`] and [`ScanProfile::step`] once per quantum with the quantum's
//! duration; nothing here reads a clock.
//!
//! The scan speed is an **absolute source speed** (independent of the set tempo): `base`
//! (4x) while the button is held, `boost` (8x) after `boost_after_s` of continuous hold, both
//! scaled by the held **speed multiplier** (UI_15: the encoder ladder, 1.0 = the usual scan,
//! 0.01 = 1 % of it, which is below 1x playback). The rate follows its target through a
//! first-order lag in the **linear rate domain**, starting from the instantaneous rate at
//! entry (the set speed, so the pitch starts continuous).
//!
//! **Release has no ramp (UI_15).** The audio thread ends the scan at once: it sets the rate
//! straight back to the session's set speed — or, for a scan that started from paused, flushes
//! the sink and pauses exactly on the heard position — through `RampSpec::Immediate`. There is
//! deliberately no brake here: the owner wants what is on screen, what is heard and where
//! playback resumes to agree the instant the button comes up. The spin-up lag stays (holding
//! the button still ramps up to the search speed).
//!
//! All tuning constants are in [`ScanProfile`] (conservative defaults, README section 7;
//! the audible result is tuned on the device).

/// Tuning of the hold scan.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanProfile {
    /// Absolute scan speed while held (`SCAN_BASE`).
    pub base: f64,
    /// Absolute scan speed after `boost_after_s` of continuous hold (`SCAN_BOOST`).
    pub boost: f64,
    /// Hold time after which the scan speeds up to `boost` (s, `BOOST_AFTER_S`).
    pub boost_after_s: f64,
    /// Spin-up time constant toward `base` (s, `TAU_UP_S`).
    pub tau_up_s: f64,
    /// Time constant of the further spin-up to `boost` (s, `TAU_BOOST_S`).
    pub tau_boost_s: f64,
    /// The lag snaps to its target when within this fraction of `|target|` (0.5%).
    pub snap: f64,
}

impl Default for ScanProfile {
    fn default() -> Self {
        Self {
            base: 4.0,
            boost: 8.0,
            boost_after_s: 2.0,
            tau_up_s: 0.30,
            tau_boost_s: 1.0,
            snap: 0.005,
        }
    }
}

impl ScanProfile {
    /// Target rate and time constant for `dir` (+1 / -1) while the button is held for
    /// `hold_s` seconds. `cap` is the largest `|rate|` the voice and the decode headroom
    /// allow; it clamps the held target. `speed_mult` scales the usual scan speed (UI_15:
    /// `plippa_core::scan_speed_scale`; `1.0` = the tuning above).
    ///
    /// `cap` is an **upper bound on the target only**. A multiplier that puts the target below
    /// 1x (the ladder's 1 % / 5 % / 10 % entries) is used as-is: the `max(1.0)` in `cap` keeps
    /// an unusable *cap* from pulling the scan down, it is never a floor on the chosen speed.
    pub fn scan_target(&self, dir: i8, hold_s: f64, cap: f64, speed_mult: f64) -> (f64, f64) {
        let sign = if dir < 0 { -1.0 } else { 1.0 };
        let (mag, tau) = if hold_s >= self.boost_after_s {
            (self.boost, self.tau_boost_s)
        } else {
            (self.base, self.tau_up_s)
        };
        (sign * (mag * speed_mult).min(cap.max(1.0)), tau)
    }

    /// One lag step of `dt` seconds from `rate` toward `target` with time constant `tau`
    /// (linear rate domain). Monotone (never overshoots) and snapping within `snap`.
    pub fn step(&self, rate: f64, target: f64, tau: f64, dt: f64) -> f64 {
        let k = if tau <= 0.0 {
            1.0
        } else {
            1.0 - (-dt / tau).exp()
        };
        let next = rate + (target - rate) * k;
        if (next - target).abs() <= self.snap * target.abs().max(1.0) {
            target
        } else {
            next
        }
    }
}

/// The scan's `|rate|` cap: the voice cap bounded by the decode headroom (`sustainable` is
/// `max_sustainable_rate()` when measured; `None` = unknown, no clamp). Never below 1.0 (the
/// worker may only hold the window edge, never glitch, but the scan does not crawl). This is
/// the **cap**, not a floor on the speed the user selects: [`ScanProfile::scan_target`] takes
/// sub-unity targets without clamping them up.
pub fn scan_rate_cap(voice_cap: f64, sustainable: Option<f64>) -> f64 {
    match sustainable {
        Some(s) if s > 0.0 => voice_cap.min(s).max(1.0),
        _ => voice_cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: f64 = 256.0 / 48_000.0;

    /// (time, rate) trace of a hold of `hold_total_s`, from `start` at the set speed.
    fn hold(p: &ScanProfile, start: f64, dir: i8, hold_total_s: f64, cap: f64, mult: f64) -> Vec<(f64, f64)> {
        let mut t = 0.0;
        let mut r = start;
        let mut out = vec![(0.0, r)];
        while t < hold_total_s {
            let (tg, tau) = p.scan_target(dir, t, cap, mult);
            r = p.step(r, tg, tau, Q);
            t += Q;
            out.push((t, r));
        }
        out
    }

    #[test]
    fn spin_up_is_monotone_reaches_95_percent_of_4x_in_a_second_and_never_overshoots() {
        let p = ScanProfile::default();
        let tr = hold(&p, 1.0, 1, 1.9, 8.0, 1.0);
        let mut prev = 1.0;
        let mut t95 = None;
        for &(t, r) in &tr {
            assert!(r >= prev - 1e-12 && r <= 4.0 + 1e-9, "t {t} r {r}");
            if t95.is_none() && r >= 0.95 * 4.0 {
                t95 = Some(t);
            }
            prev = r;
        }
        assert!(t95.unwrap() <= 0.9, "95% at {:?}", t95);
    }

    #[test]
    fn boost_after_two_seconds_goes_toward_8x_and_is_capped() {
        let p = ScanProfile::default();
        let tr = hold(&p, 1.0, 1, 6.0, 8.0, 1.0);
        let at = |s: f64| tr.iter().find(|(t, _)| *t >= s).unwrap().1;
        assert!(at(1.9) <= 4.0 + 1e-9, "no boost before 2 s");
        assert!(at(2.5) > 4.0 && at(5.9) > 7.0 && at(5.9) <= 8.0 + 1e-9);
        // Stretch cap 3.0: the held target is clamped, both before and after the boost.
        let tr = hold(&p, 1.0, 1, 4.0, 3.0, 1.0);
        assert!(tr.iter().all(|(_, r)| *r <= 3.0 + 1e-9));
    }

    #[test]
    fn absolute_reference_scan_at_set_speed_1_5_targets_4_not_6() {
        let p = ScanProfile::default();
        let (tg, _) = p.scan_target(1, 0.0, 8.0, 1.0);
        assert_eq!(tg, 4.0);
        let tr = hold(&p, 1.5, 1, 1.5, 8.0, 1.0);
        // Starts from 1.5 (no step) and the first step is a small move toward 4.
        assert_eq!(tr[0].1, 1.5);
        assert!(tr[1].1 > 1.5 && tr[1].1 < 1.5 + 0.2);
        assert!(tr.iter().all(|(_, r)| *r <= 4.0 + 1e-9));
    }

    #[test]
    fn opposite_direction_glides_through_zero_monotonically() {
        let p = ScanProfile::default();
        let mut r = 4.0;
        let mut prev = r;
        let mut crossed = false;
        for _ in 0..400 {
            let (tg, tau) = p.scan_target(-1, 0.0, 8.0, 1.0);
            r = p.step(r, tg, tau, Q);
            assert!(r <= prev + 1e-12, "monotone");
            if r < 0.0 {
                crossed = true;
            }
            prev = r;
        }
        assert!(crossed && r >= -4.0 - 1e-9);
        assert!((r + 4.0).abs() < 0.05);
    }

    #[test]
    fn a_speed_multiplier_scales_the_target_and_supports_sub_unity_scans() {
        let p = ScanProfile::default();
        // 400 % of the usual 4x / 8x.
        assert_eq!(p.scan_target(1, 0.0, 8.0, 4.0).0, 8.0, "4x of 4x capped at 8x");
        assert_eq!(p.scan_target(1, 5.0, 8.0, 4.0).0, 8.0);
        // 150 %.
        assert_eq!(p.scan_target(1, 0.0, 8.0, 1.5).0, 6.0);
        // 1 % / 5 % / 10 % of the usual scan: well below 1x playback, and *not* clamped up to
        // 1.0 by the cap (the cap only bounds from above).
        assert!((p.scan_target(1, 0.0, 8.0, 0.01).0 - 0.04).abs() < 1e-12);
        assert!((p.scan_target(1, 0.0, 8.0, 0.05).0 - 0.20).abs() < 1e-12);
        assert!((p.scan_target(-1, 0.0, 8.0, 0.10).0 + 0.40).abs() < 1e-12);
        // A capped-voice scan (stretch, 3x) still respects the multiplier.
        assert_eq!(p.scan_target(1, 0.0, 3.0, 2.0).0, 3.0, "6x capped at 3x");
        // The lag actually reaches a sub-unity target instead of settling above it.
        let tr = hold(&p, 1.0, 1, 2.0, 8.0, 0.10);
        let last = tr.last().unwrap().1;
        assert!((last - 0.40).abs() < 0.02, "settled at {last}");
    }

    #[test]
    fn a_live_multiplier_change_re_targets_the_running_scan_through_the_lag() {
        let p = ScanProfile::default();
        // At the usual 4x, then the encoder drops to 25 % of it: the rate glides down to 1.0.
        let mut r = p.step(1.0, 4.0, p.tau_up_s, 1.5);
        assert!(r > 3.9);
        let mut t = 0.0;
        while t < 2.0 {
            let (tg, tau) = p.scan_target(1, 0.5, 8.0, 0.25);
            r = p.step(r, tg, tau, Q);
            assert!(r >= 1.0 - 1e-9, "monotone toward the new target");
            t += Q;
        }
        assert!((r - 1.0).abs() < 0.02, "re-targeted to 25 % of 4x: {r}");
    }

    #[test]
    fn headroom_clamp_table() {
        // Injected `max_sustainable_rate()` values (never measured here).
        assert_eq!(scan_rate_cap(8.0, Some(3.0)), 3.0);
        assert_eq!(scan_rate_cap(8.0, None), 8.0);
        assert_eq!(scan_rate_cap(8.0, Some(20.0)), 8.0);
        assert_eq!(scan_rate_cap(3.0, Some(5.0)), 3.0);
        // The "never crawls below 1x" guard is on the *cap*, never on the chosen speed (see
        // `a_speed_multiplier_scales_the_target_and_supports_sub_unity_scans`).
        assert_eq!(scan_rate_cap(8.0, Some(0.4)), 1.0, "never crawls below 1x");
        let p = ScanProfile::default();
        let cap = scan_rate_cap(8.0, Some(3.0));
        assert_eq!(p.scan_target(1, 5.0, cap, 1.0).0, 3.0);
        assert_eq!(p.scan_target(-1, 5.0, cap, 1.0).0, -3.0);
        // A sub-unity chosen speed is not raised to the 1x cap.
        assert!((p.scan_target(1, 0.0, cap, 0.05).0 - 0.2).abs() < 1e-12);
    }
}
