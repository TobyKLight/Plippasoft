//! On-disk credential storage.
//!
//! One JSON file per store, mode `0600`, in a directory the store owns at mode
//! `0700`. The directory always comes from the caller (`--secrets-dir` /
//! `PLIPPA_SECRETS_DIR` in the server); this crate never picks a path and never
//! writes outside it.
//!
//! Credentials are protocol secrets: a Subsonic password plus a salt produces a
//! token that is equivalent to the password for the lifetime of that request, so
//! nothing here logs, `Debug`-prints or error-renders a password.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::types::StoreId;

/// Mode for the secrets directory.
pub const DIR_MODE: u32 = 0o700;
/// Mode for a credentials file.
pub const FILE_MODE: u32 = 0o600;
/// Hard cap on a credentials file; the real file is a few hundred bytes.
pub const MAX_CREDENTIALS_BYTES: u64 = 8 * 1024;

/// A secret held in memory and wiped when it is dropped.
///
/// The password (and the salt-bearing buffer the token is computed from) would
/// otherwise stay in freed memory for an attacker with a memory read: a crash dump or
/// a debugging session hands it back. There is no `zeroize` dependency here — the
/// wipe is a volatile write loop plus a compiler fence, which is what that crate does
/// for this case.
pub struct SecretString(String);

impl SecretString {
    /// Take ownership of `value`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The bytes, for the one place that must use them.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `password + suffix` as a secret: the Subsonic token input, also wiped on drop.
    pub fn concat(&self, suffix: &str) -> SecretString {
        let mut joined = String::with_capacity(self.0.len() + suffix.len());
        joined.push_str(&self.0);
        joined.push_str(suffix);
        SecretString(joined)
    }
}

/// Overwrite `bytes` and stop the compiler from eliding the writes as dead stores.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, exclusively borrowed `u8`.
        unsafe { std::ptr::write_volatile(byte, 0) };
        #[cfg(test)]
        WIPED_BYTES.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: a fence has no preconditions; it keeps the writes above from being
    // moved after the buffer is released.
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// Overwrite a buffer that held a secret before it is freed (PAR-07).
///
/// [`SecretString`] wipes itself; this is for the other places a password passes
/// through as raw bytes — a request body read off a socket, or the JSON a credentials
/// file is written from. Both are dropped long before the process exits, so the copy
/// would otherwise sit in freed memory for a crash dump or a debugger to find.
pub fn wipe_bytes(bytes: &mut [u8]) {
    wipe(bytes);
}

#[cfg(test)]
static WIPED_BYTES: AtomicU64 = AtomicU64::new(0);

impl Drop for SecretString {
    fn drop(&mut self) {
        // SAFETY: `self` is exclusively borrowed and the buffer is ours; writing NUL
        // bytes keeps it valid UTF-8, which `String` requires.
        wipe(unsafe { self.0.as_bytes_mut() });
        self.0.clear();
    }
}

impl Clone for SecretString {
    fn clone(&self) -> Self {
        // A clone is an independent buffer, wiped by its own drop.
        Self(self.0.clone())
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for SecretString {}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    /// A candidate password is deserialised straight into the wiped buffer, so a
    /// request body's copy of it is the only one left to clear (PAR-07).
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(SecretString)
    }
}

/// A store login.
///
/// `Debug` prints only whether each field is present; use
/// [`Credentials::normalised`] before sending anything, because store copy buttons
/// are known to prepend whitespace.
///
/// This type is **not** serialisable: only [`CredentialStore`] writes credentials, and
/// it does so through a private on-disk struct. Nothing else may put a password in the
/// index database, an IPC message or a log record. The password is a
/// [`SecretString`], wiped when the credentials are dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// User name exactly as the store issued it (may need trimming).
    pub username: String,
    /// Password exactly as the store issued it (may need trimming).
    pub password: SecretString,
}

impl Credentials {
    /// Wrap a raw pair without trimming.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: SecretString::new(password),
        }
    }

    /// Trim both fields and refuse an empty result.
    ///
    /// Bandcamp's copy button can prepend a space; a request built from untrimmed
    /// values fails authentication in a way that looks identical to a wrong password.
    pub fn normalised(&self) -> Result<Credentials, StoreError> {
        let username = self.username.trim();
        let password = self.password.expose().trim();
        if username.is_empty() || password.is_empty() {
            return Err(StoreError::InvalidCredentials);
        }
        Ok(Credentials::new(username, password))
    }
}

