//! The frame protocol between `plippa-ui` and `plippa-leds` (SYS-02).
//!
//! One frame carries the colour of **every** LED of **every** stream, in board order. There
//! is no length field, no chunking and no negotiation after the connection is up: the header
//! has a fixed size and the payload is exactly `3 * sum(count)` bytes, so the only two things
//! a client can get wrong are the header (magic/version/stream slots) and the length, and both
//! end the connection. A layout that does not match the helper's own board profile is refused
//! before a single byte reaches a strip.
//!
//! ```text
//! magic   "PLED"                    4 bytes
//! version 1                         1 byte
//! seq     client sequence number    1 byte   (wrapping; the helper only logs it)
//! streams number of used slots      1 byte   (1..=MAX_STREAMS)
//! slots   MAX_STREAMS x (channel u8, count u16 little-endian)   12 bytes
//! payload sum(count) x (r, g, b)                                3 * sum(count) bytes
//! ```
//!
//! Unused slots must be all zero (one canonical encoding of a layout). Colours are logical
//! RGB; converting to the strip's wire order is the helper's business, not the UI's.

use plippa_board::BoardProfile;
use std::fmt;

/// Frame magic. A client that sends anything else is not ours.
pub const MAGIC: [u8; 4] = *b"PLED";
/// Protocol version. Bumped only for an incompatible frame shape.
pub const VERSION: u8 = 1;
/// Streams one board profile may have; the header always reserves this many slots.
pub const MAX_STREAMS: usize = 4;
/// Largest LED count one stream may declare (the header field is a `u16`; this is the sanity cap).
pub const MAX_LEDS_PER_STREAM: u16 = 1024;
/// Fixed header size: magic, version, seq, stream count and [`MAX_STREAMS`] slots.
pub const HEADER_LEN: usize = 4 + 1 + 1 + 1 + MAX_STREAMS * 3;
/// Largest payload the protocol can describe.
pub const MAX_PAYLOAD_LEN: usize = MAX_STREAMS * MAX_LEDS_PER_STREAM as usize * 3;

/// One LED's logical colour.
pub type Rgb = [u8; 3];

/// One strip on the wire: the driver channel and how many LEDs it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSpec {
    pub channel: u8,
    pub count: u16,
}

/// The board's streams in a stable order (stream name, as `[led_streams]` is a map).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    streams: Vec<StreamSpec>,
}

impl Layout {
    pub fn new(streams: Vec<StreamSpec>) -> Self {
        Self { streams }
    }

    /// The layout the helper and the UI both derive from the same board profile. Stream order
    /// is the profile's (sorted by name), which is also [`crate::protocol`]'s wire order.
    pub fn from_profile(profile: &BoardProfile) -> Self {
        Self {
            streams: profile
                .led_streams
                .values()
                .map(|stream| StreamSpec {
                    channel: stream.channel,
                    count: stream.count,
                })
                .collect(),
        }
    }

    pub fn streams(&self) -> &[StreamSpec] {
        &self.streams
    }

    /// LEDs described by the layout (0 for a board without `[led_streams]`).
    pub fn led_count(&self) -> usize {
        self.streams.iter().map(|s| s.count as usize).sum()
    }

    pub fn payload_len(&self) -> usize {
        self.led_count() * 3
    }

    pub fn frame_len(&self) -> usize {
        HEADER_LEN + self.payload_len()
    }

    /// A layout a client may announce: one to [`MAX_STREAMS`] streams, every count in
    /// `1..=MAX_LEDS_PER_STREAM`, no duplicated channel (one PWM channel drives one strip).
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.streams.is_empty() {
            return Err(ProtocolError::NoStreams);
        }
        if self.streams.len() > MAX_STREAMS {
            return Err(ProtocolError::TooManyStreams);
        }
        for (index, stream) in self.streams.iter().enumerate() {
            if stream.count == 0 || stream.count > MAX_LEDS_PER_STREAM {
                return Err(ProtocolError::CountOutOfRange);
            }
            if self.streams[..index]
                .iter()
                .any(|earlier| earlier.channel == stream.channel)
            {
                return Err(ProtocolError::DuplicateChannel);
            }
        }
        Ok(())
    }
}

/// A decoded header: what the frame claims to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub seq: u8,
    pub layout: Layout,
}

/// Every way a frame can be malformed. All of them close the connection; none of them panic
/// or read past the declared size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    /// Fewer bytes than a header.
    ShortHeader,
    BadMagic,
    BadVersion,
    NoStreams,
    TooManyStreams,
    CountOutOfRange,
    DuplicateChannel,
    /// An unused stream slot was not zeroed.
    BadPadding,
    /// The payload was not exactly `3 * sum(count)` bytes.
    WrongLength,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ProtocolError::ShortHeader => "short header",
            ProtocolError::BadMagic => "bad magic",
            ProtocolError::BadVersion => "unsupported protocol version",
            ProtocolError::NoStreams => "no streams announced",
            ProtocolError::TooManyStreams => "too many streams",
            ProtocolError::CountOutOfRange => "LED count out of range",
            ProtocolError::DuplicateChannel => "two streams on one channel",
            ProtocolError::BadPadding => "non-zero padding in the stream slots",
            ProtocolError::WrongLength => "wrong payload length",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ProtocolError {}

