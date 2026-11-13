//! Hardened file access for paths that come from a stick, a portal upload or any other
//! place somebody else controls (`SECFIX_05`).
//!
//! Three rules, applied once here so every consumer gets them:
//!
//! 1. **Never block on a special file.** Every open is `O_NONBLOCK` (a FIFO named like a
//!    media file opens at once instead of waiting for a writer) and the opened handle is
//!    `fstat`ed: anything but a regular file is refused. The check is on the handle that is
//!    then read, so there is no check/use gap.
//! 2. **Never follow a symlink.** The final component is opened with `O_NOFOLLOW`;
//!    [`open_beneath`] additionally refuses a symlink anywhere on the way and cannot leave
//!    its root (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, with a
//!    component-by-component `O_NOFOLLOW` walk on kernels without `openat2`).
//! 3. **Bound every read.** [`read_regular_capped`] reads at most `cap + 1` bytes and refuses
//!    a longer file instead of truncating it.
//!
//! Refusals are `io::ErrorKind::InvalidData` (symlink, FIFO, device, directory, escape,
//! oversize); a path that is simply not there keeps `NotFound`.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn refused(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| refused("path contains a NUL byte"))
}

/// Map the errno values that mean "this is a symlink / leaves the root" to a refusal.
fn classify(err: io::Error) -> io::Error {
    match err.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::EXDEV) => refused("symlink or path leaving the root"),
        _ => err,
    }
}

fn fd_from(raw: libc::c_int) -> io::Result<OwnedFd> {
    if raw < 0 {
        Err(classify(io::Error::last_os_error()))
    } else {
        // SAFETY: `raw` is a freshly opened descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

/// `fstat` the handle: only a regular file passes; the handle is made blocking again so
/// ordinary reads and decoders behave normally.
fn finish_regular(fd: OwnedFd) -> io::Result<File> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is a valid descriptor and `st` a valid out-pointer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(refused("not a regular file"));
    }
    // SAFETY: plain flag manipulation on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    Ok(File::from(fd))
}

const OPEN_FILE_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY;

/// Open `path` read-only as an already-checked **regular file**: no symlink on the last
/// component, never blocks, `fstat` verified on the returned handle.
pub fn open_regular_nofollow(path: &Path) -> io::Result<File> {
    let c = c_path(path)?;
    // SAFETY: `c` is a valid NUL-terminated string.
    let fd = fd_from(unsafe { libc::open(c.as_ptr(), OPEN_FILE_FLAGS) })?;
    finish_regular(fd)
}

/// Open a library media file for the decoder (`SECFIX_05`): the **one** open whose handle is
/// decoded, so there is no check-then-reopen gap.
///
/// When `path` is inside `root` — the library root the path was resolved against — the open is
/// [`open_beneath`]: every component is resolved without following a symlink (`openat2` with
/// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, or the component-wise `O_NOFOLLOW` walk) and the
/// returned descriptor cannot escape the root, even if a directory on the way has been swapped
/// for a symlink since the path was checked.
///
/// A path outside `root` (a test fixture, an upload staged elsewhere) keeps the plain
/// [`open_regular_nofollow`] open: there is no root to confine it to.
pub fn open_media_for_decode(root: &Path, path: &Path) -> io::Result<File> {
    if root.as_os_str().is_empty() {
        return open_regular_nofollow(path);
    }
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => return open_regular_nofollow(path),
    };
    // Byte paths, not `str`: a media file may carry a name the platform cannot decode, and
    // confinement must not be dropped just because the name is not readable (`SECFIX_05`).
    use std::os::unix::ffi::OsStrExt;
    let rel = rel.as_os_str().as_bytes();
    if rel.is_empty() {
        // The root itself: a directory, refused by the regular-file check below.
        return open_regular_nofollow(path);
    }
    open_beneath_bytes(root, rel)
}

/// [`open_media_for_decode`] for a machine with several library roots (multi-USB): the longest
/// root `path` is under is the one it is confined to. A path under none of them keeps the
/// plain `O_NOFOLLOW` open.
pub fn open_media_for_decode_in(roots: &[std::path::PathBuf], path: &Path) -> io::Result<File> {
    let best = roots
        .iter()
        .filter(|r| !r.as_os_str().is_empty() && path.starts_with(r))
        .max_by_key(|r| r.as_os_str().len());
    match best {
        Some(root) => open_media_for_decode(root, path),
        None => open_regular_nofollow(path),
    }
}

