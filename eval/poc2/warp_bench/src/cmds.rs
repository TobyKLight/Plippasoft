//! Sub-commands that measure (everything except the listening tests): argument handling, tables, report
//! sections and the `key = value` data lines `suggest` reads.

use crate::args::Args;
use crate::report::{key_fragment, parse_results, short_name, Data, Report, Table};
use crate::{cpu, decode, gen, info, latency, seek, suggest, usbread, voices};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const MAIN_HELP: &str = "\
warp_bench: WARP eval app for the Plippa CM4 (measurements and listening tests). Not production code.
Run it from a RELEASE build on the CM4 with the product stopped; read eval/poc2/warp_bench/README.md first.

Measurements (print a table and append a section to warp_bench_<date>.txt in the current directory):
  info                      CPU, governor, frequency, temperature, throttling, kernel, Pulse
  decode <file...>          Symphonia decode speed (x realtime, median/min of 5 runs)
  usbread <file>            sequential MB/s and random 4 KiB read latency of the file's device
  seek <file...>            forward/backward Accurate seek cost at 10/50/90 % (needs files >= 20 s)
  latency                   Pulse Simple::get_latency + underruns for tlength 60..240 ms (pulse build)
  cpu                       CPU % of one core for varispeed, boxcar, stretcher; p99 256-frame block time
  all <file...>             info, decode, usbread, seek, latency, cpu in order, then suggest
  suggest [results.txt]     turn the measured numbers into suggested constants (pure arithmetic)

Listening (play through Pulse; --out file.wav renders to a file instead):
  listen speed [file]       constant speeds 10/25/75/125/150/200 %, --mode shift|correct
  listen scan [file]        FF/RW hold: spin-up, boost, release, brake
  listen reverse <file>     backwards from back-windows (Accurate seek per block, no index)
  listen jump [file]        jog commit swoop forward/backward, near and far (waypoint)
  listen xfade [file]       crossfade lengths over tone and music

Other:
  gen tone|chirp|clicks|sweep <out.wav>   synthetic test signals
  sheet [--append]          print (or append to the report) the blank listening sheet

Every sub-command accepts --help. Measurement sub-commands accept --report <file> (default warp_bench_<date>.txt).
";

fn report_for(a: &Args) -> Report {
    Report::open(a.get("report"))
}

