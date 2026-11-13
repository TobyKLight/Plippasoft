//! Consent-gated download of media and images (`NBSecurityRequirements` §1, §2.3).
//!
//! One function, [`download_to_temp`], does a hardened GET and streams the body into a fresh
//! temporary file that the caller chose (always inside the target library's `.plippa/tmp`, so
//! the later rename is atomic on the same volume). It reuses NB_08's pieces: the
//! [`Transport`] (production = `ureq` + rustls with the connect-time address vetting),
//! [`FetchPolicy`] (https only, port 443, SSRF block-list on the first URL and on **every**
//! redirect hop) and the [`Consent`] proof. Rules enforced here:
//!
//! * `Content-Length` above the per-file cap, the batch budget or the free space is refused
//!   before the body is read; the same three limits are enforced **while streaming** because
//!   the header can lie or be absent (chunked).
//! * The file type comes from the first bytes (`sniff_audio` / `sniff_image_kind`), never from
//!   the URL or `Content-Type`; a mismatch aborts on the first chunk. Images are also
//!   dimension-checked from the header (16 MB / 5000 px) without decoding.
//! * A file that fails, is cancelled, times out or is cut off is deleted (drop guard).
//! * There is no total-duration limit (a long audiobook over slow Wi-Fi must be able to arrive):
//!   a transfer is cut off only by silence (the stall timeout, enforced by the socket owner) or by
//!   making too little progress ([`DownloadLimits::min_progress_bytes`] per stall window).
//! * The temp file is created `create_new` + `O_NOFOLLOW`, mode 0600.
//! * Nothing follows redirects to another scheme/host class; no cookies, referrer or
//!   credentials (the transport sends none).

use super::fetch::{Consent, FetchPolicy, Transport, TransportError, WebError};
use crate::unite::sniff::{
    sniff_audio, sniff_image, sniff_image_kind, AudioKind, ImageInfo, ImageKind, MediaSniff,
    SNIFF_HEAD_BYTES,
};
use plippa_core::{audio_format_label, Capabilities};
use plippa_noubin::redact_url_for_log;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Default cap for one media file (override: `PLIPPA_DOWNLOAD_MAX_MB`): 4 GiB minus one byte,
/// the FAT32 file-size ceiling, so nothing a stick can hold is refused by Plippa.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 4 * GIB - 1;
/// Default cap for everything one session downloads/uploads together
/// (override: `PLIPPA_DOWNLOAD_BATCH_MAX_MB`). Disk-fill protection comes from the shared,
/// free-space-aware [`StagingBudget`], not from small fixed caps.
pub const DEFAULT_MAX_BATCH_BYTES: u64 = 16 * GIB;
/// Ceiling no override can exceed.
pub const HARD_MAX_BYTES: u64 = 16 * GIB;
/// §1.27: artists MUST NOT link to images above 16 MB ...
pub const MAX_IMAGE_BYTES: u64 = 16 * MIB;
/// ... or above 5000 x 5000 pixels.
pub const MAX_IMAGE_SIDE: u32 = 5000;
/// Thumbnails are at most 1024 px by spec; a little slack, never the full 5000.
pub const MAX_THUMB_SIDE: u32 = 2048;
/// Smallest free space that must remain after a download or upload. On a big volume the reserve
/// grows with it, see [`DownloadLimits::reserve_for`].
pub const FREE_SPACE_RESERVE: u64 = 64 * MIB;
/// The reserve is this share of the volume (1 / 50 = 2 %) ...
pub const RESERVE_VOLUME_DIVISOR: u64 = 50;
/// ... but never more than this.
pub const RESERVE_CAP: u64 = 2 * GIB;
/// Longest silence tolerated on a download or upload connection. Re-armed by every read that
/// returns data: it is a stall timeout, never a total-duration timeout.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum-progress rule: a transfer must deliver at least this many bytes in every
/// [`STALL_TIMEOUT`] window, so a drip of one byte per 29 s cannot hold a slot for hours while
/// even a poor Wi-Fi link (10 kB/s is 300 kB per window) passes easily.
pub const MIN_PROGRESS_BYTES: u64 = 64 * 1024;
/// Redirect hops followed per download (same as `web.noudata`).
pub const MAX_DOWNLOAD_REDIRECTS: usize = 3;

/// Tunable limits; [`DownloadLimits::default`] is production.
#[derive(Clone)]
pub struct DownloadLimits {
    pub max_file_bytes: u64,
    pub max_batch_bytes: u64,
    pub max_image_bytes: u64,
    pub max_redirects: usize,
    /// Longest silence tolerated, and the length of the minimum-progress window.
    pub stall_timeout: Duration,
    /// Bytes a transfer must deliver per `stall_timeout` window (see [`MIN_PROGRESS_BYTES`]).
    pub min_progress_bytes: u64,
    /// Floor of the free space that must remain; see [`DownloadLimits::reserve_for`].
    pub free_reserve: u64,
    /// Free bytes on the volume of a directory (injectable so tests need no real full disk).
    pub free_space: fn(&Path) -> Option<u64>,
    /// Total size of the volume of a directory (injectable, like `free_space`).
    pub volume_size: fn(&Path) -> Option<u64>,
    /// Log the URL of each request (query redacted). Store audio URLs are bearer
    /// capabilities, so store downloads set this to `false` (BCS_04).
    pub log_url: bool,
    /// What this player can decode. Every path that stores audio (portal upload, unite and
    /// store downloads) refuses a format outside it before the file is kept (MEDIA_01).
    pub capabilities: Arc<Capabilities>,
}

impl std::fmt::Debug for DownloadLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadLimits")
            .field("max_file_bytes", &self.max_file_bytes)
            .field("max_batch_bytes", &self.max_batch_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for DownloadLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
            max_image_bytes: MAX_IMAGE_BYTES,
            max_redirects: MAX_DOWNLOAD_REDIRECTS,
            stall_timeout: STALL_TIMEOUT,
            min_progress_bytes: MIN_PROGRESS_BYTES,
            free_reserve: FREE_SPACE_RESERVE,
            free_space: free_bytes,
            volume_size: volume_bytes,
            log_url: true,
            capabilities: Arc::new(Capabilities::desktop_mock()),
        }
    }
}

/// Refuse audio this player cannot decode. The content decides the kind (never the name), the
/// board profile and this build's decoders decide whether it is playable
/// ([`plippa_audio::formats::format_enabled`], the same gate the play path uses).
pub fn require_playable(caps: &Capabilities, kind: AudioKind) -> Result<(), DownloadError> {
    if plippa_audio::formats::format_enabled(caps, kind.ext()) {
        return Ok(());
    }
    Err(DownloadError::WrongType(format!(
        "{} is not supported by this player ({}). The file was not added.",
        audio_format_label(kind.ext()),
        caps.board_id
    )))
}

impl DownloadLimits {
    /// Free space that must remain on the volume holding `dir`:
    /// `max(free_reserve, min(2 % of the volume, 2 GiB))`. A flat 64 MiB is nothing on a 256 GB
    /// stick, and 2 % of a 4 TB disk is more than anyone needs held back.
    pub fn reserve_for(&self, dir: &Path) -> u64 {
        let share = (self.volume_size)(dir)
            .map(|v| (v / RESERVE_VOLUME_DIVISOR).min(RESERVE_CAP))
            .unwrap_or(0);
        self.free_reserve.max(share)
    }

    /// Defaults with the two documented size overrides applied (megabytes, clamped to
    /// [`HARD_MAX_BYTES`], never below 1 MB). These raise or lower *sizes* only; nothing here
    /// can relax the address, scheme or sniffing rules.
    pub fn from_env() -> Self {
        let mut l = Self::default();
        let mb = |name: &str| -> Option<u64> {
            std::env::var(name)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
                .map(|m| m.saturating_mul(MIB).clamp(MIB, HARD_MAX_BYTES))
        };
        if let Some(v) = mb("PLIPPA_DOWNLOAD_MAX_MB") {
            l.max_file_bytes = v;
        }
        if let Some(v) = mb("PLIPPA_DOWNLOAD_BATCH_MAX_MB") {
            l.max_batch_bytes = v;
        }
        l.max_batch_bytes = l.max_batch_bytes.max(l.max_file_bytes);
        l
    }
}

