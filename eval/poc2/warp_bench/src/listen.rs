//! Listening tests: `listen speed|scan|reverse|jump|xfade`.
//!
//! Each test plays a short, scripted program through the sink (Pulse on the CM4, or `--out file.wav`)
//! and prints what is playing and what to listen for. The scan profile and the chase controller are the
//! pure functions of `profiles.rs`, so what you hear is what the production formulas produce.
//! Results are verdicts by ear: nothing here judges audio quality.

use crate::args::Args;
use crate::decode::{self, Opened};
use crate::gen::{self, Kind as SigKind};
use crate::profiles::*;
use crate::sink::{open_sink, Sink, SinkOpts};
use crate::stats;
use crate::voices::*;
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

/// Options shared by all listening tests (value options, then flag options).
pub const COMMON_VALUE: &[&str] = &[
    "signal",
    "out",
    "tlength-ms",
    "minreq-ms",
    "start",
    "max-load-s",
    "preset",
];
pub const COMMON_FLAGS: &[&str] = &["no-beep"];

pub const SYNTH_RATE: u32 = 44100;
pub const SYNTH_CH: usize = 2;
const BEEP_GAIN: f32 = 0.2;

#[derive(Debug, Clone)]
pub struct Opts {
    pub sink: SinkOpts,
    pub signal: SigKind,
    pub file: Option<PathBuf>,
    pub start_s: Option<f64>,
    pub max_load_s: f64,
    pub preset: StretchPreset,
    pub beep: bool,
}

impl Opts {
    pub fn from_args(a: &Args) -> Result<Opts> {
        let d = SinkOpts::default();
        Ok(Opts {
            sink: SinkOpts {
                tlength_ms: a.f64("tlength-ms", d.tlength_ms)?,
                minreq_ms: a.f64("minreq-ms", d.minreq_ms)?,
                out: a.get("out").map(PathBuf::from),
            },
            signal: SigKind::parse(&a.string("signal", "chirp"))?,
            file: a.positional.first().map(PathBuf::from),
            start_s: match a.get("start") {
                Some(_) => Some(a.f64("start", 0.0)?),
                None => None,
            },
            max_load_s: a.f64("max-load-s", 180.0)?,
            preset: StretchPreset::parse(&a.string("preset", "default"))?,
            beep: !a.flag("no-beep"),
        })
    }
}

pub struct Source {
    pub buf: PcmBuf,
    pub desc: String,
    pub file: Option<PathBuf>,
}

/// The audio to play: the decoded file (first `max_load_s`), or a generated signal of at least `need_s`.
pub fn load_source(o: &Opts, need_s: f64) -> Result<Source> {
    match &o.file {
        Some(p) => {
            let buf = decode::load_pcm(p, o.max_load_s)?;
            let desc = format!(
                "{} ({} Hz, {} ch, first {:.0} s loaded)",
                p.display(),
                buf.rate,
                buf.ch,
                buf.seconds()
            );
            Ok(Source {
                buf,
                desc,
                file: Some(p.clone()),
            })
        }
        None => {
            let mono = gen::generate(o.signal, need_s.max(30.0), SYNTH_RATE);
            let data = gen::to_channels(&mono, SYNTH_CH);
            Ok(Source {
                buf: PcmBuf::new(SYNTH_RATE, SYNTH_CH, 0, data),
                desc: format!(
                    "generated {:?} signal ({} Hz, {} ch)",
                    o.signal, SYNTH_RATE, SYNTH_CH
                ),
                file: None,
            })
        }
    }
}

pub fn fmt_pos(frames: f64, fs: f64) -> String {
    let s = frames / fs;
    let sign = if s < 0.0 { "-" } else { "" };
    let s = s.abs();
    format!("{sign}{}:{:04.1}", (s / 60.0).floor() as u64, s % 60.0)
}

// ------------------------------------------------------------------ engine

pub struct Engine {
    sink: Box<dyn Sink>,
    pub ch: usize,
    pub fs: f64,
    pub rate: u32,
    pub vari: Varispeed,
    preset: StretchPreset,
    pool: Vec<StretchVoice>,
    pub frames_out: u64,
    beep_on: bool,
    /// Real-time sink (Pulse): stalls and underruns are meaningful. False for `--out file.wav`.
    pub realtime: bool,
    underruns_mark: u64,
    pub non_finite: u64,
    scratch: Vec<f32>,
}

impl Engine {
    pub fn new(o: &Opts, rate: u32, ch: usize, stretch_voices: usize) -> Result<Engine> {
        let sink = open_sink(&o.sink, rate, ch)?;
        let pool = (0..stretch_voices)
            .map(|_| StretchVoice::new(ch, rate, o.preset))
            .collect();
        Ok(Engine {
            sink,
            ch,
            fs: rate as f64,
            rate,
            vari: Varispeed::new(),
            preset: o.preset,
            pool,
            frames_out: 0,
            beep_on: o.beep,
            realtime: o.sink.out.is_none(),
            underruns_mark: 0,
            non_finite: 0,
            scratch: Vec::new(),
        })
    }

    pub fn emit(&mut self, buf: &[f32]) -> Result<()> {
        self.scratch.clear();
        for &s in buf {
            if s.is_finite() {
                self.scratch.push(s.clamp(-1.0, 1.0));
            } else {
                self.non_finite += 1;
                self.scratch.push(0.0);
            }
        }
        self.sink.write(&self.scratch)?;
        self.frames_out += (buf.len() / self.ch) as u64;
        Ok(())
    }

    pub fn silence(&mut self, secs: f64) -> Result<()> {
        let n = (secs * self.fs) as usize;
        let z = vec![0.0f32; n * self.ch];
        for chunk in z.chunks(QUANTUM * self.ch) {
            self.emit(chunk)?;
        }
        Ok(())
    }

    /// A short marker beep (separates test cases).
    pub fn beep(&mut self) -> Result<()> {
        if !self.beep_on {
            return Ok(());
        }
        let mono = gen::beep(self.rate, 120.0, 880.0, BEEP_GAIN);
        let b = gen::to_channels(&mono, self.ch);
        for chunk in b.chunks(QUANTUM * self.ch) {
            self.emit(chunk)?;
        }
        self.silence(0.25)
    }

    pub fn take_stretch(&mut self) -> StretchVoice {
        self.pool
            .pop()
            .unwrap_or_else(|| StretchVoice::new(self.ch, self.rate, self.preset))
    }

    pub fn recycle(&mut self, layer: &mut Layer) {
        if let Some(v) = layer.take_stretch() {
            self.pool.push(v);
        }
    }

    pub fn underruns_total(&self) -> u64 {
        self.sink.underruns()
    }

    /// Underruns since the previous call.
    pub fn underruns_since_mark(&mut self) -> u64 {
        let t = self.sink.underruns();
        let d = t - self.underruns_mark;
        self.underruns_mark = t;
        d
    }

    /// Play `seconds` with one layer at a constant rate.
    pub fn play(&mut self, src: &PcmBuf, layer: &mut Layer, rate: f64, seconds: f64) -> Result<()> {
        let total = (seconds * self.fs) as usize;
        let mut done = 0;
        let mut out = vec![0.0f32; QUANTUM * self.ch];
        while done < total {
            let m = QUANTUM.min(total - done);
            layer.render(&mut self.vari, src, rate, rate, &mut out[..m * self.ch]);
            self.emit(&out[..m * self.ch])?;
            done += m;
        }
        Ok(())
    }

    /// Equal-power crossfade `a -> b` over `ms`; both layers keep rendering at their own rate.
    #[allow(clippy::too_many_arguments)]
    pub fn crossfade(
        &mut self,
        a_src: &PcmBuf,
        a: &mut Layer,
        ra: f64,
        b_src: &PcmBuf,
        b: &mut Layer,
        rb: f64,
        ms: f64,
    ) -> Result<()> {
        let total = ((ms / 1000.0 * self.fs).round() as usize).max(1);
        let mut done = 0;
        let mut ba = vec![0.0f32; QUANTUM * self.ch];
        let mut bb = vec![0.0f32; QUANTUM * self.ch];
        while done < total {
            let m = QUANTUM.min(total - done);
            a.render(&mut self.vari, a_src, ra, ra, &mut ba[..m * self.ch]);
            b.render(&mut self.vari, b_src, rb, rb, &mut bb[..m * self.ch]);
            xfade_blocks(
                &mut ba[..m * self.ch],
                &bb[..m * self.ch],
                self.ch,
                done,
                total,
            );
            self.emit(&ba[..m * self.ch])?;
            done += m;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        self.sink.finish()
    }
}

fn summary_line(eng: &mut Engine, what: &str) {
    let u = eng.underruns_since_mark();
    if eng.realtime {
        println!("    {what}: underruns {u}");
    }
}

fn final_line(eng: &Engine) {
    if eng.realtime {
        println!("Underruns in total: {}", eng.underruns_total());
    } else {
        println!("(rendered to a file: no real-time sink, no underruns to count)");
    }
    if eng.non_finite > 0 {
        println!(
            "WARNING: {} non-finite samples were replaced by silence",
            eng.non_finite
        );
    }
}

// ------------------------------------------------------------------ listen speed

pub const SPEED_RATES: [f64; 6] = [0.1, 0.25, 0.75, 1.25, 1.5, 2.0];

pub fn speed_hint(correct: bool, rate: f64) -> &'static str {
    if !correct {
        if rate < 0.5 {
            "pitch drops with speed (expected); listen for muddy low rumble, zipper/steppy glide when the rate changes, ticks"
        } else if rate > 1.0 {
            "pitch rises with speed (expected); listen for metallic fizz / birdie tones on chirp or sweep (aliasing), dull treble loss"
        } else {
            "pitch drops a little (expected); listen for dull treble loss, ticks, steppy glide on the rate change"
        }
    } else if rate < 0.5 {
        "pitch must stay constant; listen for 'underwater' phasiness, repeating stutter grains, bass wobble, extra delay before the sound starts"
    } else if rate > 1.0 {
        "pitch must stay constant; listen for transient smear (drums lose attack, pre-echo), warble / metallic voice, stereo image narrowing or drifting"
    } else {
        "pitch must stay constant; listen for phasiness / flanging on sustained notes, smeared transients, stereo image drift"
    }
}

