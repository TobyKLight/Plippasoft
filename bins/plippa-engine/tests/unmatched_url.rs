//! NB_08: an unmatched URL tap raises a structured notice; local hits and plain-text keys
//! never do. The engine has no HTTP client, so "no network on a local hit" is structural;
//! this test pins the notice contract that the server/UI act on.

use plippa_core::Capabilities;
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, TapKeyPayload, TapNdefPayload, UnmatchedNoubin};
use plippa_noubin::{encode_uri_record, normalise_noubin_key, LibraryRoot, LibraryWriter};
use std::fs;

fn library_with(key: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    fs::write(root.join("a.wav"), b"RIFF").unwrap();
    let noudata = format!(
        r#"{{"noubinKey": "{key}", "noubinKeyNormalised": "{}", "playableItemTitle": "Have",
            "mediaList": [{{"localFilePaths": ["a.wav"]}}]}}"#,
        normalise_noubin_key(key)
    );
    fs::write(root.join("_have.noudata"), noudata).unwrap();
    LibraryWriter::open(&LibraryRoot::new(&root))
        .unwrap()
        .full_scan()
        .unwrap();
    (tmp, root)
}

fn unmatched(events: &[EngineEvent]) -> Vec<UnmatchedNoubin> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::UnmatchedNoubin(u) => Some(u.clone()),
            _ => None,
        })
        .collect()
}

fn tap(engine: &mut Engine, key: &str, id: u64) {
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: id,
        already_normalised: false,
    }));
}

#[test]
fn unmatched_url_tap_emits_notice_with_raw_and_normalised_key() {
    let (_t, root) = library_with("https://have.example/noubin/x/");
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    tap(&mut engine, "https://Other.example/noubin/album?ref=nfc", 1);
    let u = unmatched(&engine.take_events());
    assert_eq!(u.len(), 1);
    assert_eq!(
        u[0].noubin_key,
        "https://Other.example/noubin/album?ref=nfc"
    );
    assert_eq!(u[0].normalised_key, "other.example/noubin/album/");
    tap(&mut engine, "https://other.example/second/", 2);
    assert_eq!(unmatched(&engine.take_events())[0].seq, 2);
}

#[test]
fn unmatched_url_from_ndef_tag_uses_the_extracted_key() {
    let (_t, root) = library_with("https://have.example/noubin/x/");
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::TapNdef(TapNdefPayload {
        ndef_message: encode_uri_record("https://artist.example/noubin/album/").unwrap(),
        tap_id: 1,
    }));
    let u = unmatched(&engine.take_events());
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].noubin_key, "https://artist.example/noubin/album/");
}

#[test]
fn plain_text_and_local_hits_never_raise_the_notice() {
    let (_t, root) = library_with("https://have.example/noubin/x/");
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    // Private Noubin (plain text): keeps today's "not recognised" behaviour only.
    tap(&mut engine, "some private key", 1);
    assert!(unmatched(&engine.take_events()).is_empty());
    // Pre-normalised keys carry no scheme, so they are never offered for the web.
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: "other.example/x/".into(),
        tap_id: 2,
        already_normalised: true,
    }));
    assert!(unmatched(&engine.take_events()).is_empty());
    // A URL that matches the local library: no notice at all.
    tap(&mut engine, "https://have.example/noubin/x/", 3);
    assert!(unmatched(&engine.take_events()).is_empty());
}
