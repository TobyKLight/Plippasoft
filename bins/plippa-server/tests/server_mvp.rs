//! MVP_07 acceptance tests for `plippa-server`.

use plippa_ipc_proto::{IpcMessage, PlaybackCommand, NOTICE_NO_MEDIA_STORAGE};
use plippa_noubin::{parse_noudata, LibraryReader, LibraryRoot, PLAYER_DB_DIR};
use plippa_server::{MetadataPatch, RecordingIpc, ServerService};
use std::fs;
use std::path::{Path, PathBuf};

/// A tiny valid WAV carrying ID3v2 tags (artist / album / title as given).
fn tagged_wav(artist: Option<&str>, album: Option<&str>, title: Option<&str>) -> Vec<u8> {
    use lofty::config::WriteOptions;
    use lofty::prelude::*;
    use lofty::tag::{Tag, TagType};
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.wav");
    fs::write(&path, &b).unwrap();
    let mut tagged = lofty::probe::Probe::open(&path).unwrap().read().unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    if let Some(a) = artist {
        tag.set_artist(a.into());
    }
    if let Some(a) = album {
        tag.set_album(a.into());
    }
    if let Some(t) = title {
        tag.set_title(t.into());
    }
    tagged.insert_tag(tag);
    tagged
        .save_to_path(&path, WriteOptions::default())
        .unwrap();
    fs::read(&path).unwrap()
}

fn fixture_library(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-noubin/tests/fixtures/libraries")
        .join(name)
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

fn writable_fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(name);
    copy_dir(&fixture_library(name), &root);
    (tmp, root)
}

fn service_with_ipc(
    root: Option<PathBuf>,
    ipc: RecordingIpc,
) -> ServerService {
    ServerService::start(
        root,
        Box::new(ipc),
        ServerService::default_mock_associate(),
    )
}

#[test]
fn list_returns_fixture_playables() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(root), ipc);
    let list = svc.list_playables().unwrap();
    assert!(
        list.len() >= 2,
        "expected album + track noudata, got {}",
        list.len()
    );
    assert!(list.iter().any(|p| {
        p.title
            .as_deref()
            .is_some_and(|t| t.contains("Grooves For Machines"))
    }));
}

#[test]
fn metadata_patch_write_through_noudata() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);
    let list = svc.list_playables().unwrap();
    let id = list[0].id;
    let rel = list[0].noudata_relpath.clone();

    let detail = svc
        .patch_metadata(
            id,
            MetadataPatch {
                playable_item_title: Some("Renamed For Test".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        detail.playable_item_title.as_deref(),
        Some("Renamed For Test")
    );

    let bytes = fs::read(root.join(&rel)).unwrap();
    let doc = parse_noudata(&bytes).unwrap();
    assert_eq!(doc.playable_item_title.as_deref(), Some("Renamed For Test"));

    // Appears after rescan/reload
    let again = svc.get_playable(id).unwrap();
    assert_eq!(
        again.playable_item_title.as_deref(),
        Some("Renamed For Test")
    );
}

#[test]
fn add_media_creates_index_entry() {
    use plippa_noubin::ImportKind;
    let (_tmp, root) = writable_fixture("empty_library");
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);
    assert!(svc.list_playables().unwrap().is_empty());

    let summary = svc
        .add_media(ImportKind::Music, "demo-track.mp3", b"ID3fake-audio-bytes")
        .unwrap();
    assert!(!summary.associated);
    assert!(
        summary.noudata_relpath.contains("Music/"),
        "expected §4.4 Music/ path, got {}",
        summary.noudata_relpath
    );
    assert!(!summary.noudata_relpath.contains("Imports/"));
    assert!(root.join(&summary.noudata_relpath).is_file());

    let list = svc.list_playables().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, summary.id);
}

#[test]
fn add_media_other_uses_other_root() {
    use plippa_noubin::ImportKind;
    let (_tmp, root) = writable_fixture("empty_library");
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);
    let summary = svc
        .add_media(ImportKind::Other, "field.wav", b"RIFF")
        .unwrap();
    assert!(
        summary.noudata_relpath.contains("Other/"),
        "{}",
        summary.noudata_relpath
    );
    let bytes = fs::read(root.join(&summary.noudata_relpath)).unwrap();
    let doc = parse_noudata(&bytes).unwrap();
    // No tags: a looseTrack in the kind's root folder, never a placeholder artist.
    assert_eq!(doc.noudata_type, Some(plippa_noubin::NoudataType::LooseTrack));
    assert_eq!(doc.playable_item_profile, Some(plippa_noubin::PlayableItemProfile::Other));
    assert!(doc.release_data.is_none());
    assert!(doc.media_list[0].item_credits.is_none());
    assert!(doc.primary_artist_display().is_none());
    assert!(!String::from_utf8_lossy(&bytes).contains("Unknown"));
    assert_eq!(doc.playable_item_title.as_deref(), Some("field"));
}