pub const SPEED_HELP: &str = "\
warp_bench listen speed [file] [options]

Plays the source at constant speeds, one after the other, with a short beep between. The tempo moves from
one rate to the next with a first-order lag (TAU_TEMPO_MS) so you also hear the ramps.

  --mode shift|correct     shift = varispeed (pitch follows speed); correct = time stretch (default shift)
  --rates 0.1,0.25,...     rates to play (default 0.1,0.25,0.75,1.25,1.5,2.0 = 10,25,75,125,150,200 %)
  --seconds 10             seconds per rate
  --tau-tempo-ms 120       tempo ramp lag (TAU_TEMPO_MS in RateConfig)
  --signal chirp|tone|clicks|sweep|music   generated signal when no file is given (default chirp)
  --start S                start position in the source, seconds (default 0)
  --max-load-s 180         decode at most this much of the file into memory
  --preset default|cheaper stretcher preset (STRETCH_PRESET)
  --out file.wav           render to a WAV file instead of the audio output
  --tlength-ms 80 --minreq-ms 20   Pulse buffer attributes (PulseConfig)
  --no-beep                no marker beeps
";

pub fn speed_opts() -> Vec<&'static str> {
    let mut v = vec!["mode", "rates", "seconds", "tau-tempo-ms"];
    v.extend_from_slice(COMMON_VALUE);
    v
}

pub fn run_speed(a: &Args) -> Result<()> {
    let o = Opts::from_args(a)?;
    let correct = match a.string("mode", "shift").as_str() {
        "shift" => false,
        "correct" => true,
        m => bail!("--mode must be shift or correct, not '{m}'"),
    };
    let rates = a.f64_list("rates", &SPEED_RATES)?;
    if rates.iter().any(|r| *r < 0.05 || *r > 8.0) {
        bail!("--rates: speed rates must be between 0.05 and 8 (reverse has its own test: listen reverse)");
    }
    let seconds = a.f64("seconds", 10.0)?;
    let tau = a.f64("tau-tempo-ms", 120.0)? / 1000.0;
    let need: f64 = rates.iter().map(|r| r * seconds).sum::<f64>() + 5.0 + o.start_s.unwrap_or(0.0);
    let src = load_source(&o, need)?;
    let mut eng = Engine::new(&o, src.buf.rate, src.buf.ch, usize::from(correct))?;
    println!("Source: {}", src.desc);
    println!(
        "Mode: {} | rates {:?} | {seconds} s each | tempo lag {} ms\n",
        if correct {
            "correct (time stretch)"
        } else {
            "shift (varispeed)"
        },
        rates,
        tau * 1000.0
    );
    run_speed_program(
        &mut eng,
        &src.buf,
        correct,
        &rates,
        seconds,
        tau,
        o.start_s.unwrap_or(0.0),
    )?;
    eng.finish()?;
    final_line(&eng);
    Ok(())
}

/// The scripted program; returns the heard position (frames) at the end.
pub fn run_speed_program(
    eng: &mut Engine,
    src: &PcmBuf,
    correct: bool,
    rates: &[f64],
    seconds: f64,
    tau: f64,
    start_s: f64,
) -> Result<f64> {
    let fs = eng.fs;
    let dt = QUANTUM as f64 / fs;
    let mut heard = start_s * fs;
    let mut prev = 1.0f64;
    for (i, &target) in rates.iter().enumerate() {
        println!(
            "[{}/{}] {:.0}% ({:.2}x) {}: {}",
            i + 1,
            rates.len(),
            target * 100.0,
            target,
            if correct { "correct" } else { "shift" },
            speed_hint(correct, target)
        );
        eng.beep()?;
        let mut layer = if correct {
            let v = eng.take_stretch();
            Layer::stretch(heard, v, src, prev)
        } else {
            Layer::vari(heard)
        };
        let total = (seconds * fs) as usize;
        let mut done = 0usize;
        let mut r = prev;
        let mut out = vec![0.0f32; QUANTUM * eng.ch];
        while done < total {
            let m = QUANTUM.min(total - done);
            let a = 1.0 - (-(m as f64 / fs) / tau.max(1e-6)).exp();
            let r_new = r + (target - r) * a;
            layer.render(&mut eng.vari, src, r, r_new, &mut out[..m * eng.ch]);
            eng.emit(&out[..m * eng.ch])?;
            r = r_new;
            done += m;
            if layer.heard(r) > src.frames() as f64 - 2.0 * fs {
                println!("    (end of the source reached after {:.1} s; shorten --seconds or give a longer file)", done as f64 / fs);
                break;
            }
        }
        let _ = dt;
        heard = layer.heard(r);
        prev = r;
        eng.recycle(&mut layer);
        summary_line(eng, "this rate");
    }
    Ok(heard)
}

// ------------------------------------------------------------------ listen scan

pub const SCAN_HELP: &str = "\
warp_bench listen scan [file] [options]

Simulates the FF/RW hold (cassette scan): 1 s of normal playback, then the button is pressed, the speed
spins up to --base, after --boost-after seconds to --boost, the button is released after --hold seconds and
the speed brakes back to 1.0x, then 2 s of normal playback. Absolute source speed (tempo does not matter).

  --mode shift|correct     seek warp mode (default shift). correct = stretch, forward only, capped
  --dir fwd|rev            scan direction (default fwd). Backward is always varispeed
  --base 4 --boost 8 --boost-after 2 --hold 6        SCAN_BASE, SCAN_BOOST, BOOST_AFTER_S, hold length
  --tau-up 0.30 --tau-boost 1.0 --tau-down 0.25      TAU_UP_S, TAU_BOOST_S, TAU_DOWN_S
  --max-scan-vari 8 --max-scan-stretch 3             MAX_SCAN_VARI, MAX_SCAN_STRETCH
  --xfade-bypass-ms 5 --xfade-voice-ms 40            XFADE_BYPASS_MS, XFADE_VOICE_MS
  --pre 1 --tail 2         seconds of normal playback before the press / after the release
  --signal ... --start S --max-load-s --preset --out --tlength-ms --minreq-ms --no-beep   as for 'listen speed'
";

pub fn scan_opts() -> Vec<&'static str> {
    let mut v = vec![
        "mode",
        "dir",
        "base",
        "boost",
        "boost-after",
        "hold",
        "tau-up",
        "tau-boost",
        "tau-down",
        "max-scan-vari",
        "max-scan-stretch",
        "xfade-bypass-ms",
        "xfade-voice-ms",
        "pre",
        "tail",
    ];
    v.extend_from_slice(COMMON_VALUE);
    v
}

