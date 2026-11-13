//! The helper side: one process, one media file (inherited at [`MEDIA_FD`]), one conversation
//! over stdin / stdout. Nothing here opens a path: the parent hands over a descriptor it has
//! already confined and `fstat`ed, and the helper re-opens that same object only through
//! `pread`, so a symlink swapped in afterwards changes nothing.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;

use symphonia::core::io::MediaSource;

use super::limits::{self, Limits};
use super::proto::{
    self, Request, Response, Status, StreamInfo, MAX_CHUNK_FRAMES, MEDIA_FD, PROTOCOL_VERSION,
};
use crate::error::AudioError;
use crate::index::{spawn_index_build, IndexSlot};
use crate::prepare::OverspecPolicy;
use crate::stream::{SourceOpener, StreamDecoder};

/// A read-only view of an open file with its own cursor, over `pread`: any number of these can
/// share one descriptor without disturbing each other (the decoder re-opens for indexed seeks).
pub struct FdSource {
    file: Arc<File>,
    pos: u64,
    len: u64,
}

impl FdSource {
    pub fn new(file: Arc<File>) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, pos: 0, len })
    }
}

impl Read for FdSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for FdSource {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = new.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the file")
        })?;
        Ok(self.pos)
    }
}

impl MediaSource for FdSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn status_of(dec: &StreamDecoder, index: &Option<Arc<IndexSlot>>) -> Status {
    Status {
        position_frame: dec.position_frame(),
        start_frame: dec.start_frame(),
        skipped_packets: dec.skipped_packets(),
        indexed_seeks: dec.indexed_seeks(),
        has_index: dec.has_index(),
        index_entries: index.as_ref().map_or(0, |s| s.entries()),
        index_done: index.as_ref().is_some_and(|s| s.is_done()),
        eof: dec.is_eof(),
        last_decode_error: dec
            .last_decode_error()
            .map(|e| e.chars().take(256).collect()),
    }
}

fn send(out: &mut impl Write, resp: &Response, tail: &[u8]) -> io::Result<()> {
    proto::write_frame(out, &proto::encode(resp)?, tail)
}

fn fail(out: &mut impl Write, e: &AudioError, status: Option<Status>) -> io::Result<()> {
    send(out, &Response::Failed { error: proto::WireError::from_audio(e), status }, &[])
}

fn no_file_open(out: &mut impl Write) -> io::Result<()> {
    send(
        out,
        &Response::Failed {
            error: proto::WireError::InvalidParam("no file is open".into()),
            status: None,
        },
        &[],
    )
}

/// Parse the limit arguments the parent passes: `--as N --cpu N --nofile N`.
fn parse_limits(args: &[String]) -> Option<Limits> {
    let mut want = Limits { address_space_bytes: 0, cpu_seconds: 0, max_open_files: 0 };
    let mut seen = 0;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value: u64 = it.next()?.parse().ok()?;
        match flag.as_str() {
            "--as" => want.address_space_bytes = value,
            "--cpu" => want.cpu_seconds = value,
            "--nofile" => want.max_open_files = value,
            _ => return None,
        }
        seen += 1;
    }
    (seen == 3 && want.address_space_bytes > 0 && want.cpu_seconds > 0 && want.max_open_files > 0)
        .then_some(want)
}

/// Exit codes of a helper that refuses to start (the parent maps any of them to "unavailable").
pub const EXIT_BAD_ARGS: i32 = 64;
pub const EXIT_NOT_CONFINED: i32 = 70;
pub const EXIT_BAD_MEDIA_FD: i32 = 71;

