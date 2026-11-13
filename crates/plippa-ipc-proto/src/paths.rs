//! Default UDS paths and env overrides.

use std::env;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Environment variable: directory containing IPC sockets.
pub const ENV_IPC_DIR: &str = "PLIPPA_IPC_DIR";
/// Environment variable: absolute path override for the engine listen socket.
pub const ENV_ENGINE_SOCK: &str = "PLIPPA_IPC_ENGINE_SOCK";

extern "C" {
    // libc's `geteuid` (std already links libc); declared here so this crate needs no new dependency.
    fn geteuid() -> u32;
}

/// The effective uid of this process.
fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, has no preconditions and cannot fail.
    unsafe { geteuid() }
}

/// Resolved socket locations for one Plippa runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcPaths {
    pub dir: PathBuf,
    pub engine_sock: PathBuf,
}

impl IpcPaths {
    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let engine_sock = dir.join("engine.sock");
        Self { dir, engine_sock }
    }

    /// Prefer `{IPC_DIR}/shutdown` so lab runs do not collide under `/tmp`.
    pub fn shutdown_path(&self) -> PathBuf {
        self.dir.join("shutdown")
    }

    /// PWRSW_01: request *and* power the host off (power-button hold). A plain
    /// `shutdown` sentinel only stops the app.
    pub fn poweroff_path(&self) -> PathBuf {
        self.dir.join("poweroff")
    }
}

/// Resolve IPC paths from the environment (see `docs/design/ipc.md`).
pub fn resolve_paths() -> IpcPaths {
    if let Ok(p) = env::var(ENV_ENGINE_SOCK) {
        if !p.is_empty() {
            let engine_sock = PathBuf::from(p);
            let dir = engine_sock
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            return IpcPaths { dir, engine_sock };
        }
    }
    if let Ok(dir) = env::var(ENV_IPC_DIR) {
        if !dir.is_empty() {
            return IpcPaths::from_dir(dir);
        }
    }
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return IpcPaths::from_dir(PathBuf::from(xdg).join("plippa"));
        }
    }
    let run = PathBuf::from("/run/plippa");
    if run.is_dir() {
        return IpcPaths::from_dir(run);
    }
    // Last resort (tests / desktop). The name is derived from the effective uid, not from the
    // pid, so it is stable; `ensure_ipc_dir` creates it 0700 and refuses a directory that another
    // user (or a symlink) already put there, so a predictable name cannot be squatted.
    IpcPaths::from_dir(env::temp_dir().join(format!("plippa-{}", current_euid())))
}

fn refuse(dir: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("IPC directory {} refused: {why}", dir.display()),
    )
}

/// Create `dir` `0700` if missing; if it exists it must be a real directory (not a symlink)
/// owned by `euid` and not a shared (sticky) directory; group/other permission bits on a
/// directory we own are removed. Parents are created with the default mode.
fn secure_dir_as(dir: &Path, euid: u32) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(refuse(dir, "it is a symlink"));
    }
    if !meta.is_dir() {
        return Err(refuse(dir, "it is not a directory"));
    }
    if meta.uid() != euid {
        return Err(refuse(
            dir,
            &format!("owned by uid {} but this process runs as uid {euid}", meta.uid()),
        ));
    }
    let mode = meta.mode();
    if mode & 0o1000 != 0 {
        return Err(refuse(dir, "it is a shared (sticky) directory, not a private one"));
    }
    if mode & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let again = fs::symlink_metadata(dir)?;
        if again.mode() & 0o077 != 0 || again.uid() != euid {
            return Err(refuse(dir, "could not restrict it to the current user"));
        }
    }
    Ok(())
}

/// Create the IPC directory `0700` (and parents), or verify and tighten an existing one;
/// refuses a symlink or a directory owned by another user. Does not remove an existing socket.
pub fn ensure_ipc_dir(paths: &IpcPaths) -> io::Result<()> {
    secure_dir_as(&paths.dir, current_euid())
}

