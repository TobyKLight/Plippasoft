//! Stable playable ids across multiple per-volume WAL indexes.
//!
//! Each library root keeps its own `.plippa/library.sqlite` with local
//! auto-increment ids. Portal / IPC use a **namespaced** `u64`:
//!
//! ```text
//! global_id = (library_namespace(root) as u64) << 32 | (local_id as u32 as u64)
//! ```
//!
//! `library_namespace` is a stable hash of the canonical (or raw) root path, so
//! engine and server agree without coordinating slot indexes. Collisions across
//! two open roots are vanishingly unlikely for desk use (≤2 sticks).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Stable 32-bit namespace for a library root path.
pub fn library_namespace(root: &Path) -> u32 {
    let key = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() as u32
}

/// Encode a local WAL row id into a process-wide playable id.
pub fn encode_playable_id(root: &Path, local_id: i64) -> u64 {
    let ns = library_namespace(root) as u64;
    let local = (local_id as u64) & 0xFFFF_FFFF;
    (ns << 32) | local
}

/// Split a namespaced playable id into `(library_namespace, local_id)`.
pub fn decode_playable_id(global_id: u64) -> (u32, i64) {
    let ns = (global_id >> 32) as u32;
    let local = (global_id & 0xFFFF_FFFF) as i64;
    (ns, local)
}

/// Display path for the portal: prepend `label / ` when more than one library is open.
pub fn display_path_with_library(label: &str, relpath: &str, multi: bool) -> String {
    if multi {
        format!("{label} / {relpath}")
    } else {
        relpath.to_string()
    }
}

/// Prefer filesystem volume label; fall back to the mount folder basename.
pub fn library_display_label(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Library")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn encode_decode_roundtrip() {
        let root = PathBuf::from("/media/plip/StickA");
        let id = encode_playable_id(&root, 42);
        let (ns, local) = decode_playable_id(id);
        assert_eq!(ns, library_namespace(&root));
        assert_eq!(local, 42);
    }

    #[test]
    fn different_roots_different_namespaces() {
        let a = PathBuf::from("/tmp/lib-a-unique");
        let b = PathBuf::from("/tmp/lib-b-unique");
        assert_ne!(library_namespace(&a), library_namespace(&b));
        let id_a = encode_playable_id(&a, 1);
        let id_b = encode_playable_id(&b, 1);
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn display_prefix_only_when_multi() {
        assert_eq!(
            display_path_with_library("Stick", "Music/a.mp3", false),
            "Music/a.mp3"
        );
        assert_eq!(
            display_path_with_library("Stick", "Music/a.mp3", true),
            "Stick / Music/a.mp3"
        );
    }
}
