//! SECFIX_02 (SRV-01): who may use the LAN web portal.
//!
//! The device owns one small piece of state: whether the portal is **on**, whether it asks for a
//! **PIN**, the six-digit PIN itself, and the browser sessions that have presented it. All of it
//! lives in a file in the secrets directory (`0700` directory, `0600` file, atomic replace, a
//! symlinked directory or file refused), never on the stick. Factory default is "no file": the
//! portal is off and there is no PIN until the owner first enables it on the device.
//!
//! * **The PIN is stored, not hashed.** The owner chose to have the device display it, which needs
//!   the plain value; a hash of six digits would fall to an offline guess in microseconds and
//!   protect nothing. The protection is the `0600` file and the guess limits below. The PIN is
//!   compared in constant time and is never logged.
//! * **Guess limits** ([`LockoutPolicy`]): per client address, five consecutive failures lock that
//!   address for one minute, doubling per further failure up to 30 minutes; globally, 30 failures
//!   in ten minutes make sign-in answer "locked" to everyone for ten minutes (existing sessions are
//!   unaffected). A correct PIN during a lockout is refused too. The bound this gives an attacker
//!   who controls any number of addresses is at most 30 guesses per ten minutes (4,320 a day):
//!   under 0.5 % of the 10^6 PINs per day, about 230 days to be certain of covering them all.
//!   Counters live in memory only: a restart clears them, which only a person with physical
//!   access to the power can cause.
//! * **Sessions** are random 128-bit tokens, kept in the state file so they survive a restart,
//!   valid for 90 days from their last use, all revoked when the PIN changes **or the portal is
//!   switched off** (off means off: switching it on again needs the PIN again).
//! * **Saving never gets in the way of closing.** Switching the portal off, asking for the PIN
//!   again, changing the PIN and signing out take effect in memory first; if the state file
//!   cannot be written the caller is told ([`AccessError::NotSaved`]) and the write is retried
//!   (the next change, or [`PortalAccess::flush`] from the server's periodic retry). A failed
//!   write never makes the settings read-only. Changes in the other direction (switching on,
//!   dropping the PIN requirement) are refused unless they can be saved, so a restart can never
//!   open more than the owner asked for.
//! * **Clock jumps.** Expiry uses the wall clock (a restart must see it), recorded as absolute
//!   times. A clock that has gone back before a session was issued makes that session not valid
//!   until it comes forward again (nothing extends it meanwhile); a forward jump past the expiry
//!   expires it, which after 90 days is correct and otherwise needs the owner to sign in again.
//!
//! Time comes from a [`Clock`] so the lockout and session expiry are tested without waiting.

use crate::admission::client_key;
use crate::service::stores::StoreSecrets;
use plippa_store::FILE_MODE;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Digits in the PIN.
pub const PIN_LEN: usize = 6;
/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "plippa_session";
/// A session lives this long after its last use (90 days).
pub const SESSION_LIFETIME_SECS: u64 = 90 * 24 * 60 * 60;
/// A session is only re-stamped (and the state file rewritten) once a day.
const SESSION_REFRESH_AFTER_SECS: u64 = 24 * 60 * 60;
/// Sessions kept; the oldest is dropped past this (a few phones and laptops).
const MAX_SESSIONS: usize = 32;
/// The state file's name inside the secrets directory.
const STATE_FILE: &str = "portal-access.state";
/// A state file larger than this is not ours.
const MAX_STATE_BYTES: u64 = 64 * 1024;
/// Most client addresses remembered for lockout accounting.
const MAX_CLIENTS: usize = 4096;

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Where the clock comes from. Production uses [`SystemClock`]; tests use [`ManualClock`].
pub trait Clock: Send + Sync {
    /// Milliseconds on a monotonic scale (lockouts).
    fn mono_ms(&self) -> u64;
    /// Seconds since the Unix epoch (session expiry, which must survive a restart).
    fn unix_secs(&self) -> u64;
}

