//! Removable media library discovery (already-mounted volumes).
//!
//! Desk default on Raspberry Pi OS: mounts under `/media/<user>/…`.
//! We **do not** call `mount`/`udisks` — the OS (udisks/gvfs) mounts USB sticks;
//! Plippa only enumerates usable directories.
//!
//! Override: when `--library-root` / `PLIPPA_LIBRARY_ROOT` is set, callers skip
//! discovery and use that path only.

use crate::playable_id::library_display_label;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// One discovered (or configured) library root candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredLibrary {
    pub path: PathBuf,
    /// Volume label / mount folder basename for portal display.
    pub label: String,
}

/// Identity of the filesystem that is *currently* mounted at a library root.
///
/// A USB stick that is unplugged and plugged back in — even when udisks mounts it at the
/// same path — is a different mount. A SQLite connection opened before the unplug keeps
/// referring to the old (now disconnected) filesystem: reads can come from stale cache and
/// writes fail with `database disk image is malformed` for the rest of the process lifetime.
/// Comparing this identity across polls is how the server and UI notice that their index
/// handles are stale and must be closed and reopened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountIdentity {
    /// `st_dev` of the root directory (a fresh superblock gets a fresh device id).
    dev: u64,
    /// `st_ino` of the root directory (catches a recreated directory on the same device).
    ino: u64,
    /// Mount ID from `/proc/self/mountinfo`; `0` when `path` is not itself a mount point.
    /// This catches a remount that happens to reuse both the device number and root inode.
    mount_id: u64,
}

impl MountIdentity {
    /// Capture the identity of `path`, or `None` when it cannot be read (absent, or the
    /// filesystem is already gone).
    pub fn of(path: &Path) -> Option<Self> {
        #[cfg(unix)]
        {
            let meta = fs::metadata(path).ok()?;
            Some(Self {
                dev: meta.dev(),
                ino: meta.ino(),
                mount_id: mount_id_at(path).unwrap_or(0),
            })
        }
        // Non-unix build: no stable directory identity available, so mount checks never fire
        // (callers fall back to the existence check).
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }

    /// True while `path` is still the exact same mount this identity was captured from.
    pub fn still_valid(&self, path: &Path) -> bool {
        Self::of(path).as_ref() == Some(self)
    }
}

/// Mount ID of the mount whose target is `path`, from the current `/proc/self/mountinfo`.
fn mount_id_at(path: &Path) -> Option<u64> {
    let text = fs::read_to_string("/proc/self/mountinfo").ok()?;
    mount_id_in(&text, path)
}

/// [`mount_id_at`] against explicit mountinfo text (unit-testable).
///
/// Fields are `id parent major:minor root mount_point ... - fstype source superopts`;
/// the kernel octal-escapes whitespace in the target (see [`unescape_mount_target`]).
fn mount_id_in(mountinfo: &str, path: &Path) -> Option<u64> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    for line in mountinfo.lines() {
        let mut fields = line.split_whitespace();
        let Some(id) = fields.next().and_then(|f| f.parse::<u64>().ok()) else {
            continue;
        };
        let (_parent, _dev, _root) = match (fields.next(), fields.next(), fields.next()) {
            (Some(p), Some(d), Some(r)) => (p, d, r),
            _ => continue,
        };
        let Some(target) = fields.next() else {
            continue;
        };
        if Path::new(&unescape_mount_target(target)) == canon {
            return Some(id);
        }
    }
    None
}

/// Resolve library roots for desk / CI use.
///
/// - If `override_root` is `Some`, returns that path alone (when it exists as a dir).
/// - Otherwise enumerates conventional mount points and keeps readable directories.
///
/// Empty / brand-new sticks are valid (no `.plippa/` yet) — they only need to be
/// directories we can read. Index creation happens on first server scan/write.
pub fn resolve_library_roots(override_root: Option<&Path>) -> Vec<DiscoveredLibrary> {
    if let Some(root) = override_root {
        if root.is_dir() {
            return vec![DiscoveredLibrary {
                label: library_display_label(root),
                path: root.to_path_buf(),
            }];
        }
        return Vec::new();
    }
    discover_removable_libraries(&default_mount_parents())
}