/// Remove a stale socket file if present (before bind).
pub fn remove_stale_sock(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// UI / tests: ask the launcher to stop children (and optionally power off).
pub fn request_shutdown(paths: &IpcPaths) -> io::Result<()> {
    fs::write(paths.shutdown_path(), b"1")
}

/// PWRSW_01: the hold-to-power path. Stops the children *and* asks the host to power off.
/// Written only by the UI after the 4 s press; a deploy or `systemctl stop` must never do this.
pub fn request_poweroff(paths: &IpcPaths) -> io::Result<()> {
    fs::write(paths.poweroff_path(), b"1")
}

/// True when the IPC-dir `shutdown` or `poweroff` sentinel exists. Both mean "stop the
/// children". Nothing outside the (private) IPC directory is consulted.
pub fn shutdown_requested(paths: &IpcPaths) -> bool {
    paths.shutdown_path().is_file() || paths.poweroff_path().is_file()
}

/// True when the launcher should power the host off after the children are gone.
pub fn poweroff_requested(paths: &IpcPaths) -> bool {
    paths.poweroff_path().is_file()
}

/// Clear both shutdown sentinels after the launcher has acted.
pub fn clear_shutdown(paths: &IpcPaths) {
    let _ = fs::remove_file(paths.shutdown_path());
    let _ = fs::remove_file(paths.poweroff_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_dir_joins_engine_sock() {
        let p = IpcPaths::from_dir("/tmp/plippa-test");
        assert_eq!(p.engine_sock, PathBuf::from("/tmp/plippa-test/engine.sock"));
    }

    #[test]
    fn shutdown_sentinel_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = IpcPaths::from_dir(tmp.path());
        assert!(!shutdown_requested(&paths));
        request_shutdown(&paths).unwrap();
        assert!(shutdown_requested(&paths));
        assert!(!poweroff_requested(&paths), "plain shutdown must not power off");
        clear_shutdown(&paths);
        assert!(!paths.shutdown_path().exists());
    }

    #[test]
    fn poweroff_sentinel_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = IpcPaths::from_dir(tmp.path());
        request_poweroff(&paths).unwrap();
        assert!(shutdown_requested(&paths), "power-off also stops the children");
        assert!(poweroff_requested(&paths));
        clear_shutdown(&paths);
        assert!(!paths.poweroff_path().exists());
        assert!(!poweroff_requested(&paths));
    }

    const LEGACY_LITERAL: &str = "/tmp/plippa-shutdown";

    #[test]
    fn a_file_at_tmp_plippa_shutdown_is_ignored() {
        // Created only if absent, and removed afterwards only if this test made it.
        let made = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(LEGACY_LITERAL)
            .is_ok();
        let tmp = tempfile::tempdir().unwrap();
        let paths = IpcPaths::from_dir(tmp.path());
        let requested = shutdown_requested(&paths);
        clear_shutdown(&paths);
        let survived = Path::new(LEGACY_LITERAL).exists();
        if made {
            let _ = fs::remove_file(LEGACY_LITERAL);
        }
        assert!(!requested, "a world-writable /tmp file must not stop the launcher");
        if made {
            assert!(survived, "clear_shutdown must not touch /tmp/plippa-shutdown");
        }
    }

    fn mode_of(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().mode() & 0o7777
    }

    #[test]
    fn the_ipc_dir_is_created_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("plippa");
        let paths = IpcPaths::from_dir(&dir);
        let old = unsafe { libc_umask(0) };
        let r = ensure_ipc_dir(&paths);
        unsafe { libc_umask(old) };
        r.unwrap();
        assert_eq!(mode_of(&dir), 0o700, "must be private even under umask 0");
    }

    extern "C" {
        #[link_name = "umask"]
        fn libc_umask(mask: u32) -> u32;
    }

    #[test]
    fn a_foreign_owned_ipc_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plippa");
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let me = current_euid();
        // The same directory seen by a process running as another user.
        let err = secure_dir_as(&dir, me.wrapping_add(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("owned by uid"), "{err}");
        // Real chown (works when the sandbox runs as root).
        if me == 0 {
            std::os::unix::fs::chown(&dir, Some(4242), None).unwrap();
            let err = ensure_ipc_dir(&IpcPaths::from_dir(&dir)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn a_symlinked_ipc_dir_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("elsewhere");
        DirBuilder::new().mode(0o700).create(&real).unwrap();
        let link = tmp.path().join("plippa");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = ensure_ipc_dir(&IpcPaths::from_dir(&link)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn a_group_writable_ipc_dir_we_own_is_tightened() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plippa");
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
        ensure_ipc_dir(&IpcPaths::from_dir(&dir)).unwrap();
        assert_eq!(mode_of(&dir), 0o700);
    }

    #[test]
    fn a_shared_sticky_directory_is_never_chmodded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shared");
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(ensure_ipc_dir(&IpcPaths::from_dir(&dir)).is_err());
        assert_eq!(mode_of(&dir), 0o1777, "a shared directory must be left alone");
    }

    #[test]
    fn the_fallback_dir_is_not_pid_named_and_is_ownership_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let name = format!("plippa-{}", current_euid());
        let pid_name = format!("plippa-{}", std::process::id());
        // Resolve with every higher-priority source cleared and a private TMPDIR.
        let saved: Vec<_> = [ENV_ENGINE_SOCK, ENV_IPC_DIR, "XDG_RUNTIME_DIR", "TMPDIR"]
            .iter()
            .map(|k| (*k, env::var_os(k)))
            .collect();
        for (k, _) in &saved {
            env::remove_var(k);
        }
        env::set_var("TMPDIR", tmp.path());
        let resolved = if Path::new("/run/plippa").is_dir() {
            None
        } else {
            Some(resolve_paths())
        };
        for (k, v) in saved {
            match v {
                Some(v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        }
        if let Some(p) = resolved {
            assert_eq!(p.dir, tmp.path().join(&name));
            assert_ne!(p.dir.file_name().unwrap().to_string_lossy(), pid_name);
            ensure_ipc_dir(&p).unwrap();
            assert_eq!(mode_of(&p.dir), 0o700);
            // Squatter: same name pre-created by another uid is refused.
            assert!(secure_dir_as(&p.dir, current_euid().wrapping_add(1)).is_err());
        }
    }
}
