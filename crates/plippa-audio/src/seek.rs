//! Seek-family chase controller (WARP_14).
//!
//! `SeekTo` makes the audio travel from the actual playhead to a target over accelerated
//! time: a short, bounded swoop (varispeed by default) for near targets, a crossfaded
//! waypoint jump followed by the same swoop for far ones. The controller here is a pure
//! function of the distance left; the audio thread calls it once per quantum. Nothing in this
//! module measures time: `dt` is the duration of the quantum being rendered.
//!
//! All tuning constants live in [`ChaseConfig`] (conservative defaults, README section 7;
//! to be tuned on the device).

use plippa_core::{max_rate, ScanDir, SeekDriver, WarpMode};

/// Tuning of the commit chase.
#[derive(Debug, Clone, PartialEq)]
pub struct ChaseConfig {
    /// Time constant of the proportional part: `r_prop = |d| / T_CATCH_S + R_FLOOR` (s).
    pub t_catch_s: f64,
    /// Slowest chase rate (the swoop never crawls); also the rate it lands at.
    pub r_floor: f64,
    /// Stopping-distance deceleration for the brake bound (rate units per second).
    pub a_decel: f64,
    /// Largest rate change per second (slew limit; a flip passes through zero at this slope).
    pub a_slew: f64,
    /// Varispeed rate cap, either direction.
    pub r_max_vari: f64,
    /// Stretch (pitch-correct) rate cap, forward only.
    pub r_max_stretch: f64,
    /// A distance needing longer than `r_max x chase_max_time_s` to travel is a far jump (s).
    pub chase_max_time_s: f64,
    /// How far before a far target the waypoint jump lands (s).
    pub waypoint_lead_s: f64,
    /// Landing tolerance of voices that cannot shorten their last quantum (frames).
    pub land_eps_frames: f64,
}

impl Default for ChaseConfig {
    fn default() -> Self {
        Self {
            t_catch_s: 0.2,
            r_floor: 1.0,
            a_decel: 24.0,
            a_slew: 60.0,
            r_max_vari: 4.0,
            r_max_stretch: 2.0,
            chase_max_time_s: 1.7,
            waypoint_lead_s: 3.0,
            land_eps_frames: 256.0,
        }
    }
}

impl ChaseConfig {
    /// Rate cap of the seek voice `mode` uses for travel in direction `dir`.
    pub fn r_max(&self, mode: WarpMode, dir: ScanDir) -> f64 {
        // The core table decides which voice is used; the caps come from this config.
        let table = max_rate(mode, dir, SeekDriver::Chase);
        if table == plippa_core::MAX_SEEK_STRETCH_CHASE {
            self.r_max_stretch
        } else {
            self.r_max_vari
        }
    }

    /// Distance (seconds of media) beyond which a chase becomes a waypoint jump.
    pub fn far_jump_s(&self, r_max: f64) -> f64 {
        r_max * self.chase_max_time_s
    }
}

/// One chase step: the signed rate for the next quantum of `dt` seconds.
///
/// `rate` is the rate of the previous quantum (signed, source frames per output frame),
/// `d_frames` the signed distance still to travel (`target - playhead`), `fs` the file rate and
/// `r_max` the cap for the voice in use (already bounded by the decode headroom).
pub fn chase_step(
    rate: f64,
    cfg: &ChaseConfig,
    r_max: f64,
    d_frames: f64,
    fs: f64,
    dt: f64,
) -> f64 {
    let r_max = r_max.max(cfg.r_floor);
    let dist_s = d_frames.abs() / fs;
    let dir = if d_frames < 0.0 { -1.0 } else { 1.0 };
    let r_prop = (dist_s / cfg.t_catch_s + cfg.r_floor).clamp(cfg.r_floor, r_max);
    let r_brake = (cfg.r_floor * cfg.r_floor + 2.0 * cfg.a_decel * dist_s).sqrt();
    let cmd = dir * r_prop.min(r_brake);
    let max_delta = cfg.a_slew * dt;
    rate + (cmd - rate).clamp(-max_delta, max_delta)
}