/// The on-disk form. Private on purpose: [`Credentials`] itself has no serialiser, so
/// the clear-text password can only appear through this one struct, in this module.
#[derive(Serialize, Deserialize)]
struct OnDiskCredentials {
    username: String,
    password: String,
}

/// The form [`CredentialStore::save`] serialises: borrows the password instead of
/// copying it into another `String` on the way to the file.
#[derive(Serialize)]
struct OnDiskCredentialsRef<'a> {
    username: &'a str,
    password: &'a str,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { username: <set>, password: <redacted> }")
    }
}

/// A directory of per-store credential files.
///
/// The store owns `dir`'s mode: it creates the directory `0700` if needed and
/// tightens it to `0700` if it already exists. Pass a dedicated secrets directory,
/// never a shared one.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    dir: PathBuf,
}

impl CredentialStore {
    /// Store credentials under `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory this store writes to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Load a store's credentials, or `None` when there is no readable, valid file.
    ///
    /// The file is opened `O_NOFOLLOW`, must be a regular file and is read through the
    /// shared bounded reader ([`plippa_core::safe_fs`]), so a symlinked credentials file
    /// is never followed and a FIFO or an oversized file cannot block or allocate.
    ///
    /// A file that is missing, unreadable or not valid JSON is `None`: credentials
    /// are either usable or the store is not connected.
    pub fn load(&self, store: &StoreId) -> Option<Credentials> {
        let path = self.file_path(store).ok()?;
        let bytes = plippa_core::safe_fs::read_regular_capped(&path, MAX_CREDENTIALS_BYTES).ok()?;
        let parsed: OnDiskCredentials = serde_json::from_slice(&bytes).ok()?;
        Some(Credentials {
            username: parsed.username,
            // Moved, not copied: the deserialiser's buffer becomes the wiped buffer.
            password: SecretString::new(parsed.password),
        })
    }

