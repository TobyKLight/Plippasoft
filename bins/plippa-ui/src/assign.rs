//! Assignable Encoder A — binding table + persistence.
//!
//! # Persistence location
//!
//! Binding file (JSON), resolved in order:
//! 1. Explicit path from [`UiConfig::assign_path`] / [`AssignStore::with_path`]
//! 2. `$PLIPPA_STATE_DIR/assignable_a.json` when `PLIPPA_STATE_DIR` is set
//! 3. `$XDG_STATE_HOME/plippa/assignable_a.json` (default
//!    `$HOME/.local/state/plippa/assignable_a.json`)
//!
//! Survives process restart and reboot. Not stored under the media library
//! `.plippa/` tree (that index is USB-scoped; UI bindings are device-local).
//!
//! # File format
//!
//! ```json
//! {"encoder_a":"dummy_a"}
//! ```
//!
//! Unbound (or clear): `{"encoder_a":null}` or omit the key.
//!
//! # Keyboard temporary override (UI_05)
//!
//! While the Wi‑Fi keyboard is open, Encoder A drives charset region jumps.
//! Call [`AssignStore::begin_keyboard_override`] on enter and
//! [`AssignStore::end_keyboard_override`] on leave — the on-disk binding is
//! unchanged; only the in-memory stash is used for restore.
//!
//! # [`AssignableId`] registry
//!
//! Stable string ids shared with the control graph and future screens:
//!
//! | Id | Meaning |
//! |----|---------|
//! | `volume` | Master volume 0–100% |
//! | `dummy_a` / `dummy_b` | Debug control-graph params (UI_04 test targets) |
//!
//! Additive ids need no schema version bump.

use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Environment override for the UI state directory (device-local).
pub const ENV_STATE_DIR: &str = "PLIPPA_STATE_DIR";

/// Filename inside the state directory.
pub const ASSIGN_FILE_NAME: &str = "assignable_a.json";

/// Stable assignable control id (see module docs).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssignableId(String);

impl AssignableId {
    pub const VOLUME: &'static str = "volume";
    pub const DUMMY_A: &'static str = "dummy_a";
    pub const DUMMY_B: &'static str = "dummy_b";

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn volume() -> Self {
        Self::new(Self::VOLUME)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Documented registry entries (unknown ids still persist for forward compat).
    pub fn is_known(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::VOLUME | Self::DUMMY_A | Self::DUMMY_B
        )
    }
}

impl std::fmt::Display for AssignableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct AssignFile {
    #[serde(default)]
    encoder_a: Option<AssignableId>,
}

/// In-memory binding table for Encoder A (+ optional persist path).
#[derive(Debug, Clone)]
pub struct AssignStore {
    path: Option<PathBuf>,
    encoder_a: Option<AssignableId>,
    /// Stashed binding while keyboard mode owns Encoder A (not written to disk).
    keyboard_restore: Option<Option<AssignableId>>,
}

impl Default for AssignStore {
    fn default() -> Self {
        Self {
            path: None,
            encoder_a: None,
            keyboard_restore: None,
        }
    }
}

impl AssignStore {
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            encoder_a: None,
            keyboard_restore: None,
        }
    }

    /// Resolve default path from the environment (may not exist yet).
    pub fn default_path() -> PathBuf {
        state_file_path(ASSIGN_FILE_NAME)
    }
}

/// `file` inside the device-local UI state directory: `$PLIPPA_STATE_DIR/file`, else
/// `$XDG_STATE_HOME/plippa/file` (default `$HOME/.local/state/plippa/`). Shared by the
/// assignable binding and the play-mode settings.
pub fn state_file_path(file: &str) -> PathBuf {
    if let Ok(dir) = env::var(ENV_STATE_DIR) {
        return PathBuf::from(dir).join(file);
    }
    let state_home = env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            env::var("HOME")
                .map(|h| PathBuf::from(h).join(".local/state"))
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
        });
    state_home.join("plippa").join(file)
}

impl AssignStore {

    /// Load from `path`, or empty store on missing / invalid file.
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        match Self::load(path) {
            Ok(store) => store,
            Err(e) => {
                eprintln!(
                    "plippa-ui: assign load `{}`: {e} — starting unbound",
                    path.display()
                );
                Self::with_path(path.to_path_buf())
            }
        }
    }

    /// Load default env path (or empty unbound store).
    pub fn load_default() -> Self {
        Self::load_or_default(Self::default_path())
    }

    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::with_path(path.to_path_buf()));
        }
        let text = fs::read_to_string(path)?;
        let file: AssignFile = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            encoder_a: file.encoder_a,
            keyboard_restore: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn encoder_a(&self) -> Option<&AssignableId> {
        self.encoder_a.as_ref()
    }

    pub fn set_encoder_a(&mut self, id: Option<AssignableId>) {
        self.encoder_a = id;
    }

    /// True while keyboard mode has stashed the persisted binding.
    pub fn keyboard_override_active(&self) -> bool {
        self.keyboard_restore.is_some()
    }

    /// Stash the current binding for keyboard mode (in-memory only; no disk write).
    ///
    /// Idempotent: a nested call is ignored while already overridden.
    pub fn begin_keyboard_override(&mut self) {
        if self.keyboard_restore.is_some() {
            return;
        }
        self.keyboard_restore = Some(self.encoder_a.clone());
    }

    /// Restore the binding stashed by [`Self::begin_keyboard_override`].
    pub fn end_keyboard_override(&mut self) {
        if let Some(prior) = self.keyboard_restore.take() {
            self.encoder_a = prior;
        }
    }

    /// Write immediately (create parent dirs). No-op if no path configured.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        // Persist the real binding, not a keyboard-cleared view.
        let encoder_a = match &self.keyboard_restore {
            Some(prior) => prior.clone(),
            None => self.encoder_a.clone(),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = AssignFile { encoder_a };
        let text = serde_json::to_string_pretty(&file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn persist_reload_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(ASSIGN_FILE_NAME);
        let mut store = AssignStore::with_path(&path);
        store.set_encoder_a(Some(AssignableId::new(AssignableId::DUMMY_A)));
        store.save().unwrap();
        let loaded = AssignStore::load(&path).unwrap();
        assert_eq!(
            loaded.encoder_a().map(AssignableId::as_str),
            Some("dummy_a")
        );
    }

    #[test]
    fn missing_file_is_unbound() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let store = AssignStore::load(&path).unwrap();
        assert!(store.encoder_a().is_none());
    }

    #[test]
    fn clear_binding_persists_null() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(ASSIGN_FILE_NAME);
        let mut store = AssignStore::with_path(&path);
        store.set_encoder_a(Some(AssignableId::volume()));
        store.save().unwrap();
        store.set_encoder_a(None);
        store.save().unwrap();
        let loaded = AssignStore::load(&path).unwrap();
        assert!(loaded.encoder_a().is_none());
    }

    #[test]
    fn keyboard_override_save_restore() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(ASSIGN_FILE_NAME);
        let mut store = AssignStore::with_path(&path);
        store.set_encoder_a(Some(AssignableId::volume()));
        store.save().unwrap();
        store.begin_keyboard_override();
        assert!(store.keyboard_override_active());
        store.set_encoder_a(None);
        store.save().unwrap();
        let on_disk = AssignStore::load(&path).unwrap();
        assert_eq!(
            on_disk.encoder_a().map(AssignableId::as_str),
            Some("volume")
        );
        store.end_keyboard_override();
        assert!(!store.keyboard_override_active());
        assert_eq!(
            store.encoder_a().map(AssignableId::as_str),
            Some("volume")
        );
    }
}