#[derive(Debug, Clone)]
pub struct ScanParams {
    pub profile: ScanProfile,
    pub correct: bool,
    pub dir: f64,
    pub hold_s: f64,
    pub pre_s: f64,
    pub tail_s: f64,
    pub xfade_bypass_ms: f64,
    pub xfade_voice_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ScanOutcome {
    pub press_frames: f64,
    pub release_frames: f64,
    /// Playhead (heard) when the speed had settled back to 1.0x.
    pub settled_frames: f64,
    pub peak_rate: f64,
    pub used_stretch: bool,
    pub cap: f64,
}

pub fn scan_cap(p: &ScanParams) -> (f64, bool) {
    let stretch = p.correct && p.dir > 0.0;
    (
        if stretch {
            p.profile.max_scan_stretch
        } else {
            p.profile.max_scan_vari
        },
        stretch,
    )
}

/// Seconds of source the scan program needs before and after the press position.
pub fn scan_extent_s(p: &ScanParams, fs: f64) -> (f64, f64) {
    let (cap, _) = scan_cap(p);
    let tr = simulate_scan(&p.profile, p.dir, p.hold_s, 1.0, cap, QUANTUM as f64 / fs);
    let min = tr.pos_s.iter().cloned().fold(0.0, f64::min);
    let max = tr.pos_s.iter().cloned().fold(0.0, f64::max);
    (-min, max)
}

/// What the formulas predict: (signed travel in source seconds from press to settled, brake time in s).
pub fn scan_prediction(p: &ScanParams, fs: f64) -> (f64, f64) {
    let (cap, _) = scan_cap(p);
    let tr = simulate_scan(&p.profile, p.dir, p.hold_s, 1.0, cap, QUANTUM as f64 / fs);
    (
        *tr.pos_s.last().unwrap_or(&0.0),
        tr.settled_t - tr.release_t,
    )
}

pub fn run_scan(a: &Args) -> Result<()> {
    let o = Opts::from_args(a)?;
    let d = ScanProfile::default();
    let profile = ScanProfile {
        base: a.f64("base", d.base)?,
        boost: a.f64("boost", d.boost)?,
        boost_after_s: a.f64("boost-after", d.boost_after_s)?,
        tau_up_s: a.f64("tau-up", d.tau_up_s)?,
        tau_boost_s: a.f64("tau-boost", d.tau_boost_s)?,
        tau_down_s: a.f64("tau-down", d.tau_down_s)?,
        max_scan_vari: a.f64("max-scan-vari", d.max_scan_vari)?,
        max_scan_stretch: a.f64("max-scan-stretch", d.max_scan_stretch)?,
    };
    let correct = match a.string("mode", "shift").as_str() {
        "shift" => false,
        "correct" => true,
        m => bail!("--mode must be shift or correct, not '{m}'"),
    };
    let dir = match a.string("dir", "fwd").as_str() {
        "fwd" => 1.0,
        "rev" => -1.0,
        m => bail!("--dir must be fwd or rev, not '{m}'"),
    };
    let params = ScanParams {
        profile,
        correct,
        dir,
        hold_s: a.f64("hold", 6.0)?,
        pre_s: a.f64("pre", 1.0)?,
        tail_s: a.f64("tail", 2.0)?,
        xfade_bypass_ms: a.f64("xfade-bypass-ms", 5.0)?,
        xfade_voice_ms: a.f64("xfade-voice-ms", 40.0)?,
    };
    let fs_guess = o
        .file
        .as_ref()
        .map(|_| 48000.0)
        .unwrap_or(SYNTH_RATE as f64);
    let (back, fwd) = scan_extent_s(&params, fs_guess);
    let start_s = o.start_s.unwrap_or(2.0 + back);
    let need = start_s + params.pre_s + fwd + params.tail_s + 8.0;
    let src = load_source(&o, need)?;
    let (back, fwd) = scan_extent_s(&params, src.buf.rate as f64);
    if start_s - back < 0.0
        || start_s + fwd + params.pre_s + params.tail_s + 2.0 > src.buf.seconds()
    {
        bail!(
            "the scan needs {:.1} s before and {:.1} s after the start position {start_s:.1} s but the source is {:.1} s long: use --start, a longer file or --max-load-s",
            back, fwd + params.pre_s + params.tail_s, src.buf.seconds()
        );
    }
    let (cap, stretch) = scan_cap(&params);
    let mut eng = Engine::new(&o, src.buf.rate, src.buf.ch, usize::from(stretch))?;
    println!("Source: {}", src.desc);
    println!(
        "Scan {} | seek mode {} | voice {} | base {} boost {} after {} s | hold {} s | taus up/boost/down {}/{}/{} s | cap {}",
        if dir > 0.0 { "forward" } else { "reverse" },
        if correct { "correct" } else { "shift" },
        if stretch { "stretch (forward only)" } else { "varispeed" },
        params.profile.base, params.profile.boost, params.profile.boost_after_s, params.hold_s,
        params.profile.tau_up_s, params.profile.tau_boost_s, params.profile.tau_down_s, cap
    );
    if correct && dir < 0.0 {
        println!("Note: backward travel is ALWAYS varispeed, whatever the seek warp mode says.");
    }
    println!("{}", scan_listen_hint(&params));
    eng.beep()?;
    let out = run_scan_program(&mut eng, &src.buf, &params, start_s)?;
    eng.finish()?;
    let (pred_travel, pred_brake) = scan_prediction(&params, eng.fs);
    println!(
        "Result: pressed at {}, released at {}, speed settled at {} (travelled {:+.1} s of source, peak {:.2}x, voice {}, cap {}x; the formulas predict {:+.1} s of travel and a {:.1} s brake)",
        fmt_pos(out.press_frames, eng.fs), fmt_pos(out.release_frames, eng.fs), fmt_pos(out.settled_frames, eng.fs),
        (out.settled_frames - out.press_frames) / eng.fs, out.peak_rate,
        if out.used_stretch { "stretch" } else { "varispeed" }, out.cap, pred_travel, pred_brake
    );
    summary_line(&mut eng, "scan");
    final_line(&eng);
    Ok(())
}

pub fn scan_listen_hint(p: &ScanParams) -> String {
    if p.correct && p.dir > 0.0 {
        "LISTEN FOR: constant pitch (expected); smeared / machine-gun repeats; swishing; a click at press or release".into()
    } else {
        "LISTEN FOR: smooth rising whine (expected cassette sound); steps or zipper in pitch (ramps too coarse); a click at the press or at the release hand-over; hiss/aliasing at 8x; dropouts or stutter; brake too abrupt or too lazy; a position jump at the release".into()
    }
}

/// The scan program (press, hold, release, brake, hand-over, tail).
pub fn run_scan_program(
    eng: &mut Engine,
    src: &PcmBuf,
    p: &ScanParams,
    start_s: f64,
) -> Result<ScanOutcome> {
    let fs = eng.fs;
    let (cap, stretch) = scan_cap(p);
    let mut out = ScanOutcome {
        cap,
        used_stretch: stretch,
        ..Default::default()
    };
    // normal playback before the press (Bypass)
    let mut byp = Layer::bypass(start_s * fs);
    eng.play(src, &mut byp, 1.0, p.pre_s)?;
    out.press_frames = byp.pos;
    println!("  press at {}", fmt_pos(byp.pos, fs));
    // seek voice entry
    let mut layer = if stretch {
        let v = eng.take_stretch();
        let mut sl = Layer::stretch(byp.pos, v, src, 1.0);
        eng.crossfade(src, &mut byp, 1.0, src, &mut sl, 1.0, p.xfade_voice_ms)?;
        sl
    } else {
        Layer::vari(byp.pos) // exact at 1.0x: continuous, no crossfade
    };
    let mut rate = 1.0f64;
    let mut hold_s = 0.0f64;
    let mut buf = vec![0.0f32; QUANTUM * eng.ch];
    let dt = QUANTUM as f64 / fs;
    let mut boosted = false;
    let mut released = false;
    let limit = p.hold_s + 60.0;
    loop {
        let held = hold_s < p.hold_s;
        if held && !boosted && hold_s >= p.profile.boost_after_s {
            boosted = true;
            println!(
                "  t={hold_s:.1} s: boost target {}x",
                p.profile.boost.min(cap)
            );
        }
        if !held && !released {
            released = true;
            out.release_frames = layer.heard(rate);
            println!(
                "  t={hold_s:.1} s: release at {} (rate {rate:.2}x), braking",
                fmt_pos(out.release_frames, fs)
            );
        }
        let r_new = scan_step(&p.profile, rate, p.dir, hold_s, held, 1.0, cap, dt);
        layer.render(&mut eng.vari, src, rate, r_new, &mut buf);
        eng.emit(&buf)?;
        rate = r_new;
        out.peak_rate = out.peak_rate.max(rate.abs());
        hold_s += dt;
        if !held && scan_settled(rate, 1.0) {
            break;
        }
        if hold_s > limit {
            bail!("scan did not settle");
        }
    }
    out.settled_frames = layer.heard(rate);
    println!("  speed settled at {}", fmt_pos(out.settled_frames, fs));
    // hand-over to the speed voice (Bypass at the heard position)
    let mut byp = Layer::bypass(layer.heard(rate));
    let ms = if stretch {
        p.xfade_voice_ms
    } else {
        p.xfade_bypass_ms
    };
    eng.crossfade(src, &mut layer, rate, src, &mut byp, 1.0, ms)?;
    eng.recycle(&mut layer);
    eng.play(src, &mut byp, 1.0, p.tail_s)?;
    Ok(out)
}

// ------------------------------------------------------------------ listen jump

pub const JUMP_DISTS: [f64; 5] = [0.1, 1.0, 5.0, 10.0, 60.0];

pub const JUMP_HELP: &str = "\
warp_bench listen jump [file] [options]

Simulates the jog commit: 1.5 s of normal playback, then one smoothed jump of --dist seconds (forward and
backward) with the chase controller, then 1.5 s of normal playback at the target. Far jumps use a crossfaded
waypoint jump followed by an audible chase of the last --waypoint-lead seconds. With a file the waypoint
audio is really decoded from the file on a worker thread (so a slow USB seek shows as a late waypoint).

  --dist 0.1,1,5,10,60     jump distances in seconds
  --dir fwd|back|both      directions (default both). Backward travel is always varispeed
  --mode shift|correct     seek warp mode (default shift). correct = stretch, forward only
  --t-catch 0.2 --r-floor 1 --a-decel 24 --a-slew 60           T_CATCH_S, R_FLOOR, A_DECEL, A_SLEW
  --r-max-vari 4 --r-max-stretch 2                              R_MAX_VARI, R_MAX_STRETCH
  --chase-max-time 1.7     CHASE_MAX_TIME_S (far-jump threshold = R_MAX x this)
  --far-jump-s S           explicit far-jump threshold in seconds (overrides the line above)
  --waypoint-lead 3        WAYPOINT_LEAD_S
  --xfade-ms 60            XFADE_WAYPOINT_MS (waypoint crossfade)
  --xfade-bypass-ms 5 --xfade-voice-ms 40                       XFADE_BYPASS_MS, XFADE_VOICE_MS
  --land-eps 256           LAND_EPS_FRAMES
  --pre 1.5 --tail 1.5     seconds of normal playback around the jump
  --signal ... --start S --max-load-s --preset --out --tlength-ms --minreq-ms --no-beep   as for 'listen speed'
";

pub fn jump_opts() -> Vec<&'static str> {
    let mut v = vec![
        "dist",
        "dir",
        "mode",
        "t-catch",
        "r-floor",
        "a-decel",
        "a-slew",
        "r-max-vari",
        "r-max-stretch",
        "chase-max-time",
        "far-jump-s",
        "waypoint-lead",
        "xfade-ms",
        "xfade-bypass-ms",
        "xfade-voice-ms",
        "land-eps",
        "pre",
        "tail",
    ];
    v.extend_from_slice(COMMON_VALUE);
    v
}

