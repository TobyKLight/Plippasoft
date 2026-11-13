//! Quality ranking and `localFilePaths` selection (Noubin §3.3).

use std::path::{Path, PathBuf};

use plippa_core::Capabilities;

use crate::decode::AudioProbe;
use crate::isolate::Isolation;
use crate::error::AudioError;
use crate::prepare::{exceeds_capabilities, OverspecPolicy};

/// Ranked description of a candidate local media file.
#[derive(Debug, Clone)]
pub struct PathCandidate {
    pub path: PathBuf,
    pub probe: AudioProbe,
    pub quality_score: u64,
}

/// Higher is better. Lossless (wav/flac) ranks above lossy; then rate × depth.
pub fn quality_score(probe: &AudioProbe) -> u64 {
    let lossless = matches!(probe.extension.as_str(), "wav" | "flac");
    let tier: u64 = if lossless { 2_000_000_000 } else { 1_000_000_000 };
    // Prefer FLAC slightly over WAV when rate/depth match (common Bandcamp dual dumps).
    let format_nudge: u64 = match probe.extension.as_str() {
        "flac" => 100,
        "wav" => 50,
        "mp3" => 10,
        _ => 0,
    };
    tier + u64::from(probe.sample_rate_hz) * 1_000 + u64::from(probe.bit_depth) + format_nudge
}

fn extension_ok(path: &Path, caps: &Capabilities) -> Result<String, AudioError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .ok_or_else(|| AudioError::UnsupportedFormat {
            path: path.to_path_buf(),
        })?;
    if !crate::formats::format_enabled(caps, &ext) {
        return Err(AudioError::FormatNotEnabled {
            path: path.to_path_buf(),
            ext: ext.clone(),
        });
    }
    Ok(ext)
}

/// Whether this probed file can be played under `caps` + `policy`.
pub fn is_playable(probe: &AudioProbe, caps: &Capabilities, policy: OverspecPolicy) -> bool {
    if !crate::formats::format_enabled(caps, &probe.extension) {
        return false;
    }
    if !exceeds_capabilities(probe, caps) {
        return true;
    }
    matches!(policy, OverspecPolicy::Downsample)
}

/// Pick the highest-quality playable path from a `localFilePaths` list.
///
/// Noubin §3.3: when multiple valid media files exist, play the highest quality
/// this player supports.
pub fn select_best_local_path(
    paths: &[impl AsRef<Path>],
    capabilities: &Capabilities,
    policy: OverspecPolicy,
) -> Result<PathBuf, AudioError> {
    let mut best: Option<PathCandidate> = None;

    for path_ref in paths {
        let path = path_ref.as_ref();
        if !path.is_file() {
            continue;
        }
        if extension_ok(path, capabilities).is_err() {
            continue;
        }
        // The candidates are files from a stick: probed in the media helper, on the handle
        // opened (and checked as a plain file) here.
        let Ok(file) = plippa_core::safe_fs::open_regular_nofollow(path) else {
            continue;
        };
        let probe = match Isolation::global().validate_for_stream(
            &file,
            path,
            capabilities,
            OverspecPolicy::Downsample,
        ) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !is_playable(&probe, capabilities, policy) {
            continue;
        }
        let score = quality_score(&probe);
        let candidate = PathCandidate {
            path: path.to_path_buf(),
            probe,
            quality_score: score,
        };
        match &best {
            None => best = Some(candidate),
            Some(prev) if score > prev.quality_score => best = Some(candidate),
            _ => {}
        }
    }

    best.map(|c| c.path)
        .ok_or(AudioError::NoPlayablePath)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::Capabilities;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn flac_beats_mp3_when_both_present() {
        let caps = Capabilities::desktop_mock();
        let paths = [fixture("tone_48k.mp3"), fixture("tone_48k.flac")];
        let best = select_best_local_path(&paths, &caps, OverspecPolicy::Downsample)
            .expect("select");
        assert!(best.ends_with("tone_48k.flac"));
    }
}
