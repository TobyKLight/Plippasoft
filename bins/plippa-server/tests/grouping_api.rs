//! XCHECK: CLASSIFICATION vs GROUPING. A tagless single-file upload chosen as mix / audiobook /
//! podcast is a loose *track* (classification, `kind` = `track`, `noudata_type` = `looseTrack`)
//! that is GROUPED under Mixtapes / Audiobooks / Podcasts (`group`, `?kind=` filter, `/api/kinds`).
//! It must still not count as a release.

use plippa_noubin::{ImportKind, LibraryKind};
use plippa_server::{RecordingIpc, ServerError, ServerService};
use std::fs;

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

#[test]
fn tagless_long_form_uploads_are_grouped_by_profile_but_stay_loose_tracks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    let mut svc = service(&root);
    let mut made = Vec::new();
    for (kind, name, group) in [
        (ImportKind::Mix, "my set.wav", LibraryKind::Mix),
        (ImportKind::Audiobook, "my book.wav", LibraryKind::Audiobook),
        (ImportKind::Podcast, "my cast.wav", LibraryKind::Podcast),
        (ImportKind::Music, "my song.wav", LibraryKind::Track),
        (ImportKind::Other, "my other.wav", LibraryKind::Track),
    ] {
        let s = svc.add_media(kind, name, &wav()).unwrap();
        // The create response names both the classification and the tab.
        assert_eq!(s.kind, "track", "{name}");
        assert_eq!(s.group, group.as_str(), "{name}");
        assert_eq!(s.noudata_type.as_deref(), Some("looseTrack"), "{name}");
        made.push((s, group));
    }

    // `?kind=` (a library tab) filters by grouping; each row keeps kind = track.
    for tab in [
        LibraryKind::Mix,
        LibraryKind::Audiobook,
        LibraryKind::Podcast,
    ] {
        let rows = svc.list_collections(Some(tab)).unwrap();
        assert_eq!(rows.len(), 1, "{tab:?}");
        assert_eq!(
            rows[0].kind,
            LibraryKind::Track,
            "{tab:?}: still a loose track"
        );
        assert_eq!(rows[0].group, tab);
        assert_eq!(rows[0].noudata_type.as_deref(), Some("looseTrack"));
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(json["kind"], "track");
        assert_eq!(json["group"], tab.as_str());
        assert_eq!(json["noudata_type"], "looseTrack");
    }
    let tracks: Vec<String> = svc
        .list_collections(Some(LibraryKind::Track))
        .unwrap()
        .into_iter()
        .map(|c| c.title)
        .collect();
    assert_eq!(
        tracks.len(),
        2,
        "music + other loose tracks only: {tracks:?}"
    );
    assert!(
        tracks
            .iter()
            .all(|t| t.contains("song") || t.contains("other")),
        "{tracks:?}"
    );
    assert!(svc
        .list_collections(Some(LibraryKind::Album))
        .unwrap()
        .is_empty());
    assert_eq!(svc.list_collections(None).unwrap().len(), 5);

    // `/api/kinds` (the tabs) counts by grouping.
    let kinds = svc.collection_kinds().unwrap();
    let n = |k| kinds.iter().find(|c| c.kind == k).unwrap().count;
    assert_eq!(
        (
            n(LibraryKind::Mix),
            n(LibraryKind::Audiobook),
            n(LibraryKind::Podcast),
            n(LibraryKind::Track)
        ),
        (1, 1, 1, 2)
    );
    assert_eq!(n(LibraryKind::Album), 0);

    // Not a release: no per-track association, no online refresh/unite target.
    for (s, _) in made.iter().take(3) {
        let id: u64 = s.id;
        let e = svc
            .begin_associate_item(id, Some(0), false)
            .err()
            .expect("not a link target");
        assert!(matches!(e, ServerError::Invalid(_)), "{e:?}");
        let svc_arc = std::sync::Arc::new(std::sync::Mutex::new(service(&root)));
        let r = plippa_server::unite::refresh::refresh_prepare(
            &svc_arc,
            id,
            &plippa_server::unite::refresh::RefreshRequest::default(),
        );
        assert!(
            r.is_err(),
            "a loose track has no online release metadata to refresh"
        );
    }
}

#[test]
fn an_artist_turns_a_loose_mix_into_a_mix_release_and_the_tab_follows() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    let mut svc = service(&root);
    let s = svc.add_media(ImportKind::Mix, "set.wav", &wav()).unwrap();
    assert_eq!((s.kind.as_str(), s.group.as_str()), ("track", "mix"));
    svc.patch_metadata(
        s.id,
        plippa_server::MetadataPatch {
            primary_artist: Some("DJ Fresh".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let rows = svc.list_collections(Some(LibraryKind::Mix)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].kind, rows[0].group),
        (LibraryKind::Mix, LibraryKind::Mix)
    );
    assert_eq!(rows[0].noudata_type.as_deref(), Some("release"));
}