/// Free bytes for unprivileged users on the volume holding `dir`.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path; `st` is a properly sized out-parameter.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        // The field widths differ between targets (u32 on 32-bit): keep the casts.
        #[allow(clippy::unnecessary_cast)]
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
}

/// Total bytes of the volume holding `dir`.
pub fn volume_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path; `st` is a properly sized out-parameter.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((st.f_blocks as u64).saturating_mul(st.f_frsize as u64))
    }
}

/// Bytes as a person reads them: `4 GB`, `512 MB`.
pub fn describe_bytes(n: u64) -> String {
    if n >= GIB - MIB {
        let g = (n as f64) / (GIB as f64);
        let rounded = (g * 10.0).round() / 10.0;
        if rounded.fract() == 0.0 {
            format!("{} GB", rounded as u64)
        } else {
            format!("{rounded:.1} GB")
        }
    } else {
        format!("{} MB", n / MIB)
    }
}

/// Cooperative cancel flag shared with the worker. A cancel can be *linked* to others (a
/// session's flag and the upload slot's flag): the result counts as cancelled when any of them
/// is set.
#[derive(Debug, Clone, Default)]
pub struct Cancel {
    own: Arc<AtomicBool>,
    watched: Vec<Arc<AtomicBool>>,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.own.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.own.load(Ordering::SeqCst)
            || self.watched.iter().any(|f| f.load(Ordering::SeqCst))
    }
    /// A new cancel that is set when `self` or `other` is. Cancelling the new one sets neither
    /// of them.
    pub fn linked_with(&self, other: &Cancel) -> Cancel {
        let mut watched = vec![Arc::clone(&self.own), Arc::clone(&other.own)];
        watched.extend(self.watched.iter().cloned());
        watched.extend(other.watched.iter().cloned());
        Cancel {
            own: Arc::new(AtomicBool::new(false)),
            watched,
        }
    }
}

/// Why a [`Budget`] refused bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetRefusal {
    /// The session's own batch cap.
    Batch,
    /// The staging cap shared by every session on this server.
    Shared,
}

/// Bytes everything staged on the stick may use, all sessions and routes together: staged files
/// plus bytes still arriving. One of these is shared by the import hub, the unite hub and the
/// single-file route; each session's [`Budget`] draws from it.
///
/// The cap is `min(hard_cap, free space - reserve)` (the free space counted as what the staged
/// bytes already took plus what is still free), evaluated when a transfer is admitted
/// ([`StagingBudget::admit`]) and while it streams (the hard cap on every chunk, the free-space
/// reserve every 8 MiB).
#[derive(Debug)]
pub struct StagingBudget {
    hard_cap: u64,
    used: AtomicU64,
}

impl StagingBudget {
    pub fn new(hard_cap: u64) -> Self {
        Self {
            hard_cap,
            used: AtomicU64::new(0),
        }
    }

    /// The production budget: [`HARD_MAX_BYTES`].
    pub fn production() -> Self {
        Self::new(HARD_MAX_BYTES)
    }

    pub fn hard_cap(&self) -> u64 {
        self.hard_cap
    }

    /// Bytes staged or arriving right now, across all sessions.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::SeqCst)
    }

    /// The most that may be staged right now for a transfer into `dir`.
    pub fn cap(&self, lim: &DownloadLimits, dir: &Path) -> u64 {
        match (lim.free_space)(dir) {
            Some(free) => self.hard_cap.min(
                self.used()
                    .saturating_add(free.saturating_sub(lim.reserve_for(dir))),
            ),
            None => self.hard_cap,
        }
    }

    /// Admission of a transfer of `declared` bytes into `dir`.
    pub fn admit(
        &self,
        lim: &DownloadLimits,
        dir: &Path,
        declared: u64,
    ) -> Result<(), DownloadError> {
        let cap = self.cap(lim, dir);
        let used = self.used();
        if used.saturating_add(declared) > cap {
            return Err(DownloadError::NoSpace {
                needed: declared,
                free: cap.saturating_sub(used),
            });
        }
        Ok(())
    }

    fn try_add(&self, n: u64) -> bool {
        let mut cur = self.used.load(Ordering::SeqCst);
        loop {
            let Some(next) = cur.checked_add(n).filter(|v| *v <= self.hard_cap) else {
                return false;
            };
            match self
                .used
                .compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(v) => cur = v,
            }
        }
    }

    fn release(&self, n: u64) {
        let _ = self
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_sub(n))
            });
    }
}

/// Bytes one batch (a session's downloads + uploads) may use in total. A budget created with
/// [`Budget::with_staging`] also draws every byte from the shared [`StagingBudget`] and gives
/// whatever it still holds back when it is dropped (the session ended).
#[derive(Debug)]
pub struct Budget {
    max: u64,
    used: AtomicU64,
    staging: Option<Arc<StagingBudget>>,
}

impl Budget {
    pub fn new(max: u64) -> Self {
        Self {
            max,
            used: AtomicU64::new(0),
            staging: None,
        }
    }
    /// A batch of at most `max` bytes that also counts against `staging`.
    pub fn with_staging(max: u64, staging: Arc<StagingBudget>) -> Self {
        Self {
            max,
            used: AtomicU64::new(0),
            staging: Some(staging),
        }
    }
    /// Reserve `n` more bytes; false (nothing reserved) when that would exceed the batch cap or
    /// the shared staging cap.
    pub fn try_add(&self, n: u64) -> bool {
        self.try_reserve(n).is_ok()
    }
    /// [`Budget::try_add`], saying which cap refused.
    pub fn try_reserve(&self, n: u64) -> Result<(), BudgetRefusal> {
        let mut cur = self.used.load(Ordering::SeqCst);
        loop {
            let Some(next) = cur.checked_add(n).filter(|v| *v <= self.max) else {
                return Err(BudgetRefusal::Batch);
            };
            match self
                .used
                .compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(v) => cur = v,
            }
        }
        if let Some(st) = &self.staging {
            if !st.try_add(n) {
                // give the batch share back: nothing was reserved
                let _ = self
                    .used
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                        Some(v.saturating_sub(n))
                    });
                return Err(BudgetRefusal::Shared);
            }
        }
        Ok(())
    }
    pub fn release(&self, n: u64) {
        let released = self
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_sub(n))
            })
            .map(|before| before.min(n))
            .unwrap_or(0);
        if let Some(st) = &self.staging {
            st.release(released);
        }
    }
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::SeqCst)
    }
    pub fn remaining(&self) -> u64 {
        self.max.saturating_sub(self.used())
    }
    pub fn max(&self) -> u64 {
        self.max
    }
    /// Admission of a transfer of `declared` bytes into `dir` against the shared staging cap
    /// (a budget without one admits everything; the batch cap is checked by the caller).
    pub fn admit_shared(
        &self,
        lim: &DownloadLimits,
        dir: &Path,
        declared: u64,
    ) -> Result<(), DownloadError> {
        match &self.staging {
            Some(st) => st.admit(lim, dir, declared),
            None => Ok(()),
        }
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        if let Some(st) = &self.staging {
            st.release(self.used.load(Ordering::SeqCst));
        }
    }
}

/// What the caller expects to receive; decides the sniffing rule and the size cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Audio Plippa can play.
    Media,
    /// Cover art: JPEG / PNG / WebP.
    Image,
    /// Thumbnail tier: JPEG / WebP.
    Thumbnail,
}