/// Encode one full frame. `pixels` must be exactly [`Layout::led_count`] colours.
pub fn encode(layout: &Layout, seq: u8, pixels: &[Rgb]) -> Result<Vec<u8>, ProtocolError> {
    layout.validate()?;
    if pixels.len() != layout.led_count() {
        return Err(ProtocolError::WrongLength);
    }
    let mut out = Vec::with_capacity(layout.frame_len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(seq);
    out.push(layout.streams.len() as u8);
    for slot in 0..MAX_STREAMS {
        match layout.streams.get(slot) {
            Some(stream) => {
                out.push(stream.channel);
                out.extend_from_slice(&stream.count.to_le_bytes());
            }
            None => out.extend_from_slice(&[0, 0, 0]),
        }
    }
    for pixel in pixels {
        out.extend_from_slice(pixel);
    }
    Ok(out)
}

/// Decode and validate a fixed-size header. `buf` may be longer than [`HEADER_LEN`].
pub fn decode_header(buf: &[u8]) -> Result<Header, ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtocolError::ShortHeader);
    }
    if buf[0..4] != MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    if buf[4] != VERSION {
        return Err(ProtocolError::BadVersion);
    }
    let seq = buf[5];
    let count = buf[6] as usize;
    if count == 0 {
        return Err(ProtocolError::NoStreams);
    }
    if count > MAX_STREAMS {
        return Err(ProtocolError::TooManyStreams);
    }
    let mut streams = Vec::with_capacity(count);
    for slot in 0..MAX_STREAMS {
        let at = 7 + slot * 3;
        let channel = buf[at];
        let leds = u16::from_le_bytes([buf[at + 1], buf[at + 2]]);
        if slot < count {
            streams.push(StreamSpec {
                channel,
                count: leds,
            });
        } else if channel != 0 || leds != 0 {
            return Err(ProtocolError::BadPadding);
        }
    }
    let layout = Layout::new(streams);
    layout.validate()?;
    Ok(Header { seq, layout })
}

