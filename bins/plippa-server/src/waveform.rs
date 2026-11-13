//! Waveform sidecar extraction (WAVE_01): decode off the playback path, write `.wfm` atomically.
//!
//! Policy (operator decision): always running in this process at `nice +10`, the currently
//! playing track jumps the queue, throttled while playing, full speed when idle. One worker
//! thread, one decoder at a time: predictable USB I/O, no second core stolen from playback.
//!
//! The sidecar lives next to the media (`<root>/.plippa/waveforms/<hash>.wfm`) and is written
//! tmp + `sync_all` + rename, so a crash never leaves a half file that a reader would trust.
//!
//! The sidecar directory is on **untrusted removable media**: `.plippa`, `waveforms` or a
//! planted tmp name can be a symlink to anywhere on the filesystem. The writer therefore never
//! follows a symlink (refusing the whole root when `.plippa`/`waveforms` is one), creates the
//! tmp with `O_NOFOLLOW | O_EXCL` and a random name, and renames inside that directory only.

use crate::scanner::lower_thread_priority;
use crate::service::{ScanPlan, ServerService};
use plippa_audio::isolate::Isolation;
use plippa_audio::{OverspecPolicy, DEFAULT_CHUNK_FRAMES};
use plippa_core::Capabilities;
use plippa_core::waveform::{
    self, decode_header, Accumulator, Waveform, HEADER_LEN, MAX_PAYLOAD_BYTES,
};
use plippa_noubin::{
    waveform_path, LibraryReader, LibraryRoot, PLAYER_DB_DIR, WAVEFORM_DIR,
};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

// ---- On-device tunables ----------------------------------------------------------------
// Start values from the package; measure on the Pi before changing them.

/// Bucket size of the v1 format (20 ms). The reader takes it from the header.
pub const WAVE_BUCKET_MS: u16 = 20;
/// Sleep after each decoded chunk while the engine is playing.
pub const WAVE_PLAYING_PAUSE_MS: u64 = 8;
/// How often the current track is re-checked mid-file while playing.
pub const WAVE_PRIORITY_CHECK_MS: u64 = 500;
/// Idle poll for newly scanned / removed tracks.
pub const WAVE_LOOP_INTERVAL_MS: u64 = 2_000;

/// `Mutex` access that survives a poisoned lock (the worker must never panic out).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- Registry (worker ↔ HTTP) ----------------------------------------------------------

/// Per-file analysis state shown by `GET /api/waveforms/:id`.
///
/// [`WaveformState::Unavailable`] is not a registry state: the endpoint derives it from
/// `playable_items.available = 0` (the media file is missing, so nothing will be analysed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformState {
    Analysed,
    Analysing,
    Queued,
    Failed,
    Unavailable,
}

/// Shared, in-process view of what the worker is doing.
///
/// Keys are library-relative media paths (the strings [`analysis_candidates`] returns), so one
/// registry serves every root; the root label is only added in the HTTP response.
///
/// [`analysis_candidates`]: plippa_noubin::LibraryWriter::analysis_candidates
#[derive(Debug, Default)]
pub struct WaveformRegistry {
    /// Header-verified complete sidecars (seeded by the worker's sweep, too).
    analysed: Mutex<HashSet<String>>,
    /// Unsupported / corrupt / unreadable: never retried until the process restarts.
    failed: Mutex<HashSet<String>>,
    /// The file being decoded right now.
    analysing: Mutex<Option<String>>,
    /// TRACK_01: malformed frames the decoder skipped, for files analysed in this process.
    /// A file found already analysed on disk (after a restart) has no entry: the sidecar does
    /// not record it, so "none known" and "none skipped" read the same.
    skipped: Mutex<HashMap<String, u64>>,
    /// TRACK_01: files the user asked to re-analyse that the worker has not drained yet.
    /// Lock order is `priority` then `requested`.
    priority: Mutex<Vec<WorkItem>>,
    /// Relpaths with a re-analysis requested and not finished. They read as `queued` even
    /// though a complete sidecar is on disk, until the worker has run them.
    requested: Mutex<HashSet<String>>,
}

impl WaveformRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy of the three sets, taken under the locks and then released: stat/read headers
    /// **outside** the lock.
    pub fn snapshot(&self) -> WaveformSnapshot {
        WaveformSnapshot {
            analysed: lock(&self.analysed).clone(),
            failed: lock(&self.failed).clone(),
            analysing: lock(&self.analysing).clone(),
            skipped: lock(&self.skipped).clone(),
            requested: lock(&self.requested).clone(),
        }
    }

    /// The user asked for `item` to be analysed again, ahead of the backlog.
    ///
    /// It stops reading as `analysed` / `failed` at once (a `failed` file the user retries
    /// must not keep saying `failed`). Asking again while it is still waiting replaces the
    /// earlier request, so the list never holds one file twice.
    pub fn request_reanalysis(&self, item: WorkItem) {
        let mut priority = lock(&self.priority);
        let mut requested = lock(&self.requested);
        lock(&self.analysed).remove(&item.relpath);
        lock(&self.failed).remove(&item.relpath);
        lock(&self.skipped).remove(&item.relpath);
        requested.insert(item.relpath.clone());
        priority.retain(|w| w.sidecar != item.sidecar);
        priority.push(item);
    }

    /// Hand the waiting requests to the worker, oldest first.
    pub fn take_priority(&self) -> Vec<WorkItem> {
        std::mem::take(&mut *lock(&self.priority))
    }

    /// Whether `media_relpath` has a re-analysis that has not finished.
    pub fn is_requested(&self, media_relpath: &str) -> bool {
        lock(&self.requested).contains(media_relpath)
    }

    /// The worker finished a file (analysed or failed). Its request is settled unless the user
    /// asked again while it was running: that newer request is still waiting in `priority`.
    fn settle_request(&self, media_relpath: &str) {
        let priority = lock(&self.priority);
        if !priority.iter().any(|w| w.relpath == media_relpath) {
            lock(&self.requested).remove(media_relpath);
        }
    }

    /// The worker starts a file (also used by tests).
    pub fn begin(&self, media_relpath: &str) {
        *lock(&self.analysing) = Some(media_relpath.to_string());
    }

    /// The worker stopped a file, for any reason (success, abandon, failure).
    pub fn end(&self) {
        *lock(&self.analysing) = None;
    }

    /// A complete sidecar was written (or found valid on the first sweep).
    pub fn mark_analysed(&self, media_relpath: &str) {
        lock(&self.failed).remove(media_relpath);
        lock(&self.analysed).insert(media_relpath.to_string());
    }

    /// Like [`Self::mark_analysed`], remembering how many malformed frames were skipped.
    pub fn mark_analysed_with_skips(&self, media_relpath: &str, skipped_frames: u64) {
        {
            let mut skipped = lock(&self.skipped);
            if skipped_frames > 0 {
                skipped.insert(media_relpath.to_string(), skipped_frames);
            } else {
                skipped.remove(media_relpath);
            }
        }
        self.mark_analysed(media_relpath);
    }

    /// This file will not be retried until the process restarts.
    pub fn mark_failed(&self, media_relpath: &str) {
        lock(&self.skipped).remove(media_relpath);
        lock(&self.analysed).remove(media_relpath);
        lock(&self.failed).insert(media_relpath.to_string());
    }

    /// First match wins: analysed → failed → analysing (queued otherwise).
    pub fn state(&self, media_relpath: &str) -> WaveformState {
        self.snapshot().state_of(media_relpath)
    }
}

/// A consistent copy of [`WaveformRegistry`] for one HTTP response.
#[derive(Debug, Clone, Default)]
pub struct WaveformSnapshot {
    pub analysed: HashSet<String>,
    pub failed: HashSet<String>,
    pub analysing: Option<String>,
    pub skipped: HashMap<String, u64>,
    pub requested: HashSet<String>,
}

impl WaveformSnapshot {
    /// Per-item precedence from the package: analysed → failed → analysing → queued.
    ///
    /// The worker clears `analysing` before recording a failure, so the order only guards
    /// against a stale read.
    pub fn state_of(&self, media_relpath: &str) -> WaveformState {
        // A re-analysis the worker has not started yet: queued, whatever the last run said.
        if self.requested.contains(media_relpath)
            && self.analysing.as_deref() != Some(media_relpath)
        {
            WaveformState::Queued
        } else if self.analysed.contains(media_relpath) {
            WaveformState::Analysed
        } else if self.failed.contains(media_relpath) {
            WaveformState::Failed
        } else if self.analysing.as_deref() == Some(media_relpath) {
            WaveformState::Analysing
        } else {
            WaveformState::Queued
        }
    }
}

// ---- Sidecar I/O -----------------------------------------------------------------------