/// Read everything from an open file, at most `cap` bytes; a longer file is refused
/// (`InvalidData`) rather than truncated.
pub fn read_capped(file: File, cap: u64) -> io::Result<Vec<u8>> {
    if file.metadata()?.len() > cap {
        return Err(refused("file is larger than the allowed size"));
    }
    let mut out = Vec::new();
    file.take(cap + 1).read_to_end(&mut out)?;
    if out.len() as u64 > cap {
        return Err(refused("file is larger than the allowed size"));
    }
    Ok(out)
}

/// [`open_regular_nofollow`] + [`read_capped`].
pub fn read_regular_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    read_capped(open_regular_nofollow(path)?, cap)
}

fn open_root_dir(root: &Path) -> io::Result<OwnedFd> {
    let c = c_path(root)?;
    // SAFETY: valid C string.
    fd_from(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })
}

/// `rel` must be a plain `/`-separated relative path of normal components.
fn check_plain_relative(rel: &[u8]) -> io::Result<()> {
    if rel.is_empty()
        || rel.starts_with(b"/")
        || rel.contains(&0)
        || rel.split(|b| *b == b'/')
            .any(|s| s.is_empty() || s == b"." || s == b"..")
    {
        return Err(refused("not a plain relative path"));
    }
    Ok(())
}

/// Open `rel` below `root` as an already-checked regular file, refusing any symlink on the
/// way and any escape from `root`. `rel` is library-relative with `/` separators and no
/// `.`/`..` components (resolve those lexically first).
pub fn open_beneath(root: &Path, rel: &str) -> io::Result<File> {
    open_beneath_bytes(root, rel.as_bytes())
}

/// [`open_beneath`] for a relative path that need not be UTF-8: a media file may have a name
/// the platform does not decode, and confinement must not depend on the name being readable.
pub fn open_beneath_bytes(root: &Path, rel: &[u8]) -> io::Result<File> {
    check_plain_relative(rel)?;
    let dir = open_root_dir(root)?;
    match open_beneath_openat2(&dir, rel) {
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) => {
            open_beneath_walk(&dir, rel)
        }
        other => other,
    }
}

fn open_beneath_openat2(dir: &OwnedFd, rel: &[u8]) -> io::Result<File> {
    let c = CString::new(rel).map_err(|_| refused("path contains a NUL byte"))?;
    // `open_how` is `#[non_exhaustive]` in libc: start from zero and set the fields.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = OPEN_FILE_FLAGS as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
    // SAFETY: `how` outlives the call and its size is passed; `c` is a valid C string.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            c.as_ptr(),
            &how as *const libc::open_how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    finish_regular(fd_from(raw as libc::c_int)?)
}

/// Component-wise `O_NOFOLLOW` walk: the fallback for kernels without `openat2`, kept
/// reachable (and tested for parity) rather than assumed.
fn open_beneath_walk(dir: &OwnedFd, rel: &[u8]) -> io::Result<File> {
    let parts: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
    let mut current: Option<OwnedFd> = None;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let c = CString::new(*part).map_err(|_| refused("path contains a NUL byte"))?;
        let parent = current.as_ref().unwrap_or(dir).as_raw_fd();
        let flags = if last {
            OPEN_FILE_FLAGS
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        // SAFETY: valid descriptor and C string.
        let fd = fd_from(unsafe { libc::openat(parent, c.as_ptr(), flags) }).map_err(|e| {
            // A symlink in a directory position is `ENOTDIR` with `O_DIRECTORY|O_NOFOLLOW`.
            if !last && e.raw_os_error() == Some(libc::ENOTDIR) {
                refused("symlink or non-directory on the path")
            } else {
                e
            }
        })?;
        if last {
            return finish_regular(fd);
        }
        current = Some(fd);
    }
    Err(refused("empty path"))
}

/// Result of [`try_lock_range_exclusive`].
#[derive(Debug)]
pub enum RangeLock {
    /// The file does not exist.
    Absent,
    /// Another process holds a conflicting lock on that range. `exclusive` says whether the
    /// holder has it exclusively (a writer preparing the file) or shared (a live reader).
    Contended { exclusive: bool },
    /// The range is locked until this handle is dropped.
    Acquired(File),
}

