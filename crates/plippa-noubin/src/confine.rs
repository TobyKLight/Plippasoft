//! One confinement function for every media path that reaches a decoder (`SECFIX_05`,
//! `PAR-03` / `SYS-13`).
//!
//! A `.noudata`, an index row and a waveform request are all **untrusted input** (the stick
//! belongs to somebody else). This module is the single place that decides which file such a
//! path may name, so the scanner, the engine and the server's waveform worker cannot
//! disagree:
//!
//! 1. [`resolve_declared`] / [`clean_relpath`]: a lexical check. No absolute form, drive
//!    letter, backslash, NUL, control or bidi character, and `..` may never climb above the
//!    library root. (`..` that stays inside the root is allowed: playlists in `Playlists/`
//!    reach `../Music/...`, and the repo's fixtures also use it from release folders.)
//! 2. [`open_media`]: the physical check. The file is opened **beneath the root without
//!    following any symlink**, non-blocking, and `fstat`ed: only a regular file is returned,
//!    and it is returned *open*, so the decoder reads the very handle that was checked (no
//!    check/use gap).
//!
//! Not enforced here, on purpose: FAT-reserved names and characters. Those restrict the
//! names Plippa *creates* ([`crate::is_safe_relative_path`]); a media file that already
//! exists on a stick with such a name must stay playable.

use crate::sanitise::has_unsafe_chars;
use plippa_core::safe_fs;
use std::fs::File;
use std::path::Path;

/// Longest path accepted (bytes), and longest component. Linux itself allows 4096/255.
const MAX_PATH_BYTES: usize = 4096;
const MAX_COMPONENT_BYTES: usize = 255;

/// Why a media path was refused.
#[derive(Debug)]
pub enum ConfineError {
    /// The path is not acceptable as a library path (absolute, escapes the root, control
    /// characters, ...). Nothing was opened.
    Unsafe,
    /// A safe path naming nothing (yet).
    Missing,
    /// A safe path whose target is not a plain regular file reachable without symlinks
    /// (symlink, FIFO, device, directory, escape through a link).
    Refused,
    /// Any other I/O failure (unreadable, stick vanished).
    Io(std::io::Error),
}

impl std::fmt::Display for ConfineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsafe => f.write_str("media path is not acceptable"),
            Self::Missing => f.write_str("media file not found"),
            Self::Refused => f.write_str("media path is not a plain file inside the library"),
            Self::Io(e) => write!(f, "media file cannot be opened: {e}"),
        }
    }
}

impl std::error::Error for ConfineError {}

/// A media file opened through [`open_media`].
#[derive(Debug)]
pub struct ConfinedMedia {
    /// The checked, already-open regular file. Hand this to the decoder; never reopen `rel`.
    pub file: File,
    /// The normalised library-relative path (`/` separators, no `.` / `..`).
    pub rel: String,
}

fn component_ok(seg: &str) -> bool {
    seg.len() <= MAX_COMPONENT_BYTES
}

fn text_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_PATH_BYTES
        && !s.starts_with('/')
        && !s.contains('\\')
        && !has_unsafe_chars(s) // control chars incl. NUL, bidi, zero-width
        && !{
            let b = s.as_bytes();
            b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()
        }
}

/// Resolve `declared` (a `localFilePaths`-style path, relative to the folder `base_dir` of
/// the document that names it) to a normalised library-relative path.
///
/// `None` when the path is not acceptable or would leave the library root. `base_dir` is a
/// library-relative folder (`""` = the root) and must itself be clean.
pub fn resolve_declared(base_dir: &str, declared: &str) -> Option<String> {
    if !text_ok(declared) {
        return None;
    }
    let mut stack: Vec<&str> = Vec::new();
    for seg in base_dir.split('/').filter(|s| !s.is_empty()) {
        if seg == "." || seg == ".." || !component_ok(seg) {
            return None;
        }
        stack.push(seg);
    }
    for seg in declared.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            s if component_ok(s) => stack.push(s),
            _ => return None,
        }
    }
    (!stack.is_empty()).then(|| stack.join("/"))
}

/// `Some(normalised)` when `rel` is acceptable as a library-relative path (a path read back
/// from the index is checked with this before it is used); `None` otherwise.
pub fn clean_relpath(rel: &str) -> Option<String> {
    resolve_declared("", rel)
}

/// Open the media file `declared` (relative to `base_dir`, see [`resolve_declared`]) below
/// `root`: lexical check, then a symlink-free `openat2`-style open, then `fstat`. Returns the
/// open regular file.
pub fn open_media(
    root: &Path,
    base_dir: &str,
    declared: &str,
) -> Result<ConfinedMedia, ConfineError> {
    let rel = resolve_declared(base_dir, declared).ok_or(ConfineError::Unsafe)?;
    match safe_fs::open_beneath(root, &rel) {
        Ok(file) => Ok(ConfinedMedia { file, rel }),
        Err(e) => Err(match e.kind() {
            std::io::ErrorKind::NotFound => ConfineError::Missing,
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::NotADirectory => {
                ConfineError::Refused
            }
            _ => ConfineError::Io(e),
        }),
    }
}

/// Library-relative convenience for [`open_media`] (`base_dir` = the root).
pub fn open_library_media(root: &Path, rel: &str) -> Result<ConfinedMedia, ConfineError> {
    open_media(root, "", rel)
}

/// Whether the library-relative `rel` names a plain regular file reachable without a
/// symlink (the confined twin of `Path::is_file`).
pub fn is_confined_regular_file(root: &Path, rel: &str) -> bool {
    open_library_media(root, rel).is_ok()
}
