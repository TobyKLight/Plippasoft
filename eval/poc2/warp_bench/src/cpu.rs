//! `cpu` sub-command: process CPU time (percent of one core) while rendering audio offline through
//! each voice, plus the p99 render time of one 256-frame block.
//!
//! The numbers only mean something on the CM4 with a release build. Nothing here asserts them.

use crate::stats;
use crate::voices::{PcmBuf, StretchPreset, StretchVoice, Varispeed, QUANTUM};
use anyhow::Result;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CaseKind {
    /// Windowed-sinc varispeed (|rate| <= 2).
    Vari,
    /// Boxcar + cubic (|rate| > 2).
    Boxcar,
    /// Stretcher as the speed voice (steady rate).
    StretchSpeed(StretchPreset),
    /// Stretcher as the seek voice: rate wobbles every block and the stretcher is re-primed every 2 s.
    StretchSeek,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Case {
    pub kind: CaseKind,
    pub rate: f64,
}

/// `2.0`, `0.25`, `-1.0`: stable key fragments.
pub fn fmt_rate(r: f64) -> String {
    if ((r * 10.0).round() - r * 10.0).abs() < 1e-9 {
        format!("{r:.1}")
    } else {
        format!("{r:.2}")
    }
}

impl Case {
    pub fn key(&self) -> String {
        let r = fmt_rate(self.rate);
        match self.kind {
            CaseKind::Vari => format!("cpu.vari.{r}"),
            CaseKind::Boxcar => format!("cpu.boxcar.{r}"),
            CaseKind::StretchSpeed(StretchPreset::Default) => format!("cpu.stretch_speed.{r}"),
            CaseKind::StretchSpeed(StretchPreset::Cheaper) => {
                format!("cpu.stretch_speed_cheaper.{r}")
            }
            CaseKind::StretchSeek => format!("cpu.stretch_seek.{r}"),
        }
    }

    pub fn label(&self) -> String {
        match self.kind {
            CaseKind::Vari => "varispeed sinc".into(),
            CaseKind::Boxcar => "boxcar+cubic".into(),
            CaseKind::StretchSpeed(p) => format!("stretch speed voice ({})", p.name()),
            CaseKind::StretchSeek => "stretch seek voice".into(),
        }
    }
}

/// The cases of the spec: varispeed 0.25 0.5 1.5 2.0 -1.0; boxcar 4 8; stretcher at 0.5 1 1.5 2 3 4 as
/// speed voice and as seek voice; cheaper preset at 0.5 1.0 2.0 (for the preset-fallback rule).
pub fn default_cases() -> Vec<Case> {
    let mut v = Vec::new();
    for r in [0.25, 0.5, 1.5, 2.0, -1.0] {
        v.push(Case {
            kind: CaseKind::Vari,
            rate: r,
        });
    }
    for r in [4.0, 8.0] {
        v.push(Case {
            kind: CaseKind::Boxcar,
            rate: r,
        });
    }
    for r in [0.5, 1.0, 1.5, 2.0, 3.0, 4.0] {
        v.push(Case {
            kind: CaseKind::StretchSpeed(StretchPreset::Default),
            rate: r,
        });
    }
    for r in [0.5, 1.0, 1.5, 2.0, 3.0, 4.0] {
        v.push(Case {
            kind: CaseKind::StretchSeek,
            rate: r,
        });
    }
    for r in [0.5, 1.0, 2.0] {
        v.push(Case {
            kind: CaseKind::StretchSpeed(StretchPreset::Cheaper),
            rate: r,
        });
    }
    v
}

/// Seconds of source audio a case consumes for `seconds` of output.
pub fn source_needed_s(cases: &[Case], seconds: f64) -> f64 {
    cases.iter().map(|c| c.rate.abs()).fold(1.0, f64::max) * seconds * 1.05 + 2.0
}

/// CPU time consumed by this process so far (all threads), seconds.
pub fn process_cpu_time_s() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // `blocks` is checked by the tests
pub struct CaseResult {
    pub cpu_pct: f64,
    pub p99_block_us: f64,
    pub max_block_us: f64,
    pub blocks: usize,
}

/// Number of 256-frame blocks for `seconds` of output at `rate` Hz.
pub fn block_count(seconds: f64, rate: u32) -> usize {
    ((seconds * rate as f64) / QUANTUM as f64).ceil() as usize
}

/// Render `seconds` of audio for one case and measure.
pub fn run_case(case: Case, seconds: f64, src: &PcmBuf) -> Result<CaseResult> {
    let blocks = block_count(seconds, src.rate);
    let ch = src.ch;
    let mut out = vec![0.0f32; QUANTUM * ch];
    let mut times_us = Vec::with_capacity(blocks);
    let mut vari = Varispeed::new();
    let mut sink = 0.0f32; // keep the optimiser honest
    let start_pos = if case.rate < 0.0 {
        src.frames() as f64 - 2.0
    } else {
        1.0
    };
    let mut pos = start_pos;
    let mut stretch = match case.kind {
        CaseKind::StretchSpeed(p) => Some(StretchVoice::new(ch, src.rate, p)),
        CaseKind::StretchSeek => Some(StretchVoice::new(ch, src.rate, StretchPreset::Default)),
        _ => None,
    };
    if let Some(sv) = stretch.as_mut() {
        sv.prime(src, pos, case.rate);
    }
    let reprime_blocks = (2.0 * src.rate as f64 / QUANTUM as f64) as usize;
    let cpu0 = process_cpu_time_s();
    let wall0 = Instant::now();
    for blk in 0..blocks {
        let t = Instant::now();
        match case.kind {
            CaseKind::Vari | CaseKind::Boxcar => {
                vari.render(src, &mut pos, case.rate, case.rate, &mut out);
            }
            CaseKind::StretchSpeed(_) => {
                stretch
                    .as_mut()
                    .unwrap()
                    .render(src, &mut pos, case.rate, &mut out);
            }
            CaseKind::StretchSeek => {
                let sv = stretch.as_mut().unwrap();
                if blk > 0 && blk % reprime_blocks == 0 {
                    sv.prime(src, pos, case.rate);
                }
                let r = case.rate * (1.0 + 0.05 * (blk as f64 * 0.1).sin());
                sv.render(src, &mut pos, r, &mut out);
            }
        }
        times_us.push(t.elapsed().as_secs_f64() * 1e6);
        sink += out[0];
    }
    let cpu = process_cpu_time_s() - cpu0;
    let _wall = wall0.elapsed();
    if sink.is_nan() {
        anyhow::bail!("NaN in rendered audio");
    }
    let audio_s = blocks as f64 * QUANTUM as f64 / src.rate as f64;
    Ok(CaseResult {
        cpu_pct: cpu / audio_s * 100.0,
        p99_block_us: stats::percentile(&times_us, 99.0),
        max_block_us: stats::max(&times_us),
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{generate, to_channels, Kind};

    #[test]
    fn default_cases_cover_the_spec() {
        let c = default_cases();
        let keys: Vec<String> = c.iter().map(Case::key).collect();
        for k in [
            "cpu.vari.0.25",
            "cpu.vari.0.5",
            "cpu.vari.1.5",
            "cpu.vari.2.0",
            "cpu.vari.-1.0",
            "cpu.boxcar.4.0",
            "cpu.boxcar.8.0",
            "cpu.stretch_speed.0.5",
            "cpu.stretch_speed.4.0",
            "cpu.stretch_seek.1.5",
            "cpu.stretch_seek.2.0",
            "cpu.stretch_seek.3.0",
            "cpu.stretch_seek.4.0",
            "cpu.stretch_speed_cheaper.2.0",
        ] {
            assert!(keys.contains(&k.to_string()), "missing {k}");
        }
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "keys are unique");
        // every key suggest.rs reads exists here
        assert!(source_needed_s(&c, 20.0) > 20.0 * 8.0);
    }

    #[test]
    fn rate_formatting_is_stable() {
        assert_eq!(fmt_rate(2.0), "2.0");
        assert_eq!(fmt_rate(0.25), "0.25");
        assert_eq!(fmt_rate(-1.0), "-1.0");
        assert_eq!(fmt_rate(1.5), "1.5");
        assert_eq!(block_count(1.0, 44100), 173);
    }

    #[test]
    fn run_case_renders_every_block_for_each_kind() {
        // structure only: the CPU numbers on the cloud box are meaningless and never asserted
        let mono = generate(Kind::Music, 12.0, 44100);
        let src = PcmBuf::new(44100, 2, 0, to_channels(&mono, 2));
        for case in [
            Case {
                kind: CaseKind::Vari,
                rate: -1.0,
            },
            Case {
                kind: CaseKind::Boxcar,
                rate: 4.0,
            },
            Case {
                kind: CaseKind::StretchSpeed(StretchPreset::Default),
                rate: 2.0,
            },
            Case {
                kind: CaseKind::StretchSeek,
                rate: 3.0,
            },
        ] {
            let r = run_case(case, 1.0, &src).unwrap();
            assert_eq!(r.blocks, 173, "{}", case.key());
            assert!(r.cpu_pct.is_finite() && r.cpu_pct >= 0.0);
            assert!(r.p99_block_us.is_finite() && r.p99_block_us <= r.max_block_us);
        }
    }
}
