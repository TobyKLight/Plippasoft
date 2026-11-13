//! NB_04: engine resume requests are written through to `.noudata`.

use plippa_core::PlayableItemId;
use plippa_ipc_proto::ResumeWriteback;
use plippa_noubin::{
    encode_playable_id, parse_noudata, LibraryKind, LibraryReader, LibraryRoot, NoubinTimestamp,
};
use plippa_server::{RecordingIpc, ServerService};
use std::fs;
use std::path::{Path, PathBuf};

const BOOK: &str = r#"{
  "_whatAmILookingAt": "x",
  "noudataVersion": "0.1.0",
  "playableItemTitle": "Book",
  "playableItemProfile": "audiobook", "releaseData": {},
  "localPlaybackData": {"shuffle": false, "userNotes": {"userComment": "keep me"}},
  "mediaList": [
    {"mediaTitle": "One", "localFilePaths": ["1.wav"], "itemLocalPlaybackData": {"repeatItem": true}},
    {"mediaTitle": "Two", "localFilePaths": ["2.wav"]},
    {"mediaTitle": "Three", "localFilePaths": ["3.wav"]}
  ]
}"#;

fn setup() -> (
    tempfile::TempDir,
    PathBuf,
    RecordingIpc,
    ServerService,
    PlayableItemId,
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(root.join("Audiobooks/Book")).unwrap();
    for n in 1..=3 {
        fs::write(root.join(format!("Audiobooks/Book/{n}.wav")), b"RIFF").unwrap();
    }
    fs::write(root.join("Audiobooks/Book/_book.noudata"), BOOK).unwrap();
    let ipc = RecordingIpc::new();
    let svc = ServerService::start(
        Some(root.clone()),
        Box::new(ipc.clone()),
        ServerService::default_mock_associate(),
    );
    let reader = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    let rec = reader
        .list_by_kind(LibraryKind::Audiobook)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let id = PlayableItemId::new(encode_playable_id(&root, rec.id));
    (tmp, root, ipc, svc, id)
}

fn doc(root: &Path) -> plippa_noubin::NoudataDocument {
    parse_noudata(&fs::read(root.join("Audiobooks/Book/_book.noudata")).unwrap()).unwrap()
}

#[test]
fn resume_point_is_written_moved_and_cleared() {
    let (_tmp, root, ipc, mut svc, id) = setup();

    ipc.inject_resume_writeback(ResumeWriteback {
        playable_id: id,
        resume_at_item: Some(2),
        resume_from_ms: Some(61_500),
    });
    assert_eq!(svc.apply_resume_writebacks(), 1);
    let d = doc(&root);
    assert_eq!(
        d.local_playback_data.as_ref().unwrap().resume_at_item,
        Some(2)
    );
    assert_eq!(
        d.media_list[1]
            .item_local_playback_data
            .as_ref()
            .unwrap()
            .resume_from_time,
        Some(NoubinTimestamp::from_millis(61_500))
    );
    // Written in spec form and everything else preserved.
    let text = fs::read_to_string(root.join("Audiobooks/Book/_book.noudata")).unwrap();
    assert!(text.contains("\"resumeAtItem\": 2"), "{text}");
    assert!(
        text.contains("\"resumeFromTime\": \"00:01:01.5\""),
        "{text}"
    );
    assert!(text.contains("keep me"));
    assert!(
        !text.contains("shuffle"),
        "Noubin v0.4.1 retired key echoed back: {text}"
    );
    assert_eq!(
        d.media_list[0]
            .item_local_playback_data
            .as_ref()
            .unwrap()
            .repeat_item,
        Some(true)
    );

    // Same request again: nothing to change.
    ipc.inject_resume_writeback(ResumeWriteback {
        playable_id: id,
        resume_at_item: Some(2),
        resume_from_ms: Some(61_500),
    });
    assert_eq!(svc.apply_resume_writebacks(), 0);

    // Moving to item 3 clears item 2's time (one resume point per playable).
    ipc.inject_resume_writeback(ResumeWriteback {
        playable_id: id,
        resume_at_item: Some(3),
        resume_from_ms: Some(1_000),
    });
    assert_eq!(svc.apply_resume_writebacks(), 1);
    let d = doc(&root);
    assert_eq!(
        d.local_playback_data.as_ref().unwrap().resume_at_item,
        Some(3)
    );
    assert!(
        d.media_list[1].item_local_playback_data.is_none(),
        "empty container removed"
    );
    assert_eq!(
        d.media_list[2]
            .item_local_playback_data
            .as_ref()
            .unwrap()
            .resume_from_time,
        Some(NoubinTimestamp::from_millis(1_000))
    );

    // Completion clears both, keeps unrelated data.
    ipc.inject_resume_writeback(ResumeWriteback {
        playable_id: id,
        resume_at_item: None,
        resume_from_ms: None,
    });
    assert_eq!(svc.apply_resume_writebacks(), 1);
    let d = doc(&root);
    let lp = d.local_playback_data.as_ref().unwrap();
    assert_eq!(lp.resume_at_item, None);
    assert!(lp.user_notes.is_some());
    assert!(d.media_list.iter().all(|m| m
        .item_local_playback_data
        .as_ref()
        .is_none_or(|i| i.resume_from_time.is_none())));
    assert_eq!(
        d.media_list[0]
            .item_local_playback_data
            .as_ref()
            .unwrap()
            .repeat_item,
        Some(true)
    );
}

#[test]
fn a_resume_writeback_removes_a_legacy_shuffle_key() {
    let (_tmp, root, ipc, mut svc, id) = setup();
    let before = fs::read_to_string(root.join("Audiobooks/Book/_book.noudata")).unwrap();
    assert!(
        before.contains("\"shuffle\": false"),
        "fixture is legacy input: {before}"
    );

    ipc.inject_resume_writeback(ResumeWriteback {
        playable_id: id,
        resume_at_item: Some(2),
        resume_from_ms: Some(61_500),
    });
    assert_eq!(svc.apply_resume_writebacks(), 1);

    // Noubin v0.4.1: the retired player setting is dropped by the read-modify-write, not
    // echoed through the serde `extra` map.
    let text = fs::read_to_string(root.join("Audiobooks/Book/_book.noudata")).unwrap();
    assert!(!text.contains("shuffle"), "retired key echoed back: {text}");
    // Everything else is preserved, including the untouched per-item `repeatItem`.
    assert!(text.contains("\"resumeAtItem\": 2"), "{text}");
    assert!(text.contains("keep me"), "{text}");
    assert_eq!(
        doc(&root).media_list[0]
            .item_local_playback_data
            .as_ref()
            .unwrap()
            .repeat_item,
        Some(true)
    );
}

#[test]
fn invalid_requests_are_ignored_not_fatal() {
    let (_tmp, root, ipc, mut svc, id) = setup();
    let before = fs::read(root.join("Audiobooks/Book/_book.noudata")).unwrap();
    for wb in [
        // item beyond the list -> treated as a clear (nothing to clear)
        ResumeWriteback {
            playable_id: id,
            resume_at_item: Some(9),
            resume_from_ms: Some(5),
        },
        // unknown playable id
        ResumeWriteback {
            playable_id: PlayableItemId::new(encode_playable_id(&root, 9_999)),
            resume_at_item: Some(1),
            resume_from_ms: Some(5),
        },
    ] {
        ipc.inject_resume_writeback(wb);
    }
    assert_eq!(svc.apply_resume_writebacks(), 0);
    assert_eq!(
        fs::read(root.join("Audiobooks/Book/_book.noudata")).unwrap(),
        before
    );
}
