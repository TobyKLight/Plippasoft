mod control;
mod decode;
mod engine;
mod mouse;
mod pulse_env;
mod pulse_out;

use anyhow::{bail, Context, Result};
use control::{mode_label, warp_display, WarpControl};
use engine::{PlaybackEngine, SharedEngine};
use mouse::{ensure_mouse_available, find_usb_mice, pick_default_mouse, print_mouse_list, MouseInfo};
use pulse_env::{apply_pulse_env, pulse_session_available, pulse_session_label};
use pulse_out::PulseOutput;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const OUTPUT_SAMPLE_RATE: u32 = 48_000;

struct Args {
    mp3_path: Option<PathBuf>,
    verbose: bool,
    list_only: bool,
    device_index: Option<usize>,
    pulse_server: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut mp3_path = None;
    let mut verbose = false;
    let mut list_only = false;
    let mut device_index = None;
    let mut pulse_server = None;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--verbose" | "-v" => verbose = true,
            "--list" => list_only = true,
            "--device" => {
                let value = args
                    .next()
                    .context("--device requires an index (from --list)")?;
                device_index = Some(
                    value
                        .parse()
                        .with_context(|| format!("invalid device index '{value}'"))?,
                );
            }
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
            other if other.starts_with('-') => bail!("unknown option: {other}"),
            other => {
                if mp3_path.is_some() {
                    bail!("unexpected argument: {other}");
                }
                mp3_path = Some(PathBuf::from(other));
            }
        }
    }

    Ok(Args {
        mp3_path,
        verbose,
        list_only,
        device_index,
        pulse_server,
    })
}

fn print_usage() {
    eprintln!(
        "usage: poc2_audio_test_warp_mouse [OPTIONS] <track.mp3>\n\
         \n\
         Mouse controls:\n\
           X move     adjust warp % (left=slower, right=faster)\n\
           left click toggle pitch-preserved / vinyl mode\n\
           right click reset warp to 0% (100% speed)\n\
         \n\
         Options:\n\
           --verbose, -v       log warp/mode changes\n\
           --list              list USB mice and exit\n\
           --device N          mouse index from --list\n\
           --pulse-server ADDR override Pulse socket"
    );
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let mice = find_usb_mice().context("enumerate USB mice")?;
    ensure_mouse_available(&mice)?;

    let mouse_index = args
        .device_index
        .unwrap_or_else(|| pick_default_mouse(&mice));
    print_mouse_list(&mice, Some(mouse_index));

    if args.list_only {
        return Ok(());
    }

    let mp3_path = args
        .mp3_path
        .context("missing MP3 path argument (or pass --list)")?;
    let mouse = mice
        .get(mouse_index)
        .with_context(|| format!("device index {mouse_index} out of range ({} found)", mice.len()))?
        .clone();

    apply_pulse_env(args.pulse_server.as_deref());
    if !pulse_session_available() {
        eprintln!(
            "warning: no Pulse session found ({}) — the system audio stack is retired; start the user-session PipeWire server",
            pulse_session_label()
        );
    } else {
        println!("Pulse session: {}", pulse_session_label());
    }

    println!("Decoding {}…", mp3_path.display());
    let decoded = decode::decode_mp3(&mp3_path)?.with_silence_padding(0.5);
    println!(
        "Decoded {:.1}s @ {} Hz, {} channel(s)",
        decoded.duration_s(),
        decoded.sample_rate,
        decoded.channels
    );

    println!("Output: Pulse default sink @ {OUTPUT_SAMPLE_RATE} Hz (F32)");
    print_controls_help(&mouse);

    let control = WarpControl::new();
    let playback_engine = PlaybackEngine::new(
        decoded.samples,
        decoded.channels,
        decoded.sample_rate,
        OUTPUT_SAMPLE_RATE,
        Arc::clone(&control),
    )?;
    let shared = SharedEngine::new(playback_engine);

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = Arc::clone(&running);
        let stop = Arc::clone(&shared);
        ctrlc::set_handler(move || {
            running.store(false, Ordering::SeqCst);
            stop.request_stop();
        })
        .context("install Ctrl+C handler")?;
    }

    if args.verbose {
        spawn_status_logger(Arc::clone(&control), Arc::clone(&running));
    }

    let mouse_control = Arc::clone(&control);
    let mouse_running = Arc::clone(&running);
    let stop_on_mouse_error = Arc::clone(&running);
    let stop_audio_on_mouse_error = Arc::clone(&shared);
    let mouse_thread = thread::spawn(move || {
        if let Err(err) = mouse::run_loop(mouse_control, mouse_running, &mouse) {
            eprintln!("mouse input stopped: {err:#}");
            stop_on_mouse_error.store(false, Ordering::SeqCst);
            stop_audio_on_mouse_error.request_stop();
        }
    });

    let mut pulse = PulseOutput::new(decoded.channels, OUTPUT_SAMPLE_RATE)?;
    let audio_result = pulse.play_loop(&shared);

    running.store(false, Ordering::SeqCst);
    let _ = mouse_thread.join();

    audio_result?;
    println!("Stopped");
    Ok(())
}

fn print_controls_help(mouse: &MouseInfo) {
    println!();
    println!("Mouse [{}] {} — controls:", mouse.hidraw_path.display(), mouse.name);
    println!("  move X       warp slower / faster");
    println!("  left click   toggle pitch-preserved / vinyl");
    println!("  right click  reset warp to 0%");
    println!("  Ctrl+C       quit");
    println!();
}

fn spawn_status_logger(control: Arc<WarpControl>, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut last_warp = i32::MIN;
        let mut last_mode = None;
        while running.load(Ordering::SeqCst) {
            let snapshot = control.snapshot();
            if snapshot.warp_pct != last_warp || Some(snapshot.mode) != last_mode {
                eprintln!(
                    "{} | mode: {}",
                    warp_display(snapshot.warp_pct),
                    mode_label(snapshot.mode)
                );
                last_warp = snapshot.warp_pct;
                last_mode = Some(snapshot.mode);
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
}
