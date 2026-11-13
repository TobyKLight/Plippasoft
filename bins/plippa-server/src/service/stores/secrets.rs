//! On-disk credential storage for a store connection (BCS_03).
//!
//! A thin wrapper around [`plippa_store::CredentialStore`] (which owns the 0700/0600
//! atomic write) that adds the server's own defensiveness:
//!
//! * a **symlinked** secrets directory is refused instead of followed (someone who can
//!   replace the directory with a link chooses where the password lands);
//! * a credentials **file** that is a symlink is never read;
//! * a file that is readable by group/others is tightened to `0600` on load (and a
//!   warning is logged), because the secret may already have been copied elsewhere but
//!   leaving it open would keep the exposure going;
//! * the directory can be probed without touching a credentials file, so the portal can
//!   report `stores.available = false` while everything else keeps working.
//!
//! Nothing here logs the file name, the user name or the password: log lines name the
//! directory only.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use plippa_store::{CredentialStore, Credentials, StoreId, DIR_MODE, FILE_MODE};

use super::{log_line, STORE_SLUG};

/// One store's credentials in a secrets directory the server owns.
#[derive(Debug, Clone)]
pub struct StoreSecrets {
    dir: PathBuf,
    store: StoreId,
    inner: CredentialStore,
}

impl StoreSecrets {
    /// Wrap `dir` for the Bandcamp store.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::for_store(dir, STORE_SLUG)
    }

    /// Wrap `dir` for an arbitrary store slug (the crate supports more than one).
    pub fn for_store(dir: impl Into<PathBuf>, store: &str) -> Self {
        let dir = dir.into();
        let store = StoreId::new(store).expect("a compile-time store slug is valid");
        Self {
            inner: CredentialStore::new(dir.clone()),
            dir,
            store,
        }
    }

    /// The secrets directory (safe to log; file names are not).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `true` when a usable credentials file exists for this store.
    pub fn is_connected(&self) -> bool {
        self.load().is_some()
    }

    /// Check that the directory can hold credentials: exists (or can be created),
    /// is a real directory on this filesystem and is writable by this process.
    ///
    /// Creates the directory `0700` when it is missing, so a first save needs no
    /// separate step. Returns a plain reason for the portal when it cannot be used.
    pub fn probe(&self) -> Result<(), String> {
        match fs::symlink_metadata(&self.dir) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(
                        "the secrets directory is a symbolic link; refusing to follow it".into(),
                    );
                }
                if !meta.is_dir() {
                    return Err("the configured secrets path is not a directory".into());
                }
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    if fs::set_permissions(&self.dir, fs::Permissions::from_mode(DIR_MODE))
                        .is_err()
                    {
                        return Err(format!(
                            "the secrets directory is accessible to others and cannot be restricted: {}",
                            self.dir.display()
                        ));
                    }
                    log_line(&format!(
                        "tightened the secrets directory to 0700: {}",
                        self.dir.display()
                    ));
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                DirBuilder::new()
                    .recursive(true)
                    .mode(DIR_MODE)
                    .create(&self.dir)
                    .map_err(|err| {
                        format!(
                            "cannot create the secrets directory {}: {err}",
                            self.dir.display()
                        )
                    })?;
                // `recursive(true)` applies the mode to created parents on Linux; set it
                // on the leaf explicitly so a pre-existing parent cannot widen it.
                let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(DIR_MODE));
            }
            Err(err) => {
                return Err(format!(
                    "cannot inspect the secrets directory {}: {err}",
                    self.dir.display()
                ))
            }
        }
        self.check_writable()
    }

    /// Load the credentials, or `None` when the store is not connected.
    ///
    /// A symlinked or non-regular file is refused; a file readable by others is
    /// tightened to `0600` first (and refused if that fails).
    pub fn load(&self) -> Option<Credentials> {
        let path = self.path();
        let meta = fs::symlink_metadata(&path).ok()?;
        if meta.file_type().is_symlink() {
            log_line("refusing a symlinked credentials file; the store stays disconnected");
            return None;
        }
        if !meta.is_file() {
            return None;
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            log_line("credentials file was readable by others; tightening it to 0600");
            if fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE)).is_err() {
                log_line("cannot tighten the credentials file; refusing to read it");
                return None;
            }
        }
        self.inner.load(&self.store)
    }

    /// Save (atomically, `0600`) after checking the directory is usable.
    pub fn save(&self, creds: &Credentials) -> Result<(), String> {
        self.probe()?;
        self.inner.save(&self.store, creds).map_err(|err| {
            // `StoreError`'s message carries the directory only, never the contents.
            format!("cannot store the credentials: {err}")
        })
    }

    /// Remove the credentials. `true` when a file was removed; a missing file is fine.
    pub fn remove(&self) -> bool {
        self.inner.remove(&self.store)
    }

    fn path(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.store))
    }

    fn check_writable(&self) -> Result<(), String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let probe = self
            .dir
            .join(format!(".probe.{}.{n}", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .open(&probe)?;
            file.write_all(b"plippa")?;
            file.flush()
        })();
        let _ = fs::remove_file(&probe);
        result.map_err(|err| {
            format!(
                "the secrets directory {} is not writable: {err}",
                self.dir.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::stores::tests_support::TempDir;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn probe_creates_the_directory_0700_and_is_idempotent() {
        let temp = TempDir::new("secrets-probe");
        let dir = temp.path().join("secrets");
        let secrets = StoreSecrets::new(&dir);
        assert_eq!(secrets.probe(), Ok(()));
        assert!(dir.is_dir());
        assert_eq!(mode_of(&dir), DIR_MODE);
        assert_eq!(secrets.probe(), Ok(()));
    }

    #[test]
    fn a_symlinked_directory_is_refused_even_when_it_points_at_a_real_one() {
        let temp = TempDir::new("secrets-symlink");
        let real = temp.path().join("real");
        fs::create_dir_all(&real).expect("real");
        let link = temp.path().join("secrets");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let err = StoreSecrets::new(&link).probe().expect_err("must refuse");
        assert!(err.contains("symbolic link"), "{err}");
        assert!(StoreSecrets::new(&link).save(&Credentials::new("u", "p")).is_err());
        assert!(!real.join("bandcamp.json").exists());
    }

    #[test]
    fn a_world_readable_file_is_tightened_on_load_and_still_loads() {
        let temp = TempDir::new("secrets-tighten");
        let secrets = StoreSecrets::new(temp.path().join("secrets"));
        secrets.save(&Credentials::new("fan", "secret")).expect("save");
        let path = secrets.dir().join("bandcamp.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("widen");
        assert_eq!(mode_of(&path), 0o644);

        let loaded = secrets.load().expect("loaded after tightening");
        assert_eq!(loaded, Credentials::new("fan", "secret"));
        assert_eq!(mode_of(&path), FILE_MODE);
    }

    #[test]
    fn a_symlinked_credentials_file_is_never_read() {
        let temp = TempDir::new("secrets-file-link");
        let secrets = StoreSecrets::new(temp.path().join("secrets"));
        secrets.save(&Credentials::new("fan", "secret")).expect("save");
        let elsewhere = temp.path().join("stolen.json");
        fs::rename(secrets.dir().join("bandcamp.json"), &elsewhere).expect("move");
        std::os::unix::fs::symlink(&elsewhere, secrets.dir().join("bandcamp.json")).expect("link");

        assert!(secrets.load().is_none());
        // The target is untouched: nothing was rewritten through the link either.
        assert!(secrets.save(&Credentials::new("x", "y")).is_ok());
        assert!(fs::symlink_metadata(secrets.dir().join("bandcamp.json"))
            .expect("metadata")
            .file_type()
            .is_file());
    }

    #[test]
    fn a_path_that_is_a_file_is_reported_not_panicked_on() {
        let temp = TempDir::new("secrets-not-a-dir");
        let blocked = temp.path().join("blocked");
        fs::write(&blocked, b"not a directory").expect("write");
        let err = StoreSecrets::new(&blocked).probe().expect_err("must refuse");
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn remove_is_idempotent() {
        let temp = TempDir::new("secrets-remove");
        let secrets = StoreSecrets::new(temp.path().join("secrets"));
        assert!(!secrets.remove());
        secrets.save(&Credentials::new("u", "p")).expect("save");
        assert!(secrets.remove());
        assert!(!secrets.remove());
        assert!(secrets.load().is_none());
    }
}
