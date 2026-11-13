use rand::seq::SliceRandom;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let device_index = parse_device_index()?;
    let assets_dir = assets_dir()?;

    let wav_files = list_wav_files(&assets_dir)?;
    let mut rng = rand::thread_rng();
    let chosen = wav_files
        .choose(&mut rng)
        .ok_or_else(|| format!("no .wav files in {}", assets_dir.display()))?;

    let device = format!("plughw:{device_index},0");
    let file_name = chosen
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");

    println!(
        "Playing {file_name} on PCM5102 (ALSA {device}, card index {device_index})…"
    );

    let status = Command::new("aplay")
        .args(["-D", &device, chosen.to_str().ok_or("invalid wav path")?])
        .status()
        .map_err(|e| format!("failed to run aplay: {e} (is alsa-utils installed?)"))?;

    if !status.success() {
        return Err(format!("aplay exited with {status}"));
    }

    println!("Playback finished");
    Ok(())
}

fn parse_device_index() -> Result<u32, String> {
    let mut args = env::args().skip(1);
    match args.next() {
        None => detect_pcm5102_card(),
        Some(index_str) => {
            if args.next().is_some() {
                return Err(usage());
            }
            index_str
                .parse()
                .map_err(|_| {
                    format!(
                        "invalid device index '{index_str}' (expected a number, e.g. 2)"
                    )
                })
        }
    }
}

fn detect_pcm5102_card() -> Result<u32, String> {
    let output = Command::new("aplay")
        .arg("-l")
        .output()
        .map_err(|e| format!("failed to run aplay -l: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if !line.starts_with("card ") {
            continue;
        }
        if !(lower.contains("hifiberry") || lower.contains("pcm5102")) {
            continue;
        }
        let rest = line.strip_prefix("card ").unwrap_or(line);
        let index_str = rest.split(':').next().unwrap_or("").trim();
        if let Ok(index) = index_str.parse::<u32>() {
            return Ok(index);
        }
    }

    Err(
        "could not find HiFiBerry/PCM5102 ALSA card (is dtoverlay=hifiberry-dac enabled? run: aplay -l)"
            .to_string(),
    )
}

fn usage() -> String {
    "usage: poc2_audio_test_PCM5102 [aplay_card_index]\n\
       default: auto-detect HiFiBerry/PCM5102 card from `aplay -l`\n\
       example: poc2_audio_test_PCM5102 2\n\
       list cards: aplay -l"
        .to_string()
}

fn assets_dir() -> Result<PathBuf, String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../assets/plippaSFX");
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!(
            "assets directory not found: {} (expected repo assets/plippaSFX)",
            dir.display()
        ))
    }
}

fn list_wav_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("wav")))
        .collect();

    files.sort();
    if files.is_empty() {
        return Err(format!("no .wav files in {}", dir.display()));
    }
    Ok(files)
}