fn finish_section(rep: &Report, name: &str, table: &Table, data: &Data) -> Result<()> {
    let body = format!("{}\n{}", table.render(), data.render());
    rep.append(name, &body)?;
    println!(
        "(appended section '== {name} ==' to {})",
        rep.path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------- info

pub const INFO_HELP: &str = "warp_bench info [--report FILE]\nRecords what the board is doing: CPU model, governor, frequency, temperature, `vcgencmd get_throttled`, kernel, memory, Pulse server and default sink, and whether this is a release build.\n";

pub fn cmd_info(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["report"], &[])?;
    if a.help() {
        print!("{INFO_HELP}");
        return Ok(());
    }
    let items = info::collect();
    let mut t = Table::new(&["item", "value"]);
    let mut d = Data::default();
    for (k, v) in &items {
        t.row(vec![k.clone(), v.clone()]);
        d.push(&format!("info.{k}"), v);
    }
    println!("{}", t.render());
    let get = |k: &str| {
        items
            .iter()
            .find(|(ik, _)| ik == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or("n/a")
    };
    if get("build_profile") == "debug" {
        println!("WARNING: this is a DEBUG build: every performance number is invalid. Use `PROFILE=release scripts/deploy.sh eval warp_bench`.");
    }
    match get("throttled") {
        "0x0" => {}
        "n/a" => println!("note: vcgencmd not available: cannot verify throttling"),
        t => println!("WARNING: throttled = {t}: under-voltage / thermal limit seen. The run is INVALID: cool down, check the supply, repeat."),
    }
    if let Ok(temp) = get("temp_c").parse::<f64>() {
        if temp > 70.0 {
            println!("WARNING: temperature {temp:.0} C: let the board cool down before measuring.");
        }
    }
    finish_section(&report_for(&a), "info", &t, &d)
}

// ---------------------------------------------------------------- decode

pub const DECODE_HELP: &str = "warp_bench decode <file...> [--seconds 120] [--runs 5] [--report FILE]\nDecodes each file (the whole file or the first --seconds) --runs times with Symphonia and reports x realtime (audio seconds per wall second): median and minimum. Give one file per format and sample rate the product plays (MP3, FLAC, WAV). The worst median over all files is D in the mapping table.\n";

pub fn cmd_decode(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["seconds", "runs", "report"], &[])?;
    if a.help() {
        print!("{DECODE_HELP}");
        return Ok(());
    }
    if a.positional.is_empty() {
        bail!("decode needs at least one media file (see --help)");
    }
    run_decode(
        &a.positional,
        a.f64("seconds", 120.0)?,
        a.usize("runs", 5)?,
        &report_for(&a),
    )
}

pub fn run_decode(files: &[String], seconds: f64, runs: usize, rep: &Report) -> Result<()> {
    let mut t = Table::new(&[
        "file", "codec", "rate", "ch", "audio s", "median x", "min x", "runs",
    ]);
    let mut d = Data::default();
    let (mut worst, mut max_bps) = (f64::INFINITY, 0.0f64);
    for f in files {
        println!("decoding {f} ...");
        let r = decode::measure(Path::new(f), seconds, runs)?;
        let name = Path::new(f)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(f);
        let frag = key_fragment(name);
        t.row(vec![
            short_name(name, 40),
            r.codec.clone(),
            r.rate.to_string(),
            r.ch.to_string(),
            format!("{:.0}", r.audio_s),
            format!("{:.1}", r.median_x()),
            format!("{:.1}", r.min_x()),
            r.runs_x
                .iter()
                .map(|x| format!("{x:.0}"))
                .collect::<Vec<_>>()
                .join("/"),
        ]);
        d.push_f(&format!("decode.{frag}.median_x"), r.median_x(), 1);
        d.push_f(&format!("decode.{frag}.min_x"), r.min_x(), 1);
        worst = worst.min(r.median_x());
        max_bps = max_bps.max(r.rate as f64 * r.ch as f64 * 4.0);
    }
    d.push_f("decode.worst_median_x", worst, 1);
    d.push_f("decode.max_frame_bytes_per_s", max_bps, 0);
    println!("\n{}", t.render());
    println!("D (worst median) = {worst:.1}x realtime. Good: > 16 (allows 8x scan with the 0.5 safety factor). Worrying: < 8.");
    finish_section(rep, "decode", &t, &d)
}

// ---------------------------------------------------------------- usbread

pub const USBREAD_HELP: &str = "warp_bench usbread <file> [--mb 512] [--reads 400] [--report FILE]\nDrop the caches first: sync; echo 3 | sudo tee /proc/sys/vm/drop_caches (the app also asks the kernel to drop the file's pages). Reads up to --mb MB sequentially (MB/s), then --reads random 4 KiB blocks (latency median/p95/max). Derives the estimated MP3 index build time (size / MB/s x 1.5) and the estimated indexed-seek cost (p95 random read + decode of 3 frames). Use a LARGE file on the USB stick (the 1-hour MP3).\n";

pub fn cmd_usbread(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["mb", "reads", "report"], &[])?;
    if a.help() {
        print!("{USBREAD_HELP}");
        return Ok(());
    }
    let Some(file) = a.positional.first() else {
        bail!("usbread needs a file (see --help)")
    };
    run_usbread(
        file,
        a.f64("mb", 512.0)?,
        a.usize("reads", 400)?,
        &report_for(&a),
    )
}

