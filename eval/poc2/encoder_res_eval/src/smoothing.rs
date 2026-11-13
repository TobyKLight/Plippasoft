//! Detent → value mapping with interval-based acceleration, plus optional EMA.
//!
//! SWE3 has two alternate specs (toggle with SWE3 push):
//! - **Accel** — raw gain (legacy feel knob).
//! - **Turns** — target encoder revolutions for 0→100% at a reference fast spin;
//!   gain is derived from steps, detents/rev, and that turn count.
//!
//! Slow / first clicks are always 1 step in both modes.

use std::time::{Duration, Instant};

/// Absolute min/max for the SWE5 step grid.
pub const STEPS_MIN: u32 = 10;
pub const STEPS_MAX: u32 = 2000;
/// Pivot: ≤ this uses 10-increments; above uses 50-increments.
pub const STEPS_PIVOT: u32 = 100;
pub const STEPS_INC_LO: u32 = 10;
pub const STEPS_INC_HI: u32 = 50;

pub const ACCEL_MIN: f32 = 0.0;
pub const ACCEL_MAX: f32 = 8.0;
pub const ACCEL_INC: f32 = 0.01;

/// Fast full-scale turns (0 = OFF / always 1 step). 0.2 increments.
pub const TURNS_MIN: f32 = 0.0;
pub const TURNS_MAX: f32 = 20.0;
pub const TURNS_INC: f32 = 0.2;
/// Sensible default when entering turns mode from accel OFF.
pub const TURNS_DEFAULT: f32 = 3.0;

pub const EMA_MS_MIN: u32 = 0;
pub const EMA_MS_MAX: u32 = 500;
pub const EMA_MS_INC: u32 = 1;

/// Mechanical detents per revolution after the detent filter (POC2 18-detent parts).
pub const DETENTS_PER_REV: f32 = 18.0;

/// Inter-detent gap ≥ this → always exactly 1 step (slow / deliberate).
const SLOW_GAP: Duration = Duration::from_millis(100);
/// Gap ≤ this → full speed contribution.
const FAST_GAP: Duration = Duration::from_millis(25);
/// Scales SWE3 accel at full speed: delta ≈ 1 + round(accel * SCALE).
const ACCEL_SCALE: f32 = 5.0;
/// Reference "fast volume spin" speed (0..1 on the gap curve).
/// Calibrated so accel≈1.0 ≈ ~3 turns for 100 steps (lab feel).
const REF_SPEED: f32 = 0.413;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swe3Mode {
    /// Direct gain knob.
    Accel,
    /// Target revolutions for 0→100% at reference fast spin.
    Turns,
}

pub struct ValueController {
    steps: u32,
    target_idx: i32,
    display: f32,
    pub accel: f32,
    /// Fast full-scale turns; 0 = same as accel OFF.
    pub fast_turns: f32,
    pub swe3_mode: Swe3Mode,
    pub ema_ms: u32,
    last_detent_at: Option<Instant>,
}

impl ValueController {
    pub fn new(steps: u32, start_pct: f32) -> Self {
        let steps = clamp_steps(steps);
        let idx = ((start_pct.clamp(0.0, 1.0) * steps as f32).round() as i32).clamp(0, steps as i32);
        let display = idx as f32 / steps as f32;
        Self {
            steps,
            target_idx: idx,
            display,
            accel: ACCEL_MIN,
            fast_turns: TURNS_MIN,
            swe3_mode: Swe3Mode::Accel,
            ema_ms: EMA_MS_MIN,
            last_detent_at: None,
        }
    }

    pub fn steps(&self) -> u32 {
        self.steps
    }

    pub fn target_step(&self) -> i32 {
        self.target_idx
    }

    pub fn display_step(&self) -> i32 {
        (self.display * self.steps as f32)
            .round()
            .clamp(0.0, self.steps as f32) as i32
    }

    pub fn target_pct(&self) -> f32 {
        self.target_idx as f32 / self.steps as f32
    }

    pub fn display_pct(&self) -> f32 {
        self.display
    }

    pub fn accel_label(&self) -> String {
        if self.accel <= 0.001 {
            "OFF".to_string()
        } else {
            format!("{:.2}", self.accel)
        }
    }

    pub fn turns_label(&self) -> String {
        if self.fast_turns <= 0.001 {
            "OFF".to_string()
        } else {
            format!("{:.1}", self.fast_turns)
        }
    }

