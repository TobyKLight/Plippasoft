//! Embedded tag extract for portal import (MP3 / FLAC / WAV).
//!
//! Unsupported containers: return empty [`ExtractedFields`] (filename fallbacks).
//! Corrupt/partial files: also empty — import must not fail solely on missing tags.

use lofty::prelude::*;
use lofty::probe::Probe;
use plippa_noubin::{sanitise_title, ExtractedFields};

/// Read common tags from an MP3, FLAC or WAV file on disk (uploads are streamed to disk and
/// never held in memory; lofty only reads the tag regions it needs). Other formats / parse
/// failures → empty fields (caller uses the filename stem).
pub fn extract_tags_from_path(filename: &str, path: &std::path::Path) -> ExtractedFields {
    match std::fs::File::open(path) {
        Ok(f) => extract_tags(filename, std::io::BufReader::new(f)),
        Err(_) => ExtractedFields::default(),
    }
}

fn extract_tags<R: std::io::Read + std::io::Seek>(filename: &str, reader: R) -> ExtractedFields {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "mp3" | "flac" | "wav") {
        return ExtractedFields::default();
    }

    let tagged = match Probe::new(reader).guess_file_type() {
        Ok(p) => match p.read() {
            Ok(t) => t,
            Err(_) => return ExtractedFields::default(),
        },
        Err(_) => return ExtractedFields::default(),
    };

    let tag = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(t) => t,
        None => return ExtractedFields::default(),
    };

    sanitise_fields(ExtractedFields {
        title: tag.title().map(|s| s.to_string()),
        album: tag.album().map(|s| s.to_string()),
        artist: tag.artist().map(|s| s.to_string()),
        author_lastname: None,
        author_firstname: None,
    })
}

/// Tags come from files someone else prepared: control/bidi/zero-width characters are stripped,
/// text is NFC-normalised and capped (NBSecurityRequirements §3); empty results become `None`.
pub fn sanitise_fields(mut f: ExtractedFields) -> ExtractedFields {
    for field in [
        &mut f.title,
        &mut f.album,
        &mut f.artist,
        &mut f.author_lastname,
        &mut f.author_firstname,
    ] {
        *field = field.take().map(|s| sanitise_title(&s)).filter(|s| !s.is_empty());
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_sanitised_on_ingest() {
        let f = sanitise_fields(ExtractedFields {
            title: Some("Ti\u{202E}tle\u{0007}".into()),
            album: Some("\u{200B}".into()),
            artist: Some("x".repeat(1000)),
            ..Default::default()
        });
        assert_eq!(f.title.as_deref(), Some("Title"));
        assert_eq!(f.album, None);
        assert_eq!(f.artist.unwrap().chars().count(), 256);
    }
}