#[derive(Debug, Clone)]
pub struct JumpParams {
    pub cfg: ChaseConfig,
    pub correct: bool,
    pub xfade_bypass_ms: f64,
    pub xfade_voice_ms: f64,
    pub pre_s: f64,
    pub tail_s: f64,
}

#[derive(Debug, Clone, Default)]
pub struct JumpOutcome {
    pub far: bool,
    pub stretch: bool,
    pub landed_frames: f64,
    pub target_frames: f64,
    pub travel_s: f64,
    pub peak_rate: f64,
    pub waypoint_ready_ms: Option<f64>,
    /// Seconds the chase ran before the waypoint voice was ready (far jumps).
    pub waypoint_wait_s: Option<f64>,
    /// The chase arrived before the waypoint voice was ready (the waypoint was too late to be used).
    pub waypoint_late: bool,
}

type PrepResult = Result<(PcmBuf, f64)>;

/// Prepare the waypoint window: decoded from the file on a worker thread (own file handle), or cut from
/// memory for generated signals. The thread answers with the window and the milliseconds it took.
fn spawn_prep(
    file: Option<PathBuf>,
    mem: &PcmBuf,
    start: i64,
    frames: usize,
) -> mpsc::Receiver<PrepResult> {
    let (tx, rx) = mpsc::channel();
    match file {
        Some(path) => {
            std::thread::spawn(move || {
                let t0 = Instant::now();
                let r = (|| {
                    let mut o = decode::open(&path)?;
                    decode::read_window(&mut o, start, frames)
                })()
                .map(|w| (w, t0.elapsed().as_secs_f64() * 1000.0));
                let _ = tx.send(r);
            });
        }
        None => {
            let mut d = Vec::new();
            mem.copy_frames(start, frames, &mut d);
            let _ = tx.send(Ok((PcmBuf::new(mem.rate, mem.ch, start, d), 0.0)));
        }
    }
    rx
}

/// One jump: pre-roll, chase, hand-over, tail. `commit_s` is where the playhead is when the jump is
/// committed (the pre-roll starts `pre_s` earlier); `file` enables real waypoint decoding.
pub fn run_jump_case(
    eng: &mut Engine,
    src: &PcmBuf,
    file: Option<&Path>,
    p: &JumpParams,
    commit_s: f64,
    target_s: f64,
) -> Result<JumpOutcome> {
    let fs = eng.fs;
    let dt = QUANTUM as f64 / fs;
    let cfg = &p.cfg;
    let mut byp = Layer::bypass((commit_s - p.pre_s).max(0.0) * fs);
    eng.play(src, &mut byp, 1.0, p.pre_s)?;
    let p0 = byp.pos;
    let target = (target_s * fs).round();
    let d0 = (target - p0) / fs;
    let stretch = p.correct && d0 > 0.0;
    let r_max = cfg.r_max(stretch);
    let far = is_far(cfg, r_max, d0);
    let mut out = JumpOutcome {
        far,
        stretch,
        target_frames: target,
        ..Default::default()
    };
    // seek voice entry
    let mut layer = if stretch {
        let v = eng.take_stretch();
        let mut sl = Layer::stretch(p0, v, src, 1.0);
        eng.crossfade(src, &mut byp, 1.0, src, &mut sl, 1.0, p.xfade_voice_ms)?;
        sl
    } else {
        Layer::vari(p0)
    };
    // far jump: prepare the waypoint voice (its own source window)
    let mut prep: Option<mpsc::Receiver<PrepResult>> = None;
    let mut wp_frames = 0.0;
    if far {
        wp_frames = (waypoint_s(cfg, p0 / fs, target / fs) * fs).round();
        let lo = wp_frames.min(target) as i64 - fs as i64;
        let hi = wp_frames.max(target) as i64 + ((p.tail_s + 1.5) * fs) as i64;
        prep = Some(spawn_prep(
            file.map(Path::to_path_buf),
            src,
            lo.max(0),
            (hi - lo.max(0)) as usize,
        ));
    }
    let mut cur_src_owned: Option<PcmBuf> = None;
    let mut rate = 1.0f64;
    let mut buf = vec![0.0f32; QUANTUM * eng.ch];
    let mut blocks = 0usize;
    loop {
        // waypoint voice ready? then crossfade to it (never stall: until then the chase just continues).
        // With a file sink there is no real time, so wait for the worker to keep the render deterministic.
        if let Some(rx) = &prep {
            let got = if eng.realtime {
                rx.try_recv().ok()
            } else {
                rx.recv().ok()
            };
            if let Some(res) = got {
                let (win, ms) = res?;
                out.waypoint_ready_ms = Some(ms);
                out.waypoint_wait_s = Some(blocks as f64 * dt);
                prep = None;
                let mut b = if stretch {
                    let v = eng.take_stretch();
                    Layer::stretch(wp_frames, v, &win, rate.max(1.0))
                } else {
                    Layer::vari(wp_frames)
                };
                let cur_ref: &PcmBuf = cur_src_owned.as_ref().unwrap_or(src);
                eng.crossfade(
                    cur_ref,
                    &mut layer,
                    rate,
                    &win,
                    &mut b,
                    rate,
                    cfg.xfade_waypoint_ms,
                )?;
                eng.recycle(&mut layer);
                layer = b;
                cur_src_owned = Some(win);
            }
        }
        let cur: &PcmBuf = cur_src_owned.as_ref().unwrap_or(src);
        let heard = layer.heard(rate);
        let d_s = (target - heard) / fs;
        if (target - heard).abs() <= cfg.land_eps_frames {
            out.waypoint_late = prep.is_some();
            break;
        }
        // a pending waypoint means the playhead is still far away: the chase aims at the real target
        let r_new = chase_step(cfg, r_max, rate, d_s, dt);
        layer.render(&mut eng.vari, cur, rate, r_new, &mut buf);
        eng.emit(&buf)?;
        rate = r_new;
        out.peak_rate = out.peak_rate.max(rate.abs());
        blocks += 1;
        if blocks as f64 * dt > 120.0 {
            bail!("jump did not land within 120 s of audio");
        }
    }
    // the waypoint crossfade is audible travel time too (the chase keeps moving during it)
    out.travel_s = blocks as f64 * dt
        + if out.waypoint_ready_ms.is_some() {
            cfg.xfade_waypoint_ms / 1000.0
        } else {
            0.0
        };
    let cur: &PcmBuf = cur_src_owned.as_ref().unwrap_or(src);
    out.landed_frames = layer.heard(rate);
    // landing hand-over to Bypass at the heard position
    let mut byp2 = Layer::bypass(layer.heard(rate));
    let ms = if stretch {
        p.xfade_voice_ms
    } else {
        p.xfade_bypass_ms
    };
    eng.crossfade(cur, &mut layer, rate, cur, &mut byp2, 1.0, ms)?;
    eng.recycle(&mut layer);
    eng.play(cur, &mut byp2, 1.0, p.tail_s)?;
    Ok(out)
}