pub fn run_usbread(file: &str, mb: f64, reads: usize, rep: &Report) -> Result<()> {
    println!("reading {file} (sequential up to {mb} MB, then {reads} random 4 KiB reads) ...");
    let r = usbread::measure(Path::new(file), mb, reads)?;
    let dec = decode::time_per_packet(Path::new(file), 300);
    let (dec_s, dec_note) = match dec {
        Ok(s) => (s, String::new()),
        Err(e) => (
            0.003,
            format!("(could not decode the file ({e}); using 3 ms per frame)"),
        ),
    };
    let index_s = usbread::index_build_estimate_s(r.size_bytes, r.seq_mb_s);
    let seek_s = usbread::indexed_seek_estimate_s(r.rand_p95_ms, dec_s);
    let mut t = Table::new(&["measure", "value", "good / worrying"]);
    t.row(vec![
        "file size (MB)".to_string(),
        format!("{:.0}", r.size_bytes as f64 / 1e6),
        String::new(),
    ]);
    t.row(vec![
        "sequential bytes read (MB)".to_string(),
        format!("{:.0}", r.seq_bytes as f64 / 1e6),
        String::new(),
    ]);
    t.row(vec![
        "random reads".to_string(),
        r.reads.to_string(),
        String::new(),
    ]);
    t.row(vec![
        "sequential read (MB/s)".to_string(),
        format!("{:.1}", r.seq_mb_s),
        "good > 30; worrying < 10".to_string(),
    ]);
    t.row(vec![
        "random 4 KiB median (ms)".to_string(),
        format!("{:.2}", r.rand_median_ms),
        "good < 2; worrying > 10".to_string(),
    ]);
    t.row(vec![
        "random 4 KiB p95 (ms)".to_string(),
        format!("{:.2}", r.rand_p95_ms),
        "good < 10; worrying > 50".to_string(),
    ]);
    t.row(vec![
        "random 4 KiB max (ms)".to_string(),
        format!("{:.2}", r.rand_max_ms),
        String::new(),
    ]);
    t.row(vec![
        "decode per frame (ms)".to_string(),
        format!("{:.3}", dec_s * 1000.0),
        String::new(),
    ]);
    t.row(vec![
        "est. MP3 index build time B (s)".to_string(),
        format!("{index_s:.1}"),
        "good < 20; worrying > 20".to_string(),
    ]);
    t.row(vec![
        "est. indexed-seek cost S_i (ms)".to_string(),
        format!("{:.0}", seek_s * 1000.0),
        "good < 100; worrying > 300".to_string(),
    ]);
    let mut d = Data::default();
    d.push_f("usbread.size_mb", r.size_bytes as f64 / 1e6, 0);
    d.push_f("usbread.seq_mb_s", r.seq_mb_s, 1);
    d.push_f("usbread.rand_median_ms", r.rand_median_ms, 2);
    d.push_f("usbread.rand_p95_ms", r.rand_p95_ms, 2);
    d.push_f("usbread.rand_max_ms", r.rand_max_ms, 2);
    d.push_f("usbread.decode_frame_ms", dec_s * 1000.0, 3);
    d.push_f("usbread.index_build_s", index_s, 1);
    d.push_f("usbread.indexed_seek_s", seek_s, 3);
    println!("\n{}{dec_note}", t.render());
    finish_section(rep, "usbread", &t, &d)
}

// ---------------------------------------------------------------- seek

pub const SEEK_HELP: &str = "warp_bench seek <file...> [--reps 5] [--report FILE]\nForward and backward Accurate seek at 10 %, 50 % and 90 % of each file, --reps timed seeks each: wall time (median, max), bytes the demuxer read, seek calls, and the landing offset (frames the decoder lands before the requested frame). Run it on the 1-hour MP3 (the worst case before the MP3 index exists) and on a FLAC and a WAV. Drop the caches first (the app also drops each file's pages before every seek).\n";

pub fn cmd_seek(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["reps", "report"], &[])?;
    if a.help() {
        print!("{SEEK_HELP}");
        return Ok(());
    }
    if a.positional.is_empty() {
        bail!("seek needs at least one media file (see --help)");
    }
    run_seek(&a.positional, a.usize("reps", 5)?, &report_for(&a))
}