/// Conventional parents that hold user USB mounts on Debian / Raspberry Pi OS.
pub fn default_mount_parents() -> Vec<PathBuf> {
    let mut parents = Vec::new();
    if let Ok(user) = std::env::var("USER") {
        if !user.is_empty() {
            parents.push(PathBuf::from(format!("/media/{user}")));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if let Some(name) = Path::new(&home).file_name().and_then(|s| s.to_str()) {
            let p = PathBuf::from(format!("/media/{name}"));
            if !parents.iter().any(|x| x == &p) {
                parents.push(p);
            }
        }
    }
    // Common fallback when USER is unset in some service contexts.
    let plip = PathBuf::from("/media/plip");
    if !parents.iter().any(|x| x == &plip) {
        parents.push(plip);
    }
    parents
}

/// Enumerate immediate children of `parents` that look like usable library roots.
pub fn discover_removable_libraries(parents: &[PathBuf]) -> Vec<DiscoveredLibrary> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for parent in parents {
        let entries = match fs::read_dir(parent) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            // Skip non-mount clutter if we can detect it; still accept plain dirs
            // (tests use temp dirs under a fake parent).
            if !is_usable_library_candidate(&path) {
                continue;
            }
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !seen.insert(canon.clone()) {
                continue;
            }
            out.push(DiscoveredLibrary {
                label: library_display_label(&path),
                path: canon,
            });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label).then(a.path.cmp(&b.path)));
    out
}

fn is_usable_library_candidate(path: &Path) -> bool {
    // Must be listable by this process — skips leftover udisks mountpoint dirs
    // that are root-only (`drwx------ root:root`) after a stick was removed.
    if fs::read_dir(path).is_err() {
        return false;
    }
    // Under `/media/…`, only accept paths that are actually mounted. Empty
    // placeholder directories otherwise look like library roots and confuse the
    // engine into warning about a missing index on a dead path.
    if path.starts_with("/media") {
        return is_path_mounted(path);
    }
    path.is_dir()
}

fn is_path_mounted(path: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        // Cannot tell: accept the directory rather than hide a stick we can read.
        return true;
    };
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    is_mounted_per(mounts.as_str(), &canon)
}

/// Whether `canon` is a mount point in the given `/proc/mounts` text.
fn is_mounted_per(mounts: &str, canon: &Path) -> bool {
    mounts.lines().any(|line| {
        let mut parts = line.split_whitespace();
        let _src = parts.next();
        match parts.next() {
            Some(mnt) => Path::new(&unescape_mount_target(mnt)) == canon,
            None => false,
        }
    })
}

/// Decode the octal escapes the kernel writes into `/proc/mounts` (and mountinfo)
/// target fields: `\040` space, `\011` tab, `\012` newline, `\134` backslash.
///
/// A volume whose label contains a space mounts at e.g. `/media/plip/USB\040DISK`;
/// comparing that field literally against `/media/plip/USB DISK` never matches, so
/// the stick looked unmounted and disappeared from discovery entirely.
fn unescape_mount_target(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i..i + 4);
        match octal {
            Some([b'\\', a, b, c]) if a.is_ascii_digit() && b.is_ascii_digit() && c.is_ascii_digit()
                && *a < b'8' && *b < b'8' && *c < b'8' =>
            {
                out.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
                i += 4;
            }
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    // The kernel only escapes ASCII whitespace/backslash, so decoding cannot break
    // UTF-8 held in the path; the lossy conversion is a belt-and-braces fallback.
    String::from_utf8_lossy(&out).into_owned()
}