#[test]
fn add_media_writes_spec_conformant_noudata() {
    use plippa_noubin::ImportKind;
    let (_tmp, root) = writable_fixture("empty_library");
    let mut svc = service_with_ipc(Some(root.clone()), RecordingIpc::new());
    let summary = svc
        .add_media(
            ImportKind::Audiobook,
            "book.wav",
            &tagged_wav(Some("Andy Weir"), Some("Project Hail Mary"), Some("Chapter 1")),
        )
        .unwrap();
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(&summary.noudata_relpath)).unwrap()).unwrap();

    // `_whatAmILookingAt` is the first key; version and type are set.
    let text = fs::read_to_string(root.join(&summary.noudata_relpath)).unwrap();
    assert!(text.lines().nth(1).unwrap().trim().starts_with("\"_whatAmILookingAt\""));
    assert_eq!(raw["noudataVersion"], "0.4.0");
    assert_eq!(raw["noudataType"], "release");
    assert_eq!(raw["playableItemProfile"], "audiobook");

    assert!(raw["playableItemFolderSafeTitle"].is_string());
    let rd = &raw["releaseData"];
    assert!(rd.get("releaseProfile").is_none(), "profile moved to the root in 0.4.0");
    assert!(rd.get("primaryArtists").is_none(), "primaryArtists belongs under credits");
    let credits = &rd["credits"];
    assert_eq!(credits["primaryArtists"][0]["primaryArtistName"], "Weir, Andy");
    assert!(credits["primaryArtists"][0].get("primaryArtistRole").is_none());
    assert!(credits["primaryArtistFolderSafeName"].is_string());
    assert_eq!(credits["contributors"][0]["contributorRole"], "author");

    // …and it re-parses without warnings and is a release, not a playlist.
    let (doc, warnings) =
        plippa_noubin::parse_noudata_with_warnings(text.as_bytes()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!plippa_noubin::is_playlist(&doc, |_| None));
    assert!(doc.type_violations().is_empty());
    assert_eq!(doc.profile(), plippa_noubin::PlayableItemProfile::Audiobook);
}

#[test]
fn tags_with_artist_or_album_give_a_release_and_title_only_stays_loose() {
    use plippa_noubin::{ImportKind, NoudataType};
    let (_tmp, root) = writable_fixture("empty_library");
    let mut svc = service_with_ipc(Some(root.clone()), RecordingIpc::new());
    let read = |rel: &str| parse_noudata(&fs::read(root.join(rel)).unwrap()).unwrap();

    let s = svc
        .add_media(ImportKind::Music, "a.wav", &tagged_wav(Some("Prince"), Some("Purple Rain"), Some("Doves")))
        .unwrap();
    assert!(s.noudata_relpath.starts_with("Music/Prince/Purple Rain/"), "{}", s.noudata_relpath);
    assert_eq!(read(&s.noudata_relpath).noudata_type, Some(NoudataType::Release));

    let s = svc
        .add_media(ImportKind::Music, "b.wav", &tagged_wav(None, None, Some("Only Title")))
        .unwrap();
    let d = read(&s.noudata_relpath);
    assert_eq!(d.noudata_type, Some(NoudataType::LooseTrack));
    assert_eq!(d.playable_item_title.as_deref(), Some("Only Title"));
    assert!(s.noudata_relpath.starts_with("Music/") && s.noudata_relpath.matches('/').count() == 1);

    let s = svc
        .add_media(ImportKind::Mix, "c.wav", b"RIFF")
        .unwrap();
    assert!(s.noudata_relpath.starts_with("Music/Mixes/"), "{}", s.noudata_relpath);
}