    /// Write a store's credentials atomically with mode `0600`.
    ///
    /// The file is written to a temporary name in the same directory, flushed, given
    /// mode `0600`, then renamed over the destination, so a reader never sees half a
    /// file and an older wider-permissioned file cannot survive the replacement.
    pub fn save(&self, store: &StoreId, creds: &Credentials) -> Result<(), StoreError> {
        let path = self.file_path(store)?;
        self.ensure_dir()?;

        // Borrow the password rather than copying it: the only other copy on this path
        // is the body buffer below, which is wiped before the function returns.
        let mut body = serde_json::to_vec(&OnDiskCredentialsRef {
            username: &creds.username,
            password: creds.password.expose(),
        })
        .map_err(|err| StoreError::policy(format!("cannot encode credentials: {err}")))?;
        let temp = self.temp_path(store);

        let write = || -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&temp)?;
            file.write_all(&body)?;
            file.flush()?;
            file.sync_all()?;
            fs::set_permissions(&temp, fs::Permissions::from_mode(FILE_MODE))
        };
        let written = write();
        // The file has its bytes; the buffer must not outlive this call in freed memory.
        wipe(&mut body);
        if let Err(err) = written {
            let _ = fs::remove_file(&temp);
            return Err(StoreError::policy(format!(
                "cannot write credentials for {store}: {err}"
            )));
        }
        if let Err(err) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(StoreError::policy(format!(
                "cannot store credentials for {store}: {err}"
            )));
        }
        // Belt and braces: an older destination with wider permissions is replaced by
        // the rename, but the explicit chmod makes the guarantee independent of the
        // temporary file's inherited mode.
        fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE)).map_err(|err| {
            StoreError::policy(format!("cannot set credentials mode for {store}: {err}"))
        })
    }

    /// Delete a store's credentials. `true` when a file was removed.
    ///
    /// Any failure (including a missing file) reads as "no credentials remain usable
    /// from here"; a permission problem is reported by the next [`Self::save`].
    pub fn remove(&self, store: &StoreId) -> bool {
        let Ok(path) = self.file_path(store) else {
            return false;
        };
        fs::remove_file(path).is_ok()
    }

    /// Stores with a credential file, sorted.
    pub fn list(&self) -> Vec<StoreId> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut stores: Vec<StoreId> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    return None;
                }
                let stem = path.file_stem()?.to_str()?;
                StoreId::new(stem).ok()
            })
            .collect();
        stores.sort();
        stores.dedup();
        stores
    }

    fn file_path(&self, store: &StoreId) -> Result<PathBuf, StoreError> {
        // Re-validate rather than trusting the newtype: ids also arrive from the
        // portal and from on-disk names, and this is the boundary that stops `../`.
        if !crate::types::is_valid_slug(store.as_str()) {
            return Err(StoreError::policy("refusing an unsafe store id"));
        }
        Ok(self.dir.join(format!("{store}.json")))
    }

    fn temp_path(&self, store: &StoreId) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!(
            ".{store}.json.{}.{}.tmp",
            std::process::id(),
            n
        ))
    }

    fn ensure_dir(&self) -> Result<(), StoreError> {
        match fs::metadata(&self.dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(StoreError::policy(format!(
                    "credential path is not a directory: {}",
                    self.dir.display()
                )))
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(DIR_MODE)
                    .create(&self.dir)
                    .map_err(|err| {
                        StoreError::policy(format!(
                            "cannot create credential directory {}: {err}",
                            self.dir.display()
                        ))
                    })?;
            }
            Err(err) => {
                return Err(StoreError::policy(format!(
                    "cannot inspect credential directory {}: {err}",
                    self.dir.display()
                )))
            }
        }
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(DIR_MODE)).map_err(|err| {
            StoreError::policy(format!(
                "cannot set credential directory mode on {}: {err}",
                self.dir.display()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "plippa-store-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        /// A path inside the temporary directory that does not exist yet, so the
        /// store creates it.
        fn fresh_secrets_dir(&self) -> PathBuf {
            self.0.join("secrets")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store_id() -> StoreId {
        StoreId::new("bandcamp").unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn debug_never_prints_the_password() {
        let creds = Credentials::new("fan@example.com", "hunter2-copy-pasted");
        assert_eq!(
            format!("{creds:?}"),
            "Credentials { username: <set>, password: <redacted> }"
        );
        assert!(!format!("{creds:?}").contains("hunter2"));
        assert!(!format!("{creds:?}").contains("fan@example.com"));
    }

    #[test]
    fn normalised_trims_and_refuses_empty_fields() {
        let creds = Credentials::new("  fan ", "\t secret \n").normalised().unwrap();
        assert_eq!(creds, Credentials::new("fan", "secret"));

        assert_eq!(
            Credentials::new("   ", "secret").normalised(),
            Err(StoreError::InvalidCredentials)
        );
        assert_eq!(
            Credentials::new("fan", "  \n").normalised(),
            Err(StoreError::InvalidCredentials)
        );
        assert_eq!(
            Credentials::new("fan", "").normalised().unwrap_err().code(),
            "invalid_credentials"
        );
    }

    #[test]
    fn save_writes_0600_and_reload_returns_identical_credentials() {
        let temp = TempDir::new("round-trip");
        let dir = temp.fresh_secrets_dir();
        let store = CredentialStore::new(&dir);
        let creds = Credentials::new("fan", "s3cr3t-\u{00e9}\u{00e8}");

        store.save(&store_id(), &creds).expect("save");

        assert_eq!(mode_of(&dir), DIR_MODE, "secrets dir mode");
        assert_eq!(mode_of(&dir.join("bandcamp.json")), FILE_MODE, "file mode");
        assert_eq!(store.load(&store_id()), Some(creds.clone()));
        assert_eq!(store.list(), vec![store_id()]);
    }

    #[test]
    fn save_tightens_a_file_that_already_had_wider_permissions() {
        let temp = TempDir::new("tighten");
        let dir = temp.fresh_secrets_dir();
        let store = CredentialStore::new(&dir);
        store.save(&store_id(), &Credentials::new("a", "b")).expect("save");

        let path = dir.join("bandcamp.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("widen");
        assert_eq!(mode_of(&path), 0o644);

        store.save(&store_id(), &Credentials::new("c", "d")).expect("re-save");
        assert_eq!(mode_of(&path), FILE_MODE);
        assert_eq!(
            store.load(&store_id()),
            Some(Credentials::new("c", "d"))
        );
    }

    #[test]
    fn traversal_and_path_like_ids_are_refused() {
        let temp = TempDir::new("traversal");
        let dir = temp.fresh_secrets_dir();
        let store = CredentialStore::new(&dir);

        // A StoreId can only hold a valid slug; this checks the same boundary with the
        // raw strings that arrive from the portal and from on-disk names.
        for bad in ["../bandcamp", "bandcamp/../x", "bandcamp.json", "..", "a/b"] {
            assert!(!crate::types::is_valid_slug(bad), "accepted {bad:?}");
        }

        store
            .save(&store_id(), &Credentials::new("u", "p"))
            .expect("save");
        assert!(dir.join("bandcamp.json").exists());
        assert!(
            !temp.0.join("bandcamp.json").exists(),
            "must never write outside the secrets dir"
        );
        assert!(
            !temp.0.join("../bandcamp.json").exists(),
            "must never write outside the secrets dir"
        );
    }

    #[test]
    fn remove_list_and_missing_files_behave() {
        let temp = TempDir::new("lifecycle");
        let dir = temp.fresh_secrets_dir();
        let store = CredentialStore::new(&dir);

        assert_eq!(store.load(&store_id()), None);
        assert!(store.list().is_empty());
        assert!(!store.remove(&store_id()));

        let other = StoreId::new("mock").unwrap();
        store.save(&store_id(), &Credentials::new("u", "p")).expect("save");
        store.save(&other, &Credentials::new("u2", "p2")).expect("save");
        assert_eq!(store.list(), vec![store_id(), other.clone()]);
        assert!(store.remove(&store_id()));
        assert!(!store.remove(&store_id()));
        assert_eq!(store.list(), vec![other]);
    }

    #[test]
    fn corrupt_files_are_not_credentials_and_list_ignores_non_json() {
        let temp = TempDir::new("corrupt");
        let dir = temp.fresh_secrets_dir();
        let store = CredentialStore::new(&dir);
        store.save(&store_id(), &Credentials::new("u", "p")).expect("save");

        fs::write(dir.join("bandcamp.json"), b"not json").expect("corrupt");
        assert_eq!(store.load(&store_id()), None);

        fs::remove_file(dir.join("bandcamp.json")).expect("remove");
        fs::write(dir.join("notes.txt"), b"x").expect("write");
        fs::write(dir.join(".hidden.json"), b"{}").expect("write");
        assert_eq!(store.list(), Vec::<StoreId>::new());
    }

    #[test]
    fn a_symlinked_credentials_file_is_not_followed() {
        let temp = TempDir::new("creds-symlink");
        let dir = temp.fresh_secrets_dir();
        let store = CredentialStore::new(&dir);
        store.save(&store_id(), &Credentials::new("u", "p")).expect("save");

        // Move the real file out of the way and link to it from the expected name: the
        // bytes are still valid credentials, but reading them would follow the link.
        let real = temp.0.join("real.json");
        fs::rename(dir.join("bandcamp.json"), &real).expect("move");
        std::os::unix::fs::symlink(&real, dir.join("bandcamp.json")).expect("symlink");

        assert_eq!(
            store.load(&store_id()),
            None,
            "a symlinked credentials file must not be read"
        );
    }

    #[test]
    fn a_fifo_named_like_the_credentials_file_does_not_block() {
        let temp = TempDir::new("creds-fifo");
        let dir = temp.fresh_secrets_dir();
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("bandcamp.json");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "mkfifo failed");

        let started = std::time::Instant::now();
        assert_eq!(CredentialStore::new(&dir).load(&store_id()), None);
        assert!(
            started.elapsed().as_secs() < 2,
            "a FIFO must be refused, not awaited"
        );
    }

    #[test]
    fn credentials_are_wiped_on_drop() {
        // The wipe helper itself zeroes the buffer it is given.
        let mut buf = *b"hunter2-secret";
        wipe(&mut buf);
        assert_eq!(buf, [0u8; 14]);

        // And `Drop` calls it for the whole secret (the test build counts wiped bytes).
        let before = WIPED_BYTES.load(Ordering::Relaxed);
        {
            let secret = SecretString::new("hunter2-secret");
            assert_eq!(secret.expose(), "hunter2-secret");
            assert_eq!(format!("{secret:?}"), "<redacted>");
        }
        assert_eq!(
            WIPED_BYTES.load(Ordering::Relaxed) - before,
            14,
            "drop must wipe every byte of the password"
        );
    }

    #[test]
    fn a_candidate_password_deserialises_into_the_wiped_buffer() {
        // A request body's password must not need a plain `String` on the way in: serde
        // builds it straight into a `SecretString`, which wipes on drop.
        #[derive(Deserialize)]
        struct Body {
            password: SecretString,
        }
        let before = WIPED_BYTES.load(Ordering::Relaxed);
        {
            let body: Body = serde_json::from_str(r#"{"password":"hunter2"}"#).expect("json");
            assert_eq!(body.password.expose(), "hunter2");
            assert_eq!(format!("{:?}", body.password), "<redacted>");
        }
        assert_eq!(WIPED_BYTES.load(Ordering::Relaxed) - before, 7);
    }
}