    pub fn swe3_label(&self) -> String {
        match self.swe3_mode {
            Swe3Mode::Accel => format!("accel {}", self.accel_label()),
            Swe3Mode::Turns => format!("turns {}", self.turns_label()),
        }
    }

    pub fn ema_label(&self) -> String {
        if self.ema_ms == 0 {
            "OFF".to_string()
        } else {
            format!("{} ms", self.ema_ms)
        }
    }

    /// Toggle SWE3 edit mode; convert the active value into the other representation.
    pub fn toggle_swe3_mode(&mut self) {
        match self.swe3_mode {
            Swe3Mode::Accel => {
                self.fast_turns = if self.accel <= 0.001 {
                    TURNS_DEFAULT
                } else {
                    turns_from_accel(self.accel, self.steps)
                };
                self.swe3_mode = Swe3Mode::Turns;
            }
            Swe3Mode::Turns => {
                self.accel = if self.fast_turns <= 0.001 {
                    ACCEL_MIN
                } else {
                    accel_from_turns(self.fast_turns, self.steps)
                };
                self.swe3_mode = Swe3Mode::Accel;
            }
        }
    }

    pub fn set_steps(&mut self, steps: u32) {
        let pct = self.target_pct();
        self.steps = clamp_steps(steps);
        self.target_idx = ((pct * self.steps as f32).round() as i32).clamp(0, self.steps as i32);
        if self.ema_ms == 0 {
            self.display = self.target_pct();
        }
        // Keep the inactive SWE3 representation in sync with the new grid.
        match self.swe3_mode {
            Swe3Mode::Turns if self.fast_turns > 0.001 => {
                self.accel = accel_from_turns(self.fast_turns, self.steps);
            }
            Swe3Mode::Accel if self.accel > 0.001 => {
                self.fast_turns = turns_from_accel(self.accel, self.steps);
            }
            _ => {}
        }
    }

    pub fn nudge_steps(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let mut s = self.steps;
        let n = delta.unsigned_abs();
        for _ in 0..n {
            s = step_grid(s, delta.signum());
        }
        self.set_steps(s);
    }

    pub fn nudge_swe3(&mut self, delta: i32) {
        match self.swe3_mode {
            Swe3Mode::Accel => self.nudge_accel(delta),
            Swe3Mode::Turns => self.nudge_turns(delta),
        }
    }

    pub fn nudge_accel(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let next = self.accel + delta as f32 * ACCEL_INC;
        self.accel = ((next / ACCEL_INC).round() * ACCEL_INC).clamp(ACCEL_MIN, ACCEL_MAX);
        if self.accel > 0.001 {
            self.fast_turns = turns_from_accel(self.accel, self.steps);
        } else {
            self.fast_turns = TURNS_MIN;
        }
    }

    pub fn nudge_turns(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let next = self.fast_turns + delta as f32 * TURNS_INC;
        self.fast_turns = ((next / TURNS_INC).round() * TURNS_INC).clamp(TURNS_MIN, TURNS_MAX);
        if self.fast_turns > 0.001 {
            self.accel = accel_from_turns(self.fast_turns, self.steps);
        } else {
            self.accel = ACCEL_MIN;
        }
    }

    pub fn nudge_ema_ms(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let next = self.ema_ms as i32 + delta * EMA_MS_INC as i32;
        self.ema_ms = next.clamp(EMA_MS_MIN as i32, EMA_MS_MAX as i32) as u32;
        if self.ema_ms == 0 {
            self.display = self.target_pct();
        }
    }

    pub fn apply_swe1_ticks(&mut self, ticks: i32, now: Instant) {
        if ticks == 0 {
            return;
        }
        let dir = ticks.signum();
        for _ in 0..ticks.abs() {
            let gap = self.last_detent_at.map(|t| now.duration_since(t));
            self.last_detent_at = Some(now);

            let step_size = self.step_size_for_gap(gap);
            self.target_idx = (self.target_idx + dir * step_size).clamp(0, self.steps as i32);
        }
        if self.ema_ms == 0 {
            self.display = self.target_pct();
        }
    }