/// Try to take a non-blocking exclusive POSIX byte-range lock on `[start, start + len)` of
/// `path`. Used to find out whether any other process still has an SQLite index open (every
/// open SQLite WAL connection holds a shared lock on one byte of the `-shm` file).
///
/// POSIX locks belong to the process: the probe says nothing about other connections in the
/// calling process, and closing any descriptor of the file drops the lock. Callers keep the
/// returned file alive for as long as they need the lock.
pub fn try_lock_range_exclusive(path: &Path, start: u64, len: u64) -> io::Result<RangeLock> {
    let c = c_path(path)?;
    // SAFETY: valid C string.
    let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
    if raw < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::NotFound {
            Ok(RangeLock::Absent)
        } else {
            Err(classify(e))
        };
    }
    // SAFETY: freshly opened descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as _;
    fl.l_whence = libc::SEEK_SET as _;
    fl.l_start = start as _;
    fl.l_len = len as _;
    // SAFETY: valid descriptor and flock struct.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETLK, &fl) } == 0 {
        return Ok(RangeLock::Acquired(File::from(fd)));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EAGAIN) | Some(libc::EACCES) => {
            // Who holds it, and how? (The conflicting lock may be gone again by now: then it
            // reads as shared and the caller simply retries its policy.)
            let mut probe: libc::flock = unsafe { std::mem::zeroed() };
            probe.l_type = libc::F_WRLCK as _;
            probe.l_whence = libc::SEEK_SET as _;
            probe.l_start = start as _;
            probe.l_len = len as _;
            // SAFETY: valid descriptor and flock struct.
            let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETLK, &mut probe) };
            let exclusive = rc == 0 && probe.l_type == libc::F_WRLCK as libc::c_short;
            Ok(RangeLock::Contended { exclusive })
        }
        _ => Err(e),
    }
}

/// Result of [`lock_file_exclusive`].
#[derive(Debug)]
pub enum FileLock {
    /// The range is locked until this handle is dropped.
    Held(File),
    /// The volume is mounted read-only (`EROFS`): nothing was locked.
    ReadOnly,
    /// The file system cannot lock (`ENOLCK`, `EOPNOTSUPP`, ...): nothing was locked.
    Unsupported,
}

