//! Playlist `.noudata` file helpers (Noubin §4.5, §4.9): layout, naming, atomic writes.
//!
//! A playlist is `Playlists/_<title>.noudata` at the root of a library, with no
//! `releaseData` / `credits`; `createdByUser` lives in `localPlaybackData.userNotes`.
//! Entries are `mediaList` items whose `localFilePaths` point (relative, may use `../`) at
//! media whose authoritative metadata is in another `.noudata` (or nowhere, for orphans).

use crate::noudata::{MediaListItem, NoudataDocument, NOUDATA_VERSION, WHAT_AM_I_LOOKING_AT};
use crate::noudata_types::{LocalPlaybackData, UserNotes};
use crate::safe_name::sanitize_path_segment;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Folder (off the library root) that holds playlists (§4.5).
pub const PLAYLISTS_DIR: &str = "Playlists";

/// Longest accepted playlist title, in characters.
pub const MAX_PLAYLIST_TITLE_CHARS: usize = 200;

/// Longest file-name stem we derive from a title (bytes; FAT/ext4 allow 255 for the whole name).
const MAX_STEM_BYTES: usize = 150;

/// Change-detection fingerprint of a file: modification time (ns) + size. Same measure the
/// index uses, so "the file changed underneath us" means the same thing everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRevision {
    pub mtime_ns: i64,
    pub size: u64,
}

impl FileRevision {
    pub fn of(path: &Path) -> std::io::Result<Self> {
        Ok(Self::of_meta(&fs::metadata(path)?))
    }

    pub fn of_meta(meta: &fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Self {
            mtime_ns,
            size: meta.len(),
        }
    }

    /// Opaque string for HTTP clients (`<mtime_ns>-<size>`).
    pub fn token(&self) -> String {
        format!("{}-{}", self.mtime_ns, self.size)
    }
}

/// Validate + normalise a user-supplied playlist title. `Err` carries a client-facing reason.
pub fn validate_playlist_title(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("playlist title must not be empty".into());
    }
    if t.chars().count() > MAX_PLAYLIST_TITLE_CHARS {
        return Err(format!(
            "playlist title is too long (max {MAX_PLAYLIST_TITLE_CHARS} characters)"
        ));
    }
    if t.chars().any(char::is_control) {
        return Err("playlist title must not contain control characters".into());
    }
    // Bidi overrides and zero-width characters are stripped, the text is NFC-normalised
    // (NBSecurityRequirements §3); the length cap applies to what is actually stored.
    let t = crate::sanitise::sanitise_title(t);
    if t.is_empty() {
        return Err("playlist title must not be empty".into());
    }
    if t.chars().count() > MAX_PLAYLIST_TITLE_CHARS {
        return Err(format!(
            "playlist title is too long (max {MAX_PLAYLIST_TITLE_CHARS} characters)"
        ));
    }
    Ok(t)
}

/// `_<safe title>` (no extension): §4.5 naming for collection `.noudata` files.
pub fn playlist_file_stem(title: &str) -> String {
    let safe = sanitize_path_segment(title, "Playlist");
    let mut end = safe.len().min(MAX_STEM_BYTES);
    while !safe.is_char_boundary(end) {
        end -= 1;
    }
    let cut = safe[..end].trim_end().trim_end_matches('.');
    let cut = if cut.is_empty() { "Playlist" } else { cut };
    format!("_{cut}")
}

/// Unused `_<title>.noudata` path in `dir`. Collisions (compared case-insensitively, as FAT
/// volumes are) get ` (2)`, ` (3)`… appended. `keep` is a path that may be reused (the file
/// being renamed), so renaming to the same name is not a collision.
pub fn unique_playlist_path(dir: &Path, title: &str, keep: Option<&Path>) -> PathBuf {
    let stem = playlist_file_stem(title);
    let taken: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| Some(e.path().as_path()) != keep)
        .filter_map(|e| e.file_name().to_str().map(str::to_lowercase))
        .collect();
    let mut n = 1u32;
    loop {
        let name = if n == 1 {
            format!("{stem}.noudata")
        } else {
            format!("{stem} ({n}).noudata")
        };
        if !taken.contains(&name.to_lowercase()) {
            return dir.join(name);
        }
        n += 1;
    }
}