/// Read a `.wfm` without trusting its header: 48-byte header first, then exactly the payload
/// the header declares, never more than [`MAX_PAYLOAD_BYTES`].
///
/// The file must be exactly `HEADER_LEN + bucket_count` bytes: the format has no extension
/// mechanism, so a trailing byte means a corrupt or foreign file and is rejected.
///
/// Returns `None` for any I/O or validation failure; the file is untrusted removable media.
pub fn read_sidecar(path: &Path) -> Option<Waveform> {
    let mut f = File::open(path).ok()?;
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header).ok()?;
    // Validates magic / version / bucket_ms and caps `bucket_count` before we allocate.
    let parsed = decode_header(&header).ok()?;
    let payload_len = parsed.bucket_count as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return None;
    }
    let mut data = vec![0u8; HEADER_LEN + payload_len];
    data[..HEADER_LEN].copy_from_slice(&header);
    f.read_exact(&mut data[HEADER_LEN..]).ok()?;
    let mut trailing = [0u8; 1];
    match f.read_exact(&mut trailing) {
        // Any byte after the declared payload is a corrupt or foreign file.
        Ok(()) => return None,
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
        Err(_) => return None,
    }
    waveform::decode(&data).ok()
}

/// Cheap status check: is there a complete, exactly-sized `.wfm` at `path`?
///
/// Reads only the header (48 bytes) plus the file length, so the HTTP endpoint can answer
/// after a restart without touching the payload.
pub fn sidecar_is_complete(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let mut header = [0u8; HEADER_LEN];
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    if f.read_exact(&mut header).is_err() {
        return false;
    }
    let Ok(parsed) = decode_header(&header) else {
        return false;
    };
    parsed.is_complete() && meta.len() == (HEADER_LEN + parsed.bucket_count as usize) as u64
}

/// Why a sidecar could not be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarError {
    /// `.plippa` or `.plippa/waveforms` is a symlink (or not a directory): never write there.
    UnsafeDir(String),
    /// Any other I/O failure (the tmp file is removed before this is returned).
    Io(String),
}

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafeDir(m) | Self::Io(m) => write!(f, "{m}"),
        }
    }
}

/// A directory that must be a real directory (never a symlink) before anything is written in
/// it. Missing directories are created one level at a time and re-checked after an
/// `AlreadyExists` race.
fn ensure_real_dir(dir: &Path) -> Result<(), SidecarError> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(SidecarError::UnsafeDir(format!(
            "refusing to write waveform sidecars: {} is a symbolic link",
            dir.display()
        ))),
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(SidecarError::UnsafeDir(format!(
            "refusing to write waveform sidecars: {} is not a directory",
            dir.display()
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound => match fs::create_dir(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => ensure_real_dir(dir),
            Err(e) => Err(SidecarError::Io(format!(
                "cannot create {}: {e}",
                dir.display()
            ))),
        },
        Err(e) => Err(SidecarError::Io(format!(
            "cannot inspect {}: {e}",
            dir.display()
        ))),
    }
}

/// Write `bytes` to `path` atomically (tmp in the same directory, `sync_all`, rename).
///
/// Hardened for untrusted media: `.plippa`/`waveforms` symlinks are refused outright, the tmp
/// is created with `O_NOFOLLOW | O_EXCL` under an unpredictable name (a planted `<hash>.wfm.tmp`
/// can neither be followed nor block the write), and a random-name collision simply retries.
/// On any error the tmp file is removed and a previous sidecar is left byte-identical.
pub fn write_sidecar_atomic(path: &Path, bytes: &[u8]) -> Result<(), SidecarError> {
    let dir = path
        .parent()
        .ok_or_else(|| SidecarError::Io(format!("sidecar has no parent: {}", path.display())))?;
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_real_dir(parent)?;
    }
    ensure_real_dir(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| SidecarError::Io(format!("sidecar has no file name: {}", path.display())))?
        .to_string_lossy()
        .into_owned();
    for _ in 0..8 {
        let nonce: u64 = rand::random();
        let tmp = dir.join(format!(".{name}.{nonce:016x}.tmp"));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
        {
            Ok(f) => f,
            // Someone planted *this* name: try another, never fail the file over it.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(SidecarError::Io(format!(
                    "cannot create tmp in {}: {e}",
                    dir.display()
                )))
            }
        };
        let result = file
            .write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| SidecarError::Io(format!("cannot write tmp: {e}")))
            .and_then(|_| {
                drop(file);
                // Same directory, so this is an atomic replace; a symlink at `path` is
                // replaced as an entry, never followed.
                fs::rename(&tmp, path)
                    .map_err(|e| SidecarError::Io(format!("cannot rename tmp: {e}")))
            });
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok(());
    }
    Err(SidecarError::Io(format!(
        "cannot create a unique tmp file in {} after 8 attempts",
        dir.display()
    )))
}

/// The reason the sidecar directory of `root` must not be written to, if any.
///
/// `.plippa` and `.plippa/waveforms` may be symlinks planted on the stick. Checked with
/// `symlink_metadata` (never `metadata`, which would follow them) before the worker decodes
/// anything for that root, so a hostile stick costs no decoding work.
pub fn sidecar_dir_problem(root: &Path) -> Option<String> {
    let plippa = root.join(PLAYER_DB_DIR);
    let problem = |p: &Path| match fs::symlink_metadata(p) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Some(format!("{} is a symbolic link", p.display()))
        }
        Ok(meta) if !meta.is_dir() => Some(format!("{} is not a directory", p.display())),
        _ => None,
    };
    problem(&plippa).or_else(|| problem(&plippa.join(WAVEFORM_DIR)))
}

/// Whether a `playable_items.media_relpath` may be joined under a library root.
///
/// The index DB is on the same untrusted stick as the media, so a tampered row must not be
/// able to point the decoder (or the sidecar path) outside the root. This is the same
/// lexical rule the engine applies ([`plippa_noubin::confine`], `SECFIX_05`): no absolute
/// paths, no `..`, `.` or empty components, no backslash, NUL, control or bidi characters.
/// The physical check (no symlink, a regular file, never blocking) is [`media_fingerprint`]
/// and [`confined_media`].
fn safe_media_relpath(relpath: &str) -> bool {
    plippa_noubin::confine::clean_relpath(relpath).is_some_and(|clean| clean == relpath)
}

/// Open `relpath` below `root` through the shared confinement function: a plain regular file
/// reachable without a symlink, opened without blocking. The engine uses the same function.
fn confined_media(root: &Path, relpath: &str) -> Option<plippa_noubin::confine::ConfinedMedia> {
    if !safe_media_relpath(relpath) {
        return None;
    }
    plippa_noubin::confine::open_library_media(root, relpath).ok()
}

/// Source fingerprint of a media file (`mtime_ns = -1` when unknown), taken from the
/// already-checked open handle (`None` when the path is not a confined regular file).
fn media_fingerprint(root: &Path, relpath: &str) -> Option<(u64, i64)> {
    let meta = confined_media(root, relpath)?.file.metadata().ok()?;
    Some(fingerprint_of(&meta))
}

fn fingerprint_of(meta: &fs::Metadata) -> (u64, i64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(-1);
    (meta.len(), mtime)
}

/// Source fingerprint of a sidecar file (`mtime_ns = -1` when unknown).
fn source_fingerprint(path: &Path) -> Option<(u64, i64)> {
    Some(fingerprint_of(&fs::metadata(path).ok()?))
}

fn sidecar_fingerprint(path: &Path) -> Option<(u64, i64)> {
    source_fingerprint(path)
}

// ---- Queue ------------------------------------------------------------------------------

/// One media file that needs a sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub root: PathBuf,
    /// Library-relative media path (the registry key).
    pub relpath: String,
    pub media: PathBuf,
    pub sidecar: PathBuf,
    pub source_size: u64,
    pub source_mtime_ns: i64,
}

/// The work item for one media file under `root`, built from the same checks the sweep uses:
/// a plain relative path (the index is on untrusted media), a sidecar directory that is not a
/// planted symlink, and a media file that is still there. `None` when any of them fails.
pub(crate) fn item_for(root: &Path, relpath: &str) -> Option<WorkItem> {
    if !safe_media_relpath(relpath) || sidecar_dir_problem(root).is_some() {
        return None;
    }
    let media = root.join(relpath);
    let (size, mtime_ns) = media_fingerprint(root, relpath)?;
    Some(WorkItem {
        root: root.to_path_buf(),
        relpath: relpath.to_string(),
        sidecar: waveform_path(root, relpath),
        media,
        source_size: size,
        source_mtime_ns: mtime_ns,
    })
}

/// What the worker remembers about a file it has already accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidecarCheck {
    source: (u64, i64),
    /// `None` = the sidecar was missing when checked (a later write invalidates the entry).
    sidecar: Option<(u64, i64)>,
}

// ---- Analysis seam (fake in tests) ------------------------------------------------------

/// What a successful analysis has to tell the caller beyond "the sidecar is written".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnalysisReport {
    /// Malformed frames the decoder skipped to finish the file (MEDIA_02).
    pub skipped_frames: u64,
}

/// Outcome of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalysisError {
    /// A more important track took over (or playback started): no write, retry later.
    Abandoned,
    /// Unsupported / corrupt / I/O failure: the file goes into the failure set.
    Failed(String),
}

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Abandoned => write!(f, "abandoned"),
            Self::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// Decodes one media file and writes its sidecar (or abandons without writing).