    fn step_size_for_gap(&self, gap: Option<Duration>) -> i32 {
        let boost = match self.swe3_mode {
            Swe3Mode::Accel => {
                if self.accel <= 0.001 {
                    return 1;
                }
                // Max extra at speed=1: accel * SCALE.
                self.accel * ACCEL_SCALE
            }
            Swe3Mode::Turns => {
                if self.fast_turns <= 0.001 {
                    return 1;
                }
                // At REF_SPEED, steps/detent ≈ m = steps/(D*turns).
                // boost * REF^2 = m - 1  →  boost = (m-1)/REF^2
                let m = target_steps_per_detent(self.fast_turns, self.steps);
                (m - 1.0).max(0.0) / (REF_SPEED * REF_SPEED).max(1e-6)
            }
        };

        let Some(g) = gap else {
            return 1;
        };
        if g >= SLOW_GAP {
            return 1;
        }
        let slow = SLOW_GAP.as_secs_f32();
        let fast = FAST_GAP.as_secs_f32();
        let t = g.as_secs_f32().clamp(fast, slow);
        let speed = (slow - t) / (slow - fast).max(1e-4);
        let extra = boost * speed * speed;
        (1.0 + extra).round().max(1.0) as i32
    }

    pub fn tick_display(&mut self, dt: f32) {
        let target = self.target_pct();
        if self.ema_ms == 0 {
            self.display = target;
            return;
        }
        let tau = (self.ema_ms as f32 / 1000.0).max(1e-4);
        let alpha = 1.0 - (-dt / tau).exp();
        self.display += (target - self.display) * alpha.clamp(0.0, 1.0);
        if (self.display - target).abs() < 1e-5 {
            self.display = target;
        }
    }
}

/// Steps per detent at reference fast spin for a turns target.
fn target_steps_per_detent(turns: f32, steps: u32) -> f32 {
    let turns = turns.max(TURNS_INC);
    let denom = DETENTS_PER_REV * turns;
    (steps as f32 / denom).max(1.0)
}

/// accel → equivalent turns at REF_SPEED (matches lab: accel 1 ≈ 3 turns @ 100 steps).
pub fn turns_from_accel(accel: f32, steps: u32) -> f32 {
    if accel <= 0.001 {
        return TURNS_MIN;
    }
    let m = 1.0 + accel * REF_SPEED * REF_SPEED * ACCEL_SCALE;
    let turns = steps as f32 / (DETENTS_PER_REV * m.max(1.0));
    let snapped = ((turns / TURNS_INC).round() * TURNS_INC).clamp(TURNS_INC, TURNS_MAX);
    snapped
}

pub fn accel_from_turns(turns: f32, steps: u32) -> f32 {
    if turns <= 0.001 {
        return ACCEL_MIN;
    }
    let m = target_steps_per_detent(turns, steps);
    let denom = REF_SPEED * REF_SPEED * ACCEL_SCALE;
    let accel = ((m - 1.0) / denom.max(1e-6)).clamp(ACCEL_MIN, ACCEL_MAX);
    ((accel / ACCEL_INC).round() * ACCEL_INC).clamp(ACCEL_MIN, ACCEL_MAX)
}

fn step_grid(current: u32, dir: i32) -> u32 {
    if dir > 0 {
        if current < STEPS_PIVOT {
            (current + STEPS_INC_LO).min(STEPS_PIVOT)
        } else if current == STEPS_PIVOT {
            STEPS_PIVOT + STEPS_INC_HI
        } else {
            (current + STEPS_INC_HI).min(STEPS_MAX)
        }
    } else if dir < 0 {
        if current <= STEPS_PIVOT {
            current.saturating_sub(STEPS_INC_LO).max(STEPS_MIN)
        } else if current == STEPS_PIVOT + STEPS_INC_HI {
            STEPS_PIVOT
        } else {
            current.saturating_sub(STEPS_INC_HI).max(STEPS_PIVOT)
        }
    } else {
        current
    }
}

pub fn clamp_steps(steps: u32) -> u32 {
    let s = steps.clamp(STEPS_MIN, STEPS_MAX);
    if s <= STEPS_PIVOT {
        let n = (s + STEPS_INC_LO / 2) / STEPS_INC_LO;
        (n * STEPS_INC_LO).clamp(STEPS_MIN, STEPS_PIVOT)
    } else {
        let n = ((s - STEPS_PIVOT) + STEPS_INC_HI / 2) / STEPS_INC_HI;
        let n = n.max(1);
        (STEPS_PIVOT + n * STEPS_INC_HI).min(STEPS_MAX)
    }
}