/// The real clock.
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn mono_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn unix_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// A clock that only moves when told to (tests).
pub struct ManualClock {
    mono_ms: AtomicU64,
    unix_secs: AtomicU64,
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            mono_ms: AtomicU64::new(1_000),
            unix_secs: AtomicU64::new(1_800_000_000),
        }
    }

    pub fn advance(&self, by: Duration) {
        self.mono_ms
            .fetch_add(u64::try_from(by.as_millis()).unwrap_or(u64::MAX), Ordering::SeqCst);
        self.unix_secs.fetch_add(by.as_secs(), Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn mono_ms(&self) -> u64 {
        self.mono_ms.load(Ordering::SeqCst)
    }
    fn unix_secs(&self) -> u64 {
        self.unix_secs.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// The guess limits. `Default` is the production set the owner accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockoutPolicy {
    /// Consecutive failures from one address before it is locked.
    pub free_failures: u32,
    /// First lock, in milliseconds; doubles with every further failure.
    pub base_lock_ms: u64,
    /// Longest per-address lock.
    pub max_lock_ms: u64,
    /// Failures from anywhere, within `global_window_ms`, that lock sign-in for everyone.
    pub global_failures: usize,
    pub global_window_ms: u64,
    pub global_lock_ms: u64,
    /// An address with no failure for this long starts again from zero.
    pub forget_after_ms: u64,
}

impl Default for LockoutPolicy {
    fn default() -> Self {
        Self {
            free_failures: 5,
            base_lock_ms: 60_000,
            max_lock_ms: 30 * 60_000,
            global_failures: 30,
            global_window_ms: 10 * 60_000,
            global_lock_ms: 10 * 60_000,
            forget_after_ms: 24 * 60 * 60_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSession {
    /// 32 lowercase hex digits.
    token: String,
    expires_unix: u64,
    /// When it was issued (0 for a state file written before this field existed).
    #[serde(default)]
    issued_unix: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PortalData {
    version: u32,
    enabled: bool,
    pin_required: bool,
    pin: Option<String>,
    sessions: Vec<StoredSession>,
    /// SECFIX_03: serve the portal over TLS with the device's own certificate. **Absent means
    /// on**: a state file written before this setting existed must come up on HTTPS, never on
    /// the weaker protocol it was not asked to use.
    #[serde(default = "yes")]
    https_enabled: bool,
}

/// `true`, for a setting whose absent value is the safe one.
fn yes() -> bool {
    true
}

impl std::fmt::Debug for PortalData {
    // Never print the PIN or a token (a stray `{:?}` must not leak them into a log).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalData")
            .field("enabled", &self.enabled)
            .field("pin_required", &self.pin_required)
            .field("https_enabled", &self.https_enabled)
            .field("pin", &self.pin.as_ref().map(|_| "<set>"))
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl Default for PortalData {
    /// Factory default: off, asks for a PIN once it is on, no PIN yet, nobody signed in.
    fn default() -> Self {
        Self {
            version: 1,
            enabled: false,
            pin_required: true,
            pin: None,
            sessions: Vec::new(),
            https_enabled: true,
        }
    }
}

/// Where the state is kept. Private: the file store and the in-memory one are the only two.
trait PortalStore: Send {
    /// `Ok(None)` is the factory default (nothing stored).
    fn load(&mut self) -> Result<Option<PortalData>, String>;
    fn save(&mut self, data: &PortalData) -> Result<(), String>;
    /// Can state be kept at all? A reason when it cannot.
    fn usable(&mut self) -> Result<(), String>;
}

struct MemoryStore {
    data: Option<PortalData>,
}

impl PortalStore for MemoryStore {
    fn load(&mut self) -> Result<Option<PortalData>, String> {
        Ok(self.data.clone())
    }
    fn save(&mut self, data: &PortalData) -> Result<(), String> {
        self.data = Some(data.clone());
        Ok(())
    }
    fn usable(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// The state file in the secrets directory.
struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    fn path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    fn temp_path(&self) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.dir
            .join(format!(".{STATE_FILE}.{}.{n}.tmp", std::process::id()))
    }
}

impl PortalStore for FileStore {
    fn usable(&mut self) -> Result<(), String> {
        // Creates the directory 0700, refuses a symlink, tightens a wider mode, proves it is
        // writable: the same checks the store credentials get.
        StoreSecrets::new(&self.dir).probe()
    }

    fn load(&mut self) -> Result<Option<PortalData>, String> {
        let path = self.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot inspect the portal state file: {e}")),
        };
        if meta.file_type().is_symlink() {
            return Err("the portal state file is a symbolic link; refusing to follow it".into());
        }
        if !meta.is_file() {
            return Err("the portal state path is not a regular file".into());
        }
        if meta.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE)).map_err(|_| {
                "the portal state file is readable by others and cannot be restricted".to_string()
            })?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("cannot read the portal state file: {e}"))?;
        let mut bytes = Vec::new();
        Read::take(&mut file, MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot read the portal state file: {e}"))?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err("the portal state file is far larger than it should be".into());
        }
        match serde_json::from_slice::<PortalData>(&bytes) {
            Ok(data) if data.version == 1 => Ok(Some(data)),
            // An unreadable file is the factory default (portal off, no PIN): failing closed. The
            // next enable replaces it atomically.
            _ => {
                eprintln!("plippa-server: the portal state file is not valid; starting from the factory default");
                Ok(None)
            }
        }
    }

    fn save(&mut self, data: &PortalData) -> Result<(), String> {
        self.usable()?;
        let json = serde_json::to_vec(data).map_err(|e| format!("cannot encode the portal state: {e}"))?;
        let tmp = self.temp_path();
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
            fs::rename(&tmp, self.path())?;
            // The rename itself must survive a power cut.
            fs::File::open(&self.dir)?.sync_all()
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(format!("cannot save the portal state: {e}"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The access object
// ---------------------------------------------------------------------------

/// Why a settings change was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessError {
    /// State cannot be kept (no directory configured, symlink, read-only ...): the portal stays
    /// off. The text is safe to show.
    Unavailable(String),
    /// The PIN is not exactly six digits.
    BadPin,
    /// The change took effect in memory but could not be written to the state file; it is
    /// retried. (Only for changes that tighten access, which are never refused for this.)
    NotSaved(String),
}

impl AccessError {
    /// Stable code for the JSON answer.
    pub fn code(&self) -> &'static str {
        match self {
            AccessError::Unavailable(_) => "unavailable",
            AccessError::BadPin => "bad_pin",
            AccessError::NotSaved(_) => "not_saved",
        }
    }

    pub fn message(&self) -> String {
        match self {
            AccessError::Unavailable(reason) => reason.clone(),
            AccessError::BadPin => format!("the PIN must be exactly {PIN_LEN} digits"),
            AccessError::NotSaved(reason) => format!(
                "done, but not saved yet (it will be retried): {reason}"
            ),
        }
    }
}

/// The answer to a sign-in attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    /// Signed in; the token goes in the session cookie.
    Signed(String),
    /// Wrong PIN.
    BadPin,
    /// Locked out: try again after this many seconds. The PIN was not even looked at.
    Locked { retry_after_secs: u64 },
}

/// What the device menu shows and changes. Only ever sent over the control listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortalSettings {
    /// State can be kept (so the portal can be enabled).
    pub available: bool,
    /// Why not, when it cannot.
    pub unavailable_reason: Option<String>,
    pub enabled: bool,
    /// SECFIX_03: the portal serves HTTPS with its own certificate.
    pub https_enabled: bool,
    pub pin_required: bool,
    /// The six digits, once the portal has been enabled for the first time.
    pub pin: Option<String>,
    /// A sign-in lockout is in force somewhere.
    pub locked: bool,
    /// The LAN socket is open right now.
    pub listening: bool,
    /// Certificate fingerprint, when the portal serves HTTPS (SECFIX_03 supplies the value).
    pub fingerprint: Option<String>,
    /// The last change could not be written to the state file; it is being retried.
    pub unsaved: Option<String>,
    /// The portal is on but its socket could not be opened (filled by the server).
    pub listen_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ClientFails {
    fails: u32,
    locked_until: u64,
    last_fail: u64,
}

struct Inner {
    store: Box<dyn PortalStore>,
    data: PortalData,
    /// In-memory state is ahead of the file: the last save failed and is being retried.
    dirty: Option<String>,
    /// `Err` while state cannot be kept.
    usable: Result<(), String>,
    clients: HashMap<IpAddr, ClientFails>,
    global_fails: VecDeque<u64>,
    global_locked_until: u64,
    fingerprint: Option<String>,
}

/// Portal access state: the persisted settings, the sessions and the guess limits.
pub struct PortalAccess {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
    policy: LockoutPolicy,
}

impl PortalAccess {
    /// State in `dir` (the secrets directory). `None` means no state can be kept: the portal can
    /// never be enabled and the device menu says so.
    pub fn open(dir: Option<&Path>) -> Self {
        Self::open_with(dir, Arc::new(SystemClock::new()), LockoutPolicy::default())
    }

    /// [`PortalAccess::open`] with an injected clock and policy (tests).
    pub fn open_with(dir: Option<&Path>, clock: Arc<dyn Clock>, policy: LockoutPolicy) -> Self {
        match dir {
            Some(dir) => Self::from_store(Box::new(FileStore { dir: dir.to_path_buf() }), clock, policy),
            None => Self::unavailable(
                "no secrets directory is configured, so the portal PIN cannot be kept".into(),
                clock,
                policy,
            ),
        }
    }

    /// State held in memory only (tests that need no files).
    pub fn memory(clock: Arc<dyn Clock>, policy: LockoutPolicy) -> Self {
        Self::from_store(Box::new(MemoryStore { data: None }), clock, policy)
    }

    fn from_store(mut store: Box<dyn PortalStore>, clock: Arc<dyn Clock>, policy: LockoutPolicy) -> Self {
        let mut usable = store.usable();
        let mut data = PortalData::default();
        if usable.is_ok() {
            match store.load() {
                Ok(Some(loaded)) => data = loaded,
                Ok(None) => {}
                Err(reason) => usable = Err(reason),
            }
        }
        if usable.is_err() {
            // Fail closed: whatever is on disk is not trusted, nothing is served.
            data = PortalData::default();
        }
        Self {
            inner: Mutex::new(Inner {
                store,
                data,
                dirty: None,
                usable,
                clients: HashMap::new(),
                global_fails: VecDeque::new(),
                global_locked_until: 0,
                fingerprint: None,
            }),
            clock,
            policy,
        }
    }

    fn unavailable(reason: String, clock: Arc<dyn Clock>, policy: LockoutPolicy) -> Self {
        struct Nothing;
        impl PortalStore for Nothing {
            fn load(&mut self) -> Result<Option<PortalData>, String> {
                Ok(None)
            }
            fn save(&mut self, _: &PortalData) -> Result<(), String> {
                Err("no state directory".into())
            }
            fn usable(&mut self) -> Result<(), String> {
                Err("no state directory".into())
            }
        }
        Self {
            inner: Mutex::new(Inner {
                store: Box::new(Nothing),
                data: PortalData::default(),
                dirty: None,
                usable: Err(reason),
                clients: HashMap::new(),
                global_fails: VecDeque::new(),
                global_locked_until: 0,
                fingerprint: None,
            }),
            clock,
            policy,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Make `new` current and write it out.
    ///
    /// `tightens` is true for a change that can only close things (off, PIN required, sign-out,
    /// a PIN change): it takes effect even if the write fails (then [`AccessError::NotSaved`] and
    /// a retry). Any other change is refused unless it is saved, and then changes nothing.
    fn commit(inner: &mut Inner, new: PortalData, tightens: bool) -> Result<(), AccessError> {
        if let Err(reason) = &inner.usable {
            // Nothing can be kept here: only closing is allowed, and it is in memory only.
            if tightens {
                inner.data = new;
                return Ok(());
            }
            return Err(AccessError::Unavailable(reason.clone()));
        }
        match inner.store.save(&new) {
            Ok(()) => {
                inner.data = new;
                inner.dirty = None;
                Ok(())
            }
            Err(reason) if tightens => {
                inner.data = new;
                inner.dirty = Some(reason.clone());
                Err(AccessError::NotSaved(reason))
            }
            Err(reason) => Err(AccessError::Unavailable(reason)),
        }
    }

    /// Write out state a failed save left behind. `true` when the file matches memory now.
    pub fn flush(&self) -> bool {
        let mut inner = self.lock();
        Self::flush_locked(&mut inner)
    }

    fn flush_locked(inner: &mut Inner) -> bool {
        if inner.dirty.is_none() {
            return true;
        }
        let data = inner.data.clone();
        match inner.store.save(&data) {
            Ok(()) => {
                inner.dirty = None;
                true
            }
            Err(reason) => {
                inner.dirty = Some(reason);
                false
            }
        }
    }

    // ---- reading ------------------------------------------------------------------------

    /// The portal should be serving the LAN.
    pub fn enabled(&self) -> bool {
        self.lock().data.enabled
    }

    /// Callers must sign in.
    pub fn pin_required(&self) -> bool {
        self.lock().data.pin_required
    }

    /// Everything the device menu shows. `listening` is filled by the caller that knows.
    pub fn settings(&self, listening: bool) -> PortalSettings {
        let now = self.clock.mono_ms();
        let inner = self.lock();
        PortalSettings {
            available: inner.usable.is_ok(),
            unavailable_reason: inner.usable.clone().err(),
            enabled: inner.data.enabled,
            https_enabled: inner.data.https_enabled,
            pin_required: inner.data.pin_required,
            pin: inner.data.pin.clone(),
            locked: self.any_lock_active(&inner, now),
            listening,
            fingerprint: inner.fingerprint.clone(),
            unsaved: inner.dirty.clone(),
            listen_error: None,
        }
    }

    fn any_lock_active(&self, inner: &Inner, now: u64) -> bool {
        inner.global_locked_until > now || inner.clients.values().any(|c| c.locked_until > now)
    }

    /// SECFIX_03 supplies the certificate fingerprint it serves; the menu shows it when present.
    pub fn set_fingerprint(&self, fingerprint: Option<String>) {
        self.lock().fingerprint = fingerprint;
    }

    // ---- settings changes (control listener only) ------------------------------------------

    /// Turn the portal on or off. The first enable generates and keeps the random PIN; later
    /// enables reuse it. Switching off also signs every browser out.
    pub fn set_enabled(&self, on: bool) -> Result<(), AccessError> {
        let mut inner = self.lock();
        Self::flush_locked(&mut inner);
        let mut new = inner.data.clone();
        new.enabled = on;
        if on && new.pin.is_none() {
            new.pin = Some(generate_pin());
        }
        if !on {
            new.sessions.clear();
        }
        if new == inner.data {
            return Ok(());
        }
        Self::commit(&mut inner, new, !on)
    }

    /// SECFIX_03: serve HTTPS (`true`, the default) or plain HTTP (`false`).
    ///
    /// Both directions must be written to the state file before they happen: switching *on* is
    /// tightening, switching *off* is a downgrade the owner asked for explicitly and still must
    /// survive a restart (a setting that silently reverts to "on" after a power cut would leave
    /// the owner with a portal they believe is encrypted-or-not in the other state). The caller
    /// re-binds the listener afterwards, which drops every live connection.
    ///
    /// **Every session is revoked.** The cookie's `Secure` meaning changes with the protocol, and
    /// while HTTPS is off a token can cross the network in the clear — that is what the warning
    /// says. Without this, a token a LAN listener recorded during the clear-text window would
    /// still be accepted after the owner switches HTTPS back on, for up to the 90-day session
    /// lifetime, and `Regenerate certificate` would not end it either.
    pub fn set_https_enabled(&self, on: bool) -> Result<(), AccessError> {
        let mut inner = self.lock();
        Self::flush_locked(&mut inner);
        let mut new = inner.data.clone();
        new.https_enabled = on;
        if new == inner.data {
            return Ok(());
        }
        new.sessions.clear();
        // `false` in both directions: the flip is either a saved change or no change at all. The
        // revocations above are only meaningful once the new protocol is the one being served.
        Self::commit(&mut inner, new, false)
    }

    /// SECFIX_03: is the portal served over TLS? (Absent in an old state file: yes.)
    pub fn https_enabled(&self) -> bool {
        self.lock().data.https_enabled
    }

    /// Ask for the PIN (`true`, the safe state) or serve with no sign-in (`false`).
    pub fn set_pin_required(&self, required: bool) -> Result<(), AccessError> {
        let mut inner = self.lock();
        Self::flush_locked(&mut inner);
        let mut new = inner.data.clone();
        new.pin_required = required;
        if new == inner.data {
            return Ok(());
        }
        Self::commit(&mut inner, new, required)
    }

    /// The owner typed a new PIN: exactly six digits, nothing else is checked. Every session is
    /// revoked.
    pub fn set_pin(&self, pin: &str) -> Result<(), AccessError> {
        validate_pin(pin)?;
        let mut inner = self.lock();
        Self::flush_locked(&mut inner);
        let mut new = inner.data.clone();
        new.pin = Some(pin.to_string());
        new.sessions.clear();
        if inner.usable.is_err() {
            let reason = inner.usable.clone().err().unwrap_or_default();
            return Err(AccessError::Unavailable(reason));
        }
        // Signing everybody out is the tightening part; the new PIN itself must be saved or the
        // old one would come back after a restart, so a failed write is reported either way.
        Self::commit(&mut inner, new, true)
    }

    /// Forget every lockout (the owner is at the device).
    pub fn clear_lockout(&self) {
        let mut inner = self.lock();
        inner.clients.clear();
        inner.global_fails.clear();
        inner.global_locked_until = 0;
    }

    // ---- sign-in -----------------------------------------------------------------------------

    /// Check `pin` for `client`. A locked client or a locked portal is refused without the PIN
    /// being compared (a correct guess during a lockout gains nothing).
    pub fn login(&self, client: IpAddr, pin: &str) -> LoginOutcome {
        let now = self.clock.mono_ms();
        let key = client_key(client);
        let mut inner = self.lock();

        if inner.global_locked_until > now {
            return LoginOutcome::Locked {
                retry_after_secs: ceil_secs(inner.global_locked_until - now),
            };
        }
        if let Some(c) = inner.clients.get(&key) {
            if c.locked_until > now {
                return LoginOutcome::Locked {
                    retry_after_secs: ceil_secs(c.locked_until - now),
                };
            }
        }

        // Compare against the stored PIN. With none stored, compare against a value no input
        // can equal, so the work and the outcome are the same shape.
        let stored = inner.data.pin.clone();
        let good = match stored.as_deref() {
            Some(real) => constant_time_eq(real.as_bytes(), pin.as_bytes()),
            None => {
                let _ = constant_time_eq(b"000000", pin.as_bytes());
                false
            }
        };

        if good {
            inner.clients.remove(&key);
            let token = format!("{:032x}", rand::random::<u128>());
            let unix = self.clock.unix_secs();
            let mut new = inner.data.clone();
            new.sessions.retain(|s| s.expires_unix > unix);
            new.sessions.push(StoredSession {
                token: token.clone(),
                expires_unix: unix.saturating_add(SESSION_LIFETIME_SECS),
                issued_unix: unix,
            });
            while new.sessions.len() > MAX_SESSIONS {
                new.sessions.remove(0);
            }
            // If the state cannot be written the sign-in still works for this run (the session
            // is in memory and the write is retried); it may not survive a restart.
            if Self::commit(&mut inner, new.clone(), true).is_err() {
                inner.data = new;
            }
            return LoginOutcome::Signed(token);
        }

        self.record_failure(&mut inner, key, now);
        LoginOutcome::BadPin
    }

    fn record_failure(&self, inner: &mut Inner, key: IpAddr, now: u64) {
        let p = self.policy;
        // Per client address.
        if inner.clients.len() >= MAX_CLIENTS && !inner.clients.contains_key(&key) {
            inner
                .clients
                .retain(|_, c| c.locked_until > now || now.saturating_sub(c.last_fail) < p.forget_after_ms);
            if inner.clients.len() >= MAX_CLIENTS {
                // Still full of live records: drop the stalest rather than grow without bound.
                if let Some(oldest) = inner
                    .clients
                    .iter()
                    .filter(|(_, c)| c.locked_until <= now)
                    .min_by_key(|(_, c)| c.last_fail)
                    .map(|(k, _)| *k)
                {
                    inner.clients.remove(&oldest);
                }
            }
        }
        let entry = inner.clients.entry(key).or_default();
        if entry.fails > 0 && now.saturating_sub(entry.last_fail) >= p.forget_after_ms {
            *entry = ClientFails::default();
        }
        entry.fails = entry.fails.saturating_add(1);
        entry.last_fail = now;
        if entry.fails >= p.free_failures {
            let doublings = (entry.fails - p.free_failures).min(20);
            let lock = p.base_lock_ms.saturating_mul(1u64 << doublings).min(p.max_lock_ms);
            entry.locked_until = now.saturating_add(lock);
        }

        // Everyone together.
        inner.global_fails.push_back(now);
        while inner
            .global_fails
            .front()
            .is_some_and(|t| now.saturating_sub(*t) > p.global_window_ms)
        {
            inner.global_fails.pop_front();
        }
        if inner.global_fails.len() >= p.global_failures {
            inner.global_locked_until = now.saturating_add(p.global_lock_ms);
            inner.global_fails.clear();
        }
    }

    // ---- sessions ----------------------------------------------------------------------------

    /// Is `token` a live session?
    pub fn session_valid(&self, token: &str) -> bool {
        self.session_check(token).is_some()
    }

    /// `None` when `token` is not a live session; `Some(slid)` when it is, where `slid` says the
    /// server just extended it (at most once a day), so the browser's cookie should be re-issued.
    pub fn session_check(&self, token: &str) -> Option<bool> {
        if token.len() != 32 {
            return None;
        }
        let unix = self.clock.unix_secs();
        let mut inner = self.lock();
        let mut found: Option<usize> = None;
        // Every stored token is compared, so a hit does not answer faster than a miss.
        for (i, s) in inner.data.sessions.iter().enumerate() {
            // A clock that went back before the session was issued does not honour it.
            if constant_time_eq(s.token.as_bytes(), token.as_bytes())
                && s.expires_unix > unix
                && unix >= s.issued_unix
            {
                found = Some(i);
            }
        }
        let i = found?;
        let new_expiry = unix.saturating_add(SESSION_LIFETIME_SECS);
        if new_expiry.saturating_sub(inner.data.sessions[i].expires_unix) >= SESSION_REFRESH_AFTER_SECS {
            let mut new = inner.data.clone();
            new.sessions[i].expires_unix = new_expiry;
            if Self::commit(&mut inner, new.clone(), true).is_err() {
                inner.data = new;
            }
            return Some(true);
        }
        Some(false)
    }

    /// End one session (sign out). Durable or loud: a failed write is reported
    /// ([`AccessError::NotSaved`]) and retried, never silently dropped.
    pub fn sign_out(&self, token: &str) -> Result<(), AccessError> {
        let mut inner = self.lock();
        let mut new = inner.data.clone();
        new.sessions
            .retain(|s| !constant_time_eq(s.token.as_bytes(), token.as_bytes()));
        if new == inner.data {
            return Ok(());
        }
        Self::commit(&mut inner, new, true)
    }

    /// Live sessions (tests, diagnostics).
    pub fn session_count(&self) -> usize {
        let unix = self.clock.unix_secs();
        self.lock()
            .data
            .sessions
            .iter()
            .filter(|s| s.expires_unix > unix)
            .count()
    }
}

fn ceil_secs(ms: u64) -> u64 {
    ms.div_ceil(1000).max(1)
}

/// Six uniformly random digits, zero padded.
fn generate_pin() -> String {
    format!("{:0width$}", rand::thread_rng().gen_range(0..1_000_000u32), width = PIN_LEN)
}

/// Exactly six ASCII digits. Nothing else is judged: the owner chose to type their own PIN, and
/// the guess limits (not the PIN's shape) are what protect it.
fn validate_pin(pin: &str) -> Result<(), AccessError> {
    if pin.len() == PIN_LEN && pin.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(AccessError::BadPin)
    }
}

/// Compare two byte strings without stopping at the first difference. A length difference is
/// folded in rather than returned early, so the time depends on the longer input only.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u64;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= u64::from(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

/// The value of the session cookie in a `Cookie` request header.
pub fn session_token_from_cookie(header: &str) -> Option<&str> {
    header.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE).then_some(value.trim())
    })
}

/// `Set-Cookie` value for a fresh session. `secure` is true when the answer goes over TLS.
pub fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={SESSION_LIFETIME_SECS}; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

/// `Set-Cookie` value that removes the session cookie.
pub fn expired_session_cookie(secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

// ---------------------------------------------------------------------------
// The status cache
// ---------------------------------------------------------------------------

/// One value reused for a short time, so rapid polling does not take the service lock each time.
/// Computing happens under the cache's own lock, so concurrent callers share one computation.
pub struct TimedCache<T: Clone> {
    ttl_ms: u64,
    slot: Mutex<Option<(u64, T)>>,
}

impl<T: Clone> TimedCache<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
            slot: Mutex::new(None),
        }
    }

    /// The cached value if it is younger than the ttl at `now_ms`, else `compute()` stored.
    pub fn get_or_compute(&self, now_ms: u64, compute: impl FnOnce() -> T) -> T {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, value)) = slot.as_ref() {
            if now_ms.saturating_sub(*at) < self.ttl_ms {
                return value.clone();
            }
        }
        let value = compute();
        *slot = Some((now_ms, value.clone()));
        value
    }

    /// Drop the value (something changed).
    pub fn invalidate(&self) {
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, n))
    }

    fn mem() -> (PortalAccess, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        (
            PortalAccess::memory(clock.clone(), LockoutPolicy::default()),
            clock,
        )
    }

    fn enabled() -> (PortalAccess, Arc<ManualClock>, String) {
        let (a, c) = mem();
        a.set_enabled(true).unwrap();
        let pin = a.settings(false).pin.unwrap();
        (a, c, pin)
    }

    fn wrong(pin: &str) -> String {
        // A six-digit string that is not `pin`.
        if pin == "000001" { "000002".into() } else { "000001".into() }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "plippa-portal-access-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn factory_default_is_off_with_no_pin() {
        let (a, _) = mem();
        let s = a.settings(false);
        assert!(!s.enabled && s.pin.is_none() && s.available);
        assert!(s.pin_required, "the safe state is the default");
        assert!(!a.enabled());
    }

    #[test]
    fn first_enable_generates_a_six_digit_pin_and_later_enables_reuse_it() {
        let (a, _) = mem();
        a.set_enabled(true).unwrap();
        let pin = a.settings(false).pin.expect("a PIN after the first enable");
        assert_eq!(pin.len(), 6);
        assert!(pin.bytes().all(|b| b.is_ascii_digit()), "{pin}");
        a.set_enabled(false).unwrap();
        assert_eq!(a.settings(false).pin.as_deref(), Some(pin.as_str()), "kept while off");
        a.set_enabled(true).unwrap();
        assert_eq!(a.settings(false).pin.as_deref(), Some(pin.as_str()));
    }

    #[test]
    fn generated_pins_use_the_whole_range_and_keep_leading_zeros() {
        for _ in 0..200 {
            let p = generate_pin();
            assert_eq!(p.len(), 6, "{p}");
        }
        assert_eq!(format!("{:06}", 42u32), "000042");
    }

    #[test]
    fn the_state_survives_a_restart_in_a_0600_file_in_a_0700_directory() {
        let dir = tmp_dir("persist");
        let clock = Arc::new(ManualClock::new());
        let a = PortalAccess::open_with(Some(&dir), clock.clone(), LockoutPolicy::default());
        a.set_enabled(true).unwrap();
        let pin = a.settings(false).pin.unwrap();
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else {
            panic!("the PIN signs in");
        };
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(STATE_FILE)), 0o600);

        let b = PortalAccess::open_with(Some(&dir), clock, LockoutPolicy::default());
        assert!(b.enabled());
        assert_eq!(b.settings(false).pin.as_deref(), Some(pin.as_str()));
        assert!(b.session_valid(&token), "a session survives a restart");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlinked_state_file_or_directory_is_refused_and_the_portal_cannot_be_enabled() {
        let dir = tmp_dir("symlink");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let target = dir.join("elsewhere");
        let planted: &[u8] =
            br#"{"version":1,"enabled":true,"pin_required":false,"pin":"135790","sessions":[]}"#;
        fs::write(&target, planted).unwrap();
        std::os::unix::fs::symlink(&target, dir.join(STATE_FILE)).unwrap();
        let a = PortalAccess::open(Some(&dir));
        let s = a.settings(false);
        assert!(!s.available, "a symlinked state file makes the state unavailable");
        assert!(!s.enabled, "and what it pointed at is not obeyed");
        assert!(matches!(a.set_enabled(true), Err(AccessError::Unavailable(_))));
        assert!(!a.enabled());
        assert_eq!(fs::read(&target).unwrap(), planted, "nothing was written through the link");

        let real = tmp_dir("symlink-real");
        fs::create_dir_all(&real).unwrap();
        let link = tmp_dir("symlink-dir");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let b = PortalAccess::open(Some(&link));
        assert!(!b.settings(false).available);
        assert!(b.set_enabled(true).is_err());
        assert!(!real.join(STATE_FILE).exists());
        for p in [&dir, &real] {
            let _ = fs::remove_dir_all(p);
        }
        let _ = fs::remove_file(&link);
    }

    #[test]
    fn a_wider_state_file_is_tightened_and_a_corrupt_one_is_the_factory_default() {
        let dir = tmp_dir("tighten");
        let a = PortalAccess::open(Some(&dir));
        a.set_enabled(true).unwrap();
        let file = dir.join(STATE_FILE);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let b = PortalAccess::open(Some(&dir));
        assert!(b.enabled());
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        fs::write(&file, b"{ not json").unwrap();
        let c = PortalAccess::open(Some(&dir));
        assert!(!c.enabled() && c.settings(false).pin.is_none() && c.settings(false).available);
        c.set_enabled(true).unwrap();
        assert!(c.settings(false).pin.is_some(), "enabling replaces the corrupt file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_no_state_directory_the_portal_can_never_be_enabled() {
        let a = PortalAccess::open(None);
        let s = a.settings(false);
        assert!(!s.available && s.unavailable_reason.is_some());
        assert!(matches!(a.set_enabled(true), Err(AccessError::Unavailable(_))));
        assert!(matches!(a.set_pin("135790"), Err(AccessError::Unavailable(_))));
    }

    #[test]
    fn the_right_pin_signs_in_and_a_wrong_one_does_not() {
        let (a, _, pin) = enabled();
        assert_eq!(a.login(ip(1), &wrong(&pin)), LoginOutcome::BadPin);
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else {
            panic!("signed in");
        };
        assert_eq!(token.len(), 32);
        assert!(a.session_valid(&token));
        assert!(!a.session_valid(&"0".repeat(32)));
        assert!(!a.session_valid("short"));
        // The comparison looks at the whole input: a prefix, an extension and an empty string
        // are all just wrong.
        for guess in [&pin[..5], &format!("{pin}0"), "", "      "] {
            assert_eq!(a.login(ip(2), guess), LoginOutcome::BadPin, "{guess:?}");
        }
        a.sign_out(&token);
        assert!(!a.session_valid(&token));
    }

    #[test]
    fn constant_time_eq_is_correct_for_every_length_combination() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"123456", b"123456"));
        assert!(!constant_time_eq(b"123456", b"123457"));
        assert!(!constant_time_eq(b"123456", b"12345"));
        assert!(!constant_time_eq(b"12345", b"123456"));
        assert!(!constant_time_eq(b"123456", b"123456\0"));
    }

    #[test]
    fn repeated_wrong_pins_lock_the_client_out_with_backoff() {
        let (a, _clock, pin) = enabled();
        let bad = wrong(&pin);
        for _ in 0..4 {
            assert_eq!(a.login(ip(9), &bad), LoginOutcome::BadPin);
        }
        // The fifth failure locks the address for one minute; the sixth attempt is refused.
        assert_eq!(a.login(ip(9), &bad), LoginOutcome::BadPin);
        assert_eq!(a.login(ip(9), &bad), LoginOutcome::Locked { retry_after_secs: 60 });
        // A correct PIN during the lockout is refused too.
        assert!(matches!(a.login(ip(9), &pin), LoginOutcome::Locked { .. }));
        // Another address is not affected.
        assert!(matches!(a.login(ip(10), &pin), LoginOutcome::Signed(_)));
        assert!(a.settings(false).locked);
        // After each lock one more failure doubles the next lock, up to the cap.
        let (a, clock, pin) = enabled();
        let bad = wrong(&pin);
        for _ in 0..5 {
            a.login(ip(9), &bad);
        }
        let mut lock = 60u64;
        for _ in 0..8 {
            assert_eq!(a.login(ip(9), &pin), LoginOutcome::Locked { retry_after_secs: lock });
            clock.advance(Duration::from_secs(lock));
            assert_eq!(a.login(ip(9), &bad), LoginOutcome::BadPin, "one try after the lock");
            lock = (lock * 2).min(30 * 60);
        }
        assert_eq!(lock, 1800, "capped at 30 minutes");
        assert_eq!(a.login(ip(9), &pin), LoginOutcome::Locked { retry_after_secs: 1800 });
        // A successful sign-in resets the count.
        clock.advance(Duration::from_secs(1800));
        assert!(matches!(a.login(ip(9), &pin), LoginOutcome::Signed(_)));
        for _ in 0..4 {
            assert_eq!(a.login(ip(9), &bad), LoginOutcome::BadPin);
        }
    }

    #[test]
    fn many_clients_cannot_brute_force_past_the_global_limit() {
        let (a, clock, pin) = enabled();
        let bad = wrong(&pin);
        // Thirty failures from thirty different addresses: no single address ever locks.
        for n in 0..30u8 {
            assert_eq!(a.login(ip(n + 1), &bad), LoginOutcome::BadPin);
        }
        // Now everybody is locked, with the correct PIN as well.
        for n in [1u8, 99, 200] {
            assert_eq!(
                a.login(ip(n), &pin),
                LoginOutcome::Locked { retry_after_secs: 600 },
                "address {n}"
            );
        }
        // Existing sessions are unaffected by the lock.
        // (None yet here; the HTTP test covers a live session.)
        clock.advance(Duration::from_secs(601));
        assert!(matches!(a.login(ip(77), &pin), LoginOutcome::Signed(_)));
        // Failures spread over more than the window do not add up.
        let (a, clock, pin) = enabled();
        let bad = wrong(&pin);
        for n in 0..29u8 {
            a.login(ip(n + 1), &bad);
        }
        clock.advance(Duration::from_secs(11 * 60));
        for n in 0..29u8 {
            assert_eq!(a.login(ip(n + 100), &bad), LoginOutcome::BadPin);
        }
        assert!(matches!(a.login(ip(250), &pin), LoginOutcome::Signed(_)));
    }

    #[test]
    fn clear_lockout_lets_the_owner_in_again() {
        let (a, _, pin) = enabled();
        let bad = wrong(&pin);
        for _ in 0..5 {
            a.login(ip(3), &bad);
        }
        assert!(a.settings(false).locked);
        a.clear_lockout();
        assert!(!a.settings(false).locked);
        assert!(matches!(a.login(ip(3), &pin), LoginOutcome::Signed(_)));
    }

    #[test]
    fn an_ipv6_client_is_limited_per_prefix_not_per_address() {
        let (a, _, pin) = enabled();
        let bad = wrong(&pin);
        for n in 0..5u16 {
            let addr: IpAddr = format!("2001:db8:1:2::{n:x}").parse().unwrap();
            a.login(addr, &bad);
        }
        let other_host_same_prefix: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        assert!(matches!(a.login(other_host_same_prefix, &pin), LoginOutcome::Locked { .. }));
    }

    #[test]
    fn sessions_last_ninety_days_from_last_use_and_a_pin_change_revokes_them() {
        let (a, clock, pin) = enabled();
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else { panic!() };
        clock.advance(Duration::from_secs(60 * 24 * 3600));
        assert!(a.session_valid(&token), "day 60: still valid, and extended by the use");
        clock.advance(Duration::from_secs(60 * 24 * 3600));
        assert!(a.session_valid(&token), "day 120 is within 90 days of the day-60 use");
        clock.advance(Duration::from_secs(91 * 24 * 3600));
        assert!(!a.session_valid(&token), "unused for 91 days");

        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else { panic!() };
        a.set_pin("135790").unwrap();
        assert!(!a.session_valid(&token), "changing the PIN signs everybody out");
        assert_eq!(a.session_count(), 0);
        assert!(matches!(a.login(ip(1), "135790"), LoginOutcome::Signed(_)));
        assert_eq!(a.login(ip(1), &pin), LoginOutcome::BadPin);
    }

    #[test]
    fn a_typed_pin_is_exactly_six_digits_and_nothing_else_is_judged() {
        let (a, _) = mem();
        for bad in ["12345", "1234567", "12345a", "", "１２３４５６", "12 456"] {
            assert_eq!(a.set_pin(bad), Err(AccessError::BadPin), "{bad:?}");
        }
        // The owner typed it: even the first things anyone would try are accepted.
        for any in ["000000", "111111", "123456", "654321", "121212", "135790", "048215"] {
            a.set_pin(any).unwrap();
            assert_eq!(a.settings(false).pin.as_deref(), Some(any), "{any}");
        }
    }

    /// A store whose writes fail while `fail` is set.
    struct FlakyStore {
        fail: Arc<std::sync::atomic::AtomicBool>,
        saved: Arc<Mutex<Option<PortalData>>>,
    }

    impl PortalStore for FlakyStore {
        fn load(&mut self) -> Result<Option<PortalData>, String> {
            Ok(self.saved.lock().unwrap().clone())
        }
        fn save(&mut self, data: &PortalData) -> Result<(), String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("disk full".into());
            }
            *self.saved.lock().unwrap() = Some(data.clone());
            Ok(())
        }
        fn usable(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    fn flaky() -> (
        PortalAccess,
        Arc<ManualClock>,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<Mutex<Option<PortalData>>>,
    ) {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saved = Arc::new(Mutex::new(None));
        let clock = Arc::new(ManualClock::new());
        let access = PortalAccess::from_store(
            Box::new(FlakyStore { fail: fail.clone(), saved: saved.clone() }),
            clock.clone(),
            LockoutPolicy::default(),
        );
        (access, clock, fail, saved)
    }

    #[test]
    fn one_failed_save_never_makes_the_settings_read_only() {
        let (a, _, fail, saved) = flaky();
        a.set_enabled(true).unwrap();
        // The write starts failing: off still works at once, in memory, and says it is unsaved.
        fail.store(true, Ordering::SeqCst);
        assert!(matches!(a.set_enabled(false), Err(AccessError::NotSaved(_))));
        assert!(!a.enabled(), "off means off, saved or not");
        assert!(a.settings(false).unsaved.is_some(), "and the menu can say so");
        assert!(saved.lock().unwrap().as_ref().unwrap().enabled, "the file still says on");
        // Turning ON needs the write to work: refused, nothing changes, nothing is poisoned.
        assert!(matches!(a.set_enabled(true), Err(AccessError::Unavailable(_))));
        assert!(!a.enabled());
        // The cause goes away: the next flush (or change) writes the state out and clears it.
        fail.store(false, Ordering::SeqCst);
        assert!(a.flush());
        assert!(a.settings(false).unsaved.is_none());
        assert!(!saved.lock().unwrap().as_ref().unwrap().enabled, "the file now says off");
        a.set_enabled(true).unwrap();
        assert!(a.enabled() && saved.lock().unwrap().as_ref().unwrap().enabled);
    }

    #[test]
    fn requiring_the_pin_again_and_a_pin_change_work_while_saving_fails() {
        let (a, _, fail, _) = flaky();
        a.set_enabled(true).unwrap();
        a.set_pin_required(false).unwrap();
        fail.store(true, Ordering::SeqCst);
        assert!(matches!(a.set_pin_required(true), Err(AccessError::NotSaved(_))));
        assert!(a.pin_required(), "back to the safe state at once");
        // Dropping the requirement is the loosening direction: refused while it cannot be saved.
        assert!(matches!(a.set_pin_required(false), Err(AccessError::Unavailable(_))));
        assert!(a.pin_required());
    }

    #[test]
    fn a_signed_out_session_cannot_come_back_after_a_failed_write_and_a_restart() {
        let (a, clock, fail, saved) = flaky();
        a.set_enabled(true).unwrap();
        let pin = a.settings(false).pin.unwrap();
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else { panic!() };
        fail.store(true, Ordering::SeqCst);
        // Sign-out is loud: the caller is told it was not saved, and it is dead in memory.
        assert!(matches!(a.sign_out(&token), Err(AccessError::NotSaved(_))));
        assert!(!a.session_valid(&token));
        // The write is retried once the disk works, before any restart could resurrect it.
        fail.store(false, Ordering::SeqCst);
        assert!(a.flush());
        let restarted = PortalAccess::from_store(
            Box::new(FlakyStore { fail: fail.clone(), saved: saved.clone() }),
            clock,
            LockoutPolicy::default(),
        );
        assert!(!restarted.session_valid(&token), "still signed out after a restart");
    }

    #[test]
    fn switching_off_signs_every_browser_out() {
        let (a, _, pin) = enabled();
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else { panic!() };
        a.set_enabled(false).unwrap();
        a.set_enabled(true).unwrap();
        assert!(!a.session_valid(&token), "off means off: signing in again is needed");
        assert!(matches!(a.login(ip(1), &pin), LoginOutcome::Signed(_)));
    }

    #[test]
    fn a_session_in_use_slides_once_a_day_and_says_so() {
        let (a, clock, pin) = enabled();
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else { panic!() };
        assert_eq!(a.session_check(&token), Some(false), "just issued: nothing to slide");
        clock.advance(Duration::from_secs(3600));
        assert_eq!(a.session_check(&token), Some(false), "inside the day");
        clock.advance(Duration::from_secs(24 * 3600));
        assert_eq!(a.session_check(&token), Some(true), "a day on: slid, cookie to be renewed");
        assert_eq!(a.session_check(&token), Some(false));
        assert_eq!(a.session_check("0".repeat(32).as_str()), None);
    }

    #[test]
    fn a_clock_that_went_back_before_issue_does_not_honour_the_session() {
        let clock = Arc::new(ManualClock::new());
        let a = PortalAccess::memory(clock.clone(), LockoutPolicy::default());
        a.set_enabled(true).unwrap();
        let pin = a.settings(false).pin.unwrap();
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else { panic!() };
        // Simulate a clock reset to an earlier time by issuing a session "in the future".
        {
            let mut inner = a.lock();
            let issued = clock.unix_secs() + 10 * 24 * 3600;
            inner.data.sessions[0].issued_unix = issued;
            inner.data.sessions[0].expires_unix = issued + SESSION_LIFETIME_SECS;
        }
        assert!(!a.session_valid(&token), "issued after 'now': the clock went backwards");
        clock.advance(Duration::from_secs(11 * 24 * 3600));
        assert!(a.session_valid(&token), "valid again once the clock is sane");
    }

    #[test]
    fn debug_output_never_contains_the_pin_or_a_token() {
        let (a, _, pin) = enabled();
        let LoginOutcome::Signed(token) = a.login(ip(1), &pin) else { panic!() };
        let text = format!("{:?}", a.lock().data);
        assert!(!text.contains(&pin) && !text.contains(&token), "{text}");
    }

    #[test]
    fn the_cookie_is_http_only_strict_and_secure_only_over_tls() {
        let plain = session_cookie("abc", false);
        assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Strict"));
        assert!(plain.contains("Path=/") && plain.contains("Max-Age=7776000"));
        assert!(!plain.contains("Secure"));
        assert!(session_cookie("abc", true).contains("; Secure"));
        assert!(expired_session_cookie(false).contains("Max-Age=0"));
        assert_eq!(
            session_token_from_cookie("a=b; plippa_session=tok; c=d"),
            Some("tok")
        );
        assert_eq!(session_token_from_cookie("plippa_session2=tok"), None);
        assert_eq!(session_token_from_cookie(""), None);
    }

    #[test]
    fn the_timed_cache_computes_once_per_ttl_and_can_be_invalidated() {
        let cache: TimedCache<u32> = TimedCache::new(Duration::from_secs(1));
        let mut calls = 0;
        let mut get = |now: u64, cache: &TimedCache<u32>| {
            cache.get_or_compute(now, || {
                calls += 1;
                calls
            })
        };
        assert_eq!(get(0, &cache), 1);
        assert_eq!(get(999, &cache), 1, "still fresh");
        assert_eq!(get(1000, &cache), 2, "a second later it is computed again");
        cache.invalidate();
        assert_eq!(get(1001, &cache), 3, "invalidation forces a new value");
    }
}
