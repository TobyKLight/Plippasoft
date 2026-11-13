//! Scan profile (FF/RW hold) and chase controller (jog commit) as PURE functions.
//!
//! These are the formulas of WARP_17 (`ScanProfile`) and WARP_14 (`ChaseConfig`, `chase_step`) so the
//! owner hears what will ship. All constants are fields of the two structs below and are overridable
//! from the command line (`listen scan`, `listen jump`). Nothing here reads a clock: callers pass `dt`.

/// WARP_17 `ScanProfile` (constants register, README section 7).
#[derive(Debug, Clone, PartialEq)]
pub struct ScanProfile {
    /// `SCAN_BASE`: absolute source speed during the first `boost_after_s` of the hold.
    pub base: f64,
    /// `SCAN_BOOST`: absolute source speed after `boost_after_s`.
    pub boost: f64,
    /// `BOOST_AFTER_S`.
    pub boost_after_s: f64,
    /// `TAU_UP_S`: lag toward `base`.
    pub tau_up_s: f64,
    /// `TAU_BOOST_S`: lag toward `boost`.
    pub tau_boost_s: f64,
    /// `TAU_DOWN_S`: release brake lag toward the set speed.
    pub tau_down_s: f64,
    /// `MAX_SCAN_VARI`: cap with the varispeed seek voice (either direction).
    pub max_scan_vari: f64,
    /// `MAX_SCAN_STRETCH`: cap with the stretch seek voice (forward only).
    pub max_scan_stretch: f64,
}

impl Default for ScanProfile {
    fn default() -> Self {
        ScanProfile {
            base: 4.0,
            boost: 8.0,
            boost_after_s: 2.0,
            tau_up_s: 0.30,
            tau_boost_s: 1.0,
            tau_down_s: 0.25,
            max_scan_vari: 8.0,
            max_scan_stretch: 3.0,
        }
    }
}

/// The rate the first-order lag aims at. While held: `dir x min(base|boost, cap)`; after release: the
/// set speed (absolute reference: the scan never depends on tempo).
pub fn scan_target(
    p: &ScanProfile,
    dir: f64,
    hold_s: f64,
    held: bool,
    set_rate: f64,
    cap: f64,
) -> f64 {
    if !held {
        return set_rate;
    }
    let mag = if hold_s >= p.boost_after_s {
        p.boost
    } else {
        p.base
    };
    dir.signum() * mag.min(cap)
}

/// One lag step in the linear rate domain (exact exponential for any `dt`).
#[allow(clippy::too_many_arguments)]
pub fn scan_step(
    p: &ScanProfile,
    rate: f64,
    dir: f64,
    hold_s: f64,
    held: bool,
    set_rate: f64,
    cap: f64,
    dt: f64,
) -> f64 {
    let target = scan_target(p, dir, hold_s, held, set_rate, cap);
    let tau = if !held {
        p.tau_down_s
    } else if hold_s >= p.boost_after_s {
        p.tau_boost_s
    } else {
        p.tau_up_s
    };
    let a = if tau <= 0.0 {
        1.0
    } else {
        1.0 - (-dt / tau).exp()
    };
    rate + (target - rate) * a
}

/// Released and settled: within 0.5% of the set speed (then the voice hands over).
pub fn scan_settled(rate: f64, set_rate: f64) -> bool {
    (rate - set_rate).abs() <= 0.005 * set_rate.abs().max(1e-9)
}

#[derive(Debug, Clone, Default)]
pub struct ScanTrace {
    pub t: Vec<f64>,
    pub rate: Vec<f64>,
    /// Source position in seconds relative to the press position (signed).
    pub pos_s: Vec<f64>,
    pub release_t: f64,
    pub settled_t: f64,
}

/// Simulated scan: hold for `hold_total_s`, release, run until settled. Simulated time only.
pub fn simulate_scan(
    p: &ScanProfile,
    dir: f64,
    hold_total_s: f64,
    set_rate: f64,
    cap: f64,
    dt: f64,
) -> ScanTrace {
    let mut tr = ScanTrace {
        release_t: hold_total_s,
        ..Default::default()
    };
    let (mut t, mut rate, mut pos) = (0.0f64, set_rate, 0.0f64);
    tr.t.push(t);
    tr.rate.push(rate);
    tr.pos_s.push(pos);
    let limit = hold_total_s + 60.0;
    loop {
        let held = t < hold_total_s;
        let new = scan_step(p, rate, dir, t, held, set_rate, cap, dt);
        pos += 0.5 * (rate + new) * dt;
        rate = new;
        t += dt;
        tr.t.push(t);
        tr.rate.push(rate);
        tr.pos_s.push(pos);
        if (!held && scan_settled(rate, set_rate)) || t > limit {
            tr.settled_t = t;
            break;
        }
    }
    tr
}

