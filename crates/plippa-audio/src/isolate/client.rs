//! The parent side: spawn a helper per file, talk to it, watch it, reap it, and remember the
//! files that killed one.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use plippa_core::Capabilities;

use super::limits::{self, Limits};
use super::marks::{CrashMarks, FileStamp};
use super::proto::{
    self, Request, Response, Status, StreamInfo, MAX_CHUNK_FRAMES, MEDIA_FD, PROTOCOL_VERSION,
};
use crate::decode::{AudioProbe, PcmBuffer};
use crate::error::AudioError;
use crate::index::Mp3Index;
use crate::position::ms_to_frame;
use crate::prepare::OverspecPolicy;
use crate::worker::BlockSource;

/// Environment variable overriding where the helper binary is (tests, odd installs).
pub const HELPER_ENV: &str = "PLIPPA_MEDIA_HELPER";
/// File name of the helper, found next to the running executable.
pub const HELPER_NAME: &str = "plippa-media-helper";

/// How long a helper may stay **silent** before it is killed. These are stall rules, never
/// total-duration limits: a stream that keeps answering can run for hours.
#[derive(Debug, Clone, Copy)]
pub struct Budgets {
    /// Startup (the `Hello` frame): the helper has not touched the file yet.
    pub start: Duration,
    /// Opening / probing a file: headers only.
    pub open: Duration,
    /// One decoded chunk.
    pub chunk: Duration,
    /// One seek (an unindexed backward seek in a long MP3 re-parses its prefix).
    pub seek: Duration,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(10),
            open: Duration::from_secs(10),
            chunk: Duration::from_secs(30),
            seek: Duration::from_secs(120),
        }
    }
}

struct Inner {
    helper: Result<PathBuf, String>,
    marks: Arc<CrashMarks>,
    budgets: Budgets,
    spawned: AtomicU64,
}

/// Shared handle for starting helpers. Cheap to clone.
#[derive(Clone)]
pub struct Isolation {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Isolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Isolation").field("helper", &self.inner.helper).finish()
    }
}

/// Where the helper is: `$PLIPPA_MEDIA_HELPER`, else next to the running executable (the
/// launcher starts its siblings from their own directory), else, for a test binary in
/// `target/<profile>/deps`, in `target/<profile>`.
///
/// In test builds (this crate's unit tests, and the engine and the server with the
/// `testsupport` feature; never a shipped binary) the helper is first brought up to date with `cargo build`, because `cargo test`
/// of another package does not build this crate's binary.
pub fn find_helper() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os(HELPER_ENV).filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() { Ok(p) } else { Err(format!("{} does not exist", p.display())) };
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this executable: {e}"))?;
    let dir = exe.parent().ok_or("this executable has no directory")?;
    #[cfg(any(test, feature = "testsupport"))]
    build_helper_for_tests(dir)?;
    let mut candidates = vec![dir.join(HELPER_NAME)];
    if dir.file_name().is_some_and(|n| n == "deps") {
        if let Some(up) = dir.parent() {
            candidates.push(up.join(HELPER_NAME));
        }
    }
    candidates
        .iter()
        .find(|c| c.is_file())
        .cloned()
        .ok_or_else(|| format!("{HELPER_NAME} not found next to {}", exe.display()))
}