pub fn run_jump(a: &Args) -> Result<()> {
    let o = Opts::from_args(a)?;
    let d = ChaseConfig::default();
    let cfg = ChaseConfig {
        t_catch_s: a.f64("t-catch", d.t_catch_s)?,
        r_floor: a.f64("r-floor", d.r_floor)?,
        a_decel: a.f64("a-decel", d.a_decel)?,
        a_slew: a.f64("a-slew", d.a_slew)?,
        r_max_vari: a.f64("r-max-vari", d.r_max_vari)?,
        r_max_stretch: a.f64("r-max-stretch", d.r_max_stretch)?,
        chase_max_time_s: a.f64("chase-max-time", d.chase_max_time_s)?,
        waypoint_lead_s: a.f64("waypoint-lead", d.waypoint_lead_s)?,
        land_eps_frames: a.f64("land-eps", d.land_eps_frames)?,
        xfade_waypoint_ms: a.f64("xfade-ms", d.xfade_waypoint_ms)?,
        far_jump_override_s: match a.get("far-jump-s") {
            Some(_) => Some(a.f64("far-jump-s", 0.0)?),
            None => None,
        },
    };
    let correct = match a.string("mode", "shift").as_str() {
        "shift" => false,
        "correct" => true,
        m => bail!("--mode must be shift or correct, not '{m}'"),
    };
    let dists = a.f64_list("dist", &JUMP_DISTS)?;
    let dirs: Vec<f64> = match a.string("dir", "both").as_str() {
        "fwd" => vec![1.0],
        "back" => vec![-1.0],
        "both" => vec![1.0, -1.0],
        m => bail!("--dir must be fwd, back or both, not '{m}'"),
    };
    let params = JumpParams {
        cfg,
        correct,
        xfade_bypass_ms: a.f64("xfade-bypass-ms", 5.0)?,
        xfade_voice_ms: a.f64("xfade-voice-ms", 40.0)?,
        pre_s: a.f64("pre", 1.5)?,
        tail_s: a.f64("tail", 1.5)?,
    };
    let maxd = dists.iter().cloned().fold(0.0, f64::max);
    let need = 2.0 * maxd + o.start_s.unwrap_or(5.0) + 20.0;
    let src = load_source(&o, need)?;
    let mut eng = Engine::new(&o, src.buf.rate, src.buf.ch, if correct { 3 } else { 0 })?;
    println!("Source: {}", src.desc);
    println!("Jump | seek mode {} | caps vari {} stretch {} | t_catch {} s | far jump beyond {:.1} s (vari) / {:.1} s (stretch) | waypoint lead {} s, xfade {} ms",
        if correct { "correct" } else { "shift" }, params.cfg.r_max_vari, params.cfg.r_max_stretch, params.cfg.t_catch_s,
        params.cfg.far_jump_s(params.cfg.r_max_vari), params.cfg.far_jump_s(params.cfg.r_max_stretch), params.cfg.waypoint_lead_s, params.cfg.xfade_waypoint_ms);
    println!("LISTEN FOR: a short pitch chirp that ends exactly at the target (expected); chirp too long / too short / too violent; overshoot or wobble at the end; a click at the start or end; for far jumps a double-audio blend or click at the waypoint, or a silence gap while the file seeks.\n");
    let mut table = crate::report::Table::new(&[
        "jump",
        "path",
        "voice",
        "swoop (ms)",
        "formula predicts (ms)",
        "peak rate",
        "landing error (frames)",
        "waypoint ready (ms)",
    ]);
    let start = o.start_s.unwrap_or(5.0).max(params.pre_s);
    for &sign in &dirs {
        for &dist in &dists {
            let from = if sign > 0.0 { start } else { start + dist };
            let target = from + sign * dist;
            if target < 0.0 || from.max(target) + params.tail_s + 6.0 > src.buf.seconds() {
                println!("skip {:+} s: needs a source of at least {:.0} s (have {:.0} s); use --start / --max-load-s / a longer file", sign * dist, from.max(target) + params.tail_s + 6.0, src.buf.seconds());
                continue;
            }
            println!(
                "jump {:+} s ({} -> {})",
                sign * dist,
                fmt_pos(from * eng.fs, eng.fs),
                fmt_pos(target * eng.fs, eng.fs)
            );
            eng.beep()?;
            let out = run_jump_case(
                &mut eng,
                &src.buf,
                src.file.as_deref(),
                &params,
                from,
                target,
            )?;
            summary_line(&mut eng, "this jump");
            let predicted = simulate_chase(
                &params.cfg,
                params.cfg.r_max(out.stretch),
                eng.fs,
                QUANTUM,
                from,
                1.0,
                target,
                120.0,
            )
            .sim
            .t;
            table.row(vec![
                format!("{:+} s", sign * dist),
                if out.far {
                    "waypoint".to_string()
                } else {
                    "chase".to_string()
                },
                if out.stretch {
                    "stretch".to_string()
                } else {
                    "varispeed".to_string()
                },
                format!("{:.0}", out.travel_s * 1000.0),
                format!("{:.0}", predicted * 1000.0),
                format!("{:.2}", out.peak_rate),
                format!("{:.0}", out.landed_frames - out.target_frames),
                match (out.waypoint_ready_ms, out.waypoint_late) {
                    (Some(m), _) => format!("{m:.0}"),
                    (None, true) => "TOO LATE (chase arrived first)".to_string(),
                    _ => "-".to_string(),
                },
            ]);
        }
    }
    eng.finish()?;
    println!("\n{}", table.render());
    println!("Read off: 'swoop (ms)' = how long the audible chirp lasts; 'waypoint ready (ms)' = how long the file took to deliver the waypoint audio (compare with the chirp time before the crossfade: a late waypoint means WAYPOINT_LEAD_S must go up or the far-jump threshold down).");
    final_line(&eng);
    Ok(())
}

// ------------------------------------------------------------------ listen xfade

pub const XFADE_LENGTHS_MS: [f64; 5] = [5.0, 10.0, 20.0, 40.0, 60.0];

pub const XFADE_HELP: &str = "\
warp_bench listen xfade [file] [options]

Plays a hard jump (30 s away) with a crossfade of each length, first over a 1 kHz tone, then over music
(the file, or the generated 'music' signal). Each length is announced and marked by a beep; every length
makes --reps joins, one second apart. Short fades click; long fades sound doubled or fluttery.

  --lengths-ms 5,10,20,40,60   crossfade lengths
  --reps 3                     joins per length
  --jump-s 30                  distance of the jump
  --signal ... --start S --max-load-s --out --tlength-ms --minreq-ms --no-beep   as for 'listen speed'
";

pub fn xfade_opts() -> Vec<&'static str> {
    let mut v = vec!["lengths-ms", "reps", "jump-s"];
    v.extend_from_slice(COMMON_VALUE);
    v
}

/// Hard jump with an equal-power crossfade: `a` is faded out, a bypass layer at `to` is faded in.
pub fn play_with_join(
    eng: &mut Engine,
    src: &PcmBuf,
    from: f64,
    to: f64,
    ms: f64,
    lead_s: f64,
) -> Result<()> {
    let mut a = Layer::bypass(from);
    eng.play(src, &mut a, 1.0, lead_s)?;
    let mut b = Layer::bypass(to);
    eng.crossfade(src, &mut a, 1.0, src, &mut b, 1.0, ms)?;
    eng.play(src, &mut b, 1.0, 0.3)?;
    Ok(())
}

pub fn run_xfade(a: &Args) -> Result<()> {
    let o = Opts::from_args(a)?;
    let lengths = a.f64_list("lengths-ms", &XFADE_LENGTHS_MS)?;
    let reps = a.usize("reps", 3)?;
    let jump_s = a.f64("jump-s", 30.0)?;
    let start = o.start_s.unwrap_or(2.0);
    let need = start + jump_s + 8.0;
    let tone = {
        let mono = gen::generate(SigKind::Tone, need.max(40.0), SYNTH_RATE);
        PcmBuf::new(SYNTH_RATE, SYNTH_CH, 0, gen::to_channels(&mono, SYNTH_CH))
    };
    let music_opts = Opts {
        signal: if o.file.is_none() && a.get("signal").is_none() {
            SigKind::Music
        } else {
            o.signal
        },
        ..o.clone()
    };
    let music = load_source(&music_opts, need)?;
    if music.buf.seconds() < start + jump_s + 4.0 {
        bail!("the music source is only {:.0} s long; need {:.0} s (use --jump-s, --start or a longer file)", music.buf.seconds(), start + jump_s + 4.0);
    }
    // one sink at the music rate; the tone is generated at the same rate
    let rate = music.buf.rate;
    let tone = if rate == tone.rate {
        tone
    } else {
        let mono = gen::generate(SigKind::Tone, need.max(40.0), rate);
        PcmBuf::new(rate, music.buf.ch, 0, gen::to_channels(&mono, music.buf.ch))
    };
    let tone = if tone.ch == music.buf.ch {
        tone
    } else {
        let mono = gen::generate(SigKind::Tone, need.max(40.0), rate);
        PcmBuf::new(rate, music.buf.ch, 0, gen::to_channels(&mono, music.buf.ch))
    };
    let mut eng = Engine::new(&o, rate, music.buf.ch, 0)?;
    let fs = eng.fs;
    println!("Music source: {}", music.desc);
    println!("LISTEN FOR: a click or tick at the join (fade too short), or an audible double / flutter / level dip (fade too long). Pick the shortest length without a click.\n");
    for (name, buf) in [("tone", &tone), ("music", &music.buf)] {
        for &ms in &lengths {
            println!("xfade over {name}: {ms} ms x{reps}");
            eng.beep()?;
            for r in 0..reps {
                let (from, to) = if r % 2 == 0 {
                    (start, start + jump_s)
                } else {
                    (start + jump_s, start)
                };
                play_with_join(&mut eng, buf, from * fs, to * fs, ms, 1.0)?;
            }
            summary_line(&mut eng, "this length");
        }
    }
    eng.finish()?;
    final_line(&eng);
    Ok(())
}

// ------------------------------------------------------------------ listen reverse

pub const REVERSE_RATES: [f64; 4] = [-0.25, -0.5, -1.0, -2.0];

pub const REVERSE_HELP: &str = "\
warp_bench listen reverse <file> [options]

Plays the file backwards at each rate. The audio comes from back-windows: blocks of --back-window-s seconds
decoded from the file by an Accurate seek per block on a worker thread, NO index (the worst case before the
MP3 index exists). The program reports how long each block took and counts the stalls (the next block was not
ready, the output goes silent until it is). Reverse is always varispeed.

  --rates -0.25,-0.5,-1,-2   reverse rates (negative)
  --seconds 10               seconds per rate
  --back-window-s 2          BACK_WINDOW_S (1, 2, 4, 8)
  --start S                  start position in the file, seconds (default: 30 % into the file, or far
                             enough in for all the rates; try small and large values)
  --signal ... --out --tlength-ms --minreq-ms --no-beep   as for 'listen speed'
  (without a file a generated signal is played from memory: the seek cost is then NOT represented)
";

pub fn reverse_opts() -> Vec<&'static str> {
    let mut v = vec!["rates", "seconds", "back-window-s"];
    v.extend_from_slice(COMMON_VALUE);
    v
}

pub enum BackSource {
    File(Box<Opened>),
    Memory(PcmBuf),
}

impl BackSource {
    fn fetch(&mut self, start: i64, frames: usize) -> Result<PcmBuf> {
        match self {
            BackSource::File(o) => decode::read_window(o, start.max(0), frames),
            BackSource::Memory(m) => {
                let mut d = Vec::new();
                m.copy_frames(start, frames, &mut d);
                Ok(PcmBuf::new(m.rate, m.ch, start, d))
            }
        }
    }
}