/// Why a download did not produce a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    Cancelled,
    /// URL, redirect or resolved address refused (SSRF rules, scheme, port).
    Refused(WebError),
    NotFound,
    Http(u16),
    /// Over the per-file cap (from the header or while streaming).
    TooLarge {
        limit: u64,
    },
    /// Over the batch cap.
    BatchTooLarge {
        limit: u64,
    },
    NoSpace {
        needed: u64,
        free: u64,
    },
    /// Sniffed content is not what was expected (HTML, video, executable, wrong image...).
    WrongType(String),
    /// Image is too big in pixels or its header is damaged.
    BadImage(String),
    /// The server closed early or sent fewer bytes than `Content-Length`.
    Truncated,
    Timeout,
    Connect(String),
    Io(String),
}

impl DownloadError {
    pub fn code(&self) -> &'static str {
        match self {
            DownloadError::Cancelled => "cancelled",
            DownloadError::Refused(e) => e.code(),
            DownloadError::NotFound => "not_found",
            DownloadError::Http(_) => "http_error",
            DownloadError::TooLarge { .. } => "too_large",
            DownloadError::BatchTooLarge { .. } => "batch_too_large",
            DownloadError::NoSpace { .. } => "no_space",
            DownloadError::WrongType(_) => "wrong_type",
            DownloadError::BadImage(_) => "bad_image",
            DownloadError::Truncated => "truncated",
            DownloadError::Timeout => "timeout",
            DownloadError::Connect(_) => "connect_failed",
            DownloadError::Io(_) => "io",
        }
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Cancelled => f.write_str("cancelled"),
            DownloadError::Refused(e) => write!(f, "{e}"),
            DownloadError::NotFound => f.write_str("not found (404)"),
            DownloadError::Http(c) => write!(f, "the server answered HTTP {c}"),
            DownloadError::TooLarge { limit } => {
                write!(f, "larger than the {} limit", describe_bytes(*limit))
            }
            DownloadError::BatchTooLarge { limit } => {
                write!(f, "the download batch would exceed {}", describe_bytes(*limit))
            }
            DownloadError::NoSpace { needed, free } => write!(
                f,
                "not enough free space on the storage ({} MB needed, {} MB free)",
                needed / MIB,
                free / MIB
            ),
            DownloadError::WrongType(m) => write!(f, "unsupported file: {m}"),
            DownloadError::BadImage(m) => write!(f, "unusable image: {m}"),
            DownloadError::Truncated => f.write_str("the download was cut off"),
            DownloadError::Timeout => f.write_str("the transfer stalled or was too slow"),
            DownloadError::Connect(m) => write!(f, "could not download: {m}"),
            DownloadError::Io(m) => write!(f, "could not write the file: {m}"),
        }
    }
}

impl std::error::Error for DownloadError {}