/// The helper's `main`. Returns the process exit code.
pub fn helper_main(args: Vec<String>) -> i32 {
    let Some(limits) = parse_limits(&args) else { return EXIT_BAD_ARGS };
    // Never decode unconfined: a helper started by hand, or by a parent that forgot, stops here.
    if !limits::verify(&limits) {
        return EXIT_NOT_CONFINED;
    }
    limits::close_from(MEDIA_FD + 1);
    // SAFETY: the parent put the media file at MEDIA_FD before exec; nothing else owns it.
    let media = unsafe { File::from_raw_fd(MEDIA_FD) };
    match media.metadata() {
        Ok(m) if m.file_type().is_file() => {}
        _ => return EXIT_BAD_MEDIA_FD,
    }
    let media = Arc::new(media);
    // SAFETY: fds 0 and 1 are the protocol pipes; they are used only through these handles.
    let mut input = unsafe { File::from_raw_fd(0) };
    let mut output = unsafe { File::from_raw_fd(1) };
    match serve(&media, &mut input, &mut output) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// The conversation loop. `Ok` on a clean shutdown or when the parent closes the pipe.
pub fn serve(media: &Arc<File>, input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    send(output, &Response::Hello { version: PROTOCOL_VERSION }, &[])?;
    let mut decoder: Option<StreamDecoder> = None;
    let mut index: Option<Arc<IndexSlot>> = None;
    loop {
        let payload = match proto::read_frame(input) {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let (body, _) = proto::split_frame(&payload)?;
        let request: Request = proto::decode(body)?;

        // A finished background index walk is installed between requests.
        if let (Some(dec), Some(slot)) = (decoder.as_mut(), index.as_ref()) {
            if !dec.has_index() {
                if let Some(idx) = slot.get() {
                    dec.set_index(idx);
                }
            }
        }

        match request {
            Request::Shutdown => return Ok(()),
            Request::HeaderDuration => {
                let ms = header_duration_ms(media);
                send(output, &Response::Duration(ms), &[])?;
            }
            Request::Open { name, capabilities, refuse_overspec } => {
                let policy = if refuse_overspec {
                    OverspecPolicy::Refuse
                } else {
                    OverspecPolicy::Downsample
                };
                let file = Arc::clone(media);
                let opener: SourceOpener = Arc::new(move || {
                    FdSource::new(Arc::clone(&file)).map(|s| Box::new(s) as Box<dyn MediaSource>)
                });
                let path = PathBuf::from(&name);
                match StreamDecoder::open_with(&path, opener, &capabilities, policy) {
                    Ok(dec) => {
                        let info = StreamInfo {
                            channels: dec.channels(),
                            sample_rate_hz: dec.sample_rate_hz(),
                            bit_depth: dec.bit_depth(),
                            extension: dec.extension().to_string(),
                            duration_ms: dec.duration_ms(),
                            duration_frames: dec.duration_frames(),
                            duration_known: dec.duration_known(),
                            status: status_of(&dec, &index),
                        };
                        decoder = Some(dec);
                        send(output, &Response::Opened(info), &[])?;
                    }
                    Err(e) => fail(output, &e, None)?,
                }
            }
            Request::Next { max_frames } => match decoder.as_mut() {
                None => no_file_open(output)?,
                Some(dec) => {
                    let max = (max_frames.clamp(1, MAX_CHUNK_FRAMES)) as usize;
                    match dec.next_chunk(max) {
                        Ok(Some(pcm)) => {
                            let frames = pcm.frame_count() as u32;
                            let resp = Response::Chunk {
                                status: status_of(dec, &index),
                                frames,
                                channels: pcm.channels,
                                sample_rate_hz: pcm.sample_rate_hz,
                            };
                            send(output, &resp, &proto::samples_to_bytes(&pcm.samples))?;
                        }
                        Ok(None) => send(output, &Response::End { status: status_of(dec, &index) }, &[])?,
                        Err(e) => fail(output, &e, Some(status_of(dec, &index)))?,
                    }
                }
            },
            Request::SeekFrame { frame } => match decoder.as_mut() {
                None => no_file_open(output)?,
                Some(dec) => match dec.seek_frame(frame) {
                    Ok(()) => send(output, &Response::Seeked { status: status_of(dec, &index) }, &[])?,
                    Err(e) => fail(output, &e, Some(status_of(dec, &index)))?,
                },
            },
            Request::BuildIndex { stride_frames } => {
                if index.is_none() {
                    let file = Arc::clone(media);
                    index = Some(spawn_index_build(
                        move || FdSource::new(file).map(|s| Box::new(s) as Box<dyn MediaSource>),
                        stride_frames,
                    ));
                }
                match decoder.as_ref() {
                    Some(dec) => send(output, &Response::IndexStatus(status_of(dec, &index)), &[])?,
                    None => no_file_open(output)?,
                }
            }
        }
    }
}

/// Container-header duration of the inherited file (the scanner's probe), `None` if unreadable
/// or zero. The format is sniffed from the content.
fn header_duration_ms(media: &Arc<File>) -> Option<u64> {
    use lofty::prelude::*;
    let source = FdSource::new(Arc::clone(media)).ok()?;
    let tagged = lofty::probe::Probe::new(io::BufReader::new(source))
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let ms = tagged.properties().duration().as_millis() as u64;
    (ms > 0).then_some(ms)
}