/// WARP_14 `ChaseConfig` (constants register, README section 7).
#[derive(Debug, Clone, PartialEq)]
pub struct ChaseConfig {
    /// `T_CATCH_S`: proportional time constant of the chase.
    pub t_catch_s: f64,
    /// `R_FLOOR`: minimum travel rate (so the chase always arrives).
    pub r_floor: f64,
    /// `A_DECEL`: stopping-distance deceleration (rate units per second).
    pub a_decel: f64,
    /// `A_SLEW`: rate slew limit (rate units per second).
    pub a_slew: f64,
    /// `R_MAX_VARI`: peak travel rate with the varispeed voice, either direction.
    pub r_max_vari: f64,
    /// `R_MAX_STRETCH`: peak travel rate with the stretch voice (forward only).
    pub r_max_stretch: f64,
    /// `CHASE_MAX_TIME_S`: far-jump threshold = `R_MAX x` this.
    pub chase_max_time_s: f64,
    /// `WAYPOINT_LEAD_S`: how far before the target the waypoint jump lands.
    pub waypoint_lead_s: f64,
    /// `LAND_EPS_FRAMES`: landed when the playhead is this close.
    pub land_eps_frames: f64,
    /// `XFADE_WAYPOINT_MS`: crossfade of the waypoint jump.
    pub xfade_waypoint_ms: f64,
    /// Explicit far-jump threshold in seconds (`--far-jump-s`), overriding `R_MAX x CHASE_MAX_TIME_S`.
    pub far_jump_override_s: Option<f64>,
}

impl Default for ChaseConfig {
    fn default() -> Self {
        ChaseConfig {
            t_catch_s: 0.2,
            r_floor: 1.0,
            a_decel: 24.0,
            a_slew: 60.0,
            r_max_vari: 4.0,
            r_max_stretch: 2.0,
            chase_max_time_s: 1.7,
            waypoint_lead_s: 3.0,
            land_eps_frames: 256.0,
            xfade_waypoint_ms: 60.0,
            far_jump_override_s: None,
        }
    }
}

impl ChaseConfig {
    pub fn r_max(&self, stretch: bool) -> f64 {
        if stretch {
            self.r_max_stretch
        } else {
            self.r_max_vari
        }
    }

    /// Distance (seconds) beyond which the chase uses the waypoint jump.
    pub fn far_jump_s(&self, r_max: f64) -> f64 {
        self.far_jump_override_s
            .unwrap_or(r_max * self.chase_max_time_s)
    }
}

/// Commanded travel rate before slewing; `d_s` = (target - playhead) in seconds, signed.
pub fn chase_cmd(cfg: &ChaseConfig, r_max: f64, d_s: f64) -> f64 {
    let dist = d_s.abs();
    let r_prop = (dist / cfg.t_catch_s + cfg.r_floor).clamp(cfg.r_floor, r_max.max(cfg.r_floor));
    let r_brake = (cfg.r_floor * cfg.r_floor + 2.0 * cfg.a_decel * dist).sqrt();
    d_s.signum() * r_prop.min(r_brake)
}

/// One controller step (called once per 256-frame quantum): slew-limited toward [`chase_cmd`].
pub fn chase_step(cfg: &ChaseConfig, r_max: f64, rate: f64, d_s: f64, dt: f64) -> f64 {
    let cmd = chase_cmd(cfg, r_max, d_s);
    let max_dr = cfg.a_slew * dt;
    rate + (cmd - rate).clamp(-max_dr, max_dr)
}

/// Far jump rule: `|d| > far_jump_s(r_max)`.
pub fn is_far(cfg: &ChaseConfig, r_max: f64, d_s: f64) -> bool {
    d_s.abs() > cfg.far_jump_s(r_max)
}

/// Waypoint position (seconds): `target - dir x WAYPOINT_LEAD_S`.
pub fn waypoint_s(cfg: &ChaseConfig, pos_s: f64, target_s: f64) -> f64 {
    target_s - (target_s - pos_s).signum() * cfg.waypoint_lead_s
}