/// Discover under explicit fake parents (unit tests).
pub fn discover_under_parents(parents: &[PathBuf]) -> Vec<DiscoveredLibrary> {
    discover_removable_libraries(parents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn override_returns_single_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("only");
        fs::create_dir_all(&root).unwrap();
        let found = resolve_library_roots(Some(&root));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "only");
    }

    #[test]
    fn empty_one_two_fake_mounts() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("media_user");
        fs::create_dir_all(&parent).unwrap();

        assert!(discover_under_parents(&[parent.clone()]).is_empty());

        let a = parent.join("StickA");
        fs::create_dir_all(&a).unwrap();
        let one = discover_under_parents(&[parent.clone()]);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].label, "StickA");

        let b = parent.join("StickB");
        fs::create_dir_all(&b).unwrap();
        let two = discover_under_parents(&[parent.clone()]);
        assert_eq!(two.len(), 2);
        let labels: Vec<_> = two.iter().map(|d| d.label.as_str()).collect();
        assert!(labels.contains(&"StickA"));
        assert!(labels.contains(&"StickB"));
    }

    #[test]
    fn missing_override_yields_empty() {
        let found = resolve_library_roots(Some(Path::new("/no/such/plippa/usb")));
        assert!(found.is_empty());
    }

    /// The kernel escapes whitespace in mount targets as octal. A volume label with a
    /// space (Windows' default `USB DISK` on a fresh exfat format) mounts at
    /// `/media/plip/USB\040DISK`; comparing that literally hid the stick from discovery.
    #[test]
    fn escaped_mount_target_with_space_is_still_a_mount() {
        let mounts = "/dev/mmcblk0p2 / ext4 rw,relatime 0 0\n\
                      /dev/sda1 /media/plip/USB\\040DISK exfat rw,nosuid,nodev 0 0\n";
        assert!(is_mounted_per(mounts, Path::new("/media/plip/USB DISK")));
    }

    #[test]
    fn escaped_mount_target_does_not_match_a_different_path() {
        let mounts = "/dev/sda1 /media/plip/USB\\040DISK exfat rw 0 0\n";
        assert!(!is_mounted_per(mounts, Path::new("/media/plip/USB DISK2")));
        assert!(!is_mounted_per(mounts, Path::new("/media/plip/USBDISK")));
    }

    #[test]
    fn unescaped_mount_target_still_matches() {
        // Regression guard for the space-free case that always worked.
        let mounts = "/dev/sda1 /media/plip/CCD0-7B732 exfat rw 0 0\n";
        assert!(is_mounted_per(mounts, Path::new("/media/plip/CCD0-7B732")));
    }

    /// A stick that is unplugged and plugged back in is a different mount even at the same
    /// path. The old SQLite connection then reports `database disk image is malformed`
    /// forever; this identity is what makes the server/UI reopen it.
    #[test]
    fn mount_identity_changes_when_a_directory_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("stick");
        fs::create_dir_all(&root).unwrap();
        let first = MountIdentity::of(&root).expect("identity of an existing directory");
        assert!(first.still_valid(&root));
        assert!(!first.still_valid(&tmp.path().join("absent")));
        assert!(
            replace_with_new_identity(&root, &first),
            "a re-created root must not keep looking like the old mount"
        );
    }

    /// A real remount gets a fresh filesystem/mount id. A plain directory only gets a fresh
    /// inode, and ext4 hands a freed inode straight back out, so park the old inode on a
    /// decoy directory until the replacement really looks different.
    fn replace_with_new_identity(path: &Path, old: &MountIdentity) -> bool {
        let parent = path.parent().expect("test root has a parent");
        (0..32).any(|attempt| {
            fs::remove_dir_all(path).unwrap();
            fs::create_dir_all(parent.join(format!(".decoy-{attempt}"))).unwrap();
            fs::create_dir_all(path).unwrap();
            MountIdentity::of(path).as_ref() != Some(old)
        })
    }

    #[test]
    fn mount_id_matches_plain_and_escaped_mountinfo_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let canon = root.canonicalize().unwrap();
        let plain = format!(
            "41 1 179:2 / {} rw,relatime - exfat /dev/sda1 rw,uid=1000\n\
             42 1 179:1 / /boot/firmware rw - vfat /dev/mmcblk0p1 rw\n",
            canon.display()
        );
        assert_eq!(mount_id_in(&plain, &root), Some(41));

        let escaped = "7 1 179:2 / /media/plip/USB\\040DISK rw - exfat /dev/sda1 rw\n";
        assert_eq!(
            mount_id_in(escaped, Path::new("/media/plip/USB DISK")),
            Some(7)
        );
        assert_eq!(mount_id_in(escaped, Path::new("/media/plip/other")), None);
    }

    #[test]
    fn mount_target_unescapes_only_octal_escapes() {
        assert_eq!(unescape_mount_target("/media/x/USB\\040DISK"), "/media/x/USB DISK");
        assert_eq!(unescape_mount_target("/media/x/a\\011b"), "/media/x/a\tb");
        assert_eq!(unescape_mount_target("/media/x/a\\012b"), "/media/x/a\nb");
        // A literal backslash in the label arrives as \134; the digits that follow
        // it must not be eaten by a second escape.
        assert_eq!(unescape_mount_target("/media/x/back\\134040slash"), "/media/x/back\\040slash");
        // Ordinary paths are returned untouched.
        assert_eq!(unescape_mount_target("/media/plip/CCD0-7B732"), "/media/plip/CCD0-7B732");
        // A lone backslash at the end cannot form a triplet.
        assert_eq!(unescape_mount_target("/media/x/trail\\"), "/media/x/trail\\");
        assert_eq!(unescape_mount_target("/media/x/nearly\\04"), "/media/x/nearly\\04");
    }
}
