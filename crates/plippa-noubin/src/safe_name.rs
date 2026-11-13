//! Safe file and folder names (Noubin §1.26).

/// Characters forbidden in file/folder names (§1.26).
const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// True when `ch` is an ASCII control character (0–31) or otherwise forbidden.
fn is_forbidden_char(ch: char) -> bool {
    ch.is_ascii_control() || FORBIDDEN.contains(&ch)
}

/// Sanitize a single path segment (file or folder name) per §1.26.
///
/// Replaces forbidden characters with `_`, strips trailing spaces/periods,
/// and collapses empty results to `fallback`.
pub fn sanitize_path_segment(raw: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if is_forbidden_char(ch) {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    // Alternating dots and spaces (`"a. ."`) must not leave a trailing dot behind.
    let trimmed = out.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Sanitize a filename that may include an extension (keeps the last `.ext`
/// when the stem is non-empty after sanitizing).
pub fn sanitize_filename(raw: &str, fallback: &str) -> String {
    let name = std::path::Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);
    let path = std::path::Path::new(name);
    match (path.file_stem().and_then(|s| s.to_str()), path.extension().and_then(|s| s.to_str())) {
        (Some(stem), Some(ext)) if !ext.is_empty() => {
            let safe_stem = sanitize_path_segment(stem, fallback);
            let safe_ext = sanitize_path_segment(ext, "bin");
            format!("{safe_stem}.{safe_ext}")
        }
        _ => sanitize_path_segment(name, fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_forbidden_and_trailing_dot() {
        assert_eq!(sanitize_path_segment("A<B>:C?", "x"), "A_B__C_");
        assert_eq!(sanitize_path_segment("Hello. ", "x"), "Hello");
        assert_eq!(sanitize_path_segment("...", "x"), "x");
        assert_eq!(sanitize_path_segment("a. . ", "x"), "a");
        assert_eq!(sanitize_path_segment(". . .", "x"), "x");
    }

    #[test]
    fn filename_keeps_extension() {
        assert_eq!(sanitize_filename("track*.mp3", "file"), "track_.mp3");
    }
}