/// The final quantum of a swoop: the frame count and exact rate that put the playhead on the
/// target. Returns `None` while more than one full quantum remains (use the controller's rate
/// and `quantum` frames), `Some(None)` when less than one frame of travel is left (snap to the
/// target without rendering) and `Some(Some((n, rate)))` for a shortened last quantum.
pub fn final_quantum(d_frames: f64, rate: f64, quantum: usize) -> Option<Option<(usize, f64)>> {
    let m = d_frames.abs() / rate.abs().max(1e-9);
    if m > quantum as f64 {
        return None;
    }
    if m < 1.0 {
        return Some(None);
    }
    let n = m.round().max(1.0) as usize;
    // Land a hair short so a backward swoop to frame 0 never crosses it; the caller snaps.
    Some(Some((n, d_frames / n as f64 * (1.0 - 1e-9))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;
    const Q: usize = 256;

    struct Run {
        peak: f64,
        time_s: f64,
        overshoot_frames: f64,
        end: f64,
        rates: Vec<f64>,
    }

    /// Simulated plant (varispeed landing): simulated time only.
    fn simulate(cfg: &ChaseConfig, r_max: f64, start: f64, target: f64, r0: f64) -> Run {
        let mut p = start;
        let mut r = r0;
        let mut t = 0.0;
        let mut peak: f64 = 0.0;
        let mut worst: f64 = 0.0;
        let mut rates = vec![];
        for _ in 0..200_000 {
            let d = target - p;
            r = chase_step(r, cfg, r_max, d, FS, Q as f64 / FS);
            let mut n = Q;
            let mut rr = r;
            match final_quantum(d, r, Q) {
                Some(None) => {
                    p = target;
                    break;
                }
                Some(Some((nn, r2))) => {
                    n = nn;
                    rr = r2;
                }
                None => {}
            }
            p += n as f64 * rr;
            t += n as f64 / FS;
            peak = peak.max(rr.abs());
            rates.push(rr);
            if (target - p) * d.signum() < 0.0 {
                worst = worst.max((p - target).abs());
            }
            if (target - p).abs() < 1.0 {
                p = target;
                break;
            }
        }
        Run {
            peak,
            time_s: t,
            overshoot_frames: worst,
            end: p,
            rates,
        }
    }

    #[test]
    fn forward_jumps_land_exactly_without_overshoot_within_caps() {
        let cfg = ChaseConfig::default();
        let r_max = cfg.r_max_vari;
        for d_s in [0.1, 1.0, 5.0] {
            let start = 10.0 * FS;
            let target = start + d_s * FS;
            let run = simulate(&cfg, r_max, start, target, 1.0);
            assert_eq!(run.end, target, "D={d_s}");
            assert!(run.overshoot_frames <= cfg.land_eps_frames, "D={d_s}");
            assert!(run.peak <= r_max + 1e-9, "D={d_s} peak {}", run.peak);
            assert!(
                run.time_s <= d_s / (0.8 * r_max) + 0.3,
                "D={d_s} took {}",
                run.time_s
            );
        }
    }

    #[test]
    fn backward_mirrors_forward_within_ten_percent() {
        let cfg = ChaseConfig::default();
        for d_s in [0.1, 1.0, 5.0] {
            let a = simulate(&cfg, cfg.r_max_vari, 20.0 * FS, 20.0 * FS + d_s * FS, 1.0);
            let b = simulate(&cfg, cfg.r_max_vari, 20.0 * FS, 20.0 * FS - d_s * FS, -1.0);
            assert_eq!(b.end, 20.0 * FS - d_s * FS);
            assert!(
                (a.time_s - b.time_s).abs() <= 0.1 * a.time_s.max(0.05) + 0.02,
                "D={d_s}: {} vs {}",
                a.time_s,
                b.time_s
            );
        }
    }

    #[test]
    fn a_flip_passes_through_zero_with_bounded_slew() {
        let cfg = ChaseConfig::default();
        // Running forward fast, then the target is behind: the rate crosses zero.
        let mut r = 3.0;
        let mut crossed = false;
        let mut prev = r;
        for _ in 0..200 {
            r = chase_step(r, &cfg, 4.0, -10_000.0, FS, Q as f64 / FS);
            assert!((r - prev).abs() <= cfg.a_slew * Q as f64 / FS + 1e-12);
            if prev > 0.0 && r <= 0.0 || prev < 0.0 {
                crossed = true;
            }
            prev = r;
        }
        assert!(crossed);
        assert!(r < 0.0);
    }

    #[test]
    fn retarget_converges_monotonically() {
        let cfg = ChaseConfig::default();
        let mut p = 0.0;
        let mut r = 1.0;
        let target = 3.0 * FS;
        let mut last_d = (target - p).abs();
        for _ in 0..100_000 {
            let d = target - p;
            r = chase_step(r, &cfg, 4.0, d, FS, Q as f64 / FS);
            match final_quantum(d, r, Q) {
                Some(None) => {
                    p = target;
                    break;
                }
                Some(Some((n, rr))) => p += n as f64 * rr,
                None => p += Q as f64 * r,
            }
            let nd = (target - p).abs();
            assert!(nd <= last_d + 1e-9 || nd < cfg.land_eps_frames);
            last_d = nd;
            if nd < 1.0 {
                break;
            }
        }
    }

    #[test]
    fn caps_follow_the_seek_mode() {
        let cfg = ChaseConfig::default();
        assert_eq!(
            cfg.r_max(WarpMode::PitchShift, ScanDir::Forward),
            cfg.r_max_vari
        );
        assert_eq!(
            cfg.r_max(WarpMode::PitchCorrect, ScanDir::Forward),
            cfg.r_max_stretch
        );
        // Backward travel is always varispeed.
        assert_eq!(
            cfg.r_max(WarpMode::PitchCorrect, ScanDir::Backward),
            cfg.r_max_vari
        );
        assert!((cfg.far_jump_s(4.0) - 6.8).abs() < 1e-9);
        assert!((cfg.far_jump_s(2.0) - 3.4).abs() < 1e-9);
    }

    #[test]
    fn small_jump_is_a_short_chirp() {
        let cfg = ChaseConfig::default();
        let run = simulate(&cfg, 4.0, 0.0, 0.1 * FS, 1.0);
        assert!(run.peak <= 1.6, "peak {}", run.peak);
        assert!(!run.rates.is_empty());
    }
}