pub fn run_seek(files: &[String], reps: usize, rep: &Report) -> Result<()> {
    let mut t = Table::new(&[
        "file",
        "dir",
        "at",
        "median (ms)",
        "max (ms)",
        "MiB read",
        "seeks",
        "landing offset (frames)",
    ]);
    let mut d = Data::default();
    let (mut worst_back, mut worst_fwd) = (0.0f64, 0.0f64);
    for f in files {
        println!("seeking in {f} ...");
        let name = Path::new(f)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(f)
            .to_string();
        let frag = key_fragment(&name);
        let (dur, cases) = seek::run_cases(Path::new(f), reps, |l| println!("{l}"))?;
        for c in &cases {
            t.row(vec![
                format!("{} ({:.0} s)", short_name(&name, 40), dur),
                c.dir.name().to_string(),
                format!("{}%", c.pct),
                format!("{:.0}", c.median_s() * 1000.0),
                format!("{:.0}", c.max_s() * 1000.0),
                format!("{:.1}", c.median_bytes() / 1048576.0),
                c.max_seeks().to_string(),
                c.max_landing_offset().to_string(),
            ]);
            let key = format!("seek.{frag}.{}_{}", c.dir.name(), c.pct);
            d.push_f(&format!("{key}.median_s"), c.median_s(), 3);
            d.push_f(&format!("{key}.max_s"), c.max_s(), 3);
            match c.dir {
                seek::Dir::Backward => worst_back = worst_back.max(c.median_s()),
                seek::Dir::Forward => worst_fwd = worst_fwd.max(c.median_s()),
            }
        }
    }
    d.push_f("seek.worst_backward_accurate_s", worst_back, 3);
    d.push_f("seek.worst_forward_s", worst_fwd, 3);
    println!("\n{}", t.render());
    println!("S_a (worst backward median) = {:.2} s. Good: < 0.3 s. Worrying: > 2 s (typical for a long MP3 without an index: the demuxer restarts from byte 0).", worst_back);
    finish_section(rep, "seek", &t, &d)
}

// ---------------------------------------------------------------- latency

pub const LATENCY_HELP: &str = "warp_bench latency [--rate 48000] [--load 0] [--tlengths 60,80,120,160,240] [--report FILE]\nNeeds the pulse build. For each tlength (minreq = tlength / 4) writes 10 s (1 s silence, 8 s quiet tone, 1 s silence) to the real sink and reports Simple::get_latency (min/avg/max, ms) and the underrun count (detected from the latency, see README). --load N runs N busy threads meanwhile to emulate a loaded board.\n";

pub fn cmd_latency(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["rate", "load", "tlengths", "report"], &[])?;
    if a.help() {
        print!("{LATENCY_HELP}");
        return Ok(());
    }
    let tl = a.f64_list("tlengths", &latency::TLENGTHS_MS)?;
    run_latency(
        &tl,
        a.usize("rate", 48000)? as u32,
        a.usize("load", 0)?,
        &report_for(&a),
    )
}

pub fn run_latency(tlengths: &[f64], rate: u32, load: usize, rep: &Report) -> Result<()> {
    let mut t = Table::new(&[
        "tlength (ms)",
        "minreq (ms)",
        "latency min",
        "avg",
        "max (ms)",
        "underruns",
        "samples",
    ]);
    let mut d = Data::default();
    for &tl in tlengths {
        println!("tlength {tl} ms: playing {} s ...", latency::RUN_S);
        let r = latency::run(tl, rate, load)?;
        t.row(vec![
            format!("{tl:.0}"),
            format!("{:.0}", tl / 4.0),
            format!("{:.1}", r.min_ms),
            format!("{:.1}", r.avg_ms),
            format!("{:.1}", r.max_ms),
            r.underruns.to_string(),
            r.samples.to_string(),
        ]);
        let k = format!("latency.tl_{tl:.0}");
        d.push_f(&format!("{k}.min_ms"), r.min_ms, 1);
        d.push_f(&format!("{k}.avg_ms"), r.avg_ms, 1);
        d.push_f(&format!("{k}.max_ms"), r.max_ms, 1);
        d.push(&format!("{k}.underruns"), r.underruns);
    }
    d.push("latency.load_threads", load);
    println!("\n{}", t.render());
    println!("Use the smallest tlength with 0 underruns; its 'avg' is the SINK_LATENCY_MS value.");
    finish_section(rep, "latency", &t, &d)
}

// ---------------------------------------------------------------- cpu