#[test]
fn multi_library_merged_list_prefixes_labels() {
    use plippa_noubin::ImportKind;
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("StickA");
    let b = tmp.path().join("StickB");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();

    let mut svc = ServerService::start_with_roots(
        vec![a.clone(), b.clone()],
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let status = svc.status();
    assert_eq!(status.library_roots.len(), 2);
    assert!(status.available);

    let sa = svc
        .add_media(ImportKind::Music, "a.mp3", b"aa")
        .unwrap();
    fs::create_dir_all(b.join("Podcasts/Show")).unwrap();
    fs::write(b.join("Podcasts/Show/ep.mp3"), b"bb").unwrap();
    fs::write(
        b.join("Podcasts/Show/ep.noudata"),
        r#"{
  "noudataVersion": "0.1.0",
  "playableItemTitle": "Ep",
  "playableItemProfile": "podcast", "releaseData": {},
  "mediaList": [{ "mediaTitle": "Ep", "localFilePaths": ["ep.mp3"] }]
}"#,
    )
    .unwrap();
    let _ = svc.lazy_rescan();
    let list = svc.list_playables().unwrap();
    assert!(list.len() >= 2, "{list:?}");
    assert!(list.iter().any(|p| {
        p.paths_summary.starts_with("StickA /") || p.library_label.as_deref() == Some("StickA")
    }));
    assert!(list.iter().any(|p| {
        p.paths_summary.starts_with("StickB /") || p.library_label.as_deref() == Some("StickB")
    }));
    let ids: Vec<_> = list.iter().map(|p| p.id).collect();
    let mut uniq = ids.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(ids.len(), uniq.len());
    assert_ne!(sa.id >> 32, 0);
}

#[test]
fn multi_library_hot_remove_keeps_other() {
    use plippa_noubin::ImportKind;
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("Keep");
    let b = tmp.path().join("Gone");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let mut svc = ServerService::start_with_roots(
        vec![a.clone(), b.clone()],
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let _ = svc.add_media(ImportKind::Other, "x.wav", b"x").unwrap();
    fs::remove_dir_all(&b).unwrap();
    let status = svc.status();
    assert!(status.available);
    assert_eq!(status.library_roots.len(), 1);
    assert!(svc.list_playables().is_ok());
}

#[test]
fn play_emits_load_and_play_ipc() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();
    let ipc_view = ipc.clone();
    let mut svc = service_with_ipc(Some(root), ipc);
    let id = svc.list_playables().unwrap()[0].id;
    ipc_view.clear();

    let t0 = std::time::Instant::now();
    svc.play_playable(id).unwrap();
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(200),
        "play handler must return before engine Playing (took {:?})",
        t0.elapsed()
    );
    let msgs = ipc_view.messages();
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            IpcMessage::PlaybackCommand(PlaybackCommand::LoadAndPlay(pid)) if pid.as_u64() == id
        )),
        "expected LoadAndPlay({id}), got {msgs:?}"
    );
}

#[test]
fn unload_emits_unload_ipc() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();
    let ipc_view = ipc.clone();
    let mut svc = service_with_ipc(Some(root), ipc);
    ipc_view.clear();

    svc.unload_playback().unwrap();

    let msgs = ipc_view.messages();
    assert!(
        msgs.iter()
            .any(|m| matches!(m, IpcMessage::PlaybackCommand(PlaybackCommand::Unload))),
        "expected Unload command, got {msgs:?}"
    );
}

#[test]
fn associate_unlocked_allows_status_during_wait() {
    use plippa_server::associate_playable_unlocked;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    let (_tmp, root) = writable_fixture("grooves_for_machines");
    // Slow live-style associate: wait_associate returns None + supports_live → timeout path.
    // Use mock associate (RecordingIpc is not live associate) so finish is fast; the
    // important part is status() succeeding while another thread holds only IPC briefly.
    let ipc = RecordingIpc::new();
    let svc = Arc::new(Mutex::new(service_with_ipc(Some(root), ipc)));
    let id = svc.lock().unwrap().list_playables().unwrap()[0].id;

    let svc_assoc = Arc::clone(&svc);
    let assoc = thread::spawn(move || associate_playable_unlocked(&svc_assoc, id));

    thread::sleep(Duration::from_millis(5));
    let status = svc.lock().unwrap().status();
    assert!(status.available, "status must work while associate runs");

    let outcome = assoc.join().unwrap().unwrap();
    assert!(outcome.ok);
}

