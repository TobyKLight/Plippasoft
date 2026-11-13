use std::path::{Path, PathBuf};

pub fn resolve_media_root(cli_path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = cli_path {
        if p.is_dir() {
            return Ok(p.to_path_buf());
        }
        return Err(format!("media directory not found: {}", p.display()));
    }
    discover_usb_media_root().ok_or_else(|| {
        "no USB media directory found (use --media-dir or mount under /media/)".to_string()
    })
}

/// Find first mounted volume under /media that contains .mp3 files.
pub fn discover_usb_media_root() -> Option<PathBuf> {
    for base in media_search_bases() {
        let entries = std::fs::read_dir(&base).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && directory_contains_mp3(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn media_search_bases() -> Vec<PathBuf> {
    let mut bases = vec![PathBuf::from("/media")];
    if let Ok(user) = std::env::var("USER") {
        bases.push(PathBuf::from(format!("/media/{user}")));
    }
    bases
}

fn directory_contains_mp3(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"))
        })
}

/// `{key}.mp3` on the USB root (case-insensitive filename match).
pub fn find_mp3(media_root: &Path, key: &str) -> Option<PathBuf> {
    if key.is_empty() {
        return None;
    }
    let exact = media_root.join(format!("{key}.mp3"));
    if exact.is_file() {
        return Some(exact);
    }
    let entries = std::fs::read_dir(media_root).ok()?;
    let key_lower = key.to_ascii_lowercase();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let stem = path.file_stem()?.to_str()?;
        if stem.eq_ignore_ascii_case(&key_lower) {
            return Some(path);
        }
    }
    None
}