pub const CPU_HELP: &str = "warp_bench cpu [--seconds 20] [--rate 44100] [--only TEXT] [--report FILE]\nRenders --seconds of audio offline through each voice and reports the process CPU time as % of one core, plus the p99 and max time of one 256-frame block (the audio thread's deadline is 256/rate = 5.8 ms at 44.1 kHz). Varispeed sinc at 0.25 0.5 1.5 2.0 -1.0; boxcar+cubic at 4 8; stretcher at input rates 0.5 1.0 1.5 2.0 3.0 4.0 as speed voice and as seek voice (rate wobbles, re-primed every 2 s), plus the cheaper preset at 0.5 1.0 2.0. --only keeps the cases whose key contains TEXT (e.g. stretch_seek). Takes 2-5 minutes. Use a RELEASE build with the product stopped.\n";

pub fn cmd_cpu(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["seconds", "rate", "only", "report"], &[])?;
    if a.help() {
        print!("{CPU_HELP}");
        return Ok(());
    }
    run_cpu(
        a.f64("seconds", 20.0)?,
        a.usize("rate", 44100)? as u32,
        a.get("only"),
        &report_for(&a),
    )
}

pub fn run_cpu(seconds: f64, rate: u32, only: Option<&str>, rep: &Report) -> Result<()> {
    let cases: Vec<cpu::Case> = cpu::default_cases()
        .into_iter()
        .filter(|c| only.is_none_or(|o| c.key().contains(o)))
        .collect();
    if cases.is_empty() {
        bail!("--only matched no case");
    }
    let need = cpu::source_needed_s(&cases, seconds);
    println!("generating {need:.0} s of source audio ...");
    let mono = gen::generate(gen::Kind::Music, need, rate);
    let src = voices::PcmBuf::new(rate, 2, 0, gen::to_channels(&mono, 2));
    let deadline_us = voices::QUANTUM as f64 / rate as f64 * 1e6;
    let mut t = Table::new(&[
        "voice",
        "rate",
        "CPU % of one core",
        "p99 block (us)",
        "p99 % of deadline",
        "max block (us)",
    ]);
    let mut d = Data::default();
    for c in cases {
        println!("  {} at {} ...", c.label(), cpu::fmt_rate(c.rate));
        let r = cpu::run_case(c, seconds, &src)?;
        t.row(vec![
            c.label(),
            cpu::fmt_rate(c.rate),
            format!("{:.1}", r.cpu_pct),
            format!("{:.0}", r.p99_block_us),
            format!("{:.0}", r.p99_block_us / deadline_us * 100.0),
            format!("{:.0}", r.max_block_us),
        ]);
        d.push_f(&c.key(), r.cpu_pct, 1);
        d.push_f(
            &c.key().replacen("cpu.", "cpu.p99_us.", 1),
            r.p99_block_us,
            0,
        );
    }
    d.push_f("cpu.block_deadline_us", deadline_us, 0);
    println!("\n{}", t.render());
    println!("Budget: stretcher alone <= {:.0} % of one core; a p99 block time near the deadline means dropouts.", suggest::STRETCH_BUDGET_PCT);
    finish_section(rep, "cpu", &t, &d)
}

// ---------------------------------------------------------------- suggest

pub const SUGGEST_HELP: &str = "warp_bench suggest [results.txt]\nReads the `key = value` lines of a results file (default: the newest warp_bench_*.txt in the current directory), applies the mapping table of the README and prints the suggested constant values, with flags for throttled/invalid runs, stretcher over budget, decode too slow and 'no back-window size satisfies the rule'. Appends a '== suggest ==' section to that file. Pure arithmetic: it measures nothing.\n";

pub fn newest_report(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let name = p.file_name()?.to_str()?.to_string();
        if name.starts_with("warp_bench_")
            && name.ends_with(".txt")
            && best.as_ref().is_none_or(|b| p > *b)
        {
            best = Some(p);
        }
    }
    best
}

pub fn cmd_suggest(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &[], &[])?;
    if a.help() {
        print!("{SUGGEST_HELP}");
        return Ok(());
    }
    let path = match a.positional.first() {
        Some(p) => PathBuf::from(p),
        None => newest_report(Path::new("."))
            .context("no warp_bench_*.txt in the current directory; pass a results file")?,
    };
    run_suggest(&path)
}

