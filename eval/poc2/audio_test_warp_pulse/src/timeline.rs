#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspMode {
    Passthrough,
    PitchPreserved,
    Vinyl,
    Reverse,
}

#[derive(Debug, Clone, Copy)]
pub struct Phase {
    pub start_s: f64,
    pub end_s: f64,
    pub mode: DspMode,
    pub speed_start: f64,
    pub speed_end: f64,
    pub label: &'static str,
}

pub const TOTAL_DURATION_S: f64 = 24.0;

const PHASES: &[Phase] = &[
    Phase {
        start_s: 0.0,
        end_s: 4.0,
        mode: DspMode::Passthrough,
        speed_start: 1.0,
        speed_end: 1.0,
        label: "normal playback",
    },
    Phase {
        start_s: 4.0,
        end_s: 5.0,
        mode: DspMode::PitchPreserved,
        speed_start: 1.0,
        speed_end: 0.65,
        label: "complex warp: ramp to 65%",
    },
    Phase {
        start_s: 5.0,
        end_s: 7.0,
        mode: DspMode::PitchPreserved,
        speed_start: 0.65,
        speed_end: 0.65,
        label: "complex warp: hold 65%",
    },
    Phase {
        start_s: 7.0,
        end_s: 8.0,
        mode: DspMode::PitchPreserved,
        speed_start: 0.65,
        speed_end: 1.0,
        label: "complex warp: ramp to 100%",
    },
    Phase {
        start_s: 8.0,
        end_s: 9.0,
        mode: DspMode::PitchPreserved,
        speed_start: 1.0,
        speed_end: 1.35,
        label: "complex warp: ramp to 135%",
    },
    Phase {
        start_s: 9.0,
        end_s: 11.0,
        mode: DspMode::PitchPreserved,
        speed_start: 1.35,
        speed_end: 1.35,
        label: "complex warp: hold 135%",
    },
    Phase {
        start_s: 11.0,
        end_s: 12.0,
        mode: DspMode::PitchPreserved,
        speed_start: 1.35,
        speed_end: 1.0,
        label: "complex warp: ramp to 100%",
    },
    Phase {
        start_s: 12.0,
        end_s: 13.0,
        mode: DspMode::Vinyl,
        speed_start: 1.0,
        speed_end: 0.65,
        label: "vinyl: ramp to 65%",
    },
    Phase {
        start_s: 13.0,
        end_s: 15.0,
        mode: DspMode::Vinyl,
        speed_start: 0.65,
        speed_end: 0.65,
        label: "vinyl: hold 65%",
    },
    Phase {
        start_s: 15.0,
        end_s: 16.0,
        mode: DspMode::Vinyl,
        speed_start: 0.65,
        speed_end: 1.0,
        label: "vinyl: ramp to 100%",
    },
    Phase {
        start_s: 16.0,
        end_s: 17.0,
        mode: DspMode::Vinyl,
        speed_start: 1.0,
        speed_end: 1.35,
        label: "vinyl: ramp to 135%",
    },
    Phase {
        start_s: 17.0,
        end_s: 19.0,
        mode: DspMode::Vinyl,
        speed_start: 1.35,
        speed_end: 1.35,
        label: "vinyl: hold 135%",
    },
    Phase {
        start_s: 19.0,
        end_s: 20.0,
        mode: DspMode::Vinyl,
        speed_start: 1.35,
        speed_end: 1.0,
        label: "vinyl: ramp to 100%",
    },
    Phase {
        start_s: 20.0,
        end_s: 24.0,
        mode: DspMode::Reverse,
        speed_start: -1.0,
        speed_end: -1.0,
        label: "reverse playback",
    },
];

pub fn phase_at(time_s: f64) -> Option<&'static Phase> {
    if time_s >= TOTAL_DURATION_S {
        return None;
    }
    PHASES
        .iter()
        .find(|phase| time_s >= phase.start_s && time_s < phase.end_s)
}

pub fn speed_at(time_s: f64) -> f64 {
    let Some(phase) = phase_at(time_s) else {
        return 0.0;
    };
    if phase.end_s <= phase.start_s {
        return phase.speed_start;
    }
    let t = ((time_s - phase.start_s) / (phase.end_s - phase.start_s)).clamp(0.0, 1.0);
    phase.speed_start + (phase.speed_end - phase.speed_start) * t
}

pub fn mode_at(time_s: f64) -> DspMode {
    phase_at(time_s)
        .map(|phase| phase.mode)
        .unwrap_or(DspMode::Passthrough)
}

pub fn phase_index_at(time_s: f64) -> Option<usize> {
    if time_s >= TOTAL_DURATION_S {
        return None;
    }
    PHASES.iter().position(|phase| time_s >= phase.start_s && time_s < phase.end_s)
}

pub fn all_phases() -> &'static [Phase] {
    PHASES
}
