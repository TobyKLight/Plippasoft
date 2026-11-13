//! Media library path helpers (Noubin §4.4).

use std::fs;
use std::path::{Path, PathBuf};

/// Players MUST scan subfolders up to at least this many levels deep (§4.4).
pub const MIN_LIBRARY_SCAN_DEPTH: u32 = 8;

/// A selected media library root (removable volume root or user-chosen folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRoot {
    path: PathBuf,
}

impl LibraryRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// §4.4 — directories whose names start with `.` MUST NOT be scanned
/// (e.g. `.plippa/` player databases).
pub fn dir_excluded_from_scan(dir_name: &str) -> bool {
    dir_name.starts_with('.')
}

/// Walk `root` collecting `.noudata` file paths, skipping `.*` directories.
///
/// `max_depth` counts directory levels below the root (`0` = only the root folder).
/// Use at least [`MIN_LIBRARY_SCAN_DEPTH`] for standard compliance.
pub fn collect_noudata_paths(
    root: &LibraryRoot,
    max_depth: u32,
) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    visit(&root.path, 0, max_depth, &mut out)?;
    Ok(out)
}

fn visit(dir: &Path, depth: u32, max_depth: u32, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if file_type.is_dir() {
            if dir_excluded_from_scan(&name) {
                continue;
            }
            if depth < max_depth {
                visit(&entry.path(), depth + 1, max_depth, out)?;
            }
        } else if file_type.is_file() {
            if name.ends_with(".noudata") {
                out.push(entry.path());
            }
        }
    }
    Ok(())
}
