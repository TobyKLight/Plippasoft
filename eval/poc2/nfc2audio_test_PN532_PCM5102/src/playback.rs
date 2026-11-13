use rand::seq::SliceRandom;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioOutput {
    /// PipeWire/Pulse (mixes SFX + MP3). Set PCM5102 as default sink: `wpctl status`.
    Pulse,
    /// Direct ALSA `plughw:N,0` (exclusive — sequential SFX then MP3).
    AlsaDirect,
}

pub struct PlaybackManager {
    inner: Arc<Mutex<PlaybackInner>>,
    output: AudioOutput,
    alsa_device: String,
    pulse_env: Vec<(String, String)>,
}

struct PlaybackInner {
    sfx: Option<Child>,
    mp3: Option<Child>,
}

impl PlaybackManager {
    pub fn new(output: AudioOutput, card_index: u32) -> Self {
        let pulse_env = pulse_session_env();
        if output == AudioOutput::Pulse && pulse_env.is_empty() {
            eprintln!(
                "warning: PipeWire session not found; use --audio alsa or run without sudo"
            );
        }
        Self {
            inner: Arc::new(Mutex::new(PlaybackInner {
                sfx: None,
                mp3: None,
            })),
            output,
            alsa_device: format!("plughw:{card_index},0"),
            pulse_env,
        }
    }

    pub fn is_playing(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.sfx.is_some() || guard.mp3.is_some()
    }

    pub fn stop(&self) {
        let mut guard = self.inner.lock().unwrap();
        let had = guard.sfx.is_some() || guard.mp3.is_some();
        stop_child(&mut guard.mp3);
        stop_child(&mut guard.sfx);
        if had {
            println!("Playback stopped");
        }
    }

    pub fn play_sfx_then_mp3(&self, sfx_dir: &Path, mp3_path: &Path) -> Result<(), String> {
        match self.output {
            AudioOutput::Pulse => self.play_sfx_then_mp3_mixed(sfx_dir, mp3_path),
            AudioOutput::AlsaDirect => self.play_sfx_then_mp3_sequential(sfx_dir, mp3_path),
        }
    }

    /// Random SFX immediately, MP3 after 300 ms (overlapping tail).
    fn play_sfx_then_mp3_mixed(&self, sfx_dir: &Path, mp3_path: &Path) -> Result<(), String> {
        self.stop();

        let wav = pick_random_wav(sfx_dir)?;
        let wav_name = wav
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sfx.wav");
        let mp3_name = mp3_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("track.mp3")
            .to_string();

        println!("Playing SFX {wav_name}…");
        let sfx_child = self.spawn_sfx(&wav)?;

        {
            let mut guard = self.inner.lock().unwrap();
            guard.sfx = Some(sfx_child);
        }

        let inner = Arc::clone(&self.inner);
        let mp3 = mp3_path.to_path_buf();
        let this = PlaybackSpawnCtx {
            output: self.output,
            alsa_device: self.alsa_device.clone(),
            pulse_env: self.pulse_env.clone(),
        };

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            println!("Playing {mp3_name}…");
            match this.spawn_mp3(&mp3) {
                Ok(child) => {
                    let mut guard = inner.lock().unwrap();
                    guard.mp3 = Some(child);
                }
                Err(e) => eprintln!("mp3 playback failed: {e}"),
            }
        });

        Ok(())
    }

    /// Sequential playback for exclusive ALSA devices (no hardware mixer).
    fn play_sfx_then_mp3_sequential(&self, sfx_dir: &Path, mp3_path: &Path) -> Result<(), String> {
        self.stop();

        let wav = pick_random_wav(sfx_dir)?;
        let wav_name = wav
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sfx.wav");
        let mp3_name = mp3_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("track.mp3")
            .to_string();

        println!("Playing SFX {wav_name}…");
        let mut sfx_child = self.spawn_sfx(&wav)?;
        let _ = sfx_child.wait();

        println!("Playing {mp3_name}…");
        let mp3_child = self.spawn_mp3(mp3_path)?;

        let mut guard = self.inner.lock().unwrap();
        guard.mp3 = Some(mp3_child);
        Ok(())
    }

    fn spawn_sfx(&self, wav: &Path) -> Result<Child, String> {
        match self.output {
            AudioOutput::Pulse => spawn_pulse_wav(&self.pulse_env, wav),
            AudioOutput::AlsaDirect => spawn_aplay(&self.pulse_env, &self.alsa_device, wav),
        }
    }

    fn spawn_mp3(&self, mp3: &Path) -> Result<Child, String> {
        match self.output {
            AudioOutput::Pulse => spawn_mpg123_pulse(&self.pulse_env, mp3),
            AudioOutput::AlsaDirect => spawn_mpg123_alsa(&self.pulse_env, &self.alsa_device, mp3),
        }
    }
}