///
/// A trait so tests can exercise queue priority and abandonment without an audio file.
pub trait WaveformAnalyser {
    /// `checkpoint` is called between chunks; `false` means abandon (no write).
    fn analyse(
        &mut self,
        item: &WorkItem,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<AnalysisReport, AnalysisError>;
}

/// A failure message that says how many malformed frames were skipped before it (none: unchanged).
fn with_skip_count(message: String, skipped: u64) -> String {
    match skipped {
        0 => message,
        1 => format!("{message} (1 bad frame skipped before this)"),
        n => format!("{n} bad frames were skipped before this: {message}"),
    }
}

/// The real analyser: decoded chunks → [`Accumulator`] → atomic `.wfm`.
///
/// It decodes under the board's capabilities (MEDIA_01), so a file this player cannot play is
/// not analysed: it fails with the reason, exactly like any other unanalysable file.
///
/// The decoder runs in the isolated media helper (SECFIX_06): a hostile file that aborts a
/// decoder kills the helper, not this process; the file is marked and fails like any other
/// unanalysable file, and the worker goes on with the next one. A long file is bounded by the
/// helper's progress watchdog and memory limit, never by a total time.
#[derive(Debug, Default)]
pub struct StreamingAnalyser {
    capabilities: Capabilities,
    /// `None`: the process-wide helper handle.
    isolation: Option<Isolation>,
}

impl StreamingAnalyser {
    pub fn new(capabilities: Capabilities) -> Self {
        Self { capabilities, isolation: None }
    }

