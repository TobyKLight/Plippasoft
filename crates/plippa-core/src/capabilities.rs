//! Board / product capability flags (Noubin optional features + decode limits).

use serde::{Deserialize, Serialize};

/// Which optional audio encodings this board/profile will decode.
///
/// Noubin §1.23 minimum (MP3 / WAV / FLAC) is always implied when those flags
/// are set; §1.24 optional formats are opt-in per product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormatSupport {
    pub mp3: bool,
    pub wav: bool,
    pub flac: bool,
    /// AAC in `.m4a` / `.m4b` / `.aac` (Noubin §1.24 optional).
    pub aac: bool,
    /// Opus in `.opus` (Noubin §1.24 optional).
    pub opus: bool,
}

impl AudioFormatSupport {
    /// Noubin §1.23 minimum set; optional formats off (MVP default).
    pub fn noubin_minimum() -> Self {
        Self {
            mp3: true,
            wav: true,
            flac: true,
            aac: false,
            opus: false,
        }
    }
}

impl Default for AudioFormatSupport {
    fn default() -> Self {
        Self::noubin_minimum()
    }
}

/// Capabilities declared by a board profile (`plippa-board` later).
///
/// Used so Model D / Model P / desktop mocks can opt in or out of optional
/// Noubin and Plippa features without forking the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Human-readable board / product id (e.g. `plippa-D-poc2`, `desktop-mock`).
    pub board_id: String,
    /// NFC write (associate Noubin ↔ playable item).
    pub nfc_write: bool,
    /// Full media-library management (scan, import, web portal). Players without
    /// this still MUST support associate-on-device per Noubin §3.4.
    pub media_library_management: bool,
    /// Local screen UI present.
    pub local_display: bool,
    /// HDMI "now playing" / debug output present.
    pub hdmi_output: bool,
    /// Built-in microphone / record path.
    pub microphone: bool,
    /// Sync / reset TRS jacks (modular-style).
    pub sync_reset_io: bool,
    /// Max sample rate this model will play (Hz). Noubin floor: 48_000.
    pub max_sample_rate_hz: u32,
    /// Max PCM bit depth this model will play. Noubin floor: 16.
    pub max_bit_depth: u8,
    pub audio_formats: AudioFormatSupport,
}

impl Capabilities {
    /// Conservative desktop / mock defaults suitable for early MVP stubs.
    pub fn desktop_mock() -> Self {
        Self {
            board_id: "desktop-mock".into(),
            nfc_write: false,
            media_library_management: true,
            local_display: false,
            hdmi_output: false,
            microphone: false,
            sync_reset_io: false,
            max_sample_rate_hz: 48_000,
            max_bit_depth: 16,
            audio_formats: AudioFormatSupport::noubin_minimum(),
        }
    }

    /// Whether a format extension (without dot, lowercased) is supported.
    pub fn supports_extension(&self, ext: &str) -> bool {
        match ext {
            "mp3" => self.audio_formats.mp3,
            "wav" => self.audio_formats.wav,
            "flac" => self.audio_formats.flac,
            "m4a" | "m4b" | "aac" => self.audio_formats.aac,
            "opus" => self.audio_formats.opus,
            _ => false,
        }
    }
}

/// How a format (file extension, no dot) is named to a person: refusals, popups, the portal.
pub fn audio_format_label(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "m4a" | "m4b" | "aac" => "M4A/AAC".to_string(),
        "ogg" => "Ogg Vorbis".to_string(),
        "opus" => "Opus".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::desktop_mock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_noubin_minimum_audio() {
        let caps = Capabilities::default();
        assert_eq!(caps.board_id, "desktop-mock");
        assert!(caps.audio_formats.mp3);
        assert!(caps.audio_formats.wav);
        assert!(caps.audio_formats.flac);
        assert!(!caps.audio_formats.aac);
        assert!(!caps.audio_formats.opus);
        assert_eq!(caps.max_sample_rate_hz, 48_000);
        assert_eq!(caps.max_bit_depth, 16);
        assert!(!caps.nfc_write);
        assert!(caps.media_library_management);
    }

    #[test]
    fn extension_flags_behave_predictably() {
        let caps = Capabilities::default();
        assert!(caps.supports_extension("flac"));
        assert!(caps.supports_extension("mp3"));
        assert!(caps.supports_extension("wav"));
        assert!(!caps.supports_extension("m4a"));
        assert!(!caps.supports_extension("opus"));
        assert!(!caps.supports_extension("ogg"));

        let mut with_aac = caps.clone();
        with_aac.audio_formats.aac = true;
        assert!(with_aac.supports_extension("m4a"));
        assert!(with_aac.supports_extension("aac"));
    }

    #[test]
    fn formats_are_named_for_people() {
        assert_eq!(audio_format_label("m4a"), "M4A/AAC");
        assert_eq!(audio_format_label("aac"), "M4A/AAC");
        assert_eq!(audio_format_label("flac"), "FLAC");
        assert_eq!(audio_format_label("ogg"), "Ogg Vorbis");
        assert_eq!(audio_format_label(""), "");
    }

    #[test]
    fn capability_flags_are_independent() {
        let mut caps = Capabilities::desktop_mock();
        caps.nfc_write = true;
        caps.local_display = true;
        assert!(caps.nfc_write);
        assert!(caps.local_display);
        assert!(!caps.hdmi_output);
        assert!(!caps.microphone);
    }
}