/// A fresh, spec-conformant playlist document: `noudataType` `userPlaylist`, title,
/// `createdByUser`, no `releaseData`.
pub fn new_playlist_document(title: &str, created_by: &str) -> NoudataDocument {
    NoudataDocument {
        what_am_i_looking_at: Some(WHAT_AM_I_LOOKING_AT.into()),
        noudata_version: Some(NOUDATA_VERSION.into()),
        noudata_type: Some(crate::NoudataType::UserPlaylist),
        playable_item_title: Some(title.to_string()),
        local_playback_data: Some(LocalPlaybackData {
            user_notes: Some(UserNotes {
                created_by_user: Some(created_by.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A playlist entry pointing at `path` (relative to the playlist file). No release metadata
/// is copied: title/artist/length are resolved from the authoritative `.noudata`.
pub fn playlist_entry(path: &str) -> MediaListItem {
    MediaListItem {
        local_file_paths: Some(vec![path.to_string()]),
        ..Default::default()
    }
}

/// Path of the media file `media_rel` as written in a `.noudata` living in `from_dir`
/// (both library-relative, POSIX, `from_dir` `""` = the root): `../Music/A/x.mp3`.
///
/// `None` when it is not representable relative to the library: an absolute path, a path
/// with `..` segments, or an empty one (the index reports media outside the library as an
/// absolute path).
pub fn relative_media_path(from_dir: &str, media_rel: &str) -> Option<String> {
    let bad =
        |s: &str| s.starts_with('/') || s.starts_with('\\') || s.as_bytes().get(1) == Some(&b':');
    if media_rel.is_empty() || bad(media_rel) {
        return None;
    }
    let from: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = media_rel.split('/').filter(|s| !s.is_empty()).collect();
    if to.is_empty() || from.contains(&"..") || to.contains(&"..") {
        return None;
    }
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    // The file name itself is never a shared directory.
    let common = common.min(to.len() - 1);
    let mut out: Vec<&str> = vec![".."; from.len() - common];
    out.extend(&to[common..]);
    Some(out.join("/"))
}

/// Why an atomic write did not happen.
#[derive(Debug, Error)]
pub enum AtomicWriteError {
    /// The file changed (or appeared / vanished) since the caller read it.
    #[error("the file changed underneath the edit")]
    Conflict,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Noudata(#[from] crate::NoudataError),
}

/// What the destination must look like when the write commits.
#[derive(Debug, Clone, Copy)]
pub enum WriteGuard {
    /// The destination must not exist yet.
    MustNotExist,
    /// The destination must still have exactly this fingerprint.
    Unchanged(FileRevision),
}

/// Serialise `doc` and replace `path` atomically (temp file in the same folder, fsync,
/// rename). The [`WriteGuard`] is re-checked right before the rename, so an external edit
/// made while we were working is reported as [`AtomicWriteError::Conflict`], never
/// overwritten silently.
///
/// The new file's mtime is forced past the previous one when the filesystem's coarse clock
/// (FAT: 2 s) would otherwise leave it equal, so the index's mtime+size check always sees an
/// edit that keeps the byte size (a reorder).
///
/// Safety properties (shared with [`NoudataDocument::write_to_path`]): the temp file has a
/// unique name and is created with `create_new` (so concurrent writers never share or
/// truncate each other's temp file, and a pre-planted symlink is never followed); a
/// destination that is a symlink or a read-only file is refused; an existing file's
/// permission bits are kept; the temp file is removed on every failure path.
pub fn write_noudata_atomic(
    doc: &NoudataDocument,
    path: &Path,
    guard: WriteGuard,
) -> Result<FileRevision, AtomicWriteError> {
    write_atomic_impl(doc, path, Some(guard))
}

/// Replace `path` with `doc` atomically, with no revision guard (last writer wins, but the
/// file is never seen truncated or half-written). Backs `NoudataDocument::write_to_path`.
pub(crate) fn write_atomic_unguarded(
    doc: &NoudataDocument,
    path: &Path,
) -> Result<FileRevision, AtomicWriteError> {
    write_atomic_impl(doc, path, None)
}

fn write_atomic_impl(
    doc: &NoudataDocument,
    path: &Path,
    guard: Option<WriteGuard>,
) -> Result<FileRevision, AtomicWriteError> {
    // A *new* file must obey its `noudataType` rules; rewrites stay lenient so a foreign
    // file's extra data (e.g. `releaseData` on a looseTrack) survives (spec §3.1).
    let json = if matches!(guard, Some(WriteGuard::MustNotExist)) {
        doc.to_json_pretty_checked()?
    } else {
        doc.to_json_pretty()?
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("playlist.noudata");
    // Refuse to write through a symlink (also a dangling one) and over a read-only file.
    let existing = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to write through a symlink",
            )
            .into())
        }
        Ok(m) if m.permissions().readonly() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to replace a read-only file",
            )
            .into())
        }
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    // Unique temp name in the same directory (same volume => atomic rename).
    let mut tmp;
    let mut f = {
        let mut attempts = 0;
        loop {
            tmp = dir.join(format!(".{name}.{:016x}.plippa-tmp", rand::random::<u64>()));
            match fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(f) => break f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                    attempts += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    let result = (|| {
        f.write_all(json.as_bytes())?;
        if let Some(m) = &existing {
            f.set_permissions(m.permissions())?;
        }
        f.sync_all()?;
        if let Some(WriteGuard::Unchanged(prev)) = guard {
            let now = FileRevision::of_meta(&f.metadata()?);
            if now.mtime_ns <= prev.mtime_ns {
                let bumped = UNIX_EPOCH
                    + Duration::from_nanos(prev.mtime_ns.max(0) as u64)
                    + Duration::from_secs(2);
                f.set_modified(bumped.max(SystemTime::now()))?;
            }
        }
        match guard {
            None => {}
            Some(WriteGuard::MustNotExist) => {
                if fs::symlink_metadata(path).is_ok() {
                    return Err(AtomicWriteError::Conflict);
                }
            }
            Some(WriteGuard::Unchanged(prev)) => match FileRevision::of(path) {
                Ok(cur) if cur == prev => {}
                Ok(_) => return Err(AtomicWriteError::Conflict),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(AtomicWriteError::Conflict)
                }
                Err(e) => return Err(e.into()),
            },
        }
        fs::rename(&tmp, path)?;
        Ok(FileRevision::of(path)?)
    })();
    drop(f);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_from_playlists_dir() {
        assert_eq!(
            relative_media_path("Playlists", "Music/A/B/01.mp3").as_deref(),
            Some("../Music/A/B/01.mp3")
        );
        assert_eq!(
            relative_media_path("", "Loose/x.mp3").as_deref(),
            Some("Loose/x.mp3")
        );
        assert_eq!(
            relative_media_path("Music/A", "Music/A/x.mp3").as_deref(),
            Some("x.mp3")
        );
        assert_eq!(
            relative_media_path("Music/A/B", "Music/A/x.mp3").as_deref(),
            Some("../x.mp3")
        );
        // A directory named like the file must not be treated as shared.
        assert_eq!(
            relative_media_path("Music/x.mp3", "Music/x.mp3").as_deref(),
            Some("../x.mp3")
        );
        assert_eq!(relative_media_path("Playlists", "/mnt/other/x.mp3"), None);
        assert_eq!(relative_media_path("Playlists", ""), None);
        assert_eq!(relative_media_path("Playlists", "../x.mp3"), None);
    }

    #[test]
    fn stem_is_underscore_prefixed_and_safe() {
        assert_eq!(playlist_file_stem("Morning Jazz"), "_Morning Jazz");
        assert_eq!(playlist_file_stem("a/b:c?"), "_a_b_c_");
        assert_eq!(playlist_file_stem("..."), "_Playlist");
        let long = "é".repeat(300);
        assert!(playlist_file_stem(&long).len() <= MAX_STEM_BYTES + 1);
    }

    #[test]
    fn titles_are_validated() {
        assert_eq!(validate_playlist_title("  Hi  ").unwrap(), "Hi");
        assert!(validate_playlist_title("   ").is_err());
        assert!(validate_playlist_title("a\nb").is_err());
        assert!(validate_playlist_title(&"x".repeat(201)).is_err());
    }
}