#[test]
fn no_library_root_warning_no_panic() {
    // Explicit empty pin: no discovery (desk machines often have /media sticks).
    let ipc = RecordingIpc::new();
    let ipc_view = ipc.clone();
    let mut svc = ServerService::start_with_roots(
        vec![],
        Box::new(ipc),
        ServerService::default_mock_associate(),
    );
    let status = svc.status();
    assert!(!status.available);
    assert!(status.warning.is_some());

    let notices: Vec<_> = ipc_view
        .messages()
        .into_iter()
        .filter_map(|m| match m {
            IpcMessage::UserNotice(n) => Some(n),
            _ => None,
        })
        .collect();
    assert!(notices.iter().any(|n| n.code == NOTICE_NO_MEDIA_STORAGE));

    // APIs return errors, do not abort.
    assert!(svc.list_playables().is_err());
}

#[test]
fn missing_path_warning_no_panic() {
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(PathBuf::from("/no/such/plippa/library")), ipc);
    let status = svc.status();
    assert!(!status.available);
    assert!(status.warning.is_some());
}

#[test]
fn rescan_picks_up_new_noudata() {
    let (_tmp, root) = writable_fixture("empty_library");
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);
    assert!(svc.list_playables().unwrap().is_empty());

    let nou = root.join("late-drop.noudata");
    fs::write(
        &nou,
        r#"{
  "noudataVersion": "0.1.0",
  "playableItemTitle": "Late Drop",
  "mediaList": [{ "mediaTitle": "X", "localFilePaths": ["late.mp3"] }]
}"#,
    )
    .unwrap();
    fs::write(root.join("late.mp3"), b"x").unwrap();

    let report = svc.lazy_rescan().unwrap();
    assert!(report.ingested >= 1 || report.updated >= 1 || report.scanned_files >= 1);
    let list = svc.list_playables().unwrap();
    assert!(
        list.iter()
            .any(|p| p.title.as_deref() == Some("Late Drop")),
        "{list:?}"
    );
}

#[test]
fn wal_writer_and_engine_style_reader() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);
    let status = svc.status();
    assert_eq!(status.journal_mode.as_deref(), Some("wal"));
    assert!(root.join(PLAYER_DB_DIR).join("library.sqlite").is_file());

    let reader = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    assert_eq!(reader.journal_mode().unwrap(), "wal");
    let count = svc.list_playables().unwrap().len();
    assert!(count >= 1);
    // Concurrent-style read while server remains the writer.
    let _ = reader.list_playables().unwrap();
}

#[test]
fn associate_writes_keys_on_nfc_success() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();
    let ipc_view = ipc.clone();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);

    // Prefer an unassociated item if present; else use first and clear keys first.
    let list = svc.list_playables().unwrap();
    let unassociated = list.iter().find(|p| !p.associated).cloned();
    let target = if let Some(p) = unassociated {
        p
    } else {
        // Clear keys on first item so associate has a clean slate.
        let p = &list[0];
        let path = root.join(&p.noudata_relpath);
        let bytes = fs::read(&path).unwrap();
        let mut doc = parse_noudata(&bytes).unwrap();
        doc.noubin_key = None;
        doc.noubin_key_normalised = None;
        doc.write_to_path(&path).unwrap();
        let _ = svc.lazy_rescan();
        svc.list_playables()
            .unwrap()
            .into_iter()
            .find(|x| x.id == p.id)
            .unwrap()
    };

    ipc_view.clear();
    let outcome = svc.associate_playable(target.id).unwrap();
    assert!(outcome.ok, "{outcome:?}");
    assert!(outcome.noubin_key.is_some());
    assert!(outcome.noubin_key_normalised.is_some());

    let bytes = fs::read(root.join(&target.noudata_relpath)).unwrap();
    let doc = parse_noudata(&bytes).unwrap();
    assert_eq!(doc.noubin_key.as_ref(), outcome.noubin_key.as_ref());
    assert_eq!(
        doc.noubin_key_normalised.as_ref(),
        outcome.noubin_key_normalised.as_ref()
    );

    let msgs = ipc_view.messages();
    assert!(msgs
        .iter()
        .any(|m| matches!(m, IpcMessage::AssociateRequest(_))));
    assert!(msgs
        .iter()
        .any(|m| matches!(m, IpcMessage::AssociateResponse(_))));
}

