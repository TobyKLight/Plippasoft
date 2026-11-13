//! Structured audio errors (clear messages for Noubin §3.3 overspec refusal).

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("unsupported or unrecognised audio format for {path}")]
    UnsupportedFormat { path: PathBuf },

    #[error("format `{ext}` is not enabled for this player ({path})")]
    FormatNotEnabled { path: PathBuf, ext: String },

    #[error("no playable localFilePaths entry for this player's capabilities")]
    NoPlayablePath,

    #[error(
        "The file is {file_rate}Hz/{file_bits}-bit {format}. \
         This player only supports maximum {max_rate}Hz/{max_bits}-bit {format}"
    )]
    QualityExceedsCapabilities {
        path: PathBuf,
        format: String,
        file_rate: u32,
        file_bits: u8,
        max_rate: u32,
        max_bits: u8,
    },

    #[error("probe failed for {path}: {reason}")]
    Probe { path: PathBuf, reason: String },

    #[error("decode failed for {path}: {reason}")]
    Decode { path: PathBuf, reason: String },

    /// The isolated media helper died, ran out of its limits or stopped answering while it
    /// handled this file, or the file was marked after doing so earlier. The file is
    /// persistently unplayable until its path, mtime or size changes.
    #[error("this file cannot be played safely ({path}): {reason}")]
    Unplayable { path: PathBuf, reason: String },

    /// The media helper could not be started (binary missing or from another version, no
    /// permission to spawn). Says nothing about the file; the next attempt may work.
    #[error("the media helper is unavailable: {reason}")]
    HelperUnavailable { reason: String },

    #[error("resample failed: {0}")]
    Resample(String),

    #[error("sink error: {0}")]
    Sink(String),

    #[error("invalid audio parameter: {0}")]
    InvalidParam(String),
}
