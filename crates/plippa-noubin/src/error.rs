//! Error types for Noubin helpers.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NoubinError {
    #[error(transparent)]
    Ndef(#[from] NdefError),
    #[error(transparent)]
    Noudata(#[from] NoudataError),
    #[error(transparent)]
    Library(#[from] LibraryError),
    #[error(transparent)]
    Cover(#[from] CoverEditError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Why a [`crate::covers::CoverEdit`] was refused. The document is left untouched.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoverEditError {
    /// The name is absolute, has `..`, a NUL, a backslash, or another segment the library
    /// path guard refuses.
    #[error("cover image path is not a safe relative path: {path:?}")]
    UnsafePath { path: String },
    /// Only `jpg`, `jpeg`, `webp` and `png` files are cover images (Noubin §1.27).
    #[error("cover image {path:?} is not a jpg, jpeg, webp or png file")]
    UnsupportedExtension { path: String },
    /// More images than a cover list may hold.
    #[error("a cover list holds at most {max} images, got {got}")]
    TooManyImages { max: usize, got: usize },
    /// `Item { position }` is 1-based and must name an existing `mediaList` entry.
    #[error("the document has no media item at position {position}")]
    NoSuchItem { position: u32 },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NdefError {
    #[error("no NDEF record available")]
    NoRecord,
    #[error("unsupported NDEF type (need URI or Text well-known record)")]
    UnsupportedType,
    #[error("malformed NDEF message")]
    Malformed,
    #[error("invalid UTF-8 in NDEF payload")]
    InvalidUtf8,
}

#[derive(Debug, Error)]
pub enum NoudataError {
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not a JSON object")]
    NotObject,
    /// A new document broke the MUST/MUST NOT rules of its `noudataType`.
    #[error("invalid document for its noudataType: {0}")]
    TypeRules(String),
}

/// Structured errors for media library open / scan / lookup (never panic).
#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("media library root not found: {path}")]
    MissingRoot { path: PathBuf },
    #[error("media library root is not a directory: {path}")]
    InvalidRoot { path: PathBuf },
    #[error("library index not found (run a scan first): {path}")]
    IndexMissing { path: PathBuf },
    #[error("media storage vanished at {path}: {detail}")]
    StorageVanished { path: PathBuf, detail: String },
    #[error("path {path} is not under library root {root}")]
    PathNotInLibrary { root: PathBuf, path: PathBuf },
    #[error("expected WAL journal_mode, got {mode}")]
    NotWal { mode: String },
    /// The index file failed validation (foreign schema, planted object, failed integrity
    /// check, not a regular file ...): it is not ours, or not intact. It is derived data, so a
    /// writer deletes it and rebuilds it from the `.noudata` files.
    #[error("the library index is not trustworthy: {detail}")]
    ForeignIndex { detail: String },
    /// The index could not be opened *right now* (another process is still opening it, or the
    /// directory cannot be locked): temporary. Nothing is deleted or rebuilt for this; callers
    /// report the library as unavailable and try again later.
    #[error("the library index is busy: {detail}")]
    Busy { detail: String },
    #[error(transparent)]
    Noudata(#[from] NoudataError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl LibraryError {
    /// True when the SQLite index cannot be trusted any more.
    ///
    /// A connection opened before a USB stick was unplugged and plugged back in refers to
    /// the old mount; it answers reads from stale cache and fails writes with `database disk
    /// image is malformed` for the rest of its life. The same error means the file on disk
    /// was damaged by an unclean unplug. The index is **derived** from `.noudata`, so a
    /// writer may delete and rebuild it.
    pub fn is_unusable_index(&self) -> bool {
        match self {
            Self::Sqlite(e) => sqlite_is_unusable(e),
            Self::ForeignIndex { .. } => true,
            _ => false,
        }
    }

    /// True when an index read failed because the connection itself is stale (vanished
    /// mount, I/O error or corrupt index) rather than because a row is missing. Callers
    /// holding a long-lived reader use this to close and reopen it.
    pub fn is_stale_connection(&self) -> bool {
        match self {
            Self::StorageVanished { .. } | Self::Busy { .. } => true,
            Self::Sqlite(e) => sqlite_is_unusable(e) || sqlite_is_io(e),
            Self::Io(e) => e.kind() != std::io::ErrorKind::NotFound,
            _ => false,
        }
    }
}

fn sqlite_is_unusable(e: &rusqlite::Error) -> bool {
    match e {
        // A cell of the wrong type (a BLOB in a text column ...) is a file that is not ours.
        rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::InvalidColumnIndex(_)
        | rusqlite::Error::InvalidColumnName(_)
        | rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::InvalidQuery => true,
        rusqlite::Error::SqliteFailure(f, _) => matches!(
            f.code,
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
        ),
        _ => {
            let s = e.to_string().to_ascii_lowercase();
            s.contains("malformed") || s.contains("not a database")
        }
    }
}

fn sqlite_is_io(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(f, _) => matches!(
            f.code,
            rusqlite::ErrorCode::SystemIoFailure | rusqlite::ErrorCode::CannotOpen
        ),
        _ => false,
    }
}
