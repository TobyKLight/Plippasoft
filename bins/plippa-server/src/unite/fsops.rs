//! Path-safe file operations inside one library root (`NBSecurityRequirements` §3 paths, §4
//! atomic guarded writes).
//!
//! Rules enforced here, in one place, for every write the unite flows make:
//!
//! * Every library-relative path is checked segment by segment with
//!   [`plippa_noubin::is_safe_relative_path`] (no `..`, absolute forms, backslashes, NUL,
//!   reserved FAT names, over-long segments) and joined onto the **canonical** library root.
//! * No existing component of a target path may be a symlink (a stick prepared by someone else
//!   could contain `Music -> /`), and the final target must not exist: files are never
//!   overwritten (`move_no_replace` uses `link` + `unlink` where the filesystem has hard links,
//!   otherwise a checked rename).
//! * Staging happens in `<root>/.plippa/tmp/` (same volume as the library, so the final move is
//!   an atomic rename); `.plippa` and `tmp` themselves must be real directories.
//! * A [`Transaction`] remembers what it moved/created/wrote and can undo it in reverse.

use plippa_noubin::{is_safe_relative_path, PLAYER_DB_DIR};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub enum FsError {
    /// The path (or one of its parents) is not acceptable inside the library.
    Unsafe(String),
    /// The target already exists (nothing is ever overwritten).
    Exists(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::Unsafe(m) => write!(f, "unsafe path: {m}"),
            FsError::Exists(p) => write!(f, "{} already exists", p.display()),
            FsError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        FsError::Io(e)
    }
}

/// True when `p` exists and is a symlink (does not follow it).
fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// A library root with the checks above. Cheap to build; holds only the canonical path.
#[derive(Debug, Clone)]
pub struct LibraryFs {
    root: PathBuf,
}