struct Fetcher {
    tx: mpsc::Sender<i64>,
    rx: mpsc::Receiver<Result<(i64, PcmBuf, f64)>>,
    pending: usize,
}

impl Fetcher {
    fn new(mut src: BackSource, block_frames: usize) -> Fetcher {
        let (tx, req_rx) = mpsc::channel::<i64>();
        let (res_tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(start) = req_rx.recv() {
                let t0 = Instant::now();
                let r = src
                    .fetch(start, block_frames)
                    .map(|b| (start, b, t0.elapsed().as_secs_f64() * 1000.0));
                if res_tx.send(r).is_err() {
                    break;
                }
            }
        });
        Fetcher { tx, rx, pending: 0 }
    }

    fn request(&mut self, start: i64) {
        let _ = self.tx.send(start);
        self.pending += 1;
    }

    fn try_get(&mut self) -> Option<Result<(i64, PcmBuf, f64)>> {
        match self.rx.try_recv() {
            Ok(r) => {
                self.pending -= 1;
                Some(r)
            }
            Err(_) => None,
        }
    }

    fn wait(&mut self) -> Result<(i64, PcmBuf, f64)> {
        let r = self.rx.recv().map_err(|_| anyhow!("fetch worker ended"))?;
        self.pending -= 1;
        r
    }
}

/// Concatenate two adjacent blocks (`older` directly before `newer`) into one window.
pub fn join_blocks(older: &PcmBuf, newer: &PcmBuf) -> PcmBuf {
    let start = older.start;
    let end = newer.end().max(older.end());
    let n = (end - start).max(0) as usize;
    let mut data = Vec::with_capacity(n * older.ch);
    for f in start..end {
        let src = if f >= newer.start && f < newer.end() {
            newer
        } else {
            older
        };
        for c in 0..older.ch {
            data.push(src.at(f, c));
        }
    }
    PcmBuf::new(older.rate, older.ch, start, data)
}

#[derive(Debug, Clone, Default)]
pub struct ReverseOutcome {
    pub fetch_ms: Vec<f64>,
    pub stalls: usize,
    pub stalled_s: f64,
    pub reached_start: bool,
    pub heard_end_frames: f64,
}

/// Frames below the window start at which the next (older) block is needed.
pub const SWITCH_MARGIN_FRAMES: i64 = 4096;

#[allow(clippy::too_many_arguments)]
pub fn run_reverse_program(
    eng: &mut Engine,
    source: BackSource,
    rate_hz: u32,
    ch: usize,
    start_frames: f64,
    rates: &[f64],
    seconds: f64,
    back_window_s: f64,
) -> Result<ReverseOutcome> {
    let fs = eng.fs;
    let w = (back_window_s * fs).round() as i64;
    let mut out = ReverseOutcome::default();
    let mut fetcher = Fetcher::new(source, w as usize);
    let s0 = start_frames.round() as i64;
    // block j covers [s0 - (j+1) w, s0 - j w): fetch block 0 and 1 first (timed: the first-rewind cost)
    fetcher.request(s0 - w);
    fetcher.request(s0 - 2 * w);
    let (_, b0, ms0) = fetcher.wait()?;
    let (_, b1, ms1) = fetcher.wait()?;
    out.fetch_ms.extend([ms0, ms1]);
    println!(
        "  first two back-windows ready after {:.0} + {:.0} ms",
        ms0, ms1
    );
    let mut window = join_blocks(&b1, &b0);
    let mut next_block = 2i64; // index of the next block to request
    fetcher.request(s0 - (next_block + 1) * w);
    let mut pos = s0 as f64;
    let mut layer = Layer::vari(pos);
    let _ = ch;
    let _ = rate_hz;
    let mut buf = vec![0.0f32; QUANTUM * eng.ch];
    let dt = QUANTUM as f64 / fs;
    let mut rate = 0.0f64;
    let mut prev_target = -1.0f64;
    'segments: for (i, &target) in rates.iter().enumerate() {
        println!("[{}/{}] {:.2}x reverse: LISTEN FOR speech/music backwards with pitch following speed (expected); clicks at block seams (every {} s of source); stutter or stall at block boundaries; silence gaps", i + 1, rates.len(), target, back_window_s);
        eng.beep()?;
        let total_blocks = (seconds * fs / QUANTUM as f64).ceil() as usize;
        let mut r = if prev_target < 0.0 { -0.02 } else { rate };
        let mut stalled_run = false;
        for _ in 0..total_blocks {
            // switch to the next older block when the playhead nears the window start
            if (pos as i64) < window.start + SWITCH_MARGIN_FRAMES {
                let ready = match fetcher.try_get() {
                    Some(r) => Some(r?),
                    None if !eng.realtime && fetcher.pending > 0 => Some(fetcher.wait()?),
                    None => None,
                };
                match ready {
                    Some((_, blk, ms)) => {
                        out.fetch_ms.push(ms);
                        // drop the newest block: keep the older half of the window, prepend the new block
                        let mut d = Vec::new();
                        window.copy_frames(window.start, w as usize, &mut d);
                        let older_half = PcmBuf::new(window.rate, window.ch, window.start, d);
                        window = join_blocks(&blk, &older_half);
                        next_block += 1;
                        fetcher.request(s0 - (next_block + 1) * w);
                        stalled_run = false;
                    }
                    None => {
                        if !stalled_run {
                            out.stalls += 1;
                            stalled_run = true;
                        }
                        out.stalled_s += dt;
                        buf.fill(0.0);
                        eng.emit(&buf)?;
                        continue;
                    }
                }
            }
            let a = 1.0 - (-dt / 0.12f64).exp();
            let r_new = r + (target - r) * a;
            layer.pos = pos;
            layer.render(&mut eng.vari, &window, r, r_new, &mut buf);
            pos = layer.pos;
            eng.emit(&buf)?;
            r = r_new;
            if pos <= 0.0 {
                out.reached_start = true;
                println!("    reached 0:00 (the set-speed rule would bounce forward here)");
                break 'segments;
            }
        }
        rate = r;
        prev_target = target;
        let med = stats::median(&out.fetch_ms);
        println!("    at {}; back-window fetches so far: median {:.0} ms, max {:.0} ms; stalls {} ({:.1} s silent)", fmt_pos(pos, fs), med, stats::max(&out.fetch_ms), out.stalls, out.stalled_s);
        summary_line(eng, "this rate");
    }
    out.heard_end_frames = pos;
    Ok(out)
}