pub fn run_suggest(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let res = suggest::suggest(&parse_results(&text));
    let body = suggest::render(&res);
    println!("{body}");
    let rep = Report::open(path.to_str());
    rep.append("suggest", &body)?;
    rep.ensure_listening_sheet()?;
    println!(
        "(appended section '== suggest ==' and the listening sheet to {})",
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------- all

pub const ALL_HELP: &str = "warp_bench all <media file...> [--quick] [--report FILE]\nRuns info, decode (all files), usbread (the largest file), seek (all files), latency (pulse build), cpu in order, then suggest, and appends the blank listening sheet. Give the files from the README test media list (3-minute MP3, FLAC, WAV, 1-hour MP3) on the USB stick. --quick shortens everything (3 decode runs, 2 seek reps, 8 s of cpu audio): for a smoke test, not for numbers. About 30 minutes in full.\n";

pub fn cmd_all(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["report"], &["quick"])?;
    if a.help() {
        print!("{ALL_HELP}");
        return Ok(());
    }
    if a.positional.is_empty() {
        bail!("all needs the media files (see --help)");
    }
    let quick = a.flag("quick");
    let rep = report_for(&a);
    println!("=== info ===");
    cmd_info(&["--report".into(), rep.path.display().to_string()])?;
    println!("\n=== decode ===");
    run_decode(&a.positional, 120.0, if quick { 3 } else { 5 }, &rep)?;
    let largest = a
        .positional
        .iter()
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .cloned()
        .unwrap();
    println!("\n=== usbread ({largest}) ===");
    run_usbread(
        &largest,
        if quick { 64.0 } else { 512.0 },
        if quick { 100 } else { 400 },
        &rep,
    )?;
    println!("\n=== seek ===");
    run_seek(&a.positional, if quick { 2 } else { 5 }, &rep)?;
    println!("\n=== latency ===");
    if cfg!(feature = "pulse") {
        run_latency(&latency::TLENGTHS_MS, 48000, 0, &rep)?;
    } else {
        println!("skipped: this build has no Pulse support (rebuild with --features pulse)");
    }
    println!("\n=== cpu ===");
    run_cpu(if quick { 8.0 } else { 20.0 }, 44100, None, &rep)?;
    println!("\n=== suggest ===");
    run_suggest(&rep.path)
}

// ---------------------------------------------------------------- gen, sheet

pub const GEN_HELP: &str = "warp_bench gen tone|chirp|clicks|sweep|music <out.wav> [--seconds 10] [--rate 44100] [--channels 2]\nWrites a synthetic test signal as 16-bit WAV: tone = 1 kHz sine; chirp = log chirp 100 Hz-18 kHz (repeats every 10 s); clicks = click train every 100 ms; sweep = stepped sine sweep 100 Hz-16 kHz; music = deterministic music-like material.\n";

pub fn cmd_gen(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["seconds", "rate", "channels"], &[])?;
    if a.help() {
        print!("{GEN_HELP}");
        return Ok(());
    }
    if a.positional.len() != 2 {
        bail!("usage: warp_bench gen tone|chirp|clicks|sweep|music <out.wav> (see --help)");
    }
    let kind = gen::Kind::parse(&a.positional[0])?;
    let out = Path::new(&a.positional[1]);
    gen::write_signal_wav(
        out,
        kind,
        a.f64("seconds", 10.0)?,
        a.usize("rate", 44100)? as u32,
        a.usize("channels", 2)? as u16,
    )?;
    println!("wrote {}", out.display());
    Ok(())
}

