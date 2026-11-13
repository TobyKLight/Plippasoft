//! String and path sanitisers for untrusted metadata (fetched / imported metadata, tags,
//! filenames). See `NBSecurityRequirements.md` §3.
//!
//! The primitives every ingest path needs (NB_07); NB_08 added NFC normalisation of titles,
//! multiline text, tags and keys. The whole-document sanitiser built on these is
//! [`crate::sanitise_document`]; URL handling is in [`crate::sanitise_url`].

use crate::safe_name::sanitize_path_segment;
use unicode_normalization::UnicodeNormalization;

/// Longest title / name kept in a display field (characters).
pub const MAX_TITLE_CHARS: usize = 256;

/// Longest single path segment we create (bytes).
pub const MAX_SEGMENT_BYTES: usize = 200;

/// Longest key accepted (bytes).
pub const MAX_KEY_BYTES: usize = 512;

/// True for characters removed from every string: C0/C1 controls (incl. `\n`, `\t`),
/// bidi overrides/isolates, zero-width and other invisible formatting characters.
fn is_stripped(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                // deprecated format controls, Mongolian vowel separator, interlinear
                // annotation marks and the invisible "tag" block (ASCII smuggling)
                | '\u{206A}'..='\u{206F}'
                | '\u{180E}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{061C}'
                | '\u{FEFF}'
                | '\u{2028}'
                | '\u{2029}'
        )
}

/// True when `s` contains a control, bidi or zero-width character (see [`strip_unsafe_chars`]).
pub(crate) fn has_unsafe_chars(s: &str) -> bool {
    s.chars().any(is_stripped)
}

/// Remove control, bidi-override and zero-width characters (single-line fields).
pub fn strip_unsafe_chars(s: &str) -> String {
    s.chars().filter(|c| !is_stripped(*c)).collect()
}

/// Cut `s` to at most `max_chars` characters (never splits a char).
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Cut `s` to at most `max_bytes` bytes on a char boundary.
pub fn truncate_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Longest long-form text kept (`legalNotice`, `lyricsTranscript`, `additionalCreditsText`,
/// `description`, `userComment`), in bytes.
pub const MAX_LONG_TEXT_BYTES: usize = 64 * 1024;

/// Longest tag (characters) and most tags kept per list.
pub const MAX_TAG_CHARS: usize = 64;
pub const MAX_TAGS: usize = 64;

/// A single-line display string (title, name): stripped, NFC-normalised,
/// whitespace-collapsed, trimmed, capped at [`MAX_TITLE_CHARS`].
pub fn sanitise_title(s: &str) -> String {
    sanitise_line(s, MAX_TITLE_CHARS)
}

/// Like [`sanitise_title`] with an explicit character cap.
pub fn sanitise_line(s: &str, max_chars: usize) -> String {
    // Cap the input first so a hostile multi-megabyte string never reaches NFC.
    let head = truncate_bytes(s, max_chars.saturating_mul(16).max(64));
    let stripped = strip_unsafe_chars(head);
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed.nfc().collect::<String>(), max_chars)
}

/// A multi-line field: CR / CRLF become `\n`, `\t` and `\n` are kept, all other control,
/// bidi and zero-width characters are removed, capped at `max_bytes` on a char boundary.
pub fn sanitise_multiline(s: &str, max_bytes: usize) -> String {
    let head = truncate_bytes(s, max_bytes.saturating_add(4));
    let mut out = String::with_capacity(head.len());
    let mut chars = head.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if is_stripped(c) => {}
            c => out.push(c),
        }
    }
    truncate_bytes(&out, max_bytes).to_string()
}

/// One tag: single line, at most [`MAX_TAG_CHARS`] characters; `None` when empty.
pub fn sanitise_tag(s: &str) -> Option<String> {
    Some(sanitise_line(s, MAX_TAG_CHARS)).filter(|t| !t.is_empty())
}

/// A Noubin key: at most [`MAX_KEY_BYTES`] bytes and free of control/bidi/zero-width
/// characters (a key is compared byte for byte, so it is refused rather than altered).
pub fn sanitise_key(s: &str) -> Option<String> {
    if s.is_empty() || s.len() > MAX_KEY_BYTES || s.chars().any(is_stripped) {
        return None;
    }
    Some(s.to_string())
}