/// Assemble the flat pixel list the client sends from per-stream frames (stream order, then
/// LED order). `frames` must line up with `layout`.
pub fn flatten(layout: &Layout, frames: &[Vec<Rgb>]) -> Result<Vec<Rgb>, ProtocolError> {
    if frames.len() != layout.streams.len() {
        return Err(ProtocolError::WrongLength);
    }
    let mut out = Vec::with_capacity(layout.led_count());
    for (frame, stream) in frames.iter().zip(layout.streams.iter()) {
        if frame.len() != stream.count as usize {
            return Err(ProtocolError::WrongLength);
        }
        out.extend_from_slice(frame);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poc2() -> Layout {
        Layout::new(vec![
            StreamSpec {
                channel: 0,
                count: 30,
            },
            StreamSpec {
                channel: 1,
                count: 33,
            },
        ])
    }

    fn header_bytes(layout: &Layout, seq: u8, streams: u8) -> Vec<u8> {
        let pixels = vec![[0u8, 0, 0]; layout.led_count()];
        let frame = encode(layout, seq, &pixels).unwrap();
        let mut header = frame[..HEADER_LEN].to_vec();
        header[6] = streams;
        header
    }

    #[test]
    fn a_frame_round_trips_through_the_header_and_back() {
        let layout = poc2();
        let mut pixels = vec![[0u8, 0, 0]; layout.led_count()];
        pixels[0] = [1, 2, 3];
        pixels[62] = [250, 251, 252];
        let frame = encode(&layout, 7, &pixels).unwrap();
        assert_eq!(frame.len(), 19 + 63 * 3);
        let header = decode_header(&frame).unwrap();
        assert_eq!(header.seq, 7);
        assert_eq!(header.layout, layout);
        assert_eq!(&frame[HEADER_LEN..HEADER_LEN + 3], &[1, 2, 3]);
        assert_eq!(&frame[frame.len() - 3..], &[250, 251, 252]);
    }

    #[test]
    fn encoding_a_frame_of_the_wrong_length_is_refused() {
        let layout = poc2();
        assert_eq!(
            encode(&layout, 0, &[[0, 0, 0]; 62]),
            Err(ProtocolError::WrongLength)
        );
        assert_eq!(
            encode(&layout, 0, &[[0, 0, 0]; 64]),
            Err(ProtocolError::WrongLength)
        );
    }

    #[test]
    fn a_bad_magic_or_version_is_refused() {
        let layout = poc2();
        let good = header_bytes(&layout, 0, 2);

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert_eq!(decode_header(&bad_magic), Err(ProtocolError::BadMagic));

        let mut bad_version = good.clone();
        bad_version[4] = VERSION.wrapping_add(1);
        assert_eq!(decode_header(&bad_version), Err(ProtocolError::BadVersion));
    }

    #[test]
    fn a_short_header_is_refused_without_reading_past_it() {
        let layout = poc2();
        let good = header_bytes(&layout, 0, 2);
        for len in 0..HEADER_LEN {
            assert_eq!(
                decode_header(&good[..len]),
                Err(ProtocolError::ShortHeader),
                "length {len}"
            );
        }
    }

    #[test]
    fn non_zero_padding_and_impossible_counts_are_refused() {
        let layout = poc2();
        let good = header_bytes(&layout, 0, 2);

        let mut padded = good.clone();
        padded[HEADER_LEN - 1] = 1; // last byte of the unused fourth slot
        assert_eq!(decode_header(&padded), Err(ProtocolError::BadPadding));

        let mut zero_count = good.clone();
        zero_count[6] = 0;
        assert_eq!(decode_header(&zero_count), Err(ProtocolError::NoStreams));

        let mut too_many = good.clone();
        too_many[6] = (MAX_STREAMS + 1) as u8;
        assert_eq!(decode_header(&too_many), Err(ProtocolError::TooManyStreams));

        let mut huge = good.clone();
        huge[8..10].copy_from_slice(&(MAX_LEDS_PER_STREAM + 1).to_le_bytes());
        assert_eq!(decode_header(&huge), Err(ProtocolError::CountOutOfRange));

        let mut duplicate = good.clone();
        duplicate[10] = duplicate[7]; // second slot on the first slot's channel
        assert_eq!(decode_header(&duplicate), Err(ProtocolError::DuplicateChannel));
    }

    /// A table of malformed headers: never panics, and every accepted header declares exactly
    /// its own payload length, so no frame length can disagree with the bytes it carries. The
    /// "payload is not this long" half is the server's read loop (`server` tests).
    #[test]
    fn no_variable_length_field_exists() {
        let base = header_bytes(&poc2(), 0, 2);
        let mut cases: Vec<Vec<u8>> = Vec::new();
        for len in 0..HEADER_LEN {
            let mut buf = base.clone();
            buf.truncate(len);
            cases.push(buf);
        }
        for byte in [0u8, 1, 2, 0x7f, 0x80, 0xff] {
            for at in 0..HEADER_LEN {
                let mut buf = base.clone();
                buf[at] = byte;
                cases.push(buf);
            }
        }
        cases.push(vec![0xff; HEADER_LEN]);
        cases.push(vec![0x00; HEADER_LEN + 100]);
        for buf in cases {
            match decode_header(&buf) {
                Ok(header) => {
                    let declared = header.layout.frame_len();
                    let sum: usize = header
                        .layout
                        .streams()
                        .iter()
                        .map(|stream| stream.count as usize)
                        .sum();
                    assert_eq!(declared, HEADER_LEN + 3 * sum, "header {buf:?}");
                    assert!(declared <= HEADER_LEN + MAX_PAYLOAD_LEN, "header {buf:?}");
                }
                Err(error) => {
                    // Every refusal has a message and closes the connection; nothing panics.
                    assert!(!error.to_string().is_empty());
                }
            }
        }
    }

    #[test]
    fn flatten_lines_up_each_stream_with_its_frame() {
        let layout = poc2();
        let frames = vec![vec![[1u8, 0, 0]; 30], vec![[0u8, 0, 2]; 33]];
        let flat = flatten(&layout, &frames).unwrap();
        assert_eq!(flat.len(), 63);
        assert_eq!(flat[0], [1, 0, 0]);
        assert_eq!(flat[29], [1, 0, 0]);
        assert_eq!(flat[30], [0, 0, 2]);
        assert_eq!(flat[62], [0, 0, 2]);
        assert_eq!(
            flatten(&layout, &frames[..1]),
            Err(ProtocolError::WrongLength)
        );
        let mut wrong = frames.clone();
        wrong[1].pop();
        assert_eq!(flatten(&layout, &wrong), Err(ProtocolError::WrongLength));
    }

    #[test]
    fn the_profile_order_is_the_wire_order() {
        use crate::server::tests_profile;
        let profile = tests_profile();
        let layout = Layout::from_profile(&profile);
        let names: Vec<&String> = profile.led_streams.keys().collect();
        assert_eq!(names, vec!["BT_LED_DATA", "F_LED_DATA"]);
        assert_eq!(layout.streams()[0].count, 30, "BT_LED_DATA first");
        assert_eq!(layout.streams()[1].count, 33, "F_LED_DATA second");
    }
}
