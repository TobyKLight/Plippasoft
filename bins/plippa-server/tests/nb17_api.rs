//! NB_17: loose tracks, the release upgrade, and typed writers through the service API.

use plippa_noubin::{parse_noudata, ImportKind, LibraryKind, NoudataType, PlayableItemProfile};
use plippa_server::{MetadataPatch, RecordingIpc, ServerError, ServerService};
use std::fs;
use std::path::PathBuf;

fn service(root: &std::path::Path) -> ServerService {
    ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )
}

fn wav() -> Vec<u8> {
    let data = vec![0u8; 800];
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + data.len() as u32).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(8000u32.to_le_bytes());
    b.extend(16000u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend((data.len() as u32).to_le_bytes());
    b.extend(&data);
    b
}

fn read(root: &std::path::Path, rel: &str) -> plippa_noubin::NoudataDocument {
    parse_noudata(&fs::read(root.join(rel)).unwrap()).unwrap()
}

fn new_root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    (tmp, root)
}

#[test]
fn setting_an_artist_upgrades_a_loose_track_to_a_release_in_place() {
    let (_t, root) = new_root();
    let mut svc = service(&root);
    let s = svc
        .add_media_with_cues(
            ImportKind::Mix,
            "my set.wav",
            &wav(),
            Some("0:00 Intro\n0:05 Two\n"),
        )
        .unwrap()
        .summary;
    let before = read(&root, &s.noudata_relpath);
    assert_eq!(before.noudata_type, Some(NoudataType::LooseTrack));
    assert_eq!(before.media_list[0].cue_lists.len(), 1);

    // A key bound earlier must survive the upgrade.
    let mut doc = before.clone();
    doc.set_noubin_keys("https://x.example/n/", "x.example/n/");
    doc.write_to_path(&root.join(&s.noudata_relpath)).unwrap();
    let _ = svc.lazy_rescan();

    let d = svc
        .patch_metadata(
            s.id,
            MetadataPatch {
                primary_artist: Some("  DJ\u{202E} Fresh ".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(d.noudata_type, "release");
    assert_eq!(d.primary_artist.as_deref(), Some("DJ Fresh"));

    let after = read(&root, &s.noudata_relpath);
    assert_eq!(after.noudata_type, Some(NoudataType::Release));
    assert_eq!(after.profile(), PlayableItemProfile::Mix, "profile kept");
    assert!(after.release_data.is_some());
    assert_eq!(after.noubin_key.as_deref(), Some("https://x.example/n/"));
    assert_eq!(after.media_list[0].cue_lists.len(), 1, "cues kept");
    assert!(after.type_violations().is_empty());
    // File did not move; the index now lists it as a mix release.
    let recs = svc.list_playables().unwrap();
    assert_eq!(recs.len(), 1);
    let cols = svc.list_collections(Some(LibraryKind::Mix)).unwrap();
    assert_eq!(cols.len(), 1, "now a mix release, no longer a track");
}

#[test]
fn artist_edit_is_refused_for_playlists_and_link_tracks_and_blank_names() {
    let (_t, root) = new_root();
    let mut svc = service(&root);
    let s = svc.add_media(ImportKind::Music, "a.wav", &wav()).unwrap();
    // blank after sanitising
    let e = svc
        .patch_metadata(
            s.id.clone(),
            MetadataPatch {
                primary_artist: Some("\u{200B}".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(e, ServerError::Invalid(_)));
    // a link track
    let mut doc = read(&root, &s.noudata_relpath);
    doc.noudata_type = Some(NoudataType::LinkTrack);
    doc.refer_to_release_noudata = Some("_none.noudata".into());
    doc.write_to_path(&root.join(&s.noudata_relpath)).unwrap();
    let _ = svc.lazy_rescan();
    let e = svc
        .patch_metadata(
            s.id,
            MetadataPatch {
                primary_artist: Some("X".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(e, ServerError::Invalid(_)), "{e:?}");
}

#[test]
fn orphan_without_tags_gets_a_loose_track_noudata() {
    let (_t, root) = new_root();
    fs::create_dir_all(root.join("Loose")).unwrap();
    fs::write(root.join("Loose/song.wav"), wav()).unwrap();
    let mut svc = service(&root);
    svc.full_scan().unwrap();
    let orphan = svc
        .list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.is_orphan)
        .expect("orphan")
        .id;
    let made = svc
        .create_noudata_for_orphan(orphan, ImportKind::Audiobook, None)
        .unwrap();
    let doc = read(&root, "Loose/song.noudata");
    assert_eq!(doc.noudata_type, Some(NoudataType::LooseTrack));
    assert_eq!(doc.profile(), PlayableItemProfile::Audiobook);
    assert!(doc.release_data.is_none() && doc.primary_artist_display().is_none());
    assert_eq!(made.title.as_deref(), Some("song"));
}