#[derive(Clone)]
struct PlaybackSpawnCtx {
    output: AudioOutput,
    alsa_device: String,
    pulse_env: Vec<(String, String)>,
}

impl PlaybackSpawnCtx {
    fn spawn_mp3(&self, mp3: &Path) -> Result<Child, String> {
        match self.output {
            AudioOutput::Pulse => spawn_mpg123_pulse(&self.pulse_env, mp3),
            AudioOutput::AlsaDirect => spawn_mpg123_alsa(&self.pulse_env, &self.alsa_device, mp3),
        }
    }
}

pub fn detect_pcm5102_card() -> Result<u32, String> {
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

/// UID whose PipeWire/Pulse session to use (handles `sudo`).
pub fn effective_session_uid() -> u32 {
    std::env::var("SUDO_UID")
        .ok()
        .or_else(|| std::env::var("UID").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

pub fn pulse_session_available() -> bool {
    let uid = effective_session_uid();
    std::path::Path::new(&format!("/run/user/{uid}/pulse/native")).exists()
}

fn pulse_session_env() -> Vec<(String, String)> {
    let uid = effective_session_uid();
    let runtime = format!("/run/user/{uid}");
    let pulse_native = format!("{runtime}/pulse/native");
    if !std::path::Path::new(&pulse_native).exists() {
        return Vec::new();
    }
    vec![
        ("XDG_RUNTIME_DIR".into(), runtime.clone()),
        ("PULSE_RUNTIME_PATH".into(), format!("{runtime}/pulse")),
    ]
}

fn apply_env(cmd: &mut Command, extra: &[(String, String)]) {
    for (k, v) in extra {
        cmd.env(k, v);
    }
}

fn stop_child(slot: &mut Option<Child>) {
    if let Some(mut child) = slot.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn spawn_pulse_wav(env: &[(String, String)], wav: &Path) -> Result<Child, String> {
    for try_spawn in [spawn_pw_play, spawn_paplay] {
        match try_spawn(env, wav) {
            Ok(child) => return Ok(child),
            Err(e) if e.contains("not found") => {}
            Err(e) => return Err(e),
        }
    }
    spawn_aplay(env, "default", wav)
}

fn spawn_pw_play(env: &[(String, String)], wav: &Path) -> Result<Child, String> {
    let mut cmd = Command::new("pw-play");
    cmd.arg(wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    apply_env(&mut cmd, env);
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "pw-play not found".to_string()
        } else {
            format!("pw-play failed: {e}")
        }
    })
}

fn spawn_paplay(env: &[(String, String)], wav: &Path) -> Result<Child, String> {
    let mut cmd = Command::new("paplay");
    cmd.arg(wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    apply_env(&mut cmd, env);
    cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "paplay not found".to_string()
        } else {
            format!("paplay failed: {e}")
        }
    })
}

fn spawn_aplay(env: &[(String, String)], alsa_device: &str, wav: &Path) -> Result<Child, String> {
    let mut cmd = Command::new("aplay");
    cmd.args(["-q", "-D", alsa_device])
        .arg(wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_env(&mut cmd, env);
    cmd.spawn()
        .map_err(|e| format!("aplay failed: {e} (install alsa-utils?)"))
}

fn spawn_mpg123_pulse(env: &[(String, String)], mp3: &Path) -> Result<Child, String> {
    let mut cmd = Command::new("mpg123");
    cmd.args(["-q", "-o", "pulse"])
        .arg(mp3)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    apply_env(&mut cmd, env);
    spawn_with_stderr_log(cmd, "mpg123")
}

fn spawn_mpg123_alsa(
    env: &[(String, String)],
    alsa_device: &str,
    mp3: &Path,
) -> Result<Child, String> {
    let mut cmd = Command::new("mpg123");
    cmd.args(["-q", "-o", "alsa", "-a", alsa_device])
        .arg(mp3)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    apply_env(&mut cmd, env);
    spawn_with_stderr_log(cmd, "mpg123")
}

fn spawn_with_stderr_log(mut cmd: Command, name: &str) -> Result<Child, String> {
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{name} not found — install with: sudo apt install mpg123")
        } else {
            format!("{name} failed: {e}")
        }
    })?;

    if let Some(mut stderr) = child.stderr.take() {
        let label = name.to_string();
        thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            if !buf.trim().is_empty() {
                eprintln!("{label}: {}", buf.trim());
            }
        });
    }

    Ok(child)
}

fn pick_random_wav(dir: &Path) -> Result<PathBuf, String> {
    let files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("wav")))
        .collect();
    if files.is_empty() {
        return Err(format!("no .wav in {}", dir.display()));
    }
    files
        .choose(&mut rand::thread_rng())
        .cloned()
        .ok_or_else(|| "no wav".to_string())
}