    /// Use another helper handle (tests inject a counting one).
    pub fn with_isolation(mut self, isolation: Isolation) -> Self {
        self.isolation = Some(isolation);
        self
    }
}

impl WaveformAnalyser for StreamingAnalyser {
    fn analyse(
        &mut self,
        item: &WorkItem,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Result<AnalysisReport, AnalysisError> {
        let ext = item
            .media
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !plippa_audio::formats::format_enabled(&self.capabilities, &ext) {
            return Err(AnalysisError::Failed(format!(
                "this player cannot decode {ext}"
            )));
        }
        // The same confinement the engine uses: a symlink, FIFO, device or directory (a hostile
        // stick) is a failed file, never opened by the decoder. The checked open file itself is
        // handed to the helper, which never opens a path.
        let Some(confined) = confined_media(&item.root, &item.relpath) else {
            return Err(AnalysisError::Failed(
                "the media path is not a plain file inside the library".into(),
            ));
        };
        let isolation = self.isolation.clone().unwrap_or_else(|| Isolation::global().clone());
        let mut decoder = isolation
            .open_decoder(
                &confined.file,
                &item.media,
                &self.capabilities,
                OverspecPolicy::Downsample,
            )
            .map_err(|e| AnalysisError::Failed(e.to_string()))?;
        let mut acc: Option<Accumulator> = None;
        loop {
            if !checkpoint() {
                return Err(AnalysisError::Abandoned);
            }
            match decoder.next_chunk(DEFAULT_CHUNK_FRAMES) {
                Ok(Some(pcm)) => {
                    let rate = pcm.sample_rate_hz;
                    let channels = pcm.channels;
                    let a = acc.get_or_insert_with(|| Accumulator::new(rate, channels, WAVE_BUCKET_MS));
                    a.push(&pcm.samples);
                    if a.is_truncated() {
                        return Err(AnalysisError::Failed(
                            "waveform exceeds the 4 MiB sidecar cap".into(),
                        ));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(AnalysisError::Failed(with_skip_count(
                        e.to_string(),
                        decoder.skipped_packets(),
                    )))
                }
            }
        }
        let skipped = decoder.skipped_packets();
        if skipped > 0 {
            eprintln!(
                "plippa-server: waveform for {} analysed, {skipped} bad frame(s) skipped (last: {})",
                item.media.display(),
                decoder.last_decode_error().unwrap_or_default()
            );
        }
        let mut acc = acc.ok_or_else(|| AnalysisError::Failed("decoder produced no frames".into()))?;
        acc.finish();
        if acc.is_truncated() {
            return Err(AnalysisError::Failed(
                "waveform exceeds the 4 MiB sidecar cap".into(),
            ));
        }
        let wf = Waveform::new_complete(
            acc.bucket_ms(),
            acc.duration_ms(),
            acc.sample_rate_hz(),
            acc.channels(),
            item.source_size,
            item.source_mtime_ns,
            acc.into_peaks(),
        );
        write_sidecar_atomic(&item.sidecar, &waveform::encode_to_vec(&wf))
            .map_err(|e| AnalysisError::Failed(e.to_string()))?;
        Ok(AnalysisReport {
            skipped_frames: skipped,
        })
    }
}

// ---- Worker -----------------------------------------------------------------------------

/// The queue + failure set + per-file "already done" cache.
pub struct WaveformWorker {
    registry: Arc<WaveformRegistry>,
    readers: HashMap<PathBuf, LibraryReader>,
    /// Files that failed in this process (mirrors the registry's `failed` set).
    failed: HashSet<PathBuf>,
    /// Roots whose `.plippa`/`waveforms` is a symlink: logged once, never written, skipped.
    refused_roots: HashSet<PathBuf>,
    /// Index rows with an unsafe `media_relpath`, logged once each.
    rejected_media: HashSet<PathBuf>,
    /// Sidecar → the source/sidecar fingerprints it was accepted with.
    checked: HashMap<PathBuf, SidecarCheck>,
    queue: Vec<WorkItem>,
    /// Re-check cadence while playing (tests shorten it).
    priority_check_interval: Duration,
    /// PWRSW_01: set on shutdown; an analysis in progress abandons at its next checkpoint.
    stop: Option<Arc<AtomicBool>>,
}

impl WaveformWorker {
    pub fn new(registry: Arc<WaveformRegistry>) -> Self {
        Self {
            registry,
            readers: HashMap::new(),
            failed: HashSet::new(),
            refused_roots: HashSet::new(),
            rejected_media: HashSet::new(),
            checked: HashMap::new(),
            queue: Vec::new(),
            priority_check_interval: Duration::from_millis(WAVE_PRIORITY_CHECK_MS),
            stop: None,
        }
    }

    /// Abort an analysis in progress when `stop` is set (PWRSW_01 quiesce).
    pub fn with_stop(mut self, stop: Arc<AtomicBool>) -> Self {
        self.stop = Some(stop);
        self
    }

    pub fn registry(&self) -> &Arc<WaveformRegistry> {
        &self.registry
    }

    /// Files waiting for analysis, current track first.
    pub fn queue(&self) -> &[WorkItem] {
        &self.queue
    }

    /// Rebuild the queue from every open root.
    ///
    /// Cheap on repeat passes: a file whose source **and** sidecar fingerprints match the
    /// ones it was accepted with is skipped without opening either file. Anything new or
    /// changed gets one header check, and only then a queue slot.
    pub fn refresh(&mut self, plan: &ScanPlan) {
        self.readers
            .retain(|root, _| plan.roots.iter().any(|r| r == root));
        self.queue.clear();
        for root in &plan.roots {
            if !self.readers.contains_key(root) {
                match LibraryReader::open(&LibraryRoot::new(root)) {
                    Ok(r) => {
                        self.readers.insert(root.clone(), r);
                    }
                    Err(e) => {
                        eprintln!(
                            "plippa-server: waveform reader cannot open {}: {e}",
                            root.display()
                        );
                        continue;
                    }
                }
            }
            let reader = self.readers.get(root).expect("opened above");
            let candidates = match reader.analysis_candidates() {
                Ok(c) => c,
                Err(plippa_noubin::LibraryError::StorageVanished { path, detail }) => {
                    eprintln!(
                        "plippa-server: waveform scan dropped {}: {detail}",
                        path.display()
                    );
                    self.readers.remove(root);
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "plippa-server: waveform candidate query of {} failed: {e}",
                        root.display()
                    );
                    continue;
                }
            };
            // A symlinked `.plippa`/`waveforms` is a planted path: never decode for this root,
            // log once, and report its items as failed rather than `queued` for ever.
            if let Some(problem) = sidecar_dir_problem(root) {
                if self.refused_roots.insert(root.clone()) {
                    eprintln!(
                        "plippa-server: waveform sidecars disabled for {}: {problem}",
                        root.display()
                    );
                }
                for c in &candidates {
                    if safe_media_relpath(&c.relpath) {
                        self.failed.insert(waveform_path(root, &c.relpath));
                        self.registry.mark_failed(&c.relpath);
                    }
                }
                continue;
            }
            self.refused_roots.remove(root);
            for c in candidates {
                // The index DB is on the untrusted stick: never join an escaping path.
                if !safe_media_relpath(&c.relpath) {
                    let key = root.join(&c.relpath);
                    if self.rejected_media.insert(key.clone()) {
                        eprintln!(
                            "plippa-server: waveform analysis skipped an unsafe media path: {}",
                            key.display()
                        );
                    }
                    continue;
                }
                let media = root.join(&c.relpath);
                let source = if c.mtime_ns <= 0 {
                    match media_fingerprint(root, &c.relpath) {
                        Some(fp) => fp,
                        None => continue, // vanished, or not a plain file inside the library
                    }
                } else {
                    (c.size, c.mtime_ns)
                };
                let sidecar = waveform_path(root, &c.relpath);
                // A re-analysis the user asked for runs even though the sidecar on disk is
                // fine (or the file failed before), so none of the skip checks apply to it.
                if !self.registry.is_requested(&c.relpath) {
                    if self.failed.contains(&sidecar) {
                        continue;
                    }
                    let sidecar_state = sidecar_fingerprint(&sidecar);
                    let check = SidecarCheck {
                        source,
                        sidecar: sidecar_state,
                    };
                    if self.checked.get(&sidecar) == Some(&check) {
                        self.registry.mark_analysed(&c.relpath);
                        continue;
                    }
                    // One header check: complete and matching this source?
                    if read_sidecar(&sidecar).is_some_and(|w| {
                        w.is_complete() && w.header().matches_source(source.0, source.1)
                    }) {
                        self.checked.insert(sidecar, check);
                        self.registry.mark_analysed(&c.relpath);
                        continue;
                    }
                }
                self.queue.push(WorkItem {
                    root: root.clone(),
                    relpath: c.relpath,
                    media,
                    sidecar,
                    source_size: source.0,
                    source_mtime_ns: source.1,
                });
            }
        }
        // Current track first; otherwise the index order (stable).
        if let Some(current) = &plan.current_waveform {
            self.queue.sort_by_key(|w| w.sidecar != *current);
        }
    }

    /// Remove and return the next job (the current track's sidecar if it is queued).
    fn take_next(&mut self, current: Option<&Path>) -> Option<WorkItem> {
        if let Some(cur) = current {
            if let Some(i) = self.queue.iter().position(|w| w.sidecar == cur) {
                return Some(self.queue.remove(i));
            }
        }
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    /// Move the user's re-analysis requests to the front of the queue (TRACK_01).
    ///
    /// Each request forgets what the worker believed about the file (`checked`, `failed`) and
    /// replaces any queued entry for the same sidecar, so a file is never queued twice. The
    /// playing track still wins in [`Self::take_next`]. Called before every item, not once per
    /// sweep: a sweep can run for minutes.
    fn apply_priority(&mut self) {
        for item in self.registry.take_priority() {
            self.checked.remove(&item.sidecar);
            self.failed.remove(&item.sidecar);
            self.queue.retain(|w| w.sidecar != item.sidecar);
            self.queue.insert(0, item);
        }
    }

    /// Analyse one item and update the sets.
    fn analyse_one(
        &mut self,
        item: WorkItem,
        analyser: &mut dyn WaveformAnalyser,
        checkpoint: &mut dyn FnMut() -> bool,
    ) -> Outcome {
        let newly_failed = !self.failed.contains(&item.sidecar);
        self.registry.begin(&item.relpath);
        let result = analyser.analyse(&item, checkpoint);
        self.registry.end();
        match result {
            Ok(report) => {
                let check = SidecarCheck {
                    source: (item.source_size, item.source_mtime_ns),
                    sidecar: sidecar_fingerprint(&item.sidecar),
                };
                self.checked.insert(item.sidecar.clone(), check);
                self.registry
                    .mark_analysed_with_skips(&item.relpath, report.skipped_frames);
                self.registry.settle_request(&item.relpath);
                Outcome::Finished
            }
            Err(AnalysisError::Abandoned) => Outcome::Abandoned,
            Err(AnalysisError::Failed(msg)) => {
                self.failed.insert(item.sidecar.clone());
                self.registry.mark_failed(&item.relpath);
                self.registry.settle_request(&item.relpath);
                if newly_failed {
                    eprintln!(
                        "plippa-server: waveform analysis failed for {}: {msg}",
                        item.media.display()
                    );
                }
                // A vanished stick is not a failure of the file: drop the root.
                if !item.root.is_dir() {
                    self.readers.remove(&item.root);
                    self.queue.retain(|w| w.root != item.root);
                    return Outcome::RootGone;
                }
                Outcome::Finished
            }
        }
    }

    /// One sweep: refresh then work through the queue. Never panics.
    fn sweep(&mut self, service: &Mutex<ServerService>, analyser: &mut dyn WaveformAnalyser) {
        let plan = match service.lock() {
            Ok(mut s) => s.scan_plan(),
            Err(poisoned) => {
                // A poisoned service lock must not silently disable the worker: log it and
                // keep going with the inner state (the same policy as the registry locks).
                eprintln!(
                    "plippa-server: waveform worker: server service mutex poisoned; \
                     continuing with the inner state"
                );
                poisoned.into_inner().scan_plan()
            }
        };
        let mut poison_logged = false;
        self.run(
            plan,
            || match service.lock() {
                Ok(mut s) => Some(s.scan_plan()),
                Err(poisoned) => {
                    if !poison_logged {
                        poison_logged = true;
                        eprintln!(
                            "plippa-server: waveform worker: server service mutex poisoned \
                             while re-planning; keeping the last plan"
                        );
                    }
                    Some(poisoned.into_inner().scan_plan())
                }
            },
            analyser,
            &mut |pause| thread::sleep(pause),
        );
    }

    /// Work through `plan`'s queue; `replan` supplies the freshest plan between chunks and
    /// decides whether a playing track has taken over. `pace` is the playing throttle (the
    /// real thread sleep; tests inject a recorder so the decision is asserted without waiting).
    fn run(
        &mut self,
        plan: ScanPlan,
        mut replan: impl FnMut() -> Option<ScanPlan>,
        analyser: &mut dyn WaveformAnalyser,
        pace: &mut dyn FnMut(Duration),
    ) {
        self.refresh(&plan);
        let priority_every = self.priority_check_interval;
        let mut playing = plan.playing;
        let mut current = plan.current_waveform.clone();
        // An item abandoned twice in one pass is left for the next sweep (2 s) rather than
        // being decoded in a hot loop while the playing track keeps changing.
        let mut abandoned: HashSet<PathBuf> = HashSet::new();
        loop {
            self.apply_priority();
            let Some(item) = self.take_next(current.as_deref()) else {
                break;
            };
            let expected = item.sidecar.clone();
            let mut last_check = Instant::now();
            let stop = self.stop.clone();
            let mut checkpoint = || -> bool {
                // PWRSW_01: the host is going down; abandon without writing.
                if stop.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
                    return false;
                }
                if playing {
                    pace(Duration::from_millis(WAVE_PLAYING_PAUSE_MS));
                }
                if last_check.elapsed() >= priority_every {
                    last_check = Instant::now();
                    if let Some(p) = replan() {
                        playing = p.playing;
                        current = p.current_waveform.clone();
                        // Only playing has a "current track" that can steal the worker;
                        // while stopped the whole queue is analysed in order.
                        if playing {
                            if let Some(wanted) = current.as_ref() {
                                if *wanted != expected {
                                    return false;
                                }
                            }
                        }
                    }
                }
                true
            };
            let outcome = self.analyse_one(item.clone(), analyser, &mut checkpoint);
            drop(checkpoint);
            match outcome {
                Outcome::RootGone => continue,
                Outcome::Abandoned => {
                    // Re-insert at the head so the file is retried straight after the current
                    // track instead of waiting behind the whole queue; `take_next` still
                    // prefers the current track, so playback priority is not starved.
                    if abandoned.insert(item.sidecar.clone()) {
                        self.queue.insert(0, item);
                    }
                    continue;
                }
                Outcome::Finished => {}
            }
            // If the root is gone, drop the rest of its queue.
            if !item.root.is_dir() {
                self.readers.remove(&item.root);
                self.queue.retain(|w| w.root != item.root);
            }
        }
    }
}

/// What one analysis attempt did (see [`WaveformWorker::analyse_one`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Finished,
    Abandoned,
    RootGone,
}

/// Spawn the waveform worker thread (`nice +10`), polling every `interval`.
///
/// Returns the handle so shutdown can join it before the volume goes away (PWRSW_01).
pub fn spawn_waveform_worker(
    service: Arc<Mutex<ServerService>>,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("plippa-wave".into())
        .spawn(move || {
            lower_thread_priority();
            let (registry, capabilities) = match service.lock() {
                Ok(s) => (s.waveform_registry(), s.capabilities()),
                Err(poisoned) => {
                    eprintln!(
                        "plippa-server: waveform worker: server service mutex poisoned at \
                         start-up; continuing with the inner state"
                    );
                    let s = poisoned.into_inner();
                    (s.waveform_registry(), s.capabilities())
                }
            };
            let mut worker = WaveformWorker::new(registry).with_stop(Arc::clone(&stop));
            let mut analyser = StreamingAnalyser::new((*capabilities).clone());
            loop {
                worker.sweep(&service, &mut analyser);
                if crate::scanner::sleep_unless_stopped(&stop, interval) {
                    return;
                }
            }
        })
        .expect("spawn waveform thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::waveform::{encode_to_vec, Waveform};
    use plippa_noubin::LibraryWriter;

    fn audio_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/plippa-audio/tests/fixtures/tone_48k_16.wav")
    }

    /// A temp library whose `.noudata` release lists `rels` (all copied from the WAV fixture).
    fn library(rels: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mut list = Vec::new();
        for rel in rels {
            let dst = root.join(rel);
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::copy(audio_fixture(), &dst).unwrap();
            list.push(format!(r#"{{"mediaTitle": "{rel}", "localFilePaths": ["{rel}"]}}"#));
        }
        let noudata = format!(
            r#"{{"noudataVersion": "0.4.0", "noudataType": "release",
                 "playableItemTitle": "Wave", "mediaList": [{}]}}"#,
            list.join(",")
        );
        fs::write(root.join("_wave.noudata"), noudata).unwrap();
        LibraryWriter::open(&LibraryRoot::new(&root))
            .unwrap()
            .full_scan()
            .unwrap();
        (tmp, root)
    }

    /// SECFIX_05 (SYS-13): the waveform worker applies the same confinement as the engine
    /// (`engine::untrusted_path_tests` replays this corpus): the symlink and FIFO cases are
    /// what the old purely lexical `safe_media_relpath` let through.
    #[test]
    fn the_waveform_worker_and_the_engine_reject_the_same_hostile_paths() {
        use std::os::unix::fs::symlink;
        let (_tmp, root) = library(&["Music/ok.wav"]);
        let outside = root.parent().unwrap().join(format!("outside-{}.wav", std::process::id()));
        fs::write(&outside, b"RIFF").unwrap();
        symlink(&outside, root.join("Music/sym.wav")).unwrap();
        symlink("/etc", root.join("etc-link")).unwrap();
        extern "C" {
            fn mkfifo(path: *const std::os::raw::c_char, mode: u32) -> i32;
        }
        let fifo = root.join("Music/pipe.wav");
        let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(fifo.as_os_str())).unwrap();
        assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0);
        fs::create_dir_all(root.join("Music/dir.wav")).unwrap();

