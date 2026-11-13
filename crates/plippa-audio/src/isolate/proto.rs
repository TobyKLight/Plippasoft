//! Wire protocol between a parent (engine / server) and its media helper.
//!
//! Frames are `u32` little-endian length + `postcard` body, both directions, over the helper's
//! stdin / stdout. A [`Response::Chunk`] body is followed, inside the same frame, by its raw
//! little-endian `f32` samples (no per-sample encoding cost). Every length is capped and
//! checked before it is used: a helper that is itself hostile (a decoder bug turned into
//! attacker-controlled output) cannot make the parent allocate more than [`MAX_FRAME_BYTES`].

use std::io::{self, Read, Write};
use std::path::PathBuf;

use plippa_core::Capabilities;
use serde::{Deserialize, Serialize};

use crate::error::AudioError;

/// Bumped on any incompatible change; the helper announces it first thing. A deploy replaces
/// the helper binary while an engine may still run: a mismatch is "helper unavailable" (the
/// next spawn picks up the new binary), never a verdict on the file.
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest frame either side accepts (16 MiB: 2 MiB of PCM is the biggest real chunk).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Largest chunk a parent may ask for, in frames (the helper clamps to it).
pub const MAX_CHUNK_FRAMES: u32 = 1 << 16;

/// The media file is inherited at this descriptor number.
pub const MEDIA_FD: i32 = 3;

/// A request from the parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Open the inherited file as a decoder (`name` only supplies the extension and messages).
    Open {
        name: String,
        capabilities: Capabilities,
        refuse_overspec: bool,
    },
    /// Up to `max_frames` interleaved frames.
    Next { max_frames: u32 },
    /// Seek so the next frame is `frame`.
    SeekFrame { frame: u64 },
    /// Start building the MP3 seek index in the background (installed when ready); asking again
    /// is a cheap status refresh.
    BuildIndex { stride_frames: u32 },
    /// Length of the inherited file from its container headers (the scanner's probe).
    HeaderDuration,
    /// Leave cleanly.
    Shutdown,
}

/// Decoder state the parent mirrors; sent with every reply that can change it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub position_frame: u64,
    pub start_frame: u64,
    pub skipped_packets: u64,
    pub indexed_seeks: u64,
    pub has_index: bool,
    pub index_entries: u64,
    /// The background index walk has finished (installed or failed).
    pub index_done: bool,
    pub eof: bool,
    pub last_decode_error: Option<String>,
}

/// What `Open` learned about the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub bit_depth: u8,
    pub extension: String,
    pub duration_ms: Option<u64>,
    pub duration_frames: Option<u64>,
    pub duration_known: bool,
    pub status: Status,
}

/// An ordinary failure (a bad file, a refused format): not a helper crash, never marked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireError {
    Io(String),
    UnsupportedFormat,
    FormatNotEnabled { ext: String },
    QualityExceeds { format: String, file_rate: u32, file_bits: u8, max_rate: u32, max_bits: u8 },
    Probe(String),
    Decode(String),
    InvalidParam(String),
}

impl WireError {
    pub fn from_audio(e: &AudioError) -> Self {
        match e {
            AudioError::Io { source, .. } => Self::Io(source.to_string()),
            AudioError::UnsupportedFormat { .. } => Self::UnsupportedFormat,
            AudioError::FormatNotEnabled { ext, .. } => Self::FormatNotEnabled { ext: ext.clone() },
            AudioError::QualityExceedsCapabilities {
                format, file_rate, file_bits, max_rate, max_bits, ..
            } => Self::QualityExceeds {
                format: format.clone(),
                file_rate: *file_rate,
                file_bits: *file_bits,
                max_rate: *max_rate,
                max_bits: *max_bits,
            },
            AudioError::Probe { reason, .. } => Self::Probe(reason.clone()),
            AudioError::Decode { reason, .. } => Self::Decode(reason.clone()),
            other => Self::Decode(other.to_string()),
        }
    }

    pub fn into_audio(self, path: PathBuf) -> AudioError {
        match self {
            Self::Io(msg) => AudioError::Io { path, source: io::Error::other(msg) },
            Self::UnsupportedFormat => AudioError::UnsupportedFormat { path },
            Self::FormatNotEnabled { ext } => AudioError::FormatNotEnabled { path, ext },
            Self::QualityExceeds { format, file_rate, file_bits, max_rate, max_bits } => {
                AudioError::QualityExceedsCapabilities {
                    path,
                    format,
                    file_rate,
                    file_bits,
                    max_rate,
                    max_bits,
                }
            }
            Self::Probe(reason) => AudioError::Probe { path, reason },
            Self::Decode(reason) => AudioError::Decode { path, reason },
            Self::InvalidParam(msg) => AudioError::InvalidParam(msg),
        }
    }
}

/// A reply from the helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Unprompted first frame: the helper's protocol version.
    Hello { version: u16 },
    Opened(StreamInfo),
    /// `frames x channels` samples follow the body inside the frame.
    Chunk { status: Status, frames: u32, channels: u16, sample_rate_hz: u32 },
    End { status: Status },
    Seeked { status: Status },
    /// The index walk is running or finished; the status says which.
    IndexStatus(Status),
    Duration(Option<u64>),
    /// An ordinary failure; `status` is the decoder's state after it (skip counts, last error)
    /// when a decoder is open.
    Failed { error: WireError, status: Option<Status> },
}

fn too_large(len: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("frame of {len} bytes exceeds the {MAX_FRAME_BYTES} byte cap"),
    )
}

/// Write one frame: `body` (postcard) followed by `tail` (raw bytes, may be empty), in a single
/// `write` so a chunk costs one system call.
pub fn write_frame<W: Write>(w: &mut W, body: &[u8], tail: &[u8]) -> io::Result<()> {
    let len = 4 + body.len() + tail.len();
    if len > MAX_FRAME_BYTES {
        return Err(too_large(len));
    }
    // body length first so the reader can split body from tail.
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&(len as u32).to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(body);
    frame.extend_from_slice(tail);
    w.write_all(&frame)?;
    w.flush()
}

/// The declared length of the next frame, checked against the cap.
pub fn check_len(len: u32) -> io::Result<usize> {
    let len = len as usize;
    if !(4..=MAX_FRAME_BYTES).contains(&len) {
        return Err(too_large(len));
    }
    Ok(len)
}

/// Split a frame payload (everything after the length) into its postcard body and raw tail.
pub fn split_frame(payload: &[u8]) -> io::Result<(&[u8], &[u8])> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed frame");
    let body_len = u32::from_le_bytes(payload.get(..4).ok_or_else(bad)?.try_into().map_err(|_| bad())?)
        as usize;
    let rest = &payload[4..];
    if body_len > rest.len() {
        return Err(bad());
    }
    Ok(rest.split_at(body_len))
}

/// Blocking read of one whole frame payload (used by the helper on stdin).
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = check_len(u32::from_le_bytes(len))?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(payload)
}

pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    postcard::to_allocvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

pub fn decode<T: for<'a> Deserialize<'a>>(body: &[u8]) -> io::Result<T> {
    postcard::from_bytes(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Interleaved samples as raw little-endian bytes.
pub fn samples_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Raw little-endian bytes back to samples; `None` if the length is not a whole number of
/// samples.
pub fn bytes_to_samples(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}
