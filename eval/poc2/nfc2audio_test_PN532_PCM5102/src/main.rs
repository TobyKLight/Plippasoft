mod media_usb;
mod ndef_parse;
mod nfc;
mod playback;
mod rpi_i2c;

use media_usb::{find_mp3, resolve_media_root};
use nfc::{format_uid, NfcReader};
use playback::{detect_pcm5102_card, pulse_session_available, AudioOutput, PlaybackManager};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (card_index, media_dir, audio_output) = parse_args()?;

    ctrlc::set_handler(|| RUNNING.store(false, Ordering::SeqCst))
        .map_err(|e| format!("ctrl-c handler: {e}"))?;

    let media_root = resolve_media_root(media_dir.as_deref())?;
    println!("USB media root: {}", media_root.display());

    let sfx_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../assets/plippaSFX");
    if !sfx_dir.is_dir() {
        return Err(format!("SFX dir not found: {}", sfx_dir.display()));
    }

    let mut nfc = NfcReader::open()?;
    let player = PlaybackManager::new(audio_output, card_index);

    let mut last_seen_uid: Option<Vec<u8>> = None;
    let mut playing_for_uid: Option<Vec<u8>> = None;

    let audio_desc = match audio_output {
        AudioOutput::Pulse => "PipeWire/Pulse (mixed)".to_string(),
        AudioOutput::AlsaDirect => format!("ALSA direct plughw:{card_index},0 (sequential)"),
    };
    println!("Polling NFC (Ctrl-C to quit). Audio: {audio_desc}.");

    while RUNNING.load(Ordering::SeqCst) {
        if let Some(uid) = nfc.poll_uid() {
            let uid_bytes = uid.as_bytes().to_vec();

            if last_seen_uid.as_ref() == Some(&uid_bytes) {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            last_seen_uid = Some(uid_bytes.clone());

            println!("Tag UID {}", format_uid(&uid_bytes));

            if player.is_playing() {
                if playing_for_uid.as_ref() == Some(&uid_bytes) {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                player.stop();
                playing_for_uid = None;
            }

            let key = match nfc.read_media_key(&uid) {
                Some(k) => k,
                None => {
                    println!("No readable NDEF key on tag");
                    continue;
                }
            };

            let Some(mp3) = find_mp3(&media_root, &key) else {
                println!("No '{key}.mp3' on USB");
                continue;
            };

            player.play_sfx_then_mp3(&sfx_dir, &mp3)?;
            playing_for_uid = Some(uid_bytes);
        } else {
            last_seen_uid = None;
        }

        thread::sleep(Duration::from_millis(100));
    }

    println!("Shutting down…");
    player.stop();
    Ok(())
}

fn parse_args() -> Result<(u32, Option<PathBuf>, AudioOutput), String> {
    let args: Vec<String> = env::args().skip(1).collect();

    let mut card_index = None;
    let mut media_dir = None;
    let mut audio: Option<AudioOutput> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--card-index" {
            let s = args
                .get(i + 1)
                .ok_or("--card-index requires a number".to_string())?;
            card_index = Some(
                s.parse()
                    .map_err(|_| format!("invalid card index '{s}'"))?,
            );
            i += 2;
        } else if args[i] == "--media-dir" {
            let path = args
                .get(i + 1)
                .ok_or("--media-dir requires a path".to_string())?;
            media_dir = Some(PathBuf::from(path));
            i += 2;
        } else if args[i] == "--audio" {
            audio = Some(parse_audio_arg(
                args.get(i + 1)
                    .ok_or("--audio requires pulse or alsa".to_string())?,
            )?);
            i += 2;
        } else {
            return Err(usage());
        }
    }

    let audio_output = audio.unwrap_or_else(|| {
        if pulse_session_available() {
            AudioOutput::Pulse
        } else {
            AudioOutput::AlsaDirect
        }
    });

    let card_index = match card_index {
        Some(n) => n,
        None => detect_pcm5102_card()?,
    };

    Ok((card_index, media_dir, audio_output))
}

fn parse_audio_arg(s: &str) -> Result<AudioOutput, String> {
    match s {
        "pulse" | "pipewire" => Ok(AudioOutput::Pulse),
        "alsa" => Ok(AudioOutput::AlsaDirect),
        _ => Err("invalid --audio value (use pulse or alsa)".to_string()),
    }
}

fn usage() -> String {
    "usage: poc2_nfc2audio_test_PN532_PCM5102 [--card-index N] [--media-dir PATH] [--audio pulse|alsa]\n\
     default --audio: pulse if PipeWire session exists, else alsa\n\
     default card: auto-detect HiFiBerry/PCM5102 from `aplay -l`\n\
     needs: mpg123, pw-play or aplay; PN532 on I2C; dtoverlay=hifiberry-dac"
        .to_string()
}