pub fn cmd_sheet(argv: &[String]) -> Result<()> {
    let a = Args::parse(argv, &["report"], &["append"])?;
    if a.help() {
        println!("warp_bench sheet [--append] [--report FILE]\nPrints the blank listening sheet; --append adds it as the trailing '== listening ==' section of the report.");
        return Ok(());
    }
    print!("{}", crate::report::listening_sheet());
    if a.flag("append") {
        let rep = report_for(&a);
        rep.ensure_listening_sheet()?;
        println!("(listening sheet is in {})", rep.path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warp_bench_cmds_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn newest_report_picks_the_latest_date() {
        let d = tmpdir("newest");
        for n in [
            "warp_bench_2026-01-02.txt",
            "warp_bench_2026-03-04.txt",
            "other.txt",
            "warp_bench_2025-12-31.txt",
        ] {
            std::fs::write(d.join(n), "x").unwrap();
        }
        assert_eq!(
            newest_report(&d).unwrap().file_name().unwrap(),
            "warp_bench_2026-03-04.txt"
        );
        let e = tmpdir("empty");
        assert!(newest_report(&e).is_none());
        let _ = std::fs::remove_dir_all(d);
        let _ = std::fs::remove_dir_all(e);
    }

    #[test]
    fn decode_command_writes_a_parseable_section() {
        let d = tmpdir("dec");
        let wav = d.join("t.wav");
        gen::write_signal_wav(&wav, gen::Kind::Tone, 3.0, 44100, 2).unwrap();
        let rep = Report::open(Some(d.join("r.txt").to_str().unwrap()));
        run_decode(&[wav.display().to_string()], 2.0, 2, &rep).unwrap();
        let text = std::fs::read_to_string(&rep.path).unwrap();
        assert!(text.starts_with("== decode =="));
        let m = parse_results(&text);
        assert!(m["decode.worst_median_x"].parse::<f64>().unwrap() > 0.0);
        assert_eq!(m["decode.max_frame_bytes_per_s"], "352800");
        assert!(m.contains_key("decode.t_wav.median_x"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn usbread_seek_cpu_info_suggest_all_run_and_chain_into_suggest() {
        // smoke: structure only; every number here comes from the cloud box and is meaningless
        let d = tmpdir("chain");
        let wav = d.join("long.wav");
        gen::write_signal_wav(&wav, gen::Kind::Music, 40.0, 44100, 2).unwrap();
        let rep = Report::open(Some(d.join("r.txt").to_str().unwrap()));
        let file = wav.display().to_string();
        cmd_info(&["--report".into(), rep.path.display().to_string()]).unwrap();
        run_decode(std::slice::from_ref(&file), 10.0, 2, &rep).unwrap();
        run_usbread(&file, 1.0, 20, &rep).unwrap();
        run_seek(std::slice::from_ref(&file), 1, &rep).unwrap();
        run_cpu(1.0, 44100, Some("cpu.stretch_seek.2.0"), &rep).unwrap();
        run_suggest(&rep.path).unwrap();
        let text = std::fs::read_to_string(&rep.path).unwrap();
        for sec in [
            "== info ==",
            "== decode ==",
            "== usbread ==",
            "== seek ==",
            "== cpu ==",
            "== suggest ==",
        ] {
            assert!(text.contains(sec), "missing {sec}");
        }
        assert!(
            text.trim_end().ends_with("verdict:"),
            "listening sheet is the last section"
        );
        let m = parse_results(&text);
        assert!(m.contains_key("seek.worst_backward_accurate_s"));
        assert!(m.contains_key("usbread.indexed_seek_s"));
        assert!(m.contains_key("suggest.valid"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn gen_command_validates_arguments() {
        let d = tmpdir("gen");
        let out = d.join("o.wav");
        cmd_gen(&[
            "chirp".into(),
            out.display().to_string(),
            "--seconds".into(),
            "1".into(),
        ])
        .unwrap();
        assert!(out.exists());
        assert!(cmd_gen(&["chirp".into()]).is_err());
        assert!(cmd_gen(&["nope".into(), out.display().to_string()]).is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn missing_arguments_are_errors_not_panics() {
        assert!(cmd_decode(&[]).is_err());
        assert!(cmd_usbread(&[]).is_err());
        assert!(cmd_seek(&[]).is_err());
        assert!(cmd_all(&[]).is_err());
        assert!(cmd_cpu(&["--only".into(), "zzz".into()]).is_err());
        #[cfg(not(feature = "pulse"))]
        assert!(cmd_latency(&[]).is_err());
    }

    #[test]
    fn every_command_prints_help_without_side_effects() {
        for f in [
            cmd_info,
            cmd_decode,
            cmd_usbread,
            cmd_seek,
            cmd_latency,
            cmd_cpu,
            cmd_suggest,
            cmd_all,
            cmd_gen,
            cmd_sheet,
        ] {
            f(&["--help".to_string()]).unwrap();
        }
    }
}
