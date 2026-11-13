//! Length-prefixed Postcard frames for UDS.
//!
//! Wire format: `[u32 LE body_len][postcard bytes]`.
//! Never assume one `write` equals one message.

use crate::{decode, encode, IpcCodecError, IpcMessage};
use thiserror::Error;

/// Maximum Postcard body size (1 MiB). Larger length prefixes are rejected.
pub const MAX_FRAME_BODY: u32 = 1_048_576;

/// Framing / stream decode errors (never panic).
#[derive(Debug, Error)]
pub enum FrameError {
    #[error(transparent)]
    Codec(#[from] IpcCodecError),
    #[error("postcard decode failed: {0}")]
    Decode(#[from] postcard::Error),
    #[error("frame body length {0} exceeds max {MAX_FRAME_BODY}")]
    TooLarge(u32),
}

/// Encode one [`IpcMessage`] as a length-prefixed frame.
pub fn encode_frame(msg: &IpcMessage) -> Result<Vec<u8>, FrameError> {
    let body = encode(msg)?;
    let len = body.len() as u32;
    if len > MAX_FRAME_BODY {
        return Err(FrameError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// If `buf` holds at least one complete frame, decode and remove it.
///
/// Returns `Ok(None)` when more bytes are needed. On [`FrameError::TooLarge`],
/// `buf` is left unchanged — caller should drop the connection. On decode
/// failure after a valid length, the frame bytes are drained so the connection
/// can continue with the next frame.
pub fn try_pop_frame(buf: &mut Vec<u8>) -> Result<Option<IpcMessage>, FrameError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_BODY {
        return Err(FrameError::TooLarge(len));
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        return Ok(None);
    }
    let body = buf[4..total].to_vec();
    match decode::<IpcMessage>(&body) {
        Ok(msg) => {
            buf.drain(..total);
            Ok(Some(msg))
        }
        Err(e) => {
            buf.drain(..total);
            Err(FrameError::Decode(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AssociateRequest, AssociateResponse, LibraryIndexUpdated, NoticeSeverity, PlaybackCommand,
        PlaybackStatus, TapKeyPayload, UserNotice,
    };
    use plippa_core::{PlayableItemId, SessionState, Volume};

    #[test]
    fn encode_pop_round_trip() {
        let msg = IpcMessage::PlaybackCommand(PlaybackCommand::Play);
        let frame = encode_frame(&msg).unwrap();
        let mut buf = frame;
        let got = try_pop_frame(&mut buf).unwrap().unwrap();
        assert_eq!(got, msg);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_then_complete() {
        let msg = IpcMessage::TapKey(TapKeyPayload {
            noubin_key: "a.noubin.x".into(),
            tap_id: 1,
            already_normalised: false,
        });
        let frame = encode_frame(&msg).unwrap();
        let mut buf = frame[..3].to_vec();
        assert!(try_pop_frame(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&frame[3..]);
        assert_eq!(try_pop_frame(&mut buf).unwrap().unwrap(), msg);
    }

    #[test]
    fn garbage_body_errors_without_panic_and_consumes_frame() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xfe, 0xfd, 0xfc, 0xfb]);
        let err = try_pop_frame(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::Decode(_)));
        assert!(buf.is_empty(), "bad frame must be drained");
    }

    #[test]
    fn too_large_length_errors() {
        let mut buf = (MAX_FRAME_BODY + 1).to_le_bytes().to_vec();
        assert!(matches!(
            try_pop_frame(&mut buf).unwrap_err(),
            FrameError::TooLarge(_)
        ));
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn representative_messages_frame() {
        let mut session = SessionState::default();
        session.volume = Volume::new(10);
        let msgs = [
            IpcMessage::TapKey(TapKeyPayload {
                noubin_key: "k".into(),
                tap_id: 2,
                already_normalised: true,
            }),
            IpcMessage::PlaybackStatus(PlaybackStatus::from_session(session)),
            IpcMessage::UserNotice(UserNotice {
                severity: NoticeSeverity::Info,
                code: "c".into(),
                message: "m".into(),
            }),
            IpcMessage::LibraryIndexUpdated(LibraryIndexUpdated {
                generation: 1,
                item_count: 3,
            }),
            IpcMessage::AssociateRequest(AssociateRequest {
                request_id: 1,
                playable_id: PlayableItemId::new(2),
                noudata_relpath: "a.noudata".into(),
                playable_item_title: "t".into(),
                noubin_key: "t.noubin.x".into(),
                keep_existing_key: false,
            }),
            IpcMessage::AssociateResponse(AssociateResponse {
                request_id: 1,
                ok: true,
                noubin_key: Some("t.noubin.x".into()),
                noubin_key_normalised: Some("t.noubin.x".into()),
                error: None,
                ..Default::default()
            }),
            IpcMessage::PlaybackCommand(PlaybackCommand::Unload),
        ];
        for msg in &msgs {
            let mut buf = encode_frame(msg).unwrap();
            assert_eq!(&try_pop_frame(&mut buf).unwrap().unwrap(), msg);
        }
    }
}