/// Test builds only: make sure `target/<profile>/plippa-media-helper` is current.
#[cfg(any(test, feature = "testsupport"))]
fn build_helper_for_tests(exe_dir: &Path) -> Result<(), String> {
    let profile_dir = if exe_dir.file_name().is_some_and(|n| n == "deps") {
        exe_dir.parent().unwrap_or(exe_dir)
    } else {
        exe_dir
    };
    let release = profile_dir.file_name().is_some_and(|n| n == "release");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["build", "--quiet", "--bin", HELPER_NAME, "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    if release {
        cmd.arg("--release");
    }
    let out = cmd.output().map_err(|e| format!("cannot run cargo to build {HELPER_NAME}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("building {HELPER_NAME} failed: {}", String::from_utf8_lossy(&out.stderr)))
    }
}

static GLOBAL: OnceLock<Isolation> = OnceLock::new();

impl Isolation {
    /// A handle for the helper at `helper`, remembering crashes in `marks`.
    pub fn new(helper: impl Into<PathBuf>, marks: CrashMarks) -> Self {
        Self::build(Ok(helper.into()), Arc::new(marks), Budgets::default())
    }

    fn build(helper: Result<PathBuf, String>, marks: Arc<CrashMarks>, budgets: Budgets) -> Self {
        Self { inner: Arc::new(Inner { helper, marks, budgets, spawned: AtomicU64::new(0) }) }
    }

    /// Find the helper and the state directory from the environment.
    pub fn discover() -> Self {
        Self::build(find_helper(), Arc::new(CrashMarks::from_env()), Budgets::default())
    }

    /// The process-wide handle ([`discover`](Self::discover), once): what the engine and the
    /// server use. The environment is read on first use.
    pub fn global() -> &'static Isolation {
        GLOBAL.get_or_init(Self::discover)
    }

    /// The same helper and marks with other stall budgets (tests).
    pub fn with_budgets(&self, budgets: Budgets) -> Self {
        Self::build(self.inner.helper.clone(), Arc::clone(&self.inner.marks), budgets)
    }

    pub fn marks(&self) -> &CrashMarks {
        &self.inner.marks
    }

    /// Helpers started through this handle (diagnostics and tests: a marked file costs none).
    pub fn spawn_count(&self) -> u64 {
        self.inner.spawned.load(Ordering::Relaxed)
    }

    /// Refuse a file that is marked: no helper is started for it.
    fn refuse_if_marked(&self, path: &Path, stamp: FileStamp) -> Result<(), AudioError> {
        if self.inner.marks.is_marked(path, stamp) {
            return Err(AudioError::Unplayable {
                path: path.to_path_buf(),
                reason: "it crashed the media decoder earlier".into(),
            });
        }
        Ok(())
    }

    /// Whether this exact version of the file is marked (a cheap query, no spawn).
    pub fn is_marked(&self, path: &Path, file: &File) -> bool {
        FileStamp::of(file).is_ok_and(|s| self.inner.marks.is_marked(path, s))
    }

    /// Whether the file at `path` (by `stat`) is marked; for callers that have no handle yet.
    pub fn is_marked_by_path(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).is_ok_and(|m| {
            let stamp = FileStamp {
                mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                size: m.len(),
            };
            self.inner.marks.is_marked(path, stamp)
        })
    }

    fn spawn(&self, file: &File, limits: Limits) -> Result<HelperProc, AudioError> {
        let program = self
            .inner
            .helper
            .as_ref()
            .map_err(|reason| AudioError::HelperUnavailable { reason: reason.clone() })?;
        self.inner.spawned.fetch_add(1, Ordering::Relaxed);
        HelperProc::spawn(program, file, limits)
            .map_err(|e| AudioError::HelperUnavailable { reason: format!("cannot start it: {e}") })
    }

    /// Start a helper over `file`, confirm its protocol, return it with the file's stamp.
    fn start(&self, file: &File, path: &Path, limits: Limits) -> Result<(HelperProc, FileStamp), AudioError> {
        let stamp = FileStamp::of(file).map_err(|source| AudioError::Io { path: path.to_path_buf(), source })?;
        self.refuse_if_marked(path, stamp)?;
        let mut proc = self.spawn(file, limits)?;
        // Nothing about the file has been read yet: a failure before `Hello` is the helper's
        // own (missing library, refused limits, wrong version), never a verdict on the file.
        match proc.recv(self.inner.budgets.start) {
            Ok((Response::Hello { version }, _)) if version == PROTOCOL_VERSION => Ok((proc, stamp)),
            Ok((Response::Hello { version }, _)) => Err(AudioError::HelperUnavailable {
                reason: format!("helper speaks protocol {version}, expected {PROTOCOL_VERSION}"),
            }),
            Ok(_) => Err(AudioError::HelperUnavailable { reason: "helper did not say hello".into() }),
            Err(f) => {
                let how = proc.describe_failure(&f);
                Err(AudioError::HelperUnavailable { reason: format!("helper failed to start: {how}") })
            }
        }
    }

    /// Open `file` (already confined and checked by the caller; `path` names it for messages,
    /// the extension and the crash mark) as a decoder in a helper process.
    pub fn open_decoder(
        &self,
        file: &File,
        path: &Path,
        capabilities: &Capabilities,
        policy: OverspecPolicy,
    ) -> Result<RemoteDecoder, AudioError> {
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let (proc, stamp) = self.start(file, path, Limits::stream(size))?;
        let mut dec = RemoteDecoder {
            iso: self.clone(),
            proc: Some(proc),
            path: path.to_path_buf(),
            stamp,
            info: None,
            status: Status::default(),
            failed: None,
            index_stride: None,
        };
        let reply = dec.call(
            &Request::Open {
                name: path.to_string_lossy().into_owned(),
                capabilities: capabilities.clone(),
                refuse_overspec: matches!(policy, OverspecPolicy::Refuse),
            },
            self.inner.budgets.open,
        )?;
        match reply {
            (Response::Opened(info), _) => {
                dec.status = info.status.clone();
                dec.info = Some(info);
                Ok(dec)
            }
            (Response::Failed { error, .. }, _) => Err(error.into_audio(path.to_path_buf())),
            _ => Err(dec.protocol_failure("unexpected reply to open")),
        }
    }

    /// `validate_for_stream` for an untrusted `file`: the probe runs in a helper, which is gone
    /// again when this returns.
    pub fn validate_for_stream(
        &self,
        file: &File,
        path: &Path,
        capabilities: &Capabilities,
        policy: OverspecPolicy,
    ) -> Result<AudioProbe, AudioError> {
        let dec = self.open_decoder(file, path, capabilities, policy)?;
        let info = dec.info();
        Ok(AudioProbe {
            sample_rate_hz: info.sample_rate_hz,
            bit_depth: info.bit_depth,
            channels: info.channels,
            extension: info.extension.clone(),
        })
    }

    /// Length of `file` from its container headers (the scanner's probe), `Ok(None)` when it
    /// has none. A crash, a limit or a timeout marks the file and is an `Err`.
    pub fn header_duration(&self, file: &File, path: &Path) -> Result<Option<u64>, AudioError> {
        let (proc, stamp) = self.start(file, path, Limits::probe())?;
        let mut session = RemoteDecoder {
            iso: self.clone(),
            proc: Some(proc),
            path: path.to_path_buf(),
            stamp,
            info: None,
            status: Status::default(),
            failed: None,
            index_stride: None,
        };
        match session.call(&Request::HeaderDuration, self.inner.budgets.open)? {
            (Response::Duration(ms), _) => Ok(ms),
            _ => Err(session.protocol_failure("unexpected reply to a duration probe")),
        }
    }
}