/// Plant model for the chase: playhead in frames, rate integrated per quantum (trapezoid).
#[derive(Debug, Clone)]
pub struct ChaseSim {
    pub cfg: ChaseConfig,
    pub r_max: f64,
    pub fs: f64,
    pub quantum: f64,
    pub pos: f64,
    pub rate: f64,
    pub target: f64,
    pub t: f64,
    pub landed: bool,
    pub peak_rate: f64,
    /// Largest excursion beyond the target in frames (positive = overshoot).
    pub overshoot_frames: f64,
    pub waypoint_jumps: usize,
    /// Simulated time of the waypoint jump, if any.
    pub waypoint_t: Option<f64>,
    side: f64,
}

impl ChaseSim {
    pub fn new(
        cfg: &ChaseConfig,
        r_max: f64,
        fs: f64,
        quantum: usize,
        pos_s: f64,
        start_rate: f64,
        target_s: f64,
    ) -> Self {
        let mut s = ChaseSim {
            cfg: cfg.clone(),
            r_max,
            fs,
            quantum: quantum as f64,
            pos: pos_s * fs,
            rate: start_rate,
            target: target_s * fs,
            t: 0.0,
            landed: false,
            peak_rate: start_rate.abs(),
            overshoot_frames: 0.0,
            waypoint_jumps: 0,
            waypoint_t: None,
            side: (target_s - pos_s).signum(),
        };
        s.maybe_waypoint();
        s
    }

    /// New target while chasing (the far-jump rule is re-evaluated once for the new distance).
    #[cfg(test)]
    pub fn retarget(&mut self, target_s: f64) {
        self.target = target_s * self.fs;
        self.landed = false;
        self.side = (self.target - self.pos).signum();
        self.maybe_waypoint();
    }

    fn maybe_waypoint(&mut self) {
        let d_s = (self.target - self.pos) / self.fs;
        if is_far(&self.cfg, self.r_max, d_s) {
            self.pos = waypoint_s(&self.cfg, self.pos / self.fs, self.target / self.fs) * self.fs;
            self.waypoint_jumps += 1;
            self.waypoint_t = Some(self.t);
            self.side = (self.target - self.pos).signum();
        }
    }

    pub fn d_s(&self) -> f64 {
        (self.target - self.pos) / self.fs
    }

