use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;

/// Warp offset from 100% playback speed. -35 → 65%, 0 → 100%, +35 → 135%.
pub const WARP_MIN: i32 = -75;
pub const WARP_MAX: i32 = 300;
const WARP_PCT_PER_STEP: i32 = 1;
/// Mouse counts needed per 1% warp change (4 → quarter sensitivity vs 1:1).
const DX_PER_WARP_PCT: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspMode {
    PitchPreserved,
    Vinyl,
}

#[derive(Debug, Clone, Copy)]
pub struct ControlSnapshot {
    pub warp_pct: i32,
    pub mode: DspMode,
}

pub struct WarpControl {
    warp_pct: AtomicI32,
    dx_remainder: AtomicI32,
    mode: AtomicU8,
}

impl WarpControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            warp_pct: AtomicI32::new(0),
            dx_remainder: AtomicI32::new(0),
            mode: AtomicU8::new(DspMode::PitchPreserved as u8),
        })
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        ControlSnapshot {
            warp_pct: self.warp_pct.load(Ordering::SeqCst),
            mode: self.load_mode(),
        }
    }

    pub fn adjust_warp_by_dx(&self, dx: i32) {
        if dx == 0 {
            return;
        }
        let total = self.dx_remainder.fetch_add(dx, Ordering::AcqRel) + dx;
        let steps = total / DX_PER_WARP_PCT;
        if steps == 0 {
            return;
        }
        self.dx_remainder
            .store(total - steps * DX_PER_WARP_PCT, Ordering::Release);
        let delta = steps.saturating_mul(WARP_PCT_PER_STEP);
        let mut current = self.warp_pct.load(Ordering::SeqCst);
        loop {
            let next = (current + delta).clamp(WARP_MIN, WARP_MAX);
            match self.warp_pct.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn reset_warp(&self) {
        self.warp_pct.store(0, Ordering::SeqCst);
        self.dx_remainder.store(0, Ordering::SeqCst);
    }

    pub fn toggle_mode(&self) {
        let next = match self.load_mode() {
            DspMode::PitchPreserved => DspMode::Vinyl,
            DspMode::Vinyl => DspMode::PitchPreserved,
        };
        self.mode.store(next as u8, Ordering::SeqCst);
    }

    pub fn playback_speed(&self) -> f64 {
        speed_from_warp(self.warp_pct.load(Ordering::SeqCst))
    }

    fn load_mode(&self) -> DspMode {
        match self.mode.load(Ordering::SeqCst) {
            x if x == DspMode::Vinyl as u8 => DspMode::Vinyl,
            _ => DspMode::PitchPreserved,
        }
    }
}

pub fn speed_from_warp(warp_pct: i32) -> f64 {
    ((100 + warp_pct).max(1) as f64) / 100.0
}

pub fn mode_label(mode: DspMode) -> &'static str {
    match mode {
        DspMode::PitchPreserved => "pitch preserved",
        DspMode::Vinyl => "vinyl",
    }
}

pub fn warp_display(warp_pct: i32) -> String {
    let speed = speed_from_warp(warp_pct) * 100.0;
    if warp_pct == 0 {
        "warp 0% (100% speed)".to_string()
    } else if warp_pct > 0 {
        format!("warp +{warp_pct}% ({speed:.0}% speed)")
    } else {
        format!("warp {warp_pct}% ({speed:.0}% speed)")
    }
}