        assert!(item_for(&root, "Music/ok.wav").is_some());
        let outside_abs = outside.display().to_string();
        // Refused by the lexical rule (nothing is opened) ...
        for hostile in [
            outside_abs.as_str(),
            "/etc/hostname",
            "../outside.wav",
            "Music/../../outside.wav",
            "Music/a\\b.wav",
            "",
        ] {
            assert!(!safe_media_relpath(hostile), "{hostile:?}");
            assert!(item_for(&root, hostile).is_none(), "{hostile:?} became a work item");
        }
        // ... and by the physical rule: lexically fine, but a symlink, FIFO, directory or a
        // path through a symlinked directory.
        for hostile in ["Music/sym.wav", "Music/pipe.wav", "Music/dir.wav", "etc-link/hostname"] {
            assert!(safe_media_relpath(hostile), "{hostile:?}");
            assert!(item_for(&root, hostile).is_none(), "{hostile:?} became a work item");
        }
        // The analyser itself refuses an item whose media is not a confined regular file.
        let item = WorkItem {
            root: root.clone(),
            relpath: "Music/sym.wav".into(),
            media: root.join("Music/sym.wav"),
            sidecar: root.join(".plippa/waveforms/x.wfm"),
            source_size: 0,
            source_mtime_ns: 0,
        };
        let mut analyser = StreamingAnalyser::new(Capabilities::desktop_mock());
        let err = analyser.analyse(&item, &mut || true).unwrap_err();
        assert!(matches!(err, AnalysisError::Failed(_)), "{err:?}");
        fs::remove_file(outside).unwrap();
    }

    fn plan(root: &Path) -> ScanPlan {
        ScanPlan {
            roots: vec![root.to_path_buf()],
            playing: false,
            current_waveform: None,
        }
    }

    fn fake_sidecar(item: &WorkItem) -> Vec<u8> {
        encode_to_vec(&Waveform::new_complete(
            WAVE_BUCKET_MS,
            1_000,
            48_000,
            2,
            item.source_size,
            item.source_mtime_ns,
            vec![128; 50],
        ))
    }

    #[test]
    fn waveform_registry_precedence_is_analysed_failed_analysing_queued() {
        let r = WaveformRegistry::new();
        assert_eq!(r.state("a.wav"), WaveformState::Queued);
        r.begin("a.wav");
        assert_eq!(r.state("a.wav"), WaveformState::Analysing);
        r.mark_failed("a.wav");
        // A stale `analysing` must not hide the failure.
        assert_eq!(r.state("a.wav"), WaveformState::Failed);
        assert!(r.snapshot().failed.contains("a.wav"));
        r.mark_analysed("a.wav");
        assert_eq!(r.state("a.wav"), WaveformState::Analysed);
        // States stay mutually exclusive: a later success is not also a failure.
        assert!(!r.snapshot().failed.contains("a.wav"));
        r.end();
        assert!(r.snapshot().analysing.is_none());
    }

    #[test]
    fn waveform_sidecar_rejects_corrupt_and_over_cap_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.wfm");
        fs::write(&path, b"not a waveform at all").unwrap();
        assert!(read_sidecar(&path).is_none());
        assert!(!sidecar_is_complete(&path));

        // A header whose bucket_count would need a huge allocation: rejected up front.
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(b"PLWF");
        bytes[4] = 1;
        bytes[6..8].copy_from_slice(&20u16.to_le_bytes());
        bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(read_sidecar(&path).is_none());

        // Valid header, payload checksum wrong.
        let mut bytes = fake_sidecar(&WorkItem {
            root: tmp.path().to_path_buf(),
            relpath: "m.wav".into(),
            media: tmp.path().join("m.wav"),
            sidecar: path.clone(),
            source_size: 1,
            source_mtime_ns: 1,
        });
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();
        assert!(read_sidecar(&path).is_none());
    }

    #[test]
    fn waveform_atomic_write_failure_leaves_the_old_sidecar_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".plippa").join("waveforms");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wfm");
        fs::write(&path, b"old").unwrap();
        // A read-only directory makes the tmp creation fail before anything is replaced.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores directory permissions: then nothing can fail and the test cannot run.
        let probe = dir.join("probe");
        if fs::write(&probe, b"x").is_ok() {
            let _ = fs::remove_file(&probe);
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: a read-only directory does not block writes here (running as root)");
            return;
        }
        let err = write_sidecar_atomic(&path, b"new").unwrap_err();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(err, SidecarError::Io(_)), "{err}");
        assert!(err.to_string().contains("tmp"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"old");
    }

    #[test]
    fn waveform_atomic_write_cleans_up_the_tmp_when_the_rename_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".plippa").join("waveforms");
        fs::create_dir_all(&dir).unwrap();
        // The destination is a directory: the tmp is written, the rename must fail, and the
        // tmp must be removed rather than left behind on the stick.
        let path = dir.join("a.wfm");
        fs::create_dir(&path).unwrap();
        let err = write_sidecar_atomic(&path, b"new").unwrap_err();
        assert!(matches!(err, SidecarError::Io(_)), "{err}");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp left behind: {leftovers:?}");
    }

    #[test]
    fn waveform_atomic_write_does_not_follow_a_planted_tmp_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".plippa").join("waveforms");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wfm");
        let victim = tmp.path().join("victim");
        fs::write(&victim, b"do not touch").unwrap();
        // The old implementation's deterministic tmp name, pointed at a file we must not touch.
        std::os::unix::fs::symlink(&victim, dir.join("a.wfm.tmp")).unwrap();

        write_sidecar_atomic(&path, b"new sidecar").expect("a planted name must not block the write");
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
        assert_eq!(fs::read(&path).unwrap(), b"new sidecar");
        assert!(
            fs::symlink_metadata(dir.join("a.wfm.tmp"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the planted symlink is left alone"
        );
    }

    #[test]
    fn waveform_atomic_write_refuses_a_symlinked_waveforms_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        fs::create_dir_all(root.join(".plippa")).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(".plippa").join("waveforms")).unwrap();
        assert!(sidecar_dir_problem(&root).is_some());

        let path = root.join(".plippa").join("waveforms").join("a.wfm");
        let err = write_sidecar_atomic(&path, b"new").unwrap_err();
        assert!(matches!(err, SidecarError::UnsafeDir(_)), "{err}");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "victim dir untouched");

        // A symlinked `.plippa` itself is refused too.
        let root2 = tmp.path().join("lib2");
        fs::create_dir_all(&root2).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root2.join(".plippa")).unwrap();
        assert!(sidecar_dir_problem(&root2).is_some());
        let path2 = root2.join(".plippa").join("waveforms").join("b.wfm");
        let err = write_sidecar_atomic(&path2, b"new").unwrap_err();
        assert!(matches!(err, SidecarError::UnsafeDir(_)), "{err}");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn waveform_sidecar_rejects_appended_junk() {
        // The format has no extension mechanism: a longer file is corrupt even when the header
        // and payload checksum are valid.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.wfm");
        let item = WorkItem {
            root: tmp.path().to_path_buf(),
            relpath: "a.wav".into(),
            media: tmp.path().join("a.wav"),
            sidecar: path.clone(),
            source_size: 1,
            source_mtime_ns: 1,
        };
        let mut bytes = fake_sidecar(&item);
        assert!(read_sidecar(&path).is_none());
        fs::write(&path, &bytes).unwrap();
        assert!(read_sidecar(&path).is_some());
        assert!(sidecar_is_complete(&path));
        bytes.extend_from_slice(b"junk");
        fs::write(&path, &bytes).unwrap();
        assert!(read_sidecar(&path).is_none(), "trailing bytes must be rejected");
        assert!(!sidecar_is_complete(&path), "the cheap check agrees on the length");
    }

    #[test]
    fn waveform_analysis_writes_a_complete_sidecar_and_then_skips_it() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let plan = plan(&root);
        worker.refresh(&plan);
        assert_eq!(worker.queue().len(), 1);
        let item = worker.queue()[0].clone();

        let mut analyser = StreamingAnalyser::default();
        let mut keep_going = || true;
        worker.analyse_one(item.clone(), &mut analyser, &mut keep_going);
        assert_eq!(registry.state("a.wav"), WaveformState::Analysed);

        let wf = read_sidecar(&item.sidecar).expect("sidecar validates");
        assert!(wf.is_complete());
        assert!(wf.header().matches_source(item.source_size, item.source_mtime_ns));
        assert_eq!((wf.sample_rate_hz, wf.channels), (48_000, 2));
        assert_eq!(wf.bucket_ms, WAVE_BUCKET_MS);
        let expected_buckets = wf.duration_ms.div_ceil(u64::from(WAVE_BUCKET_MS));
        assert_eq!(wf.bucket_count() as u64, expected_buckets);
        assert!(item.sidecar.starts_with(root.join(".plippa/waveforms")));

        // Second pass: the accepted fingerprints mean no queue slot and no I/O.
        worker.refresh(&plan);
        assert!(worker.queue().is_empty());
    }

    #[test]
    fn waveform_analysis_uses_the_device_capabilities() {
        // An M4A on a board without AAC is not analysed: it fails with the reason, and no
        // sidecar is written. (The bytes are not decodable either; the gate comes first.)
        let (_t, root) = library(&["a.wav"]);
        let m4a = root.join("take.m4a");
        std::fs::write(&m4a, b"\0\0\0\x18ftypM4A \0\0\0\0M4A mp42").unwrap();
        let item = WorkItem {
            root: root.clone(),
            relpath: "take.m4a".into(),
            media: m4a,
            sidecar: root.join(".plippa/waveforms/take.m4a.wfm"),
            source_size: 24,
            source_mtime_ns: 0,
        };
        let mut analyser = StreamingAnalyser::new(Capabilities::desktop_mock());
        match analyser.analyse(&item, &mut || true) {
            Err(AnalysisError::Failed(why)) => {
                assert_eq!(why, "this player cannot decode m4a")
            }
            other => panic!("{other:?}"),
        }
        assert!(!item.sidecar.exists());
        // A board that claims AAC does not change it: this build has no AAC decoder, so the gate
        // refuses before any file is opened.
        let mut caps = Capabilities::desktop_mock();
        caps.audio_formats.aac = true;
        match StreamingAnalyser::new(caps).analyse(&item, &mut || true) {
            Err(AnalysisError::Failed(why)) => {
                assert_eq!(why, "this player cannot decode m4a")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn waveform_analysis_redoes_a_touched_media_file() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(registry);
        let plan = plan(&root);
        worker.refresh(&plan);
        let item = worker.queue()[0].clone();
        let mut analyser = StreamingAnalyser::default();
        worker.analyse_one(item.clone(), &mut analyser, &mut || true);
        worker.refresh(&plan);
        assert!(worker.queue().is_empty());

        // Rewrite the media file (different size + mtime), rescan, analyse again.
        fs::write(&item.media, b"different content").unwrap();
        LibraryWriter::open(&LibraryRoot::new(&root))
            .unwrap()
            .full_scan()
            .unwrap();
        worker.refresh(&plan);
        assert_eq!(worker.queue().len(), 1, "changed source re-queued");
        let again = worker.queue()[0].clone();
        assert_ne!(
            (again.source_size, again.source_mtime_ns),
            (item.source_size, item.source_mtime_ns)
        );
    }

    #[test]
    fn waveform_analysis_rewrites_a_truncated_sidecar() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let plan = plan(&root);
        worker.refresh(&plan);
        let item = worker.queue()[0].clone();
        let mut analyser = StreamingAnalyser::default();
        worker.analyse_one(item.clone(), &mut analyser, &mut || true);
        assert!(read_sidecar(&item.sidecar).is_some());

        fs::write(&item.sidecar, b"PLWF").unwrap();
        assert!(!sidecar_is_complete(&item.sidecar));
        worker.refresh(&plan);
        assert_eq!(worker.queue().len(), 1, "corrupt sidecar re-queued");
        let again = worker.queue()[0].clone();
        worker.analyse_one(again.clone(), &mut analyser, &mut || true);
        assert!(read_sidecar(&again.sidecar).is_some_and(|w| w.is_complete()));
    }

    /// Records the order it is asked to analyse and only writes when the checkpoint allows it.
    struct FakeAnalyser {
        order: Vec<String>,
        commits: Vec<String>,
        checks: usize,
        fail: bool,
        /// What every successful analysis reports as skipped frames.
        skips: u64,
    }

    impl FakeAnalyser {
        fn new(checks: usize, fail: bool) -> Self {
            Self {
                order: Vec::new(),
                commits: Vec::new(),
                checks,
                fail,
                skips: 0,
            }
        }
    }

    /// The idle/playing throttle recorder: `run` calls it once per chunk while playing.
    fn no_pace(_: Duration) {}

    impl WaveformAnalyser for FakeAnalyser {
        fn analyse(
            &mut self,
            item: &WorkItem,
            checkpoint: &mut dyn FnMut() -> bool,
        ) -> Result<AnalysisReport, AnalysisError> {
            self.order.push(item.relpath.clone());
            for _ in 0..self.checks {
                if !checkpoint() {
                    return Err(AnalysisError::Abandoned);
                }
            }
            if self.fail {
                return Err(AnalysisError::Failed("unsupported fixture".into()));
            }
            self.commits.push(item.relpath.clone());
            write_sidecar_atomic(&item.sidecar, &fake_sidecar(item))
                .map_err(|e| AnalysisError::Failed(e.to_string()))?;
            Ok(AnalysisReport {
                skipped_frames: self.skips,
            })
        }
    }

    #[test]
    fn waveform_queue_runs_the_playing_track_first_and_abandons_on_a_change() {
        let (_t, root) = library(&["a.wav", "b.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(registry);
        worker.priority_check_interval = Duration::ZERO;

        let sidecar_a = waveform_path(&root, "a.wav");
        let sidecar_b = waveform_path(&root, "b.wav");
        let first = ScanPlan {
            roots: vec![root.clone()],
            playing: true,
            current_waveform: Some(sidecar_a.clone()),
        };
        let second = ScanPlan {
            playing: true,
            current_waveform: Some(sidecar_b.clone()),
            ..first.clone()
        };
        // The first checkpoint sees a switch to b (playing), so a is abandoned unwritten and
        // re-queued at the head; b jumps in, then a is retried and written with the newest plan.
        let mut replans = vec![second.clone()];
        let mut fake = FakeAnalyser::new(2, false);
        worker.run(first, move || replans.pop(), &mut fake, &mut no_pace);
        assert_eq!(fake.order, vec!["a.wav", "b.wav", "a.wav"]);
        assert_eq!(fake.commits, vec!["b.wav", "a.wav"]);
        assert!(sidecar_a.is_file(), "the abandoned file is retried, not dropped");
        assert!(sidecar_b.is_file());
    }

    #[test]
    fn waveform_queue_does_not_hot_loop_on_a_file_abandoned_twice() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(registry);
        worker.priority_check_interval = Duration::ZERO;
        let other = waveform_path(&root, "other.wav");
        let plan = ScanPlan {
            roots: vec![root.clone()],
            playing: true,
            current_waveform: Some(other),
        };
        // A different playing track every time: the file is abandoned twice, then left for the
        // next sweep instead of being decoded again in a tight loop.
        let mut fake = FakeAnalyser::new(1, false);
        let replan = plan.clone();
        worker.run(plan, move || Some(replan.clone()), &mut fake, &mut no_pace);
        assert_eq!(fake.order, vec!["a.wav", "a.wav"]);
    }

    #[test]
    fn waveform_queue_puts_the_playing_track_first() {
        let (_t, root) = library(&["a.wav", "b.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(registry);
        let mut plan = plan(&root);
        plan.current_waveform = Some(waveform_path(&root, "b.wav"));
        worker.refresh(&plan);
        let order: Vec<&str> = worker.queue().iter().map(|w| w.relpath.as_str()).collect();
        assert_eq!(order, vec!["b.wav", "a.wav"]);
    }

    #[test]
    fn waveform_failures_are_not_retried_in_the_same_process() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let plan = plan(&root);
        worker.refresh(&plan);
        let mut fake = FakeAnalyser::new(1, true);
        worker.run(plan.clone(), || None, &mut fake, &mut no_pace);
        assert_eq!(fake.order, vec!["a.wav"]);
        assert_eq!(registry.state("a.wav"), WaveformState::Failed);

        worker.refresh(&plan);
        assert!(worker.queue().is_empty(), "failure set skips the file");
    }

    /// Replace `a.mp3` of a one-item library with `bytes`, rescan, and return the work item.
    fn mp3_item(bytes: &[u8]) -> (tempfile::TempDir, PathBuf, WorkItem) {
        let (t, root) = library(&["a.mp3"]);
        fs::write(root.join("a.mp3"), bytes).unwrap();
        LibraryWriter::open(&LibraryRoot::new(&root))
            .unwrap()
            .full_scan()
            .unwrap();
        let mut worker = WaveformWorker::new(Arc::new(WaveformRegistry::new()));
        worker.refresh(&plan(&root));
        let item = worker.queue()[0].clone();
        (t, root, item)
    }

    fn mp3_fixture() -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/plippa-audio/tests/fixtures/tone_48k_3s.mp3"),
        )
        .unwrap()
    }

    #[test]
    fn waveform_analysis_completes_on_a_file_with_a_bad_frame() {
        // The 3 s tone with 2 KiB of noise in the middle: one malformed frame used to fail the
        // whole analysis; now the sidecar is written, a few frames shorter.
        let mut bytes = mp3_fixture();
        bytes[62 * 384 + 100..62 * 384 + 100 + 2048].fill(0xFF);
        let (_t, _root, item) = mp3_item(&bytes);

        let mut analyser = StreamingAnalyser::default();
        let report = analyser.analyse(&item, &mut || true).expect("analysis completes");
        assert!(
            report.skipped_frames >= 1,
            "the skip count reaches the caller, not only the log"
        );

        let wf = read_sidecar(&item.sidecar).expect("sidecar validates");
        assert!(wf.is_complete());
        assert!(wf.header().matches_source(item.source_size, item.source_mtime_ns));
        assert!(
            (2_600..=3_100).contains(&wf.duration_ms),
            "duration {} ms",
            wf.duration_ms
        );
    }

    #[test]
    fn waveform_failure_on_a_hopeless_file_names_the_skip_budget_and_count() {
        // Ten good frames, then frames the codec refuses one after the other.
        let mut bytes = mp3_fixture()[..10 * 384].to_vec();
        for _ in 0..400 {
            let mut frame = vec![0u8; 417];
            frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            bytes.extend_from_slice(&frame);
        }
        let (_t, root, item) = mp3_item(&bytes);

        let mut analyser = StreamingAnalyser::default();
        match analyser.analyse(&item, &mut || true) {
            Err(AnalysisError::Failed(why)) => {
                assert!(why.contains("skip budget"), "{why}");
                assert!(why.contains("bad frames were skipped"), "{why}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(waveform_artifacts(&root).is_empty(), "a failed analysis writes nothing");
    }

    #[test]
    fn the_skip_count_is_added_to_a_failure_only_when_there_was_one() {
        assert_eq!(with_skip_count("boom".into(), 0), "boom");
        assert!(with_skip_count("boom".into(), 1).starts_with("boom"));
        assert!(with_skip_count("boom".into(), 3).starts_with("3 bad frames"));
    }

    #[test]
    fn waveform_worker_fails_a_real_unsupported_file_and_does_not_retry() {
        let (_t, root) = library(&["a.wav"]);
        fs::write(root.join("a.wav"), b"definitely not an audio stream").unwrap();
        LibraryWriter::open(&LibraryRoot::new(&root))
            .unwrap()
            .full_scan()
            .unwrap();
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let plan = plan(&root);
        let mut analyser = StreamingAnalyser::default();
        worker.run(plan.clone(), || None, &mut analyser, &mut no_pace);

        assert_eq!(registry.state("a.wav"), WaveformState::Failed);
        assert!(waveform_artifacts(&root).is_empty(), "no sidecar, no tmp");
        worker.refresh(&plan);
        assert!(worker.queue().is_empty(), "an unsupported file is not retried");
    }

    /// Blocks inside the analyser so the test can observe the registry while it "decodes".
    struct BlockingAnalyser {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl WaveformAnalyser for BlockingAnalyser {
        fn analyse(
            &mut self,
            item: &WorkItem,
            checkpoint: &mut dyn FnMut() -> bool,
        ) -> Result<AnalysisReport, AnalysisError> {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            if !checkpoint() {
                return Err(AnalysisError::Abandoned);
            }
            write_sidecar_atomic(&item.sidecar, &fake_sidecar(item))
                .map(|()| AnalysisReport::default())
                .map_err(|e| AnalysisError::Failed(e.to_string()))
        }
    }

    #[test]
    fn waveform_registry_reports_analysing_while_the_worker_decodes() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let plan = plan(&root);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let mut analyser = BlockingAnalyser {
                started: started_tx,
                release: release_rx,
            };
            worker.run(plan, || None, &mut analyser, &mut no_pace);
        });

        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker started the file");
        assert_eq!(registry.state("a.wav"), WaveformState::Analysing);
        assert_eq!(registry.snapshot().analysing.as_deref(), Some("a.wav"));

        release_tx.send(()).unwrap();
        handle.join().unwrap();
        assert_eq!(registry.state("a.wav"), WaveformState::Analysed);
        assert!(registry.snapshot().analysing.is_none());
    }

    #[test]
    fn waveform_throttle_pauses_between_chunks_only_while_playing() {
        // Full speed when idle: no pause at all.
        let (_idle_t, idle_root) = library(&["a.wav"]);
        let mut idle_worker = WaveformWorker::new(Arc::new(WaveformRegistry::new()));
        idle_worker.priority_check_interval = Duration::from_secs(3_600);
        let mut idle_paces = Vec::new();
        let mut fake = FakeAnalyser::new(4, false);
        idle_worker.run(plan(&idle_root), || None, &mut fake, &mut |d| {
            idle_paces.push(d)
        });
        assert_eq!(fake.commits, vec!["a.wav"]);
        assert!(idle_paces.is_empty(), "idle analysis must not sleep: {idle_paces:?}");

        // Playing: exactly one WAVE_PLAYING_PAUSE_MS pause per chunk, injected (not slept).
        let (_play_t, play_root) = library(&["a.wav"]);
        let mut play_worker = WaveformWorker::new(Arc::new(WaveformRegistry::new()));
        play_worker.priority_check_interval = Duration::from_secs(3_600);
        let mut play_paces = Vec::new();
        let play_plan = ScanPlan {
            playing: true,
            current_waveform: Some(waveform_path(&play_root, "a.wav")),
            ..plan(&play_root)
        };
        let mut fake = FakeAnalyser::new(4, false);
        play_worker.run(play_plan, || None, &mut fake, &mut |d| {
            play_paces.push(d)
        });
        assert_eq!(fake.commits, vec!["a.wav"]);
        assert_eq!(play_paces.len(), 4, "one pause per chunk: {play_paces:?}");
        assert!(play_paces
            .iter()
            .all(|d| *d == Duration::from_millis(WAVE_PLAYING_PAUSE_MS)));
    }

    /// Pulls the stick (renames the root away) partway through, then attempts the real write.
    struct VanishingAnalyser {
        root: PathBuf,
        moved_to: PathBuf,
    }

    impl WaveformAnalyser for VanishingAnalyser {
        fn analyse(
            &mut self,
            item: &WorkItem,
            checkpoint: &mut dyn FnMut() -> bool,
        ) -> Result<AnalysisReport, AnalysisError> {
            for _ in 0..3 {
                if !checkpoint() {
                    return Err(AnalysisError::Abandoned);
                }
            }
            fs::rename(&self.root, &self.moved_to).expect("root moved away");
            let err = write_sidecar_atomic(&item.sidecar, &fake_sidecar(item))
                .expect_err("the root is gone, the write must fail");
            Err(AnalysisError::Failed(format!("storage vanished: {err}")))
        }
    }

    #[test]
    fn waveform_worker_survives_the_root_vanishing_mid_analysis() {
        let (_t, root) = library(&["a.wav"]);
        let moved_dir = tempfile::tempdir().unwrap();
        let moved_to = moved_dir.path().join("pulled-stick");
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let mut analyser = VanishingAnalyser {
            root: root.clone(),
            moved_to: moved_to.clone(),
        };
        worker.run(plan(&root), || None, &mut analyser, &mut no_pace);

        assert!(worker.queue().is_empty(), "the vanished root's queue is dropped");
        assert!(worker.readers.is_empty(), "the reader is dropped");
        assert_eq!(registry.state("a.wav"), WaveformState::Failed);
        assert!(
            waveform_artifacts(&moved_to).is_empty(),
            "no partial .wfm or tmp left on the pulled stick"
        );
    }

    #[test]
    fn waveform_worker_refuses_a_root_with_a_symlinked_waveforms_dir() {
        let (tmp, root) = library(&["a.wav"]);
        let elsewhere = tmp.path().join("attacker");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(".plippa").join("waveforms")).unwrap();

        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        worker.refresh(&plan(&root));
        assert!(worker.queue().is_empty(), "never decode for a refused root");
        assert_eq!(registry.state("a.wav"), WaveformState::Failed);
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn media_relpaths_that_escape_the_root_are_rejected() {
        assert!(safe_media_relpath("Music/Band/Album/01 - One.mp3"));
        assert!(safe_media_relpath("a.b/c d.mp3"));
        for bad in [
            "",
            "../a.mp3",
            "Music/../../etc/passwd",
            "/etc/passwd",
            "./a.mp3",
            "Music//x/../y",
        ] {
            assert!(!safe_media_relpath(bad), "{bad:?} must be refused");
        }
    }

    /// Every `.wfm` / `.tmp` file below `dir` (for "nothing partial was left" assertions).
    fn waveform_artifacts(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                if is_dir {
                    stack.push(path);
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".wfm") || name.ends_with(".tmp") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// Measurement harness (not an assertion): `cargo test -p plippa-server --lib
    /// waveform_measure -- --ignored --nocapture`. Desktop debug build, warm page cache; the
    /// Pi numbers must be measured on the device.
    #[test]
    #[ignore = "manual timing measurement"]
    fn waveform_measure_decode_speed_and_sidecar_size() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(registry);
        let plan = plan(&root);
        worker.refresh(&plan);
        let item = worker.queue()[0].clone();
        let mut analyser = StreamingAnalyser::default();

        let idle = Instant::now();
        worker.analyse_one(item.clone(), &mut analyser, &mut || true);
        let idle_ms = idle.elapsed().as_secs_f64() * 1000.0;
        let sidecar_bytes = fs::metadata(&item.sidecar).unwrap().len();
        let wf = read_sidecar(&item.sidecar).unwrap();
        let audio_s = wf.duration_ms as f64 / 1000.0;

        fs::remove_file(&item.sidecar).unwrap();
        let mut playing = || {
            thread::sleep(Duration::from_millis(WAVE_PLAYING_PAUSE_MS));
            true
        };
        let started = Instant::now();
        worker.analyse_one(item.clone(), &mut analyser, &mut playing);
        let playing_ms = started.elapsed().as_secs_f64() * 1000.0;

        eprintln!(
            "waveform timing: {audio_s:.2}s audio, idle {idle_ms:.1} ms \
             ({:.1}x realtime), playing {playing_ms:.1} ms, sidecar {sidecar_bytes} bytes",
            audio_s * 1000.0 / idle_ms
        );
    }

    #[test]
    fn waveform_worker_survives_a_vanished_root() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(registry);
        let plan = plan(&root);
        worker.refresh(&plan);
        assert_eq!(worker.queue().len(), 1);
        fs::remove_dir_all(&root).unwrap();
        let mut fake = FakeAnalyser::new(1, true);
        worker.run(plan, || None, &mut fake, &mut no_pace);
        assert!(worker.queue().is_empty());
        assert!(worker.readers.is_empty(), "vanished root dropped");
    }

    // ---- TRACK_01: priority re-analysis --------------------------------------------------

    fn queue_order(worker: &WaveformWorker) -> Vec<&str> {
        worker.queue().iter().map(|w| w.relpath.as_str()).collect()
    }

    #[test]
    fn a_reanalysis_request_goes_to_the_front_but_the_playing_track_still_wins() {
        let (_t, root) = library(&["a.wav", "b.wav", "c.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        worker.refresh(&plan(&root));
        assert_eq!(queue_order(&worker), vec!["a.wav", "b.wav", "c.wav"]);

        registry.request_reanalysis(item_for(&root, "c.wav").expect("c is there"));
        worker.apply_priority();
        assert_eq!(queue_order(&worker), vec!["c.wav", "a.wav", "b.wav"]);
        let next = worker.take_next(None).expect("an item");
        assert_eq!(next.relpath, "c.wav", "the request is analysed ahead of the backlog");

        // Requested again with the playing track also queued: playback keeps its priority.
        registry.request_reanalysis(next);
        worker.apply_priority();
        let playing = waveform_path(&root, "b.wav");
        let next = worker.take_next(Some(&playing)).expect("an item");
        assert_eq!(next.relpath, "b.wav");
        assert_eq!(worker.take_next(None).expect("an item").relpath, "c.wav");
    }

    #[test]
    fn asking_again_while_queued_keeps_one_entry_at_the_front() {
        let (_t, root) = library(&["a.wav", "b.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        worker.refresh(&plan(&root));
        for _ in 0..5 {
            registry.request_reanalysis(item_for(&root, "b.wav").unwrap());
        }
        assert_eq!(
            registry.take_priority().len(),
            1,
            "five identical requests do not grow the list"
        );
        for _ in 0..5 {
            registry.request_reanalysis(item_for(&root, "b.wav").unwrap());
        }
        worker.apply_priority();
        assert_eq!(queue_order(&worker), vec!["b.wav", "a.wav"]);
        worker.apply_priority();
        assert_eq!(queue_order(&worker), vec!["b.wav", "a.wav"], "nothing left to drain");
    }

    #[test]
    fn a_failed_file_the_user_retries_stops_reading_failed_and_is_tried_again() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let mut fake = FakeAnalyser::new(1, true);
        worker.run(plan(&root), || None, &mut fake, &mut no_pace);
        assert_eq!(registry.state("a.wav"), WaveformState::Failed);

        registry.request_reanalysis(item_for(&root, "a.wav").unwrap());
        assert_eq!(
            registry.state("a.wav"),
            WaveformState::Queued,
            "Failed clears the moment the user asks"
        );
        // The next sweep: the failure set would normally skip the file; the request overrides.
        worker.run(plan(&root), || None, &mut fake, &mut no_pace);
        assert_eq!(fake.order, vec!["a.wav", "a.wav"], "one more attempt, not a loop");
        assert_eq!(registry.state("a.wav"), WaveformState::Failed, "it failed again");
        assert!(!registry.is_requested("a.wav"), "the request is settled");
    }

    #[test]
    fn a_finished_file_reads_queued_until_its_reanalysis_has_run() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let mut fake = FakeAnalyser::new(1, false);
        worker.run(plan(&root), || None, &mut fake, &mut no_pace);
        assert_eq!(registry.state("a.wav"), WaveformState::Analysed);

        registry.request_reanalysis(item_for(&root, "a.wav").unwrap());
        assert_eq!(registry.state("a.wav"), WaveformState::Queued);
        // A sweep whose fingerprints say "already analysed" must still queue it.
        worker.run(plan(&root), || None, &mut fake, &mut no_pace);
        assert_eq!(fake.order, vec!["a.wav", "a.wav"]);
        assert_eq!(registry.state("a.wav"), WaveformState::Analysed);
    }

    /// Asks for a re-analysis of the file it is decoding, as a user pressing the button would.
    struct RequestingAnalyser {
        registry: Arc<WaveformRegistry>,
        root: PathBuf,
        order: Vec<String>,
        seen_while_running: Vec<WaveformState>,
        asked: bool,
    }

    impl WaveformAnalyser for RequestingAnalyser {
        fn analyse(
            &mut self,
            item: &WorkItem,
            _checkpoint: &mut dyn FnMut() -> bool,
        ) -> Result<AnalysisReport, AnalysisError> {
            self.order.push(item.relpath.clone());
            if !self.asked {
                self.asked = true;
                self.registry
                    .request_reanalysis(item_for(&self.root, &item.relpath).unwrap());
                self.seen_while_running.push(self.registry.state(&item.relpath));
            }
            write_sidecar_atomic(&item.sidecar, &fake_sidecar(item))
                .map(|()| AnalysisReport::default())
                .map_err(|e| AnalysisError::Failed(e.to_string()))
        }
    }

    #[test]
    fn a_request_made_while_the_file_is_analysing_runs_again_afterwards() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let mut analyser = RequestingAnalyser {
            registry: Arc::clone(&registry),
            root: root.clone(),
            order: Vec::new(),
            seen_while_running: Vec::new(),
            asked: false,
        };
        // `run` begins the file (state `analysing`), the request arrives mid-run, and the same
        // sweep picks it up again once the first pass ends: one extra pass, no duplicate entry.
        worker.run(plan(&root), || None, &mut analyser, &mut no_pace);
        assert_eq!(analyser.order, vec!["a.wav", "a.wav"]);
        assert_eq!(analyser.seen_while_running, vec![WaveformState::Analysing]);
        assert!(!registry.is_requested("a.wav"));
        assert_eq!(registry.state("a.wav"), WaveformState::Analysed);
    }

    #[test]
    fn the_skip_count_is_kept_for_the_status_and_forgotten_on_reanalysis() {
        let (_t, root) = library(&["a.wav"]);
        let registry = Arc::new(WaveformRegistry::new());
        let mut worker = WaveformWorker::new(Arc::clone(&registry));
        let mut fake = FakeAnalyser::new(1, false);
        fake.skips = 2;
        worker.run(plan(&root), || None, &mut fake, &mut no_pace);
        assert_eq!(registry.snapshot().skipped.get("a.wav"), Some(&2));

        registry.request_reanalysis(item_for(&root, "a.wav").unwrap());
        assert!(registry.snapshot().skipped.is_empty(), "a stale count is not shown");
        fake.skips = 0;
        worker.run(plan(&root), || None, &mut fake, &mut no_pace);
        assert!(registry.snapshot().skipped.is_empty(), "zero skips records nothing");
    }

    #[test]
    fn item_for_refuses_escaping_paths_missing_files_and_planted_sidecar_dirs() {
        let (tmp, root) = library(&["a.wav"]);
        let item = item_for(&root, "a.wav").expect("a present file");
        assert_eq!(item.sidecar, waveform_path(&root, "a.wav"));
        assert_eq!(item.media, root.join("a.wav"));
        assert!(item.source_size > 0);

        for escaping in ["../outside.wav", "/etc/passwd", "./a.wav", "a/../a.wav", ""] {
            assert!(item_for(&root, escaping).is_none(), "{escaping:?}");
        }
        assert!(item_for(&root, "gone.wav").is_none(), "a vanished file");

        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(root.join(".plippa")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(".plippa").join("waveforms")).unwrap();
        assert!(item_for(&root, "a.wav").is_none(), "a symlinked sidecar directory");
    }
}