pub fn run_reverse(a: &Args) -> Result<()> {
    let o = Opts::from_args(a)?;
    let rates = a.f64_list("rates", &REVERSE_RATES)?;
    if rates.iter().any(|r| *r >= 0.0 || *r < -8.0) {
        bail!("--rates must be negative (and not below -8)");
    }
    let seconds = a.f64("seconds", 10.0)?;
    let bw = a.f64("back-window-s", 2.0)?;
    let dist: f64 = rates.iter().map(|r| r.abs() * seconds).sum();
    let (source, rate_hz, ch, dur_s, desc) = match &o.file {
        Some(p) => {
            let mut op = decode::open(p)?;
            let dur = op.duration_frames()? as f64 / op.rate as f64;
            let (rate, ch) = (op.rate, op.ch);
            // a fresh handle for the worker (the probe above consumed state)
            drop(op);
            let wk = decode::open(p)?;
            (
                BackSource::File(Box::new(wk)),
                rate,
                ch,
                dur,
                format!(
                    "{} ({} Hz, {} ch, {:.0} s) read by Accurate seeks, no index",
                    p.display(),
                    rate,
                    ch,
                    dur
                ),
            )
        }
        None => {
            let need = dist + o.start_s.unwrap_or(0.0) + 20.0;
            let mono = gen::generate(o.signal, need.max(30.0), SYNTH_RATE);
            let data = gen::to_channels(&mono, SYNTH_CH);
            let buf = PcmBuf::new(SYNTH_RATE, SYNTH_CH, 0, data);
            let dur = buf.seconds();
            (
                BackSource::Memory(buf),
                SYNTH_RATE,
                SYNTH_CH,
                dur,
                format!(
                    "generated {:?} signal from memory (the seek cost is NOT represented)",
                    o.signal
                ),
            )
        }
    };
    let start_s = o
        .start_s
        .unwrap_or_else(|| (dur_s * 0.3).max(dist + 2.0 * bw + 2.0).min(dur_s - 2.0));
    if start_s - dist - 2.0 * bw < 0.0 {
        println!("Note: the program needs {:.0} s before the start position {:.0} s; it will stop at 0:00 if the file is not long enough (use --start or fewer/shorter rates).", dist + 2.0 * bw, start_s);
    }
    let mut eng = Engine::new(&o, rate_hz, ch, 0)?;
    println!("Source: {desc}");
    println!(
        "Reverse from {} | back-window {} s | rates {:?} | {seconds} s each\n",
        fmt_pos(start_s * eng.fs, eng.fs),
        bw,
        rates
    );
    let fs = eng.fs;
    let out = run_reverse_program(
        &mut eng,
        source,
        rate_hz,
        ch,
        start_s * fs,
        &rates,
        seconds,
        bw,
    )?;
    eng.finish()?;
    let mut t = crate::report::Table::new(&[
        "back-windows fetched",
        "fetch median (ms)",
        "fetch p95 (ms)",
        "fetch max (ms)",
        "stalls",
        "silent (s)",
    ]);
    t.row(vec![
        out.fetch_ms.len().to_string(),
        format!("{:.0}", stats::median(&out.fetch_ms)),
        format!("{:.0}", stats::percentile(&out.fetch_ms, 95.0)),
        format!("{:.0}", stats::max(&out.fetch_ms)),
        out.stalls.to_string(),
        format!("{:.1}", out.stalled_s),
    ]);
    println!("\n{}", t.render());
    println!("Read off: a block of {bw} s must be fetched in well under {bw}/rate seconds (e.g. under {:.0} ms at 2x) or the output stalls. Stalls -> BACK_WINDOW_S up, HISTORY_S up, or lower MAX_SCAN_VARI for this media; see README.", bw * 1000.0 / 2.0 * 0.7);
    final_line(&eng);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{generate, goertzel, to_channels};

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("warp_bench_listen_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts(wav: &Path) -> Opts {
        Opts {
            sink: SinkOpts {
                out: Some(wav.to_path_buf()),
                ..Default::default()
            },
            signal: SigKind::Tone,
            file: None,
            start_s: None,
            max_load_s: 180.0,
            preset: StretchPreset::Default,
            beep: false,
        }
    }

    fn src(kind: SigKind, secs: f64) -> PcmBuf {
        PcmBuf::new(44100, 2, 0, to_channels(&generate(kind, secs, 44100), 2))
    }

    /// 16-bit WAV samples of channel 0.
    fn read_wav_ch0(p: &Path) -> Vec<f32> {
        let b = std::fs::read(p).unwrap();
        b[44..]
            .chunks_exact(4)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32767.0)
            .collect()
    }

    fn q(x: f32) -> f32 {
        ((x.clamp(-1.0, 1.0) * 32767.0).round()) / 32767.0
    }

    #[test]
    fn scan_program_matches_the_simulated_profile_and_hands_over_bit_exactly() {
        let d = tmp("scan");
        let wav = d.join("o.wav");
        let s = src(SigKind::Music, 90.0);
        let params = ScanParams {
            profile: ScanProfile::default(),
            correct: false,
            dir: 1.0,
            hold_s: 3.0,
            pre_s: 0.3,
            tail_s: 0.5,
            xfade_bypass_ms: 5.0,
            xfade_voice_ms: 40.0,
        };
        let mut eng = Engine::new(&opts(&wav), 44100, 2, 0).unwrap();
        let out = run_scan_program(&mut eng, &s, &params, 5.0).unwrap();
        eng.finish().unwrap();
        assert_eq!(eng.non_finite, 0);
        let (_, fwd) = scan_extent_s(&params, 44100.0);
        let sim = (out.settled_frames - out.press_frames) / 44100.0;
        assert!(
            (sim - fwd).abs() < 0.001,
            "rendered travel {sim} s vs simulated {fwd} s"
        );
        assert!(out.peak_rate <= 8.0 + 1e-9 && out.peak_rate > 4.0);
        // after the hand-over the tail is the source again, bit exact (up to the 16-bit file)
        let o = read_wav_ch0(&wav);
        let tail = (0.5 * 44100.0) as usize;
        let from = out.settled_frames.round() as usize + (0.005 * 44100.0f64).round() as usize;
        for i in (o.len() - tail + 10)..o.len() {
            let k = i - (o.len() - tail);
            assert_eq!(o[i], q(s.data[(from + k) * 2]), "tail frame {k}");
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn scan_reverse_and_stretch_variants_run_cleanly() {
        let d = tmp("scan2");
        let s = src(SigKind::Music, 120.0);
        for (correct, dir) in [(false, -1.0), (true, 1.0), (true, -1.0)] {
            let params = ScanParams {
                profile: ScanProfile::default(),
                correct,
                dir,
                hold_s: 2.5,
                pre_s: 0.3,
                tail_s: 0.3,
                xfade_bypass_ms: 5.0,
                xfade_voice_ms: 40.0,
            };
            let (back, _) = scan_extent_s(&params, 44100.0);
            let wav = d.join(format!("o{correct}{dir}.wav"));
            let mut eng =
                Engine::new(&opts(&wav), 44100, 2, usize::from(correct && dir > 0.0)).unwrap();
            let out = run_scan_program(&mut eng, &s, &params, 3.0 + back).unwrap();
            eng.finish().unwrap();
            assert_eq!(eng.non_finite, 0);
            let (cap, stretch) = scan_cap(&params);
            assert_eq!(
                stretch,
                correct && dir > 0.0,
                "backward is always varispeed"
            );
            assert!(
                out.peak_rate <= cap + 1e-9,
                "peak {} cap {cap}",
                out.peak_rate
            );
            assert!(read_wav_ch0(&wav).iter().all(|x| x.is_finite()));
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn jump_lands_on_the_target_and_matches_the_formula() {
        let d = tmp("jump");
        let s = src(SigKind::Music, 200.0);
        for (correct, from, to) in [
            (false, 20.0, 20.1),
            (false, 20.0, 21.0),
            (false, 20.0, 25.0),
            (false, 30.0, 25.0),
            (false, 20.0, 80.0),
            (false, 90.0, 30.0),
            (true, 20.0, 25.0),
            (true, 20.0, 80.0),
            (true, 40.0, 30.0),
        ] {
            let params = JumpParams {
                cfg: ChaseConfig::default(),
                correct,
                xfade_bypass_ms: 5.0,
                xfade_voice_ms: 40.0,
                pre_s: 0.2,
                tail_s: 0.2,
            };
            let wav = d.join("o.wav");
            let (from, to): (f64, f64) = (from, to);
            let mut eng = Engine::new(&opts(&wav), 44100, 2, 3).unwrap();
            let out = run_jump_case(&mut eng, &s, None, &params, from, to).unwrap();
            eng.finish().unwrap();
            assert_eq!(eng.non_finite, 0);
            let err = (out.landed_frames - out.target_frames).abs();
            assert!(
                err <= params.cfg.land_eps_frames + 1.0,
                "{from}->{to} correct={correct}: landing error {err}"
            );
            let dist = to - from;
            assert_eq!(
                out.stretch,
                correct && dist > 0.0,
                "backward is always varispeed"
            );
            let rmax = params.cfg.r_max(out.stretch);
            assert!(out.peak_rate <= rmax + 1e-9);
            assert_eq!(
                out.far,
                dist.abs() > params.cfg.far_jump_s(rmax),
                "{from}->{to}"
            );
            if !out.stretch {
                let sim = simulate_chase(&params.cfg, rmax, 44100.0, QUANTUM, from, 1.0, to, 120.0);
                // the formula jumps instantly; the render spends the waypoint crossfade (60 ms) travelling too
                let tol = 3.0 * QUANTUM as f64 / 44100.0
                    + if out.far {
                        params.cfg.xfade_waypoint_ms / 1000.0
                    } else {
                        0.0
                    };
                assert!(
                    (out.travel_s - sim.sim.t).abs() <= tol,
                    "{from}->{to}: rendered {} s vs formula {} s",
                    out.travel_s,
                    sim.sim.t
                );
            }
            if out.far {
                assert!(out.waypoint_ready_ms.is_some() && !out.waypoint_late);
            }
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn jump_output_is_continuous_at_entry_and_landing() {
        // a 1 kHz tone: a click would show up as a sample step far above the tone's own slope
        let d = tmp("jumpc");
        let s = src(SigKind::Tone, 120.0);
        let params = JumpParams {
            cfg: ChaseConfig::default(),
            correct: false,
            xfade_bypass_ms: 5.0,
            xfade_voice_ms: 40.0,
            pre_s: 0.3,
            tail_s: 0.3,
        };
        let wav = d.join("o.wav");
        let mut eng = Engine::new(&opts(&wav), 44100, 2, 0).unwrap();
        run_jump_case(&mut eng, &s, None, &params, 10.0, 12.0).unwrap();
        eng.finish().unwrap();
        let o = read_wav_ch0(&wav);
        // pitch rises by up to 2x (<= 4 kHz peak at 4x): the steepest legitimate step is the 4x tone's slope
        let max_step = o
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(max_step < 0.45, "max step {max_step}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn speed_program_consumes_the_source_at_the_programmed_rates() {
        let d = tmp("speed");
        let s = src(SigKind::Music, 120.0);
        for correct in [false, true] {
            let wav = d.join(format!("o{correct}.wav"));
            let mut eng = Engine::new(&opts(&wav), 44100, 2, usize::from(correct)).unwrap();
            let rates = [0.5, 1.5, 2.0];
            let tau = 0.12;
            let secs = 3.0;
            let heard = run_speed_program(&mut eng, &s, correct, &rates, secs, tau, 4.0).unwrap();
            eng.finish().unwrap();
            assert_eq!(eng.non_finite, 0);
            assert_eq!(eng.frames_out, (rates.len() as f64 * secs * 44100.0) as u64);
            // analytic source consumption of a first-order tempo ramp
            let mut prev = 1.0;
            let mut expect = 4.0;
            for r in rates {
                expect += r * secs - (r - prev) * tau * (1.0 - (-secs / tau).exp());
                prev = r;
            }
            let got = heard / 44100.0;
            let tol = if correct { 0.15 } else { 0.01 };
            assert!(
                (got - expect).abs() <= tol,
                "correct={correct}: heard {got} vs expected {expect}"
            );
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn reverse_program_is_continuous_across_back_window_seams_and_has_the_right_pitch() {
        let d = tmp("rev");
        let wav = d.join("o.wav");
        let buf = src(SigKind::Tone, 40.0);
        let mut eng = Engine::new(&opts(&wav), 44100, 2, 0).unwrap();
        let out = run_reverse_program(
            &mut eng,
            BackSource::Memory(buf),
            44100,
            2,
            30.0 * 44100.0,
            &[-1.0],
            6.0,
            1.0,
        )
        .unwrap();
        eng.finish().unwrap();
        assert_eq!(eng.non_finite, 0);
        assert_eq!(
            out.stalls, 0,
            "a file sink waits for the worker instead of stalling"
        );
        assert!(out.fetch_ms.len() >= 6, "{} fetches", out.fetch_ms.len());
        assert!(!out.reached_start);
        // 6 s at -1x from 30 s (after a 0.12 s ramp-up): ends near 24.1 s
        let end_s = out.heard_end_frames / 44100.0;
        assert!((end_s - 24.0).abs() < 0.3, "ended at {end_s}");
        let o = read_wav_ch0(&wav);
        let mid = &o[44100 / 2..];
        let f = (990..=1010i32)
            .map(|f| (f, goertzel(mid, 44100.0, f as f64)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap()
            .0;
        assert!((f - 1000).abs() <= 3, "reverse tone at {f} Hz");
        let max_step = mid
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(
            max_step < 0.2,
            "a seam click would exceed the tone slope: {max_step}"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn reverse_stops_cleanly_at_the_start_of_the_source() {
        let d = tmp("rev0");
        let wav = d.join("o.wav");
        let buf = src(SigKind::Tone, 30.0);
        let mut eng = Engine::new(&opts(&wav), 44100, 2, 0).unwrap();
        let out = run_reverse_program(
            &mut eng,
            BackSource::Memory(buf),
            44100,
            2,
            3.0 * 44100.0,
            &[-2.0],
            10.0,
            1.0,
        )
        .unwrap();
        eng.finish().unwrap();
        assert!(out.reached_start);
        assert_eq!(eng.non_finite, 0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn file_backed_far_jump_and_reverse_use_real_decoding() {
        let d = tmp("file");
        let wavp = d.join("m.wav");
        gen::write_signal_wav(&wavp, SigKind::Music, 100.0, 44100, 2).unwrap();
        // far jump: the waypoint window is decoded from the file on a worker thread
        let o = Opts {
            file: Some(wavp.clone()),
            ..opts(&d.join("o.wav"))
        };
        let source = load_source(&o, 0.0).unwrap();
        assert_eq!(source.buf.frames(), 100 * 44100);
        let params = JumpParams {
            cfg: ChaseConfig::default(),
            correct: false,
            xfade_bypass_ms: 5.0,
            xfade_voice_ms: 40.0,
            pre_s: 0.2,
            tail_s: 0.2,
        };
        let mut eng = Engine::new(&o, 44100, 2, 0).unwrap();
        let out = run_jump_case(
            &mut eng,
            &source.buf,
            source.file.as_deref(),
            &params,
            10.0,
            70.0,
        )
        .unwrap();
        eng.finish().unwrap();
        assert!(out.far && out.waypoint_ready_ms.is_some() && !out.waypoint_late);
        assert!((out.landed_frames - out.target_frames).abs() <= params.cfg.land_eps_frames + 1.0);
        assert_eq!(eng.non_finite, 0);
        // reverse straight from the file with an Accurate seek per block
        let mut eng = Engine::new(&opts(&d.join("r.wav")), 44100, 2, 0).unwrap();
        let back = BackSource::File(Box::new(decode::open(&wavp).unwrap()));
        let r = run_reverse_program(&mut eng, back, 44100, 2, 60.0 * 44100.0, &[-1.0], 5.0, 1.0)
            .unwrap();
        eng.finish().unwrap();
        assert_eq!(r.stalls, 0);
        assert!(r.fetch_ms.len() >= 4);
        assert!((r.heard_end_frames / 44100.0 - 55.0).abs() < 0.3);
        // the reversed output equals the file read backwards: compare a slice with the in-memory source
        let o = read_wav_ch0(&d.join("r.wav"));
        let expect_at = |k: usize| q(source.buf.at(60 * 44100 - k as i64, 0));
        // after the 0.12 s ramp the rate is -1: output frame i comes from source frame (pos0 - sum of rates)
        // checked loosely: the energy envelope of the output must follow the reversed source envelope
        let blk = 4410usize;
        let (mut num, mut den_a, mut den_b) = (0.0f64, 0.0f64, 0.0f64);
        for b in 10..20 {
            let out_e: f64 = o[b * blk..(b + 1) * blk]
                .iter()
                .map(|x| (*x as f64).powi(2))
                .sum();
            let k0 = b * blk + 5300; // ramp-up consumes about 0.12 s of extra source
            let src_e: f64 = (k0..k0 + blk).map(|k| (expect_at(k) as f64).powi(2)).sum();
            num += out_e * src_e;
            den_a += out_e * out_e;
            den_b += src_e * src_e;
        }
        let corr = num / (den_a.sqrt() * den_b.sqrt());
        assert!(corr > 0.9, "reversed envelope correlation {corr}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn join_blocks_concatenates_adjacent_windows() {
        let a = PcmBuf::new(100, 1, 0, vec![1.0, 2.0, 3.0]);
        let b = PcmBuf::new(100, 1, 3, vec![4.0, 5.0]);
        let j = join_blocks(&a, &b);
        assert_eq!(
            (j.start, j.data.clone()),
            (0, vec![1.0, 2.0, 3.0, 4.0, 5.0])
        );
    }

    #[test]
    fn longer_crossfades_soften_the_join() {
        let d = tmp("xf");
        let s = src(SigKind::Tone, 60.0);
        let mut worst = Vec::new();
        for ms in [1.0, 20.0, 60.0] {
            let wav = d.join(format!("o{ms}.wav"));
            let mut eng = Engine::new(&opts(&wav), 44100, 2, 0).unwrap();
            play_with_join(
                &mut eng,
                &s,
                2.0 * 44100.0 + 13.0,
                30.0 * 44100.0 + 17.0,
                ms,
                0.5,
            )
            .unwrap();
            eng.finish().unwrap();
            let o = read_wav_ch0(&wav);
            // only look at the join region (after 0.5 s of lead-in)
            let region = &o[(0.5 * 44100.0) as usize
                ..(0.5 * 44100.0) as usize + (ms / 1000.0 * 44100.0) as usize + 50];
            worst.push(
                region
                    .windows(2)
                    .map(|w| (w[1] - w[0]).abs())
                    .fold(0.0, f32::max),
            );
        }
        assert!(
            worst[0] > worst[1] && worst[1] >= worst[2] - 1e-3,
            "steps {worst:?}"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn options_parse_and_validate() {
        let a = Args::parse(
            &[
                "--signal".to_string(),
                "sweep".to_string(),
                "--no-beep".to_string(),
                "--out".to_string(),
                "x.wav".to_string(),
                "f.mp3".to_string(),
            ],
            COMMON_VALUE,
            COMMON_FLAGS,
        )
        .unwrap();
        let o = Opts::from_args(&a).unwrap();
        assert_eq!(o.signal, SigKind::Sweep);
        assert!(!o.beep);
        assert_eq!(o.file, Some(PathBuf::from("f.mp3")));
        assert_eq!(o.sink.out, Some(PathBuf::from("x.wav")));
        assert_eq!(o.sink.tlength_ms, 80.0);
        let bad = Args::parse(
            &["--signal".to_string(), "nope".to_string()],
            COMMON_VALUE,
            COMMON_FLAGS,
        )
        .unwrap();
        assert!(Opts::from_args(&bad).is_err());
    }

    #[test]
    fn all_listening_commands_run_end_to_end_to_a_file_on_generated_signals() {
        // the CLI path: argument parsing, source generation, program, summary tables (smoke)
        let d = tmp("cli");
        let wav = d.join("o.wav");
        let out = wav.display().to_string();
        let run = |name: &str, extra: &[&str]| {
            let mut v: Vec<String> = vec!["--out".into(), out.clone(), "--no-beep".into()];
            v.extend(extra.iter().map(|s| s.to_string()));
            let (opts, _) = match name {
                "speed" => (speed_opts(), 0),
                "scan" => (scan_opts(), 0),
                "reverse" => (reverse_opts(), 0),
                "jump" => (jump_opts(), 0),
                _ => (xfade_opts(), 0),
            };
            let a = Args::parse(&v, &opts, COMMON_FLAGS).unwrap();
            match name {
                "speed" => run_speed(&a),
                "scan" => run_scan(&a),
                "reverse" => run_reverse(&a),
                "jump" => run_jump(&a),
                _ => run_xfade(&a),
            }
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        };
        run(
            "speed",
            &["--rates", "0.5,1.5", "--seconds", "1", "--mode", "correct"],
        );
        run(
            "scan",
            &[
                "--hold", "2", "--tail", "0.2", "--pre", "0.2", "--dir", "rev",
            ],
        );
        run(
            "reverse",
            &[
                "--rates",
                "-0.5,-1",
                "--seconds",
                "2",
                "--back-window-s",
                "1",
                "--start",
                "20",
            ],
        );
        run(
            "jump",
            &[
                "--dist", "0.5,8", "--pre", "0.2", "--tail", "0.2", "--mode", "correct",
            ],
        );
        run("xfade", &["--lengths-ms", "5,40", "--reps", "2"]);
        let _ = std::fs::remove_dir_all(d);
    }
}