/// A temp file that deletes itself unless [`TempFile::keep`] is called.
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// The file is complete and validated: stop guarding it.
    pub fn keep(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Create a new, empty, private temp file in `dir` (never follows a symlink, never reuses a
/// name).
pub fn create_temp(dir: &Path) -> std::io::Result<(File, TempFile)> {
    use std::os::unix::fs::OpenOptionsExt;
    for _ in 0..8 {
        let n: u64 = rand::random();
        let path = dir.join(format!(".dl-{n:016x}.part"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(f) => return Ok((f, TempFile { path, armed: true })),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not create a temp file"))
}

/// A finished, validated download (still at its temp path).
#[derive(Debug)]
pub struct Downloaded {
    pub temp: TempFile,
    pub size: u64,
    pub audio: Option<AudioKind>,
    pub image: Option<ImageInfo>,
    /// URL that finally answered, query redacted.
    pub final_url: String,
}

fn io_err(e: std::io::Error) -> DownloadError {
    DownloadError::Io(e.to_string().chars().take(120).collect())
}

fn check_space(lim: &DownloadLimits, dir: &Path, needed: u64) -> Result<(), DownloadError> {
    if let Some(free) = (lim.free_space)(dir) {
        if needed.saturating_add(lim.reserve_for(dir)) > free {
            return Err(DownloadError::NoSpace { needed, free });
        }
    }
    Ok(())
}

/// GET `url` and stream it into a new temp file in `tmp_dir`. `Consent` proves the user agreed
/// to this batch (the caller keeps one for the whole job). `on_progress(received, total)` is
/// called after every chunk.
#[allow(clippy::too_many_arguments)]
pub fn download_to_temp(
    transport: &dyn Transport,
    _consent: &Consent,
    policy: &FetchPolicy,
    lim: &DownloadLimits,
    url: &str,
    tmp_dir: &Path,
    want: Want,
    budget: &Budget,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Downloaded, DownloadError> {
    let mut current = policy.first_url(url).map_err(DownloadError::Refused)?;
    let mut hops = 0usize;
    let mut resp = loop {
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        if lim.log_url {
            eprintln!(
                "plippa-server: web download {}",
                redact_url_for_log(&current)
            );
        }
        let r = transport
            .get_download(&current, lim.stall_timeout)
            .map_err(|e| match e {
                TransportError::Blocked(m) => {
                    DownloadError::Refused(WebError::Blocked(format!("target {m} is not public")))
                }
                TransportError::Timeout => DownloadError::Timeout,
                TransportError::Connect(m) => DownloadError::Connect(m),
            })?;
        match r.status {
            301 | 302 | 303 | 307 | 308 => {
                let Some(loc) = r.location.clone() else {
                    return Err(DownloadError::Refused(WebError::RedirectRefused(
                        "no Location header".into(),
                    )));
                };
                drop(r);
                if hops >= lim.max_redirects {
                    return Err(DownloadError::Refused(WebError::TooManyRedirects));
                }
                hops += 1;
                current = policy
                    .next_url(&current, &loc)
                    .map_err(DownloadError::Refused)?;
            }
            200 => break r,
            404 | 410 => return Err(DownloadError::NotFound),
            code => return Err(DownloadError::Http(code)),
        }
    };

    let cap = match want {
        Want::Media => lim.max_file_bytes,
        Want::Image | Want::Thumbnail => lim.max_image_bytes,
    };
    let declared = resp.content_length;
    if let Some(len) = declared {
        if len > cap {
            return Err(DownloadError::TooLarge { limit: cap });
        }
        if len > budget.remaining() {
            return Err(DownloadError::BatchTooLarge {
                limit: budget.max(),
            });
        }
        check_space(lim, tmp_dir, len)?;
        budget.admit_shared(lim, tmp_dir, len)?;
    }

    let streamed = stream_to_temp(
        &mut *resp.body,
        declared,
        tmp_dir,
        StreamRules {
            cap,
            lim,
            budget,
            cancel,
        },
        &mut |head| check_head(want, head, lim).map(|()| None),
        on_progress,
    );
    drop(resp);
    let Streamed {
        temp: guard,
        size: written,
        head,
        reserved,
    } = streamed?;

    let mut out = Downloaded {
        temp: guard,
        size: written,
        audio: None,
        image: None,
        final_url: redact_url_for_log(&current),
    };
    match want {
        Want::Media => {
            if let MediaSniff::Audio(k) = sniff_audio(&head) {
                out.audio = Some(k);
            }
        }
        Want::Image | Want::Thumbnail => {
            let side = if want == Want::Image {
                MAX_IMAGE_SIDE
            } else {
                MAX_THUMB_SIDE
            };
            let checked = validate_image_file(out.temp.path(), side);
            match checked {
                Ok(info) => out.image = Some(info),
                Err(e) => {
                    budget.release(reserved);
                    return Err(e);
                }
            }
        }
    }
    Ok(out)
}

/// Limits and control for one [`stream_to_temp`].
pub struct StreamRules<'a> {
    /// Largest body accepted (narrowed later by the head check, never widened).
    pub cap: u64,
    pub lim: &'a DownloadLimits,
    pub budget: &'a Budget,
    pub cancel: &'a Cancel,
}

/// Looks at the first bytes of a body; may reject it or narrow the size cap.
pub type HeadCheck<'a> = dyn FnMut(&[u8]) -> Result<Option<u64>, DownloadError> + 'a;

/// A body streamed to a temp file that has not been validated beyond its head.
#[derive(Debug)]
pub struct Streamed {
    pub temp: TempFile,
    pub size: u64,
    /// First bytes (up to [`SNIFF_HEAD_BYTES`]).
    pub head: Vec<u8>,
    /// Bytes charged to the batch budget (give back with `Budget::release` if the file is
    /// later rejected).
    pub reserved: u64,
}

/// Stream `body` into a new temp file in `tmp_dir`, enforcing (while streaming, because a
/// header can lie): the byte cap, the batch budget (and the shared staging cap), free space,
/// cancel, the minimum-progress rule and `declared` (`Content-Length`). There is no total-time
/// limit: silence is cut off by the stall timeout of whoever owns the socket (a read that times
/// out ends the transfer with [`DownloadError::Timeout`]), and a transfer that keeps trickling is
/// cut off by [`DownloadLimits::min_progress_bytes`] per [`DownloadLimits::stall_timeout`]. `head_check` sees the first [`SNIFF_HEAD_BYTES`] bytes (or all
/// of a shorter body) and may reject the content or narrow the cap (`Ok(Some(new_cap))`). On any
/// error the partial file is deleted and the budget restored.
pub fn stream_to_temp(
    body: &mut dyn Read,
    declared: Option<u64>,
    tmp_dir: &Path,
    rules: StreamRules<'_>,
    head_check: &mut HeadCheck<'_>,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Streamed, DownloadError> {
    let StreamRules {
        mut cap,
        lim,
        budget,
        cancel,
    } = rules;
    let mut window_start = Instant::now();
    let mut window_bytes: u64 = 0;
    let (mut file, guard) = create_temp(tmp_dir).map_err(io_err)?;
    let mut written: u64 = 0;
    let mut reserved: u64 = 0;
    let mut head: Vec<u8> = Vec::with_capacity(SNIFF_HEAD_BYTES);
    let mut sniffed = false;
    let mut next_space_check = 8 * MIB;
    let mut buf = vec![0u8; 64 * 1024];
    let result: Result<(), DownloadError> = (|| {
        loop {
            if cancel.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            let n = match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Err(DownloadError::Timeout)
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(DownloadError::Truncated),
            };
            let n64 = n as u64;
            // Minimum progress: every `stall_timeout` window must bring `min_progress_bytes`.
            window_bytes += n64;
            if window_start.elapsed() >= lim.stall_timeout {
                if window_bytes < lim.min_progress_bytes {
                    return Err(DownloadError::Timeout);
                }
                window_start = Instant::now();
                window_bytes = 0;
            }
            if written + n64 > cap {
                return Err(DownloadError::TooLarge { limit: cap });
            }
            match budget.try_reserve(n64) {
                Ok(()) => {}
                Err(BudgetRefusal::Batch) => {
                    return Err(DownloadError::BatchTooLarge {
                        limit: budget.max(),
                    })
                }
                Err(BudgetRefusal::Shared) => {
                    return Err(DownloadError::NoSpace {
                        needed: n64,
                        free: 0,
                    })
                }
            }
            reserved += n64;
            if !sniffed {
                let take = n.min(SNIFF_HEAD_BYTES - head.len());
                head.extend_from_slice(&buf[..take]);
                if head.len() >= SNIFF_HEAD_BYTES {
                    if let Some(c) = head_check(&head)? {
                        cap = cap.min(c);
                    }
                    sniffed = true;
                    if written + n64 > cap {
                        return Err(DownloadError::TooLarge { limit: cap });
                    }
                }
            }
            file.write_all(&buf[..n]).map_err(io_err)?;
            written += n64;
            if written >= next_space_check {
                next_space_check = written + 8 * MIB;
                check_space(lim, tmp_dir, 0)?;
            }
            on_progress(written, declared);
        }
        if !sniffed {
            if let Some(c) = head_check(&head)? {
                if written > c {
                    return Err(DownloadError::TooLarge { limit: c });
                }
            }
        }
        if declared.is_some_and(|d| d != written) {
            return Err(DownloadError::Truncated);
        }
        file.flush().map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
        Ok(())
    })();
    drop(file);
    if let Err(e) = result {
        budget.release(reserved);
        return Err(e); // `guard` deletes the partial file
    }
    Ok(Streamed {
        temp: guard,
        size: written,
        head,
        reserved,
    })
}

fn check_head(want: Want, head: &[u8], lim: &DownloadLimits) -> Result<(), DownloadError> {
    match want {
        Want::Media => match sniff_audio(head) {
            MediaSniff::Audio(kind) => require_playable(&lim.capabilities, kind),
            MediaSniff::Unsupported(what) => Err(DownloadError::WrongType(what.to_string())),
            MediaSniff::Unknown => Err(DownloadError::WrongType(
                "the content is not audio Plippa can play".into(),
            )),
        },
        Want::Image => sniff_image_kind(head).map(|_| ()).ok_or_else(|| {
            DownloadError::WrongType("the content is not a JPEG, PNG or WebP".into())
        }),
        Want::Thumbnail => match sniff_image_kind(head) {
            Some(ImageKind::Jpeg | ImageKind::Webp) => Ok(()),
            _ => Err(DownloadError::WrongType(
                "the content is not a JPEG or WebP thumbnail".into(),
            )),
        },
    }
}

/// Read an already-complete image file (<= 16 MB) and check its header (no decoding).
pub fn validate_image_file(path: &Path, max_side: u32) -> Result<ImageInfo, DownloadError> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(io_err)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(DownloadError::TooLarge {
            limit: MAX_IMAGE_BYTES,
        });
    }
    sniff_image(&bytes, max_side).map_err(|e| DownloadError::BadImage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::fetch::HopResponse;
    use std::io::Cursor;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// Body that yields `data`, then fails like a dropped connection.
    struct Cut {
        data: Cursor<Vec<u8>>,
    }
    impl Read for Cut {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.data.read(buf)?;
            if n == 0 {
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
            } else {
                Ok(n)
            }
        }
    }

    struct Endless(u8);
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(self.0);
            Ok(buf.len())
        }
    }

    type Answer = Box<dyn Fn(&str) -> Result<HopResponse, TransportError> + Send + Sync>;
    struct Mock {
        calls: AtomicUsize,
        urls: Mutex<Vec<String>>,
        answer: Answer,
    }
    impl Transport for Mock {
        fn get(&self, url: &str, _t: Duration) -> Result<HopResponse, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.urls.lock().unwrap().push(url.to_string());
            (self.answer)(url)
        }
    }

    fn mock(
        f: impl Fn(&str) -> Result<HopResponse, TransportError> + Send + Sync + 'static,
    ) -> Mock {
        Mock {
            calls: AtomicUsize::new(0),
            urls: Mutex::new(Vec::new()),
            answer: Box::new(f),
        }
    }

    fn resp(len: Option<u64>, body: Box<dyn Read + Send>) -> Result<HopResponse, TransportError> {
        Ok(HopResponse {
            status: 200,
            content_type: Some("audio/mpeg".into()),
            location: None,
            content_length: len,
            body,
        })
    }

    fn redirect(to: &str) -> Result<HopResponse, TransportError> {
        Ok(HopResponse {
            status: 302,
            content_type: None,
            location: Some(to.into()),
            content_length: Some(0),
            body: Box::new(Cursor::new(Vec::new())),
        })
    }

    fn flac(n: usize) -> Vec<u8> {
        let mut v = b"fLaC\0\0\0\"".to_vec();
        v.resize(n.max(8), 7);
        v
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]);
        v.resize(200, 0);
        v
    }

    struct Rig {
        dir: tempfile::TempDir,
    }
    impl Rig {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn files(&self) -> Vec<String> {
            std::fs::read_dir(self.dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        }
        fn get(
            &self,
            t: &dyn Transport,
            url: &str,
            want: Want,
            lim: &DownloadLimits,
            budget: &Budget,
            cancel: &Cancel,
        ) -> Result<Downloaded, DownloadError> {
            download_to_temp(
                t,
                &Consent::from_request(Some(true)).unwrap(),
                &FetchPolicy::production(),
                lim,
                url,
                self.dir.path(),
                want,
                budget,
                cancel,
                &mut |_, _| {},
            )
        }
    }

    fn big_budget() -> Budget {
        Budget::new(DEFAULT_MAX_BATCH_BYTES)
    }

    #[test]
    fn unite_download_streams_a_sniffed_file_to_temp() {
        let rig = Rig::new();
        let body = flac(3000);
        let b2 = body.clone();
        let m = mock(move |_| resp(Some(b2.len() as u64), Box::new(Cursor::new(b2.clone()))));
        let d = rig
            .get(
                &m,
                "https://dl.example/a.bin",
                Want::Media,
                &DownloadLimits::default(),
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(
            d.audio,
            Some(AudioKind::Flac),
            "kind from magic bytes, not the .bin name"
        );
        assert_eq!(d.size, 3000);
        assert_eq!(std::fs::read(d.temp.path()).unwrap(), body);
        assert!(d.temp.path().starts_with(rig.dir.path()));
        drop(d);
        assert!(
            rig.files().is_empty(),
            "guard removes an unclaimed temp file"
        );
    }

    fn m4a_bytes(n: usize) -> Vec<u8> {
        let mut v = b"\0\0\0\x18ftypM4A \0\0\0\0M4A mp42".to_vec();
        v.resize(n, 0);
        v
    }

    #[test]
    fn unite_and_store_downloads_refuse_audio_the_board_cannot_decode() {
        let rig = Rig::new();
        let body = m4a_bytes(3000);
        let m = mock(move |_| resp(Some(body.len() as u64), Box::new(Cursor::new(body.clone()))));
        let get = |lim: &DownloadLimits| {
            rig.get(&m, "https://dl.example/a.bin", Want::Media, lim, &big_budget(), &Cancel::new())
        };
        // Desktop capabilities (no AAC): refused from the first bytes, nothing kept.
        let e = get(&DownloadLimits::default()).unwrap_err();
        match &e {
            DownloadError::WrongType(m) => {
                assert!(m.contains("M4A/AAC") && m.contains("desktop-mock"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        assert!(rig.files().is_empty(), "a refused download leaves no temp file");
        // A board that claims AAC does not change the answer: this build cannot decode it.
        let mut caps = Capabilities::desktop_mock();
        caps.audio_formats.aac = true;
        let lim = DownloadLimits {
            capabilities: Arc::new(caps),
            ..DownloadLimits::default()
        };
        assert!(matches!(get(&lim), Err(DownloadError::WrongType(_))));
    }

    /// MEDIA_01: the import gate and the play gate are one decision. For every kind of audio the
    /// sniffer can name and two boards, "refused at import" is exactly "refused by the player's
    /// format check", so nothing can be imported that playback then refuses (or the reverse).
    #[test]
    fn the_import_gate_and_the_play_gate_agree() {
        use plippa_audio::{validate_for_stream, AudioError, OverspecPolicy};
        let mut with_all = Capabilities::desktop_mock();
        with_all.audio_formats.aac = true;
        with_all.audio_formats.opus = true;
        let kinds = [
            AudioKind::Mp3,
            AudioKind::Flac,
            AudioKind::Wav,
            AudioKind::Ogg,
            AudioKind::Opus,
            AudioKind::M4a,
            AudioKind::Aac,
        ];
        for caps in [Capabilities::desktop_mock(), with_all] {
            for kind in kinds {
                let path = std::path::PathBuf::from(format!("/nonexistent/x.{}", kind.ext()));
                let play_refuses_format = matches!(
                    validate_for_stream(&path, &caps, OverspecPolicy::Downsample),
                    Err(AudioError::FormatNotEnabled { .. })
                );
                assert_eq!(
                    require_playable(&caps, kind).is_err(),
                    play_refuses_format,
                    "{kind:?} on {:?}",
                    caps.audio_formats
                );
            }
        }
        // Ogg is in no capability set, so it is refused even where everything else is enabled.
        let mut all = Capabilities::desktop_mock();
        all.audio_formats.aac = true;
        all.audio_formats.opus = true;
        assert!(require_playable(&all, AudioKind::Ogg).is_err());
    }

    #[test]
    fn unite_download_refuses_private_targets_and_bad_redirects_before_connecting() {
        let rig = Rig::new();
        let m = mock(|u| {
            if u.contains("start") {
                redirect("https://169.254.169.254/latest/meta-data/")
            } else {
                resp(Some(8), Box::new(Cursor::new(flac(8))))
            }
        });
        let lim = DownloadLimits::default();
        for url in [
            "https://127.0.0.1/a.mp3",
            "https://192.168.1.10/a.mp3",
            "https://localhost/a.mp3",
            "https://nas.local/a.mp3",
            "file:///etc/passwd",
            "ftp://x.example/a.mp3",
            "https://x.example:8443/a.mp3",
            "https://user:pw@x.example/a.mp3",
        ] {
            let e = rig
                .get(&m, url, Want::Media, &lim, &big_budget(), &Cancel::new())
                .unwrap_err();
            assert!(matches!(e, DownloadError::Refused(_)), "{url}: {e:?}");
        }
        assert_eq!(
            m.calls.load(Ordering::SeqCst),
            0,
            "no socket for a refused URL"
        );
        // Public host redirecting into the metadata address: first hop happens, second never.
        let e = rig
            .get(
                &m,
                "https://dl.example/start",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert!(
            matches!(e, DownloadError::Refused(WebError::RedirectRefused(_))),
            "{e:?}"
        );
        assert_eq!(m.calls.load(Ordering::SeqCst), 1);
        // http:// redirect targets are upgraded to https (never fetched in clear text).
        let m = mock(|u| {
            if u == "https://dl.example/a" {
                redirect("http://dl.example/b")
            } else {
                resp(Some(8), Box::new(Cursor::new(flac(8))))
            }
        });
        rig.get(
            &m,
            "https://dl.example/a",
            Want::Media,
            &lim,
            &big_budget(),
            &Cancel::new(),
        )
        .unwrap();
        assert_eq!(
            m.urls.lock().unwrap().as_slice(),
            ["https://dl.example/a", "https://dl.example/b"]
        );
        // javascript:/data: redirects and hop limit.
        for loc in ["javascript:alert(1)", "data:text/html,x", "//127.0.0.1/x"] {
            let m = mock(move |_| redirect(loc));
            let e = rig
                .get(
                    &m,
                    "https://dl.example/a",
                    Want::Media,
                    &lim,
                    &big_budget(),
                    &Cancel::new(),
                )
                .unwrap_err();
            assert!(matches!(e, DownloadError::Refused(_)), "{loc}: {e:?}");
        }
        let m = mock(|_| redirect("/again"));
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::Refused(WebError::TooManyRedirects));
        assert_eq!(m.calls.load(Ordering::SeqCst), 4);
        assert!(rig.files().is_empty());
    }

    #[test]
    fn unite_download_size_caps_header_and_streaming() {
        let rig = Rig::new();
        let lim = DownloadLimits {
            max_file_bytes: 1000,
            ..Default::default()
        };
        // Honest oversize header: refused before the body is read (no temp file made).
        let m = mock(|_| resp(Some(5000), Box::new(Cursor::new(flac(5000)))));
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::TooLarge { limit: 1000 });
        // Lying header (says 10, sends endless): enforced while streaming, partial deleted.
        let m = mock(|_| {
            resp(
                Some(10),
                Box::new(Cursor::new(b"fLaC\0\0\0\"".to_vec()).chain(Endless(1))),
            )
        });
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::TooLarge { limit: 1000 });
        // No header at all (chunked) and an endless body.
        let m = mock(|_| {
            resp(
                None,
                Box::new(Cursor::new(b"fLaC\0\0\0\"".to_vec()).chain(Endless(1))),
            )
        });
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::TooLarge { limit: 1000 });
        assert!(rig.files().is_empty(), "no partial file left");
        // Image cap is 16 MB regardless of the media cap.
        let m = mock(|_| resp(Some(17 * MIB), Box::new(Endless(1))));
        let e = rig
            .get(
                &m,
                "https://dl.example/a.png",
                Want::Image,
                &DownloadLimits::default(),
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(
            e,
            DownloadError::TooLarge {
                limit: MAX_IMAGE_BYTES
            }
        );
    }

    #[test]
    fn unite_download_batch_budget_is_shared_and_released_on_failure() {
        let rig = Rig::new();
        let lim = DownloadLimits::default();
        let m = mock(|_| resp(Some(3000), Box::new(Cursor::new(flac(3000)))));
        let budget = Budget::new(5000);
        let a = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &budget,
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(budget.used(), 3000);
        let e = rig
            .get(
                &m,
                "https://dl.example/b",
                Want::Media,
                &lim,
                &budget,
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::BatchTooLarge { limit: 5000 });
        assert_eq!(budget.used(), 3000, "a refused file reserves nothing");
        // A cut-off download gives its bytes back.
        let m = mock(|_| {
            resp(
                None,
                Box::new(Cut {
                    data: Cursor::new(flac(1000)),
                }),
            )
        });
        let e = rig
            .get(
                &m,
                "https://dl.example/c",
                Want::Media,
                &lim,
                &budget,
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::Truncated);
        assert_eq!(budget.used(), 3000);
        drop(a);
    }

    /// `total` bytes, handed out `chunk` bytes per read (a real socket gives 8-64 KiB reads).
    struct Chunked {
        left: usize,
        chunk: usize,
    }
    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.left.min(self.chunk).min(buf.len());
            buf[..n].fill(7);
            self.left -= n;
            Ok(n)
        }
    }

    /// NB_VERIFY NB_09: the byte cap must hold on *every* chunk, not only on the chunk that
    /// completes the sniff head (mutation check: deleting the per-chunk cap test used to pass).
    #[test]
    fn unite_download_streaming_cap_holds_across_many_chunks() {
        let rig = Rig::new();
        let lim = DownloadLimits {
            max_file_bytes: 100_000,
            ..Default::default()
        };
        for declared in [Some(10), None] {
            let m = mock(move |_| {
                resp(
                    declared,
                    Box::new(
                        Cursor::new(b"fLaC\0\0\0\"".to_vec()).chain(Chunked {
                            left: 400_000,
                            chunk: 10_000,
                        }),
                    ),
                )
            });
            let e = rig
                .get(
                    &m,
                    "https://dl.example/a",
                    Want::Media,
                    &lim,
                    &big_budget(),
                    &Cancel::new(),
                )
                .unwrap_err();
            assert_eq!(e, DownloadError::TooLarge { limit: 100_000 }, "{declared:?}");
        }
        assert!(rig.files().is_empty(), "no partial file left");
    }

    #[test]
    fn unite_download_free_space_is_checked_before_and_while_streaming() {
        let rig = Rig::new();
        let lim = DownloadLimits {
            free_space: |_| Some(10 * MIB),
            free_reserve: 64 * MIB,
            ..Default::default()
        };
        let m = mock(|_| resp(Some(3000), Box::new(Cursor::new(flac(3000)))));
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert!(matches!(e, DownloadError::NoSpace { .. }), "{e:?}");
        assert!(rig.files().is_empty());
        // Unknown length: the volume fills while streaming (checked every 8 MiB).
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let lim = DownloadLimits {
            free_space: |_| {
                if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                    Some(GIB)
                } else {
                    Some(0)
                }
            },
            ..Default::default()
        };
        let m = mock(|_| {
            resp(
                None,
                Box::new(Cursor::new(b"fLaC\0\0\0\"".to_vec()).chain(Endless(1))),
            )
        });
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert!(matches!(e, DownloadError::NoSpace { .. }), "{e:?}");
        assert!(rig.files().is_empty());
        // The real statvfs works on a real directory.
        assert!(free_bytes(rig.dir.path()).is_some_and(|f| f > 0));
    }

    #[test]
    fn unite_download_rejects_non_audio_by_content_not_extension_or_content_type() {
        let rig = Rig::new();
        let lim = DownloadLimits::default();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "html",
                b"<!doctype html><html><body>captcha</body></html>".to_vec(),
            ),
            ("elf", {
                let mut v = b"\x7fELF\x02\x01\x01".to_vec();
                v.resize(200, 0);
                v
            }),
            ("mkv", {
                let mut v = vec![0x1A, 0x45, 0xDF, 0xA3];
                v.resize(200, 0);
                v
            }),
            ("zip", {
                let mut v = b"PK\x03\x04".to_vec();
                v.resize(200, 0);
                v
            }),
            ("script", b"#!/bin/sh\nrm -rf /\n".to_vec()),
            ("short", b"ab".to_vec()),
            ("empty", Vec::new()),
        ];
        for (name, body) in cases {
            let len = body.len() as u64;
            let m = mock(move |_| {
                Ok(HopResponse {
                    status: 200,
                    content_type: Some("audio/mpeg".into()),
                    location: None,
                    content_length: Some(len),
                    body: Box::new(Cursor::new(body.clone())),
                })
            });
            let e = rig
                .get(
                    &m,
                    "https://dl.example/track.mp3",
                    Want::Media,
                    &lim,
                    &big_budget(),
                    &Cancel::new(),
                )
                .unwrap_err();
            assert!(matches!(e, DownloadError::WrongType(_)), "{name}: {e:?}");
        }
        assert!(rig.files().is_empty(), "rejected files are deleted");
    }

    #[test]
    fn unite_download_image_rules() {
        let rig = Rig::new();
        let lim = DownloadLimits::default();
        let ok = png(1000, 1000);
        let m = mock(move |_| resp(Some(ok.len() as u64), Box::new(Cursor::new(ok.clone()))));
        let d = rig
            .get(
                &m,
                "https://cdn.example/cover",
                Want::Image,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap();
        let info = d.image.unwrap();
        assert_eq!(
            (info.kind, info.width, info.height),
            (ImageKind::Png, 1000, 1000)
        );
        drop(d);
        // 6000 px wide, tiny file: dimension cap without decoding.
        let big = png(6000, 100);
        let m = mock(move |_| resp(Some(big.len() as u64), Box::new(Cursor::new(big.clone()))));
        let e = rig
            .get(
                &m,
                "https://cdn.example/cover",
                Want::Image,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert!(matches!(e, DownloadError::BadImage(_)), "{e:?}");
        // HTML answering a missing image (SPA fallback) and SVG are refused.
        for body in [
            &b"<html>404</html>"[..],
            b"<svg xmlns='http://www.w3.org/2000/svg'/>",
        ] {
            let m = mock(move |_| {
                resp(
                    Some(body.len() as u64),
                    Box::new(Cursor::new(body.to_vec())),
                )
            });
            let e = rig
                .get(
                    &m,
                    "https://cdn.example/cover.png",
                    Want::Image,
                    &lim,
                    &big_budget(),
                    &Cancel::new(),
                )
                .unwrap_err();
            assert!(matches!(e, DownloadError::WrongType(_)), "{e:?}");
        }
        // A thumbnail must be JPEG/WebP; a PNG there is refused.
        let p = png(100, 100);
        let m = mock(move |_| resp(Some(p.len() as u64), Box::new(Cursor::new(p.clone()))));
        let e = rig
            .get(
                &m,
                "https://cdn.example/thumbnails/c-128.jpg",
                Want::Thumbnail,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert!(matches!(e, DownloadError::WrongType(_)), "{e:?}");
        assert!(rig.files().is_empty());
    }

    #[test]
    fn unite_download_killed_midway_leaves_nothing() {
        let rig = Rig::new();
        let lim = DownloadLimits::default();
        // Declared 100 kB, connection resets after 40 kB.
        let m = mock(|_| {
            resp(
                Some(100_000),
                Box::new(Cut {
                    data: Cursor::new(flac(40_000)),
                }),
            )
        });
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::Truncated);
        // Server closes cleanly but short of Content-Length.
        let m = mock(|_| resp(Some(100_000), Box::new(Cursor::new(flac(40_000)))));
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::Truncated);
        // Stall timeout surfaces as Timeout.
        struct Stall;
        impl Read for Stall {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
            }
        }
        let m = mock(|_| resp(None, Box::new(Stall)));
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &lim,
                &big_budget(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::Timeout);
        assert!(rig.files().is_empty(), "no partial media");
    }

    #[test]
    fn unite_download_cancel_stops_the_stream_and_deletes_the_partial() {
        let rig = Rig::new();
        let cancel = Cancel::new();
        let c2 = cancel.clone();
        let m = mock(|_| {
            resp(
                None,
                Box::new(Cursor::new(b"fLaC\0\0\0\"".to_vec()).chain(Endless(1))),
            )
        });
        let mut chunks = 0;
        let e = download_to_temp(
            &m,
            &Consent::from_request(Some(true)).unwrap(),
            &FetchPolicy::production(),
            &DownloadLimits::default(),
            "https://dl.example/a",
            rig.dir.path(),
            Want::Media,
            &big_budget(),
            &cancel,
            &mut |_, _| {
                chunks += 1;
                if chunks == 3 {
                    c2.cancel();
                }
            },
        )
        .unwrap_err();
        assert_eq!(e, DownloadError::Cancelled);
        assert_eq!(chunks, 3, "stopped at the next chunk");
        assert!(rig.files().is_empty());
        // Cancelled before the first request: zero sockets.
        let before = m.calls.load(Ordering::SeqCst);
        let e = rig
            .get(
                &m,
                "https://dl.example/a",
                Want::Media,
                &DownloadLimits::default(),
                &big_budget(),
                &cancel,
            )
            .unwrap_err();
        assert_eq!(e, DownloadError::Cancelled);
        assert_eq!(m.calls.load(Ordering::SeqCst), before);
    }

    #[test]
    fn unite_download_http_errors_and_temp_never_follows_a_symlink() {
        let rig = Rig::new();
        let lim = DownloadLimits::default();
        for (code, want) in [
            (404, DownloadError::NotFound),
            (500, DownloadError::Http(500)),
            (206, DownloadError::Http(206)),
        ] {
            let m = mock(move |_| {
                Ok(HopResponse {
                    status: code,
                    content_type: None,
                    location: None,
                    content_length: Some(0),
                    body: Box::new(Cursor::new(Vec::new())),
                })
            });
            let e = rig
                .get(
                    &m,
                    "https://dl.example/a",
                    Want::Media,
                    &lim,
                    &big_budget(),
                    &Cancel::new(),
                )
                .unwrap_err();
            assert_eq!(e, want);
        }
        // A directory that is a dangling/foreign symlink cannot be written through.
        let real = tempfile::tempdir().unwrap();
        let link = rig.dir.path().join("link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let m = mock(|_| resp(Some(8), Box::new(Cursor::new(flac(8)))));
        let sub = rig.dir.path().join("nope");
        let r = download_to_temp(
            &m,
            &Consent::from_request(Some(true)).unwrap(),
            &FetchPolicy::production(),
            &lim,
            "https://dl.example/a",
            &sub,
            Want::Media,
            &big_budget(),
            &Cancel::new(),
            &mut |_, _| {},
        );
        assert!(matches!(r, Err(DownloadError::Io(_))));
    }

    // -------- real sockets against a local fixture server (cfg(test) escape hatch) --------

    use crate::web::fetch::UreqTransport;

    struct Fx {
        port: u16,
        server: Arc<tiny_http::Server>,
        hits: Arc<AtomicUsize>,
    }
    impl Drop for Fx {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }

    enum Reply {
        Bytes(u16, Vec<u8>),
        /// Promise `len` bytes, send `sent`, then close.
        Short {
            len: usize,
            sent: Vec<u8>,
        },
        Redirect(String),
    }

    fn fx(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Fx {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let (srv, h) = (Arc::clone(&server), Arc::clone(&hits));
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for rq in srv.incoming_requests() {
                h.fetch_add(1, Ordering::SeqCst);
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let url = rq.url().to_string();
                    match handler(&url) {
                        Reply::Bytes(code, b) => {
                            let _ = rq.respond(
                                tiny_http::Response::from_data(b)
                                    .with_status_code(tiny_http::StatusCode(code)),
                            );
                        }
                        Reply::Short { len, sent } => {
                            let _ = rq.respond(tiny_http::Response::new(
                                tiny_http::StatusCode(200),
                                vec![],
                                Cursor::new(sent),
                                Some(len),
                                None,
                            ));
                        }
                        Reply::Redirect(to) => {
                            let _ = rq.respond(
                                tiny_http::Response::from_data(Vec::new())
                                    .with_status_code(tiny_http::StatusCode(302))
                                    .with_header(
                                        tiny_http::Header::from_bytes(
                                            &b"Location"[..],
                                            to.as_bytes(),
                                        )
                                        .unwrap(),
                                    ),
                            );
                        }
                    }
                });
            }
        });
        Fx { port, server, hits }
    }

    fn real(rig: &Rig, url: &str, want: Want) -> Result<Downloaded, DownloadError> {
        download_to_temp(
            &UreqTransport::for_local_fixtures(),
            &Consent::from_request(Some(true)).unwrap(),
            &FetchPolicy::local_fixtures(),
            &DownloadLimits {
                stall_timeout: Duration::from_secs(1),
                ..Default::default()
            },
            url,
            rig.dir.path(),
            want,
            &big_budget(),
            &Cancel::new(),
            &mut |_, _| {},
        )
    }

    #[test]
    fn unite_download_real_socket_success_short_body_and_redirects() {
        let rig = Rig::new();
        let body = flac(20_000);
        let b2 = body.clone();
        let f = fx(move |p| match p {
            "/ok" => Reply::Bytes(200, b2.clone()),
            "/short" => Reply::Short {
                len: 30_000,
                sent: b2[..10_000].to_vec(),
            },
            "/html" => Reply::Bytes(200, b"<html>hi</html>".to_vec()),
            "/hop" => Reply::Redirect("/ok".into()),
            "/private" => Reply::Redirect("https://10.0.0.1/x".into()),
            _ => Reply::Bytes(404, Vec::new()),
        });
        let url = |p: &str| format!("http://127.0.0.1:{}{p}", f.port);
        let d = real(&rig, &url("/ok"), Want::Media).unwrap();
        assert_eq!(std::fs::read(d.temp.path()).unwrap(), body);
        drop(d);
        assert_eq!(real(&rig, &url("/hop"), Want::Media).unwrap().size, 20_000);
        // Promised 30 kB, sent 10 kB and went quiet: cut off or stalled, never a file.
        assert!(matches!(
            real(&rig, &url("/short"), Want::Media).unwrap_err(),
            DownloadError::Truncated | DownloadError::Timeout
        ));
        assert!(matches!(
            real(&rig, &url("/html"), Want::Media).unwrap_err(),
            DownloadError::WrongType(_)
        ));
        assert!(matches!(
            real(&rig, &url("/private"), Want::Media).unwrap_err(),
            DownloadError::Refused(_)
        ));
        assert_eq!(
            real(&rig, &url("/missing"), Want::Media).unwrap_err(),
            DownloadError::NotFound
        );
        assert!(
            rig.files().is_empty(),
            "every failure cleaned up after itself"
        );
        assert!(f.hits.load(Ordering::SeqCst) >= 6);
    }

    #[test]
    fn unite_download_production_transport_never_reaches_loopback() {
        // The real (non-fixture) transport with the production policy: a loopback download
        // must be refused before or at connect time; the fixture server sees zero requests.
        let rig = Rig::new();
        let f = fx(|_| Reply::Bytes(200, flac(100)));
        for url in [
            format!("https://127.0.0.1:{}/a", f.port),
            format!("http://127.0.0.1:{}/a", f.port),
            "https://localhost/a".to_string(),
        ] {
            let r = download_to_temp(
                &UreqTransport::new(),
                &Consent::from_request(Some(true)).unwrap(),
                &FetchPolicy::production(),
                &DownloadLimits::default(),
                &url,
                rig.dir.path(),
                Want::Media,
                &big_budget(),
                &Cancel::new(),
                &mut |_, _| {},
            );
            assert!(matches!(r, Err(DownloadError::Refused(_))), "{url}: {r:?}");
        }
        assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    }

    /// NB_VERIFY NB_09: the *download* agent vets resolved addresses at connect time too (the
    /// `get_download` path was only tested through the URL pre-check, so a missing resolver on
    /// this agent went unnoticed). Bypasses `FetchPolicy` by calling the transport directly.
    #[test]
    fn unite_download_transport_vets_addresses_at_connect_time() {
        use crate::web::fetch::TransportError;
        let f = fx(|_| Reply::Bytes(200, flac(100)));
        let t = UreqTransport::new();
        for url in [
            format!("https://127.0.0.1:{}/a", f.port),
            format!("https://localhost:{}/a", f.port),
        ] {
            let e = t
                .get_download(&url, Duration::from_secs(2))
                .err()
                .expect("loopback must be refused");
            assert!(matches!(e, TransportError::Blocked(_)), "{url}: {e:?}");
        }
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(f.hits.load(Ordering::SeqCst), 0, "a request reached loopback");
    }

    // ----- SECFIX_01: limits, reserve, staging cap, minimum progress ----------------------------

    fn stream(
        rig: &Rig,
        body: &mut dyn Read,
        declared: Option<u64>,
        lim: &DownloadLimits,
        budget: &Budget,
    ) -> Result<Streamed, DownloadError> {
        let cancel = Cancel::new();
        stream_to_temp(
            body,
            declared,
            rig.dir.path(),
            StreamRules {
                cap: lim.max_file_bytes,
                lim,
                budget,
                cancel: &cancel,
            },
            &mut |_| Ok(None),
            &mut |_, _| {},
        )
    }

    #[test]
    fn the_free_space_reserve_scales_with_the_volume() {
        // The injectable probes are plain `fn`s, so one function per volume size.
        fn small(_: &Path) -> Option<u64> {
            Some(8 * GIB)
        }
        fn mid(_: &Path) -> Option<u64> {
            Some(64 * GIB)
        }
        fn huge(_: &Path) -> Option<u64> {
            Some(4096 * GIB)
        }
        let dir = Path::new("/");
        let with = |f: fn(&Path) -> Option<u64>| DownloadLimits {
            volume_size: f,
            ..Default::default()
        };
        // 2 % of 8 GiB (164 MB) is above the 64 MiB floor.
        assert_eq!(with(small).reserve_for(dir), 8 * GIB / 50);
        assert!(with(small).reserve_for(dir) > FREE_SPACE_RESERVE);
        // 2 % of 64 GiB is about 1.3 GB.
        assert_eq!(with(mid).reserve_for(dir), 64 * GIB / 50);
        // On a huge disk it stops at 2 GiB.
        assert_eq!(with(huge).reserve_for(dir), RESERVE_CAP);
        // A tiny volume, or one whose size is unknown, keeps the 64 MiB floor.
        assert_eq!(with(|_| Some(GIB)).reserve_for(dir), FREE_SPACE_RESERVE);
        assert_eq!(with(|_| None).reserve_for(dir), FREE_SPACE_RESERVE);

        // The reserve is what every check holds back: with 3 GiB free on a 64 GiB volume a
        // transfer may take 3 GiB minus the ~1.3 GB reserve, not 3 GiB minus 64 MiB.
        let lim = DownloadLimits {
            free_space: |_| Some(3 * GIB),
            volume_size: mid,
            ..Default::default()
        };
        assert!(check_space(&lim, dir, GIB).is_ok());
        assert!(matches!(
            check_space(&lim, dir, 2 * GIB),
            Err(DownloadError::NoSpace { .. })
        ));
        let flat = DownloadLimits {
            volume_size: |_| Some(GIB),
            ..lim.clone()
        };
        assert!(
            check_space(&flat, dir, 2 * GIB).is_ok(),
            "with the flat 64 MiB reserve that would have been admitted"
        );
    }

    #[test]
    fn the_shared_staging_cap_follows_free_space_and_the_hard_cap() {
        let lim = DownloadLimits {
            free_space: |_| Some(10 * GIB),
            volume_size: |_| Some(100 * GIB), // reserve 2 GiB
            ..Default::default()
        };
        let dir = Path::new("/");
        let st = StagingBudget::new(16 * GIB);
        assert_eq!(st.cap(&lim, dir), 8 * GIB, "free space minus the reserve");
        assert!(st.admit(&lim, dir, 8 * GIB).is_ok());
        assert!(st.admit(&lim, dir, 8 * GIB + 1).is_err());
        let small = StagingBudget::new(GIB);
        assert_eq!(small.cap(&lim, dir), GIB, "the hard cap wins when it is lower");
        // Bytes already staged count: two budgets drawing from one cap.
        let a = Budget::with_staging(16 * GIB, Arc::new(StagingBudget::new(1000)));
        assert!(a.try_add(600));
        assert_eq!(a.try_reserve(600), Err(BudgetRefusal::Shared));
        assert_eq!(a.used(), 600, "a refused reservation leaves no batch share behind");
    }

    #[test]
    fn a_stalled_or_dripping_transfer_is_cut_off_but_slow_and_steady_is_not() {
        let rig = Rig::new();
        let lim = DownloadLimits {
            stall_timeout: Duration::from_millis(200),
            min_progress_bytes: 100,
            free_space: |_| Some(1 << 40),
            ..Default::default()
        };
        /// `n` bytes per read, one read every `pause`, up to `total` bytes.
        struct Paced {
            sent: usize,
            total: usize,
            n: usize,
            pause: Duration,
        }
        impl Read for Paced {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(self.pause);
                let n = self.n.min(self.total - self.sent).min(buf.len());
                let at = self.sent;
                for (i, b) in buf[..n].iter_mut().enumerate() {
                    *b = if at + i < 8 { b"fLaC\0\0\0\""[at + i] } else { 7 };
                }
                self.sent += n;
                Ok(n)
            }
        }
        // Steady: 60 bytes per 20 ms (3 KB/s, well over 100 B per 200 ms), for a total time far
        // above the stall timeout. No total-duration limit exists any more.
        let started = Instant::now();
        let ok = stream(
            &rig,
            &mut Paced { sent: 0, total: 3600, n: 60, pause: Duration::from_millis(20) },
            None,
            &lim,
            &big_budget(),
        );
        assert!(ok.is_ok(), "{ok:?}");
        assert!(started.elapsed() > lim.stall_timeout * 3);
        drop(ok); // (a finished file is guarded until its owner is done with it)
        // Drip: one byte per 40 ms is 5 bytes per window, under the minimum.
        let e = stream(
            &rig,
            &mut Paced { sent: 0, total: 4000, n: 1, pause: Duration::from_millis(40) },
            None,
            &lim,
            &big_budget(),
        )
        .unwrap_err();
        assert_eq!(e, DownloadError::Timeout);
        assert!(rig.files().is_empty(), "the partial file is removed");
    }

    #[test]
    fn the_default_limits_accept_a_2_gib_declared_length_and_the_old_ones_still_refuse_oversize() {
        let lim = DownloadLimits::default();
        assert_eq!(lim.max_file_bytes, 4 * GIB - 1, "the FAT32 file-size ceiling");
        assert_eq!(lim.max_batch_bytes, 16 * GIB);
        assert!(2 * GIB <= lim.max_file_bytes);
        // The message names the limit in units a person reads.
        assert_eq!(
            DownloadError::TooLarge { limit: lim.max_file_bytes }.to_string(),
            "larger than the 4 GB limit"
        );
        assert_eq!(describe_bytes(512 * MIB), "512 MB");
        assert_eq!(describe_bytes(GIB + GIB / 2), "1.5 GB");
        // An environment override can lower or raise it, never past the hard ceiling.
        assert!(lim.max_file_bytes <= HARD_MAX_BYTES);
    }
}