#[test]
fn associate_nfc_failure_leaves_noudata_unchanged() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();

    // Find or create an unassociated playable.
    let mut prep = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let list = prep.list_playables().unwrap();
    let target = list
        .iter()
        .find(|p| !p.associated)
        .cloned()
        .unwrap_or_else(|| {
            let p = &list[0];
            let path = root.join(&p.noudata_relpath);
            let bytes = fs::read(&path).unwrap();
            let mut doc = parse_noudata(&bytes).unwrap();
            doc.noubin_key = None;
            doc.noubin_key_normalised = None;
            doc.write_to_path(&path).unwrap();
            p.clone()
        });
    drop(prep);

    let before = fs::read(root.join(&target.noudata_relpath)).unwrap();
    let before_doc = parse_noudata(&before).unwrap();

    let mut svc = ServerService::start(
        Some(root.clone()),
        Box::new(ipc),
        ServerService::failing_mock_associate("tag removed"),
    );
    // Rescan may have re-ingested; look up by path.
    let id = svc
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.noudata_relpath == target.noudata_relpath)
        .unwrap()
        .id;

    let outcome = svc.associate_playable(id).unwrap();
    assert!(!outcome.ok);
    assert!(outcome.error.as_deref().unwrap().contains("tag removed"));

    let after = fs::read(root.join(&target.noudata_relpath)).unwrap();
    let after_doc = parse_noudata(&after).unwrap();
    assert_eq!(after_doc.noubin_key, before_doc.noubin_key);
    assert_eq!(
        after_doc.noubin_key_normalised,
        before_doc.noubin_key_normalised
    );
}

#[test]
fn associate_keep_existing_uses_tag_key() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let ipc = RecordingIpc::new();
    let ipc_view = ipc.clone();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);

    let list = svc.list_playables().unwrap();
    let target = list
        .iter()
        .find(|p| !p.associated)
        .cloned()
        .unwrap_or_else(|| list[0].clone());

    // Ensure .noudata starts without keys so we can see the kept key land.
    let path = root.join(&target.noudata_relpath);
    let bytes = fs::read(&path).unwrap();
    let mut doc = parse_noudata(&bytes).unwrap();
    doc.noubin_key = None;
    doc.noubin_key_normalised = None;
    doc.write_to_path(&path).unwrap();
    let _ = svc.lazy_rescan();
    let id = svc
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.noudata_relpath == target.noudata_relpath)
        .unwrap()
        .id;

    ipc_view.clear();
    let outcome = svc.associate_playable_keep_existing(id).unwrap();
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(
        outcome.noubin_key.as_deref(),
        Some("existing.noubin.TAGKEY")
    );

    let bytes = fs::read(root.join(&target.noudata_relpath)).unwrap();
    let doc = parse_noudata(&bytes).unwrap();
    assert_eq!(doc.noubin_key.as_deref(), Some("existing.noubin.TAGKEY"));

    let msgs = ipc_view.messages();
    assert!(msgs.iter().any(|m| matches!(
        m,
        IpcMessage::AssociateRequest(r) if r.keep_existing_key
    )));
}

#[test]
fn associate_keep_existing_rejects_blank_normalised_key() {
    let (_tmp, root) = writable_fixture("grooves_for_machines");
    let mut svc = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::blank_key_mock_associate(),
    );
    let list = svc.list_playables().unwrap();
    let target = &list[0];
    let path = root.join(&target.noudata_relpath);
    let before = fs::read(&path).unwrap();
    let before_doc = parse_noudata(&before).unwrap();

    let outcome = svc.associate_playable_keep_existing(target.id).unwrap();
    assert!(!outcome.ok, "{outcome:?}");
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("blank"),
        "{outcome:?}"
    );

    let after = fs::read(&path).unwrap();
    let after_doc = parse_noudata(&after).unwrap();
    assert_eq!(after_doc.noubin_key, before_doc.noubin_key);
    assert_eq!(
        after_doc.noubin_key_normalised,
        before_doc.noubin_key_normalised
    );
}

#[test]
fn library_index_updated_emitted_on_scan() {
    let (_tmp, root) = writable_fixture("empty_library");
    let ipc = RecordingIpc::new();
    let ipc_view = ipc.clone();
    let _svc = service_with_ipc(Some(root), ipc);
    assert!(ipc_view
        .messages()
        .iter()
        .any(|m| matches!(m, IpcMessage::LibraryIndexUpdated(_))));
}

#[test]
fn hot_unplug_status_warns_without_abort() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    let ipc = RecordingIpc::new();
    let mut svc = service_with_ipc(Some(root.clone()), ipc);
    assert!(svc.status().available);

    // Simulate unplug by removing the directory.
    fs::remove_dir_all(&root).unwrap();
    let status = svc.status();
    assert!(!status.available);
    assert!(status.warning.is_some());
    // Further API calls must not panic.
    let _ = svc.list_playables();
}
