mod decode;
mod engine;
mod pulse_env;
mod pulse_out;
mod timeline;

use anyhow::{bail, Context, Result};
use engine::{PlaybackEngine, SharedEngine};
use pulse_env::{apply_pulse_env, pulse_session_available, pulse_session_label};
use pulse_out::PulseOutput;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use timeline::{phase_index_at, TOTAL_DURATION_S};

/// Pulse sinks on this stack are typically 44.1 kHz or 48 kHz; we render at 48 kHz
/// and let PipeWire resample to the active sink if needed.
const OUTPUT_SAMPLE_RATE: u32 = 48_000;

struct Args {
    mp3_path: PathBuf,
    verbose: bool,
    pulse_server: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut mp3_path = None;
    let mut verbose = false;
    let mut pulse_server = None;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--verbose" | "-v" => verbose = true,
            "--pulse-server" => {
                let value = args
                    .next()
                    .context("--pulse-server requires a value")?;
                pulse_server = Some(value);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other if other.starts_with('-') => {
                bail!("unknown option: {other}");
            }
            other => {
                if mp3_path.is_some() {
                    bail!("unexpected argument: {other}");
                }
                mp3_path = Some(PathBuf::from(other));
            }
        }
    }

    let mp3_path = mp3_path.context("missing MP3 path argument")?;
    Ok(Args {
        mp3_path,
        verbose,
        pulse_server,
    })
}

fn print_usage() {
    eprintln!(
        "usage: poc2_audio_test_warp_pulse [--verbose] [--pulse-server ADDR] <track.mp3>"
    );
}

fn main() -> Result<()> {
    let args = parse_args()?;

    apply_pulse_env(args.pulse_server.as_deref());
    if !pulse_session_available() {
        eprintln!(
            "warning: no Pulse session found ({}) — the system audio stack is retired; start the user-session PipeWire server",
            pulse_session_label()
        );
    } else {
        println!("Pulse session: {}", pulse_session_label());
    }

    println!("Decoding {}…", args.mp3_path.display());
    let decoded = decode::decode_mp3(&args.mp3_path)?
        .with_silence_padding(0.5);
    println!(
        "Decoded {:.1}s @ {} Hz, {} channel(s)",
        decoded.duration_s(),
        decoded.sample_rate,
        decoded.channels
    );

    if decoded.duration_s() < 25.0 {
        eprintln!(
            "warning: track is shorter than recommended 30 s — playback may run out of source audio"
        );
    }

    println!("Output: Pulse default sink @ {OUTPUT_SAMPLE_RATE} Hz (F32)");
    println!("Playing scripted timeline ({TOTAL_DURATION_S:.0}s)…");

    let playback_engine = PlaybackEngine::new(
        decoded.samples,
        decoded.channels,
        decoded.sample_rate,
        OUTPUT_SAMPLE_RATE,
    )?;
    let shared = SharedEngine::new(playback_engine);

    if args.verbose {
        let phase_tracker = Arc::new(AtomicUsize::new(usize::MAX));
        let engine_for_log = Arc::clone(&shared);
        let tracker = Arc::clone(&phase_tracker);
        thread::spawn(move || {
            while !engine_for_log.should_stop() && !engine_for_log.is_finished() {
                let time_s = engine_for_log.output_time_s();
                if let Some(index) = phase_index_at(time_s) {
                    if tracker.swap(index, Ordering::SeqCst) != index {
                        let phase = timeline::all_phases()[index];
                        eprintln!("[{:.2}s] {}", phase.start_s, phase.label);
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
    }

    let stop_flag = Arc::clone(&shared);
    ctrlc::set_handler(move || {
        stop_flag.request_stop();
    })
    .context("install Ctrl+C handler")?;

    let mut pulse = PulseOutput::new(decoded.channels, OUTPUT_SAMPLE_RATE)?;
    pulse.play_loop(&shared)?;

    println!("Playback finished");
    Ok(())
}