/// Create `path` if needed (regular file, never a symlink) and take an exclusive POSIX
/// byte-range lock on `[start, start + len)`, waiting up to `wait` for another process to
/// release it (`ErrorKind::TimedOut` after that). `ReadOnly` on `EROFS`, `Unsupported` when
/// the file system cannot lock; `EACCES`/`EPERM` on open are returned as `PermissionDenied`
/// (the caller decides whether that is a read-only tree or a file it may replace). Closing the returned handle releases the lock, and only
/// locks on this file: use a dedicated file, never one SQLite also uses.
pub fn lock_file_exclusive(
    path: &Path,
    start: u64,
    len: u64,
    wait: std::time::Duration,
) -> io::Result<FileLock> {
    let c = c_path(path)?;
    // SAFETY: valid C string.
    let raw = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600 as libc::c_uint,
        )
    };
    if raw < 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EROFS) => Ok(FileLock::ReadOnly),
            _ => Err(classify(e)),
        };
    }
    // SAFETY: freshly opened descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // Only a regular file can be locked meaningfully (a FIFO or directory is hostile).
    let file = finish_regular(fd)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        fl.l_start = start as _;
        fl.l_len = len as _;
        // SAFETY: valid descriptor and flock struct.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &fl) } == 0 {
            return Ok(FileLock::Held(file));
        }
        let e = io::Error::last_os_error();
        if matches!(
            e.raw_os_error(),
            Some(libc::ENOLCK) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::EINVAL)
        ) {
            return Ok(FileLock::Unsupported);
        }
        if !matches!(e.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EACCES)) {
            return Err(e);
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "lock wait timed out"));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn tmp() -> tempfile_dir::Dir {
        tempfile_dir::Dir::new()
    }

    /// Minimal self-cleaning temp dir (plippa-core has no `tempfile` dev-dependency).
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "plippa-safe-fs-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn mkfifo(path: &Path) {
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    fn is_refused(r: &io::Result<File>) -> bool {
        matches!(r, Err(e) if e.kind() == io::ErrorKind::InvalidData)
    }

    #[test]
    fn a_regular_file_opens_and_reads() {
        let d = tmp();
        fs::write(d.path().join("a.bin"), b"hello").unwrap();
        assert_eq!(
            read_regular_capped(&d.path().join("a.bin"), 100).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let d = tmp();
        let p = d.path().join("f");
        mkfifo(&p);
        assert!(is_refused(&open_regular_nofollow(&p)));
    }

    #[test]
    fn a_symlink_is_refused_even_to_a_regular_file() {
        let d = tmp();
        fs::write(d.path().join("real"), b"x").unwrap();
        symlink(d.path().join("real"), d.path().join("link")).unwrap();
        assert!(is_refused(&open_regular_nofollow(&d.path().join("link"))));
    }

    #[test]
    fn dev_zero_and_directories_are_refused() {
        assert!(is_refused(&open_regular_nofollow(Path::new("/dev/zero"))));
        let d = tmp();
        assert!(is_refused(&open_regular_nofollow(d.path())));
    }

    #[test]
    fn an_over_cap_file_is_refused_not_truncated() {
        let d = tmp();
        let p = d.path().join("big");
        fs::write(&p, vec![7u8; 101]).unwrap();
        let e = read_regular_capped(&p, 100).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_regular_capped(&p, 101).unwrap().len(), 101);
    }

    #[test]
    fn a_sparse_file_over_the_cap_is_refused_without_reading_it() {
        let d = tmp();
        let p = d.path().join("sparse");
        let f = fs::File::create(&p).unwrap();
        f.set_len(1 << 40).unwrap(); // 1 TiB hole, no blocks
        drop(f);
        let started = std::time::Instant::now();
        assert!(read_regular_capped(&p, 8 * 1024 * 1024).is_err());
        assert!(started.elapsed().as_secs() < 2);
    }

    #[test]
    fn a_missing_file_stays_not_found() {
        let d = tmp();
        let e = open_regular_nofollow(&d.path().join("nope")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    fn beneath_cases(root: &Path, open: &dyn Fn(&Path, &str) -> io::Result<File>) {
        fs::create_dir_all(root.join("Music/Album")).unwrap();
        fs::write(root.join("Music/Album/01.mp3"), b"x").unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        let outside = root.parent().unwrap().join("outside-secret");
        fs::write(&outside, b"secret").unwrap();
        symlink(&outside, root.join("Music/escape.mp3")).unwrap();
        symlink("/etc", root.join("etcdir")).unwrap();
        symlink(root.join("other"), root.join("Music/otherdir")).unwrap();
        symlink(root.join("Music/Album/01.mp3"), root.join("Music/inside.mp3")).unwrap();
        mkfifo(&root.join("Music/pipe.mp3"));

        assert!(open(root, "Music/Album/01.mp3").is_ok());
        for bad in [
            "Music/escape.mp3",       // symlink out of the root
            "etcdir/passwd",          // symlinked directory
            "Music/otherdir/x",       // symlinked directory inside the root
            "Music/inside.mp3",       // even an in-root symlink is refused
            "Music/pipe.mp3",         // fifo
            "Music/Album",            // directory
        ] {
            assert!(open(root, bad).is_err(), "{bad}");
        }
        assert_eq!(
            open(root, "Music/Album/missing.mp3").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a/./b", "a\0b"] {
            assert!(open(root, bad).is_err(), "{bad:?}");
        }
        fs::remove_file(outside).unwrap();
    }

    #[test]
    fn open_beneath_refuses_symlinks_and_special_files() {
        let d = tmp();
        let root = d.path().join("root");
        fs::create_dir_all(&root).unwrap();
        beneath_cases(&root, &|r, p| open_beneath(r, p));
    }

    #[test]
    fn the_component_walk_fallback_behaves_like_openat2() {
        let d = tmp();
        let root = d.path().join("root");
        fs::create_dir_all(&root).unwrap();
        beneath_cases(&root, &|r, p| {
            check_plain_relative(p.as_bytes())?;
            let dir = open_root_dir(r)?;
            open_beneath_walk(&dir, p.as_bytes())
        });
    }

    #[test]
    fn a_media_open_inside_the_root_does_not_follow_a_swapped_directory() {
        // SECFIX_05: the decoder opens the file itself, so the *open* must be the check.
        // A directory on the way replaced by a symlink between "path resolved" and "decoder
        // opens" must be refused, not followed out of the library.
        let d = tmp();
        let root = d.path().join("lib");
        let outside = d.path().join("outside");
        fs::create_dir_all(root.join("Music")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("Music/a.mp3"), b"inside").unwrap();
        fs::write(outside.join("a.mp3"), b"outside").unwrap();
        let target = root.join("Music/a.mp3");

        // The ordinary case: inside the root, opens and is the inside file.
        let mut f = open_media_for_decode(&root, &target).unwrap();
        let mut got = String::new();
        f.read_to_string(&mut got).unwrap();
        assert_eq!(got, "inside");

        // Swap `Music` for a link to the outside copy, exactly the race a check-then-reopen
        // would lose: the confined open must refuse instead of reading the outside file.
        fs::remove_dir_all(root.join("Music")).unwrap();
        symlink(&outside, root.join("Music")).unwrap();
        let err = open_media_for_decode(&root, &target).unwrap_err();
        assert!(
            matches!(err.kind(), io::ErrorKind::InvalidData | io::ErrorKind::NotFound),
            "unexpected error: {err:?}"
        );

        symlink(&target, root.join("link.mp3")).unwrap();
        assert!(open_media_for_decode(&root, &root.join("link.mp3")).is_err());
    }

    #[test]
    fn a_media_open_outside_the_root_keeps_the_plain_nofollow_open() {
        let d = tmp();
        let root = d.path().join("lib");
        fs::create_dir_all(&root).unwrap();
        let staged = d.path().join("staged.mp3");
        fs::write(&staged, b"data").unwrap();
        // Not under the root: still opened (it is where the caller staged it), still O_NOFOLLOW.
        assert!(open_media_for_decode(&root, &staged).is_ok());
        let link = d.path().join("link.mp3");
        symlink(&staged, &link).unwrap();
        assert!(open_media_for_decode(&root, &link).is_err());
    }

    #[test]
    fn the_multi_root_open_picks_the_longest_containing_root() {
        // Multi-USB: two sticks, one mounted inside the other's path tree. A file must be
        // confined to the innermost root that contains it, not whichever came first.
        let d = tmp();
        let outer = d.path().join("outer");
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("a.mp3"), b"inner").unwrap();
        let path = inner.join("a.mp3");
        let roots = vec![outer.clone(), inner.clone()];
        let mut f = open_media_for_decode_in(&roots, &path).unwrap();
        let mut got = String::new();
        f.read_to_string(&mut got).unwrap();
        assert_eq!(got, "inner");
        // Under no root at all: the plain open, still refusing a symlink.
        let outside = d.path().join("loose.mp3");
        fs::write(&outside, b"loose").unwrap();
        assert!(open_media_for_decode_in(&roots, &outside).is_ok());
    }

    #[test]
    fn a_non_utf8_media_name_inside_the_root_is_still_confined() {
        // A file name the platform does not decode must not downgrade the open to the
        // last-component check: confinement works on bytes, not on `str`.
        use std::os::unix::ffi::OsStrExt;
        let d = tmp();
        let root = d.path().join("lib");
        let outside = d.path().join("outside");
        fs::create_dir_all(root.join("Music")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let name = std::ffi::OsStr::from_bytes(b"\xff\xfe-caf\xe9.wav");
        fs::write(root.join("Music").join(name), b"inside").unwrap();
        fs::write(outside.join(name), b"outside").unwrap();
        let target = root.join("Music").join(name);
        let mut f = open_media_for_decode(&root, &target).unwrap();
        let mut got = String::new();
        f.read_to_string(&mut got).unwrap();
        assert_eq!(got, "inside");
        // Swap `Music` for a link out of the root: the byte path must be refused, not followed.
        fs::remove_dir_all(root.join("Music")).unwrap();
        symlink(&outside, root.join("Music")).unwrap();
        assert!(
            open_media_for_decode(&root, &target).is_err(),
            "a non-UTF-8 name must not escape the root"
        );
    }

    #[test]
    fn the_range_lock_probe_sees_absent_and_acquires() {        let d = tmp();
        let p = d.path().join("shm");
        assert!(matches!(
            try_lock_range_exclusive(&p, 128, 1).unwrap(),
            RangeLock::Absent
        ));
        fs::write(&p, vec![0u8; 256]).unwrap();
        let held = try_lock_range_exclusive(&p, 128, 1).unwrap();
        assert!(matches!(held, RangeLock::Acquired(_)));
    }
}