/// Reserved device names on FAT/exFAT/Windows (with or without an extension).
fn is_reserved_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// A file/folder name created from untrusted text: §1.28 characters replaced, invisible
/// characters stripped, leading dots removed (no hidden or `.plippa` folders, no `..`),
/// FAT-reserved names prefixed, at most `max_bytes` bytes (keeping a trailing `.ext` intact),
/// never empty (`fallback`).
pub fn safe_path_segment(raw: &str, fallback: &str, max_bytes: usize) -> String {
    let cleaned = strip_unsafe_chars(raw);
    let mut seg = sanitize_path_segment(cleaned.trim_start_matches(['.', ' ']), fallback);
    if seg.len() > max_bytes {
        // Keep a short extension so `very long….flac` stays a flac.
        let ext = seg
            .rsplit_once('.')
            .map(|(_, e)| e)
            .filter(|e| !e.is_empty() && e.len() <= 8 && e.is_ascii());
        seg = match ext {
            Some(e) => {
                let keep = max_bytes.saturating_sub(e.len() + 1);
                let stem = truncate_bytes(&seg[..seg.len() - e.len() - 1], keep).to_string();
                format!("{}.{e}", stem.trim_end_matches(['.', ' ']))
            }
            None => truncate_bytes(&seg, max_bytes).to_string(),
        };
        seg = seg.trim_end_matches(['.', ' ']).to_string();
    }
    if seg.is_empty() {
        seg = fallback.to_string();
    }
    if is_reserved_name(&seg) {
        seg.insert(0, '_');
    }
    seg
}

/// True when `rel` is acceptable as a library-relative path: non-empty, `/`-separated,
/// no absolute form / drive letter / backslash / NUL / control chars, no `.` or `..`
/// segments, no segment that is FAT-reserved, ends in space/dot, or exceeds
/// [`MAX_SEGMENT_BYTES`]. (Symlink and canonical-root checks are the applier's job.)
pub fn is_safe_relative_path(rel: &str) -> bool {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return false;
    }
    if rel.chars().any(is_stripped) {
        return false;
    }
    let bytes = rel.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return false;
    }
    rel.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg.len() <= MAX_SEGMENT_BYTES
            && !seg.ends_with([' ', '.'])
            && !is_reserved_name(seg)
            && !seg.contains([':', '<', '>', '"', '|', '?', '*'])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bidi_zero_width_controls() {
        assert_eq!(
            strip_unsafe_chars("a\u{202E}b\u{200B}c\u{0007}d\u{0085}e\u{FEFF}"),
            "abcde"
        );
    }

    #[test]
    fn title_is_collapsed_and_capped() {
        assert_eq!(sanitise_title("  a \n\t b  "), "a b");
        assert_eq!(
            sanitise_title(&"x".repeat(1000)).chars().count(),
            MAX_TITLE_CHARS
        );
    }

    #[test]
    fn title_is_nfc_normalised() {
        // "e" + combining acute -> single precomposed é
        assert_eq!(sanitise_title("Cafe\u{301}"), "Caf\u{e9}");
    }

    #[test]
    fn multiline_keeps_newlines_only() {
        assert_eq!(
            sanitise_multiline("a\r\nb\rc\u{0007}\u{202E}d\te", 100),
            "a\nb\ncd\te"
        );
        assert_eq!(sanitise_multiline(&"é".repeat(100), 11).len(), 10);
    }

    #[test]
    fn tags_and_keys() {
        assert_eq!(sanitise_tag("  rock\u{200B}  "), Some("rock".into()));
        assert_eq!(sanitise_tag("\u{200B}"), None);
        assert_eq!(sanitise_tag(&"x".repeat(200)).unwrap().chars().count(), MAX_TAG_CHARS);
        assert!(sanitise_key("https://a.com/x").is_some());
        assert!(sanitise_key("a\nb").is_none());
        assert!(sanitise_key(&"k".repeat(513)).is_none());
    }

    #[test]
    fn segment_rules() {
        assert_eq!(
            safe_path_segment("../../etc/passwd", "x", 200),
            "_.._etc_passwd"
        );
        assert_eq!(safe_path_segment("..", "fallback", 200), "fallback");
        assert_eq!(safe_path_segment(".plippa", "x", 200), "plippa");
        assert_eq!(safe_path_segment("CON", "x", 200), "_CON");
        assert_eq!(safe_path_segment("nul.mp3", "x", 200), "_nul.mp3");
        assert_eq!(safe_path_segment("COM0", "x", 200), "COM0");
        let long = format!("{}.flac", "é".repeat(300));
        let s = safe_path_segment(&long, "x", 200);
        assert!(s.len() <= 200 && s.ends_with(".flac"));
    }

    #[test]
    fn relative_path_rules() {
        assert!(is_safe_relative_path("a/b/c.mp3"));
        for bad in [
            "",
            "/etc/passwd",
            "C:/x",
            "c:x",
            "a/../b",
            "..",
            "a\\b",
            "a//b",
            "a/./b",
            "a\0b",
            "a/CON",
            "a/b ",
            "a/b.",
            "a/b:c",
        ] {
            assert!(!is_safe_relative_path(bad), "{bad:?}");
        }
    }
}