/// Why a helper stopped answering.
#[derive(Debug)]
pub enum Failure {
    Died { signal: Option<i32>, code: Option<i32> },
    TimedOut(Duration),
    Protocol(String),
}

/// One helper process.
struct HelperProc {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
}

impl HelperProc {
    fn spawn(program: &Path, file: &File, limits: Limits) -> io::Result<Self> {
        let media_fd = file.as_raw_fd();
        let mut cmd = Command::new(program);
        cmd.arg("--as")
            .arg(limits.address_space_bytes.to_string())
            .arg("--cpu")
            .arg(limits.cpu_seconds.to_string())
            .arg("--nofile")
            .arg(limits.max_open_files.to_string())
            .env_clear()
            // One malloc arena: per-thread arenas reserve 64 MiB of address space each.
            .env("MALLOC_ARENA_MAX", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: the closure only calls async-signal-safe functions (dup2, fcntl, setrlimit,
        // prctl) and does not allocate.
        unsafe {
            cmd.pre_exec(move || {
                if media_fd == MEDIA_FD {
                    let flags = libc::fcntl(MEDIA_FD, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(MEDIA_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(media_fd, MEDIA_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                limits::apply(&limits)
            });
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout pipe"))?;
        Ok(Self { child, stdin, stdout })
    }

    fn send(&mut self, req: &Request) -> Result<(), Failure> {
        let body = proto::encode(req).map_err(|e| Failure::Protocol(e.to_string()))?;
        let stdin = self.stdin.as_mut().ok_or_else(|| Failure::Protocol("stdin closed".into()))?;
        proto::write_frame(stdin, &body, &[]).map_err(|e| {
            if e.kind() == io::ErrorKind::BrokenPipe {
                self.exit_failure()
            } else {
                Failure::Protocol(e.to_string())
            }
        })
    }

    /// Read exactly `buf.len()` bytes; the helper may be silent for at most `stall` between
    /// two reads, or it is a [`Failure::TimedOut`].
    fn read_stalling(&mut self, buf: &mut [u8], stall: Duration) -> Result<(), Failure> {
        let fd: RawFd = self.stdout.as_raw_fd();
        let mut got = 0;
        while got < buf.len() {
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let ms = stall.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: poll on one valid pollfd.
            let n = unsafe { libc::poll(&mut p, 1, ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Failure::Protocol(e.to_string()));
            }
            if n == 0 {
                return Err(Failure::TimedOut(stall));
            }
            match self.stdout.read(&mut buf[got..]) {
                Ok(0) => return Err(self.exit_failure()),
                Ok(k) => got += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Failure::Protocol(e.to_string())),
            }
        }
        Ok(())
    }

    /// One reply and its raw tail.
    fn recv(&mut self, stall: Duration) -> Result<(Response, Vec<u8>), Failure> {
        let mut len = [0u8; 4];
        self.read_stalling(&mut len, stall)?;
        let len = proto::check_len(u32::from_le_bytes(len)).map_err(|e| Failure::Protocol(e.to_string()))?;
        let mut payload = vec![0u8; len];
        self.read_stalling(&mut payload, stall)?;
        let (resp, tail_at): (Response, usize) = {
            let (body, tail) =
                proto::split_frame(&payload).map_err(|e| Failure::Protocol(e.to_string()))?;
            let resp = proto::decode(body).map_err(|e| Failure::Protocol(e.to_string()))?;
            (resp, payload.len() - tail.len())
        };
        // What is left is the tail: a move within the buffer, no new allocation.
        payload.drain(..tail_at);
        Ok((resp, payload))
    }

    /// The pipe closed: find out how the helper ended.
    fn exit_failure(&mut self) -> Failure {
        // A helper that closed its pipe is exiting; one that closed it and lives on is not
        // playing by the rules and is killed after a short grace.
        let grace = Instant::now() + Duration::from_millis(200);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= grace => {
                    let _ = self.child.kill();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        match self.child.wait() {
            Ok(st) => Failure::Died { signal: st.signal(), code: st.code() },
            Err(e) => Failure::Protocol(format!("wait failed: {e}")),
        }
    }

    fn describe_failure(&self, f: &Failure) -> String {
        match f {
            Failure::Died { signal: Some(s), .. } => format!("killed by signal {s}{}", signal_name(*s)),
            Failure::Died { code: Some(c), .. } => format!("exited with status {c}"),
            Failure::Died { .. } => "exited".to_string(),
            Failure::TimedOut(d) => format!("no answer for {} s", d.as_secs()),
            Failure::Protocol(why) => format!("protocol violation ({why})"),
        }
    }

    /// Kill and reap. Always leaves no zombie behind.
    fn kill_and_reap(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for HelperProc {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn signal_name(s: i32) -> &'static str {
    match s {
        libc::SIGABRT => " (SIGABRT)",
        libc::SIGSEGV => " (SIGSEGV)",
        libc::SIGKILL => " (SIGKILL)",
        libc::SIGXCPU => " (SIGXCPU: CPU limit)",
        libc::SIGBUS => " (SIGBUS)",
        libc::SIGILL => " (SIGILL)",
        _ => "",
    }
}

/// A decoder that lives in a helper process. Mirrors the parts of
/// [`StreamDecoder`](crate::StreamDecoder) the engine and the server use; every call can fail
/// with [`AudioError::Unplayable`] if the helper died on this file (which also marks it).
pub struct RemoteDecoder {
    iso: Isolation,
    proc: Option<HelperProc>,
    path: PathBuf,
    stamp: FileStamp,
    info: Option<StreamInfo>,
    status: Status,
    /// Set once the helper failed on this file: every later call repeats the verdict.
    failed: Option<String>,
    /// Stride of the index build asked for, if any.
    index_stride: Option<u32>,
}

impl std::fmt::Debug for RemoteDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteDecoder").field("path", &self.path).finish()
    }
}

impl RemoteDecoder {
    fn unplayable(&self, reason: &str) -> AudioError {
        AudioError::Unplayable { path: self.path.clone(), reason: reason.to_string() }
    }

    /// The helper failed while handling this file: kill it, reap it, remember the file.
    fn file_failure(&mut self, f: Failure) -> AudioError {
        let reason = match &self.proc {
            Some(p) => p.describe_failure(&f),
            None => "the media decoder is gone".to_string(),
        };
        if let Some(mut p) = self.proc.take() {
            p.kill_and_reap();
        }
        let reason = format!("the media decoder {reason}");
        if let Err(e) = self.iso.marks().mark(&self.path, self.stamp, &reason) {
            eprintln!("plippa-audio: could not persist the crash mark for {}: {e}", self.path.display());
        }
        self.failed = Some(reason.clone());
        self.unplayable(&reason)
    }

    fn protocol_failure(&mut self, why: &str) -> AudioError {
        self.file_failure(Failure::Protocol(why.to_string()))
    }

    fn call(&mut self, req: &Request, stall: Duration) -> Result<(Response, Vec<u8>), AudioError> {
        if let Some(reason) = &self.failed {
            return Err(self.unplayable(reason));
        }
        let Some(proc) = self.proc.as_mut() else {
            return Err(self.unplayable("the media decoder is gone"));
        };
        let result = proc.send(req).and_then(|()| proc.recv(stall));
        match result {
            Ok(reply) => Ok(reply),
            Err(f) => Err(self.file_failure(f)),
        }
    }

    fn absorb(&mut self, status: Option<Status>) {
        if let Some(status) = status {
            self.status = status;
        }
    }

    fn info(&self) -> &StreamInfo {
        self.info.as_ref().expect("an opened decoder has its info")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn extension(&self) -> &str {
        &self.info().extension
    }

    pub fn channels(&self) -> u16 {
        self.info().channels
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.info().sample_rate_hz
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.info().duration_ms
    }

    pub fn duration_frames(&self) -> Option<u64> {
        self.info().duration_frames
    }

    pub fn duration_known(&self) -> bool {
        self.info().duration_known
    }

    pub fn position_frame(&self) -> u64 {
        self.status.position_frame
    }

    pub fn start_frame(&self) -> u64 {
        self.status.start_frame
    }

    pub fn skipped_packets(&self) -> u64 {
        self.status.skipped_packets
    }

    pub fn last_decode_error(&self) -> Option<String> {
        self.status.last_decode_error.clone()
    }

    pub fn is_eof(&self) -> bool {
        self.status.eof
    }

    pub fn has_index(&self) -> bool {
        self.status.has_index
    }

    pub fn indexed_seeks(&self) -> u64 {
        self.status.indexed_seeks
    }

    pub fn index_entries(&self) -> u64 {
        self.status.index_entries
    }

    /// Up to `max_frames` interleaved frames; `Ok(None)` at the end of the stream.
    pub fn next_chunk(&mut self, max_frames: usize) -> Result<Option<PcmBuffer>, AudioError> {
        let ask = max_frames.clamp(1, MAX_CHUNK_FRAMES as usize) as u32;
        let stall = self.iso.inner.budgets.chunk;
        match self.call(&Request::Next { max_frames: ask }, stall)? {
            (Response::Chunk { status, frames, channels, sample_rate_hz }, tail) => {
                let samples = proto::bytes_to_samples(&tail);
                let sane = samples.as_ref().is_some_and(|s| {
                    channels > 0 && frames > 0 && s.len() == frames as usize * channels as usize
                });
                match samples {
                    Some(samples) if sane => {
                        self.status = status;
                        Ok(Some(PcmBuffer { samples, channels, sample_rate_hz }))
                    }
                    _ => Err(self.protocol_failure("malformed chunk")),
                }
            }
            (Response::End { status }, _) => {
                self.status = status;
                Ok(None)
            }
            (Response::Failed { error, status }, _) => {
                self.absorb(status);
                Err(error.into_audio(self.path.clone()))
            }
            _ => Err(self.protocol_failure("unexpected reply to next")),
        }
    }

    /// Seek so the next frame is `frame`; the landing is [`position_frame`](Self::position_frame).
    pub fn seek_frame(&mut self, frame: u64) -> Result<(), AudioError> {
        let stall = self.iso.inner.budgets.seek;
        match self.call(&Request::SeekFrame { frame }, stall)? {
            (Response::Seeked { status }, _) => {
                self.status = status;
                Ok(())
            }
            (Response::Failed { error, status }, _) => {
                self.absorb(status);
                Err(error.into_audio(self.path.clone()))
            }
            _ => Err(self.protocol_failure("unexpected reply to seek")),
        }
    }

    pub fn seek_ms(&mut self, position_ms: u64) -> Result<(), AudioError> {
        self.seek_frame(ms_to_frame(position_ms, self.sample_rate_hz()))
    }

    /// Ask the helper to build the MP3 seek index in the background (WARP_11); the helper
    /// installs it when the walk finishes and reports it through [`has_index`](Self::has_index).
    /// Asking again is a cheap status refresh.
    pub fn start_index_build(&mut self, stride_frames: u32) -> Result<(), AudioError> {
        let stall = self.iso.inner.budgets.open;
        match self.call(&Request::BuildIndex { stride_frames }, stall)? {
            (Response::IndexStatus(status), _) => {
                self.index_stride = Some(stride_frames);
                self.status = status;
                Ok(())
            }
            (Response::Failed { error, .. }, _) => Err(error.into_audio(self.path.clone())),
            _ => Err(self.protocol_failure("unexpected reply to index build")),
        }
    }

    /// Whether no index build is outstanding: none was asked for, or the walk has finished.
    /// While one is running this refreshes the status with one round trip.
    pub fn poll_index(&mut self) -> Result<bool, AudioError> {
        match self.index_stride {
            None => Ok(true),
            Some(_) if self.status.index_done => Ok(true),
            Some(stride) => {
                self.start_index_build(stride)?;
                Ok(self.status.index_done)
            }
        }
    }

    /// Build the index and wait (at most `max_wait`) for the walk to finish: a neighbour
    /// entered backward needs the index before its end seek.
    pub fn build_index_blocking(&mut self, stride_frames: u32, max_wait: Duration) -> Result<(), AudioError> {
        let until = Instant::now() + max_wait;
        self.start_index_build(stride_frames)?;
        while !self.status.index_done && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            self.start_index_build(stride_frames)?;
        }
        Ok(())
    }

    /// Tell the helper to leave (it is killed and reaped on drop anyway).
    pub fn close(mut self) {
        if let Some(p) = self.proc.as_mut() {
            let _ = p.send(&Request::Shutdown);
        }
    }

    /// Process id of the helper while it is alive (tests).
    pub fn helper_pid(&self) -> Option<u32> {
        self.proc.as_ref().map(|p| p.child.id())
    }

    /// Why this session ended as unplayable, if it did.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }
}

impl BlockSource for RemoteDecoder {
    fn channels(&self) -> usize {
        RemoteDecoder::channels(self) as usize
    }
    fn sample_rate_hz(&self) -> u32 {
        RemoteDecoder::sample_rate_hz(self)
    }
    fn position_frame(&self) -> u64 {
        RemoteDecoder::position_frame(self)
    }
    fn seek_frame(&mut self, frame: u64) -> Result<(), AudioError> {
        RemoteDecoder::seek_frame(self, frame)
    }
    fn next_chunk(&mut self, max_frames: usize) -> Result<Option<PcmBuffer>, AudioError> {
        RemoteDecoder::next_chunk(self, max_frames)
    }
    fn install_index(&mut self, _index: Arc<Mp3Index>) {}
    fn poll_index(&mut self) -> bool {
        // A failed round trip already marked the file and ended the session: nothing pending.
        RemoteDecoder::poll_index(self).unwrap_or(true)
    }
    fn index_entries(&self) -> u64 {
        RemoteDecoder::index_entries(self)
    }
    fn has_index(&self) -> bool {
        RemoteDecoder::has_index(self)
    }
    fn indexed_seeks(&self) -> u64 {
        RemoteDecoder::indexed_seeks(self)
    }
    fn skipped_packets(&self) -> u64 {
        RemoteDecoder::skipped_packets(self)
    }
}
