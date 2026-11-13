//! The formats this build can decode, and the one gate that joins them to the board's flags.
//!
//! `symphonia` is built with exactly the readers and codecs below (see the workspace
//! `Cargo.toml`): MP3, FLAC and WAV (PCM). A board flag (`Capabilities::audio_formats`) is a
//! *promise* that the player decodes a format; a flag for a format that is not compiled in
//! (`aac`, `opus`) can therefore never be honoured. [`format_enabled`] is the single gate every
//! open uses: the board must allow the extension **and** this build must decode it. Adding AAC
//! later means enabling Symphonia's `isomp4` reader and `aac` codec, adding the extensions to
//! [`COMPILED_EXTENSIONS`], and extending the format test that pins the compiled set.

use plippa_core::Capabilities;

/// File extensions (lower case, no dot) the compiled Symphonia feature set decodes.
pub const COMPILED_EXTENSIONS: &[&str] = &["mp3", "wav", "flac"];

/// Whether this build contains a decoder for `ext` (lower case, no dot).
pub fn build_decodes(ext: &str) -> bool {
    COMPILED_EXTENSIONS.contains(&ext)
}

/// The gate: the board allows `ext` and the build can decode it.
pub fn format_enabled(capabilities: &Capabilities, ext: &str) -> bool {
    capabilities.supports_extension(ext) && build_decodes(ext)
}

/// Extensions the board's flags promise but this build cannot decode (probed over every
/// extension the capability table knows). Empty means the flags and the decoders agree.
pub fn unfulfillable_promises(capabilities: &Capabilities) -> Vec<&'static str> {
    const KNOWN: &[&str] = &["mp3", "wav", "flac", "m4a", "m4b", "aac", "opus"];
    KNOWN
        .iter()
        .copied()
        .filter(|ext| capabilities.supports_extension(ext) && !build_decodes(ext))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capability_gate_and_the_compiled_decoders_agree() {
        // The stock profiles promise only what is compiled in.
        assert!(unfulfillable_promises(&Capabilities::desktop_mock()).is_empty());
        assert!(unfulfillable_promises(&Capabilities::default()).is_empty());

        // A board that turns on AAC or Opus promises something this build cannot decode, and
        // the gate refuses it instead of reaching a decoder that is not there.
        let mut caps = Capabilities::desktop_mock();
        caps.audio_formats.aac = true;
        caps.audio_formats.opus = true;
        assert_eq!(unfulfillable_promises(&caps), ["m4a", "m4b", "aac", "opus"]);
        assert!(caps.supports_extension("m4a"));
        assert!(!format_enabled(&caps, "m4a"));
        assert!(!format_enabled(&caps, "opus"));
        for ext in COMPILED_EXTENSIONS {
            assert!(format_enabled(&caps, ext), "{ext}");
        }
    }

    /// Every compiled extension really decodes; every extension of a reader that was trimmed
    /// away is not recognised. Re-adding a reader (AAC) flips the second half on purpose: this
    /// is the test to update in the same change.
    #[test]
    fn the_compiled_set_is_what_symphonia_actually_decodes() {
        use std::path::Path;
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for (ext, file) in [("mp3", "tone_48k.mp3"), ("flac", "tone_48k.flac"), ("wav", "tone_48k_16.wav")] {
            assert!(build_decodes(ext));
            let pcm = crate::decode(&fixtures.join(file)).unwrap_or_else(|e| panic!("{ext}: {e}"));
            assert!(pcm.frame_count() > 0, "{ext}");
        }
        for (name, bytes) in trimmed_container_samples() {
            let mss = symphonia::core::io::MediaSourceStream::new(
                Box::new(std::io::Cursor::new(bytes)),
                Default::default(),
            );
            let hint = symphonia::core::probe::Hint::new();
            let verdict = symphonia::default::get_probe().format(
                &hint,
                mss,
                &Default::default(),
                &Default::default(),
            );
            assert!(verdict.is_err(), "{name} is recognised: a trimmed reader is linked again");
        }
    }

    /// Minimal headers of the container readers the build no longer links.
    fn trimmed_container_samples() -> Vec<(&'static str, Vec<u8>)> {
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&24u32.to_be_bytes());
        mp4.extend_from_slice(b"ftypM4A \0\0\0\0M4A mp42isom");
        mp4.extend_from_slice(&[0; 64]);
        let mut ogg = b"OggS\0\x02".to_vec();
        ogg.extend_from_slice(&[0; 64]);
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3, 0x80];
        mkv.extend_from_slice(&[0; 64]);
        vec![("mp4", mp4), ("ogg", ogg), ("mkv", mkv)]
    }
}
