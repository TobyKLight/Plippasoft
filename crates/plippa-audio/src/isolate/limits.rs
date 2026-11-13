//! Resource limits of an isolated helper process.
//!
//! The parent applies them between `fork` and `exec` (so the helper is bounded from its very
//! first instruction) and passes the same numbers on the command line; the helper re-reads its
//! own limits at start and refuses to run if any is missing, so a helper started by hand, or
//! with a parent that forgot, never decodes a file unconfined.

use std::io;

/// Limits for one helper process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `RLIMIT_AS` in bytes: the cap a hostile length field runs into.
    pub address_space_bytes: u64,
    /// `RLIMIT_CPU` in seconds: a backstop against a decoder that spins, never a flat budget.
    pub cpu_seconds: u64,
    /// `RLIMIT_NOFILE`: the helper needs the media file, its pipes and a spare for index
    /// re-opens, nothing else.
    pub max_open_files: u64,
}

/// A header read: duration and tags of one file.
pub const PROBE_ADDRESS_SPACE: u64 = 256 * 1024 * 1024;
/// Symphonia's real working set is a few MiB (measured, see the SECFIX_06 decision log);
/// the rest is address space for the binary, its stacks and the pipe buffers.
pub const STREAM_ADDRESS_SPACE: u64 = 512 * 1024 * 1024;
const PROBE_CPU_SECONDS: u64 = 30;
/// A stream's CPU backstop: a fixed floor plus time proportional to the file's size, so a
/// multi-hour audiobook is never cut off by a flat budget (about 128 KiB of compressed input
/// per CPU-second is far slower than any real decode, and a session's seeks re-decode).
const STREAM_CPU_FLOOR_SECONDS: u64 = 300;
const STREAM_BYTES_PER_CPU_SECOND: u64 = 128 * 1024;
const HELPER_MAX_OPEN_FILES: u64 = 16;

impl Limits {
    /// Limits for a short header probe.
    pub fn probe() -> Self {
        Self {
            address_space_bytes: PROBE_ADDRESS_SPACE,
            cpu_seconds: PROBE_CPU_SECONDS,
            max_open_files: HELPER_MAX_OPEN_FILES,
        }
    }

    /// Limits for a long decode session over a file of `file_bytes`.
    pub fn stream(file_bytes: u64) -> Self {
        Self {
            address_space_bytes: STREAM_ADDRESS_SPACE,
            cpu_seconds: STREAM_CPU_FLOOR_SECONDS + file_bytes / STREAM_BYTES_PER_CPU_SECOND,
            max_open_files: HELPER_MAX_OPEN_FILES,
        }
    }
}

fn set(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: plain syscall with a valid pointer.
    if unsafe { libc::setrlimit(resource, &lim) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn get(resource: libc::__rlimit_resource_t) -> io::Result<libc::rlimit> {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain syscall with a valid pointer.
    if unsafe { libc::getrlimit(resource, &mut lim) } == 0 {
        Ok(lim)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Apply every limit to the **calling process**. Only async-signal-safe calls (it runs in the
/// forked child before `exec`): no allocation, no locks.
pub fn apply(limits: &Limits) -> io::Result<()> {
    set(libc::RLIMIT_CORE, 0)?;
    set(libc::RLIMIT_NOFILE, limits.max_open_files)?;
    set(libc::RLIMIT_CPU, limits.cpu_seconds)?;
    set(libc::RLIMIT_AS, limits.address_space_bytes)?;
    // SAFETY: prctl with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the calling process runs under limits at least as tight as `limits`
/// (a helper checks this about itself before it touches the media file).
pub fn verify(limits: &Limits) -> bool {
    let tight = |resource, want: u64| {
        get(resource).is_ok_and(|l| l.rlim_cur as u64 <= want && l.rlim_max as u64 <= want)
    };
    // SAFETY: prctl query with constant arguments.
    let no_new_privs = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } == 1;
    no_new_privs
        && tight(libc::RLIMIT_CORE, 0)
        && tight(libc::RLIMIT_NOFILE, limits.max_open_files)
        && tight(libc::RLIMIT_CPU, limits.cpu_seconds)
        && tight(libc::RLIMIT_AS, limits.address_space_bytes)
}

/// Close every descriptor from `first` up: the helper keeps its pipes and the media file and
/// nothing a library or the parent leaked into it.
pub fn close_from(first: i32) {
    // SAFETY: close_range(2) with valid constants; on kernels without it, close one by one.
    unsafe {
        if libc::syscall(libc::SYS_close_range, first as libc::c_uint, libc::c_uint::MAX, 0) != 0 {
            for fd in first..1024 {
                libc::close(fd);
            }
        }
    }
}