    /// Advance one quantum. Returns `true` once landed.
    pub fn step(&mut self) -> bool {
        if self.landed {
            return true;
        }
        let dt = self.quantum / self.fs;
        if (self.target - self.pos).abs() <= self.cfg.land_eps_frames {
            self.landed = true;
            return true;
        }
        let new = chase_step(&self.cfg, self.r_max, self.rate, self.d_s(), dt);
        self.pos += 0.5 * (self.rate + new) * self.quantum;
        self.rate = new;
        self.t += dt;
        self.peak_rate = self.peak_rate.max(new.abs());
        let beyond = (self.pos - self.target) * self.side;
        if beyond > 0.0 {
            self.overshoot_frames = self.overshoot_frames.max(beyond);
        }
        if (self.target - self.pos).abs() <= self.cfg.land_eps_frames || beyond > 0.0 {
            self.landed = true;
        }
        self.landed
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // the traces are inspected by the tests; the listening code only needs `sim`
pub struct ChaseTrace {
    pub t: Vec<f64>,
    pub rate: Vec<f64>,
    pub pos_s: Vec<f64>,
    pub sim: ChaseSim,
}

/// Run a chase to completion (simulated time only). `max_s` guards against non-convergence.
#[allow(clippy::too_many_arguments)]
pub fn simulate_chase(
    cfg: &ChaseConfig,
    r_max: f64,
    fs: f64,
    quantum: usize,
    pos_s: f64,
    start_rate: f64,
    target_s: f64,
    max_s: f64,
) -> ChaseTrace {
    let mut sim = ChaseSim::new(cfg, r_max, fs, quantum, pos_s, start_rate, target_s);
    let (mut t, mut rate, mut pos) = (vec![0.0], vec![start_rate], vec![sim.pos / fs]);
    while !sim.landed && sim.t < max_s {
        sim.step();
        t.push(sim.t);
        rate.push(sim.rate);
        pos.push(sim.pos / fs);
    }
    ChaseTrace {
        t,
        rate,
        pos_s: pos,
        sim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 44100.0;
    const Q: usize = 256;

    #[test]
    fn scan_target_is_absolute_and_capped() {
        let p = ScanProfile::default();
        assert_eq!(scan_target(&p, 1.0, 0.0, true, 1.5, 8.0), 4.0);
        assert_eq!(scan_target(&p, -1.0, 1.99, true, 1.5, 8.0), -4.0);
        assert_eq!(scan_target(&p, 1.0, 2.0, true, 1.5, 8.0), 8.0);
        assert_eq!(
            scan_target(&p, 1.0, 5.0, true, 1.5, 3.0),
            3.0,
            "cap applies"
        );
        assert_eq!(
            scan_target(&p, 1.0, 5.0, false, 1.5, 8.0),
            1.5,
            "release aims at the set speed"
        );
    }

    #[test]
    fn scan_trace_is_monotone_without_overshoot() {
        let p = ScanProfile::default();
        let dt = Q as f64 / FS;
        let tr = simulate_scan(&p, 1.0, 6.0, 1.0, p.max_scan_vari, dt);
        let rel = tr.t.iter().position(|t| *t >= 6.0).unwrap();
        let (mut peak, mut peak_i) = (0.0, 0);
        for (i, r) in tr.rate.iter().enumerate() {
            assert!(r.is_finite());
            if *r > peak {
                peak = *r;
                peak_i = i;
            }
        }
        assert!(peak <= 8.0 + 1e-9, "never above the boost target");
        assert!(peak_i + 2 >= rel, "rate keeps rising until the release");
        for w in tr.rate[..=rel].windows(2) {
            assert!(w[1] >= w[0] - 1e-12, "monotone while held");
        }
        for w in tr.rate[rel..].windows(2) {
            assert!(w[1] <= w[0] + 1e-12, "monotone while braking");
        }
        let last = *tr.rate.last().unwrap();
        assert!(
            last >= 1.0 - 1e-9 && scan_settled(last, 1.0),
            "no undershoot below the set speed"
        );
        // position derivative equals the rate (bounded by the cap)
        for w in tr.pos_s.windows(2) {
            assert!((w[1] - w[0]) / dt <= 8.0 + 1e-6);
        }
    }

    #[test]
    fn scan_reaches_95_percent_of_4x_in_three_tau_and_brakes_in_bounded_time() {
        let p = ScanProfile::default();
        let dt = Q as f64 / FS;
        let tr = simulate_scan(&p, 1.0, 1.9, 1.0, 8.0, dt);
        let target_95 = 1.0 + 0.95 * (4.0 - 1.0);
        let t95 =
            tr.t.iter()
                .zip(&tr.rate)
                .find(|(_, r)| **r >= target_95)
                .map(|(t, _)| *t)
                .unwrap();
        assert!(t95 <= 3.0 * p.tau_up_s + 0.05, "t95 = {t95}");
        // brake from the long-hold 8x
        let tr = simulate_scan(&p, 1.0, 8.0, 1.0, 8.0, dt);
        assert!(
            tr.settled_t - tr.release_t <= 8.0 * p.tau_down_s + 0.05,
            "{}",
            tr.settled_t - tr.release_t
        );
    }

    #[test]
    fn scan_absolute_reference_at_set_speed_1_5() {
        let p = ScanProfile::default();
        let dt = Q as f64 / FS;
        let tr = simulate_scan(&p, 1.0, 1.9, 1.5, 8.0, dt);
        assert_eq!(tr.rate[0], 1.5, "starts from the set speed, no pitch step");
        assert!(
            tr.rate.iter().cloned().fold(0.0, f64::max) < 4.0 + 1e-9,
            "target is 4.0, not 6.0"
        );
        assert!(
            (tr.rate.last().unwrap() - 1.5).abs() <= 0.005 * 1.5 + 1e-9,
            "returns to 1.5"
        );
    }

    #[test]
    fn scan_reverse_is_the_mirror_image() {
        let p = ScanProfile::default();
        let dt = Q as f64 / FS;
        let f = simulate_scan(&p, 1.0, 3.0, 1.0, 8.0, dt);
        let b = simulate_scan(&p, -1.0, 3.0, 1.0, 8.0, dt);
        // the speed starts at +1.0 (set speed) and glides through zero toward -4: min is about -4..-8
        assert!(b.rate.iter().cloned().fold(0.0, f64::min) < -3.9);
        assert!(
            *f.pos_s.last().unwrap() > 0.0
                && *b.pos_s.last().unwrap() < f.pos_s.last().unwrap() - 1.0
        );
    }

    fn near_far_cases() -> Vec<(f64, bool)> {
        vec![
            (0.1, false),
            (1.0, false),
            (5.0, false),
            (10.0, true),
            (60.0, true),
        ]
    }

    #[test]
    fn chase_lands_without_overshoot_forward_and_backward() {
        let cfg = ChaseConfig::default();
        let r_max = cfg.r_max_vari;
        for sign in [1.0, -1.0] {
            for (dist, far) in near_far_cases() {
                let pos0 = 100.0;
                let tr = simulate_chase(&cfg, r_max, FS, Q, pos0, 1.0, pos0 + sign * dist, 60.0);
                let s = &tr.sim;
                assert!(s.landed, "dist {dist} sign {sign}");
                assert_eq!(s.waypoint_jumps, usize::from(far), "dist {dist}");
                assert!(
                    (s.pos - s.target).abs() <= cfg.land_eps_frames,
                    "landing error {} frames",
                    (s.pos - s.target).abs()
                );
                assert!(
                    s.overshoot_frames <= cfg.land_eps_frames,
                    "overshoot {}",
                    s.overshoot_frames
                );
                assert!(s.peak_rate <= r_max + 1e-9, "peak {}", s.peak_rate);
                let travelled = if far { cfg.waypoint_lead_s } else { dist };
                let bound = travelled / (0.8 * r_max) + if far { 0.6 } else { 0.3 };
                assert!(s.t <= bound, "dist {dist}: {} s > {bound} s", s.t);
            }
        }
    }

    #[test]
    fn chase_rate_respects_slew_and_caps_per_mode() {
        let cfg = ChaseConfig::default();
        for stretch in [false, true] {
            let r_max = cfg.r_max(stretch);
            let tr = simulate_chase(&cfg, r_max, FS, Q, 10.0, 1.0, 14.0, 30.0);
            let dt = Q as f64 / FS;
            for w in tr.rate.windows(2) {
                assert!((w[1] - w[0]).abs() <= cfg.a_slew * dt + 1e-9);
            }
            assert!(tr.sim.peak_rate <= r_max + 1e-9);
            assert!(tr.sim.landed);
        }
        assert_eq!(cfg.r_max(true), 2.0);
        assert_eq!(cfg.r_max(false), 4.0);
    }

    #[test]
    fn far_jump_threshold_follows_r_max_and_override() {
        let mut cfg = ChaseConfig::default();
        assert!((cfg.far_jump_s(4.0) - 6.8).abs() < 1e-12);
        assert!((cfg.far_jump_s(2.0) - 3.4).abs() < 1e-12);
        assert!(!is_far(&cfg, 4.0, 6.8));
        assert!(is_far(&cfg, 4.0, -6.9));
        cfg.far_jump_override_s = Some(1.0);
        assert!(is_far(&cfg, 4.0, 1.5));
        assert_eq!(waypoint_s(&cfg, 10.0, 70.0), 67.0);
        assert_eq!(waypoint_s(&cfg, 70.0, 10.0), 13.0);
    }

    #[test]
    fn retarget_converges_and_flips_through_zero_within_slew() {
        let cfg = ChaseConfig::default();
        let dt = Q as f64 / FS;
        // chase ahead, retarget further ahead mid-flight: still lands, never overshoots the final target
        let mut sim = ChaseSim::new(&cfg, 4.0, FS, Q, 10.0, 1.0, 13.0);
        for _ in 0..40 {
            sim.step();
        }
        assert!(!sim.landed);
        sim.retarget(16.0);
        while !sim.landed && sim.t < 30.0 {
            sim.step();
        }
        assert!(sim.landed && sim.overshoot_frames <= cfg.land_eps_frames);
        assert!((sim.pos - sim.target).abs() <= cfg.land_eps_frames);
        // target flips from ahead to behind: the rate passes through zero with |dr/dt| <= A_SLEW
        let mut sim = ChaseSim::new(&cfg, 4.0, FS, Q, 10.0, 1.0, 14.0);
        for _ in 0..60 {
            sim.step();
        }
        let mut prev = sim.rate;
        sim.retarget(sim.pos / FS - 3.0);
        let mut crossed_zero = false;
        while !sim.landed && sim.t < 60.0 {
            sim.step();
            assert!((sim.rate - prev).abs() <= cfg.a_slew * dt + 1e-9);
            if prev > 0.0 && sim.rate <= 0.0 || prev < 0.0 && sim.rate >= 0.0 {
                crossed_zero = true;
            }
            prev = sim.rate;
        }
        assert!(crossed_zero, "rate must pass through zero");
        assert!(sim.landed);
    }

    #[test]
    fn already_at_target_lands_immediately() {
        let cfg = ChaseConfig::default();
        let tr = simulate_chase(&cfg, 4.0, FS, Q, 5.0, 1.0, 5.0, 1.0);
        assert!(tr.sim.landed);
        assert_eq!(tr.sim.t, 0.0);
    }
}