impl LibraryFs {
    pub fn new(root: &Path) -> Result<Self, FsError> {
        let root = fs::canonicalize(root)?;
        if !fs::metadata(&root)?.is_dir() {
            return Err(FsError::Unsafe("library root is not a directory".into()));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/.plippa/tmp`, created if needed; `.plippa` and `tmp` must be real directories.
    pub fn tmp_dir(&self) -> Result<PathBuf, FsError> {
        let mut p = self.root.clone();
        for seg in [PLAYER_DB_DIR, "tmp"] {
            p.push(seg);
            match fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(FsError::Unsafe(format!("{seg} is not a plain directory")));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&p)?,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(p)
    }

    /// Absolute path for a library-relative `rel`, validated. Existing components must not be
    /// symlinks; missing ones are fine (they will be created by [`Self::ensure_dir`]).
    pub fn resolve(&self, rel: &Path) -> Result<PathBuf, FsError> {
        let text = rel
            .to_str()
            .ok_or_else(|| FsError::Unsafe("path is not valid UTF-8".into()))?;
        if !is_safe_relative_path(text) {
            return Err(FsError::Unsafe(format!(
                "{text:?} is not a safe relative path"
            )));
        }
        if text.split('/').next() == Some(PLAYER_DB_DIR) {
            return Err(FsError::Unsafe(
                "the player database folder is off limits".into(),
            ));
        }
        let mut abs = self.root.clone();
        for c in rel.components() {
            match c {
                Component::Normal(seg) => {
                    abs.push(seg);
                    if is_symlink(&abs) {
                        return Err(FsError::Unsafe(format!(
                            "{} is a symbolic link",
                            abs.display()
                        )));
                    }
                }
                _ => return Err(FsError::Unsafe(format!("{text:?} is not a plain path"))),
            }
        }
        debug_assert!(abs.starts_with(&self.root));
        Ok(abs)
    }

    /// Create every missing directory of `rel` (one `create_dir` at a time, re-checking for
    /// symlinks); newly created ones are appended to `created` (parents first).
    pub fn ensure_dir(&self, rel: &Path, created: &mut Vec<PathBuf>) -> Result<PathBuf, FsError> {
        let abs = self.resolve(rel)?;
        let mut cur = self.root.clone();
        for c in rel.components() {
            let Component::Normal(seg) = c else {
                return Err(FsError::Unsafe("not a plain path".into()));
            };
            cur.push(seg);
            match fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(FsError::Unsafe(format!(
                        "{} exists and is not a directory",
                        cur.display()
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&cur)?;
                    created.push(cur.clone());
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(abs)
    }

    /// File names in library-relative directory `rel` (empty when it does not exist yet).
    pub fn existing_names(&self, rel: &Path) -> Vec<String> {
        let Ok(abs) = self.resolve(rel) else {
            return Vec::new();
        };
        fs::read_dir(abs)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `src` must be a plain file directly inside `stage` (never a symlink, never elsewhere).
    pub fn check_staged(&self, stage: &Path, src: &Path) -> Result<(), FsError> {
        if !src.starts_with(stage) {
            return Err(FsError::Unsafe("source is outside the staging area".into()));
        }
        let m = fs::symlink_metadata(src)?;
        if !m.file_type().is_file() {
            return Err(FsError::Unsafe("source is not a regular file".into()));
        }
        Ok(())
    }
}

/// Move `src` to `dst` without ever replacing an existing `dst`.
pub fn move_no_replace(src: &Path, dst: &Path) -> Result<(), FsError> {
    if fs::symlink_metadata(dst).is_ok() {
        return Err(FsError::Exists(dst.to_path_buf()));
    }
    // Atomic no-replace where hard links exist (ext4 & co).
    match fs::hard_link(src, dst) {
        Ok(()) => {
            fs::remove_file(src)?;
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(FsError::Exists(dst.to_path_buf()));
        }
        Err(_) => {} // FAT/exFAT: no hard links; fall through to a checked rename
    }
    if fs::symlink_metadata(dst).is_ok() {
        return Err(FsError::Exists(dst.to_path_buf()));
    }
    fs::rename(src, dst)?;
    Ok(())
}

/// Write `bytes` to a new file `dst` (temp + fsync + no-replace move in the same directory).
pub fn write_new_atomic(dst: &Path, bytes: &[u8]) -> Result<(), FsError> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = dst
        .parent()
        .ok_or_else(|| FsError::Unsafe("no parent directory".into()))?;
    let name = dst
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| FsError::Unsafe("bad file name".into()))?;
    let tmp = dir.join(format!(".{name}.plippa-tmp"));
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    let res = f
        .write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(FsError::from)
        .and_then(|_| move_no_replace(&tmp, dst));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Undo log for one apply.
#[derive(Debug, Default)]
pub struct Transaction {
    moved: Vec<(PathBuf, PathBuf)>,
    created_dirs: Vec<PathBuf>,
    written: Vec<PathBuf>,
}

impl Transaction {
    pub fn dirs(&mut self) -> &mut Vec<PathBuf> {
        &mut self.created_dirs
    }

    /// Move a staged file into the library (no replace) and remember how to move it back.
    pub fn move_in(&mut self, src: &Path, dst: &Path) -> Result<(), FsError> {
        move_no_replace(src, dst)?;
        // Staging files are private (0600) temps; library media should read like any other file.
        // Best effort (FAT/exFAT mounts ignore modes).
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(0o644));
        }
        self.moved.push((src.to_path_buf(), dst.to_path_buf()));
        Ok(())
    }

    pub fn write_new(&mut self, dst: &Path, bytes: &[u8]) -> Result<(), FsError> {
        write_new_atomic(dst, bytes)?;
        self.written.push(dst.to_path_buf());
        Ok(())
    }

    /// Reverse everything: delete written files, move files back to staging, remove the
    /// directories this transaction created (only if empty).
    pub fn rollback(&mut self) {
        for w in self.written.drain(..).rev() {
            let _ = fs::remove_file(w);
        }
        for (src, dst) in self.moved.drain(..).rev() {
            if fs::rename(&dst, &src).is_err() {
                // Could not go back (staging vanished): do not leave a half release around.
                let _ = fs::remove_file(&dst);
            }
        }
        for d in self.created_dirs.drain(..).rev() {
            let _ = fs::remove_dir(d);
        }
    }

    /// Success: forget the undo log.
    pub fn commit(mut self) {
        self.moved.clear();
        self.created_dirs.clear();
        self.written.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> (tempfile::TempDir, LibraryFs) {
        let t = tempfile::tempdir().unwrap();
        let l = LibraryFs::new(t.path()).unwrap();
        (t, l)
    }

    #[test]
    fn unite_fs_resolve_refuses_escapes_and_symlinks() {
        let (t, l) = lib();
        for bad in [
            "../evil",
            "a/../../evil",
            "/etc/passwd",
            "C:/x",
            "a\\b",
            "a//b",
            "",
            "a/CON",
            ".plippa/tmp/x",
            ".plippa",
        ] {
            assert!(l.resolve(Path::new(bad)).is_err(), "{bad:?}");
        }
        assert!(l
            .resolve(Path::new("Music/Artist/Album"))
            .unwrap()
            .starts_with(l.root()));
        // A symlinked directory inside the library must not be traversed.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), t.path().join("Music")).unwrap();
        assert!(l.resolve(Path::new("Music/Artist")).is_err());
        assert!(l
            .ensure_dir(Path::new("Music/Artist"), &mut Vec::new())
            .is_err());
        assert!(!outside.path().join("Artist").exists());
    }

    #[test]
    fn unite_fs_tmp_dir_must_be_a_real_directory() {
        let (t, l) = lib();
        let tmp = l.tmp_dir().unwrap();
        assert!(tmp.ends_with(".plippa/tmp") && tmp.is_dir());
        assert_eq!(l.tmp_dir().unwrap(), tmp);
        // `.plippa/tmp` replaced by a symlink to elsewhere: refused.
        let outside = tempfile::tempdir().unwrap();
        fs::remove_dir(&tmp).unwrap();
        std::os::unix::fs::symlink(outside.path(), &tmp).unwrap();
        assert!(l.tmp_dir().is_err());
        drop(t);
    }

    #[test]
    fn unite_fs_moves_never_overwrite_and_roll_back() {
        let (t, l) = lib();
        let stage = l.tmp_dir().unwrap();
        let a = stage.join("a.mp3");
        let b = stage.join("b.mp3");
        fs::write(&a, b"A").unwrap();
        fs::write(&b, b"B").unwrap();
        let mut tx = Transaction::default();
        let dir = l.ensure_dir(Path::new("Music/X/Y"), tx.dirs()).unwrap();
        tx.move_in(&a, &dir.join("one.mp3")).unwrap();
        // Existing target: refused, original untouched.
        fs::write(dir.join("two.mp3"), b"KEEP").unwrap();
        assert!(matches!(
            tx.move_in(&b, &dir.join("two.mp3")),
            Err(FsError::Exists(_))
        ));
        assert_eq!(fs::read(dir.join("two.mp3")).unwrap(), b"KEEP");
        assert!(b.exists());
        // Roll back: `one.mp3` returns to staging; dirs created by the tx are removed when empty.
        tx.rollback();
        assert!(a.exists() && !dir.join("one.mp3").exists());
        assert!(
            dir.join("two.mp3").exists(),
            "foreign files are never deleted"
        );
        assert!(t.path().join("Music/X/Y").exists(), "non-empty dir stays");
        // A broken-out dangling symlink as target counts as existing.
        std::os::unix::fs::symlink("/nonexistent", stage.join("link.mp3")).unwrap();
        assert!(matches!(
            move_no_replace(&b, &stage.join("link.mp3")),
            Err(FsError::Exists(_))
        ));
    }

    /// Staged files are 0600 temps; in the library they must be readable like any other file
    /// (an ext4 stick plugged into a laptop must not show unreadable media).
    #[test]
    fn unite_fs_moved_in_files_become_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let (_t, l) = lib();
        let stage = l.tmp_dir().unwrap();
        let a = stage.join("a.mp3");
        fs::write(&a, b"A").unwrap();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o600)).unwrap();
        let mut tx = Transaction::default();
        let dir = l.ensure_dir(Path::new("Music/X/Y"), tx.dirs()).unwrap();
        tx.move_in(&a, &dir.join("one.mp3")).unwrap();
        let mode = fs::metadata(dir.join("one.mp3")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn unite_fs_atomic_write_is_new_only() {
        let (t, l) = lib();
        let dir = l.ensure_dir(Path::new("R"), &mut Vec::new()).unwrap();
        let f = dir.join("_r.noudata");
        write_new_atomic(&f, b"{}").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"{}");
        assert!(matches!(
            write_new_atomic(&f, b"x"),
            Err(FsError::Exists(_))
        ));
        assert_eq!(fs::read(&f).unwrap(), b"{}");
        let names: Vec<_> = l.existing_names(Path::new("R"));
        assert_eq!(
            names,
            vec!["_r.noudata".to_string()],
            "no temp file left behind"
        );
        drop(t);
    }

    #[test]
    fn unite_fs_staged_sources_must_be_plain_files_in_staging() {
        let (t, l) = lib();
        let stage = l.tmp_dir().unwrap();
        let ok = stage.join("x.mp3");
        fs::write(&ok, b"x").unwrap();
        l.check_staged(&stage, &ok).unwrap();
        let outside = t.path().join("secret.mp3");
        fs::write(&outside, b"s").unwrap();
        assert!(l.check_staged(&stage, &outside).is_err());
        let link = stage.join("l.mp3");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(l.check_staged(&stage, &link).is_err());
        assert!(l.check_staged(&stage, &stage).is_err());
    }
}
