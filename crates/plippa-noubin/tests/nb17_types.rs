//! NB_17: v0.4.0 `noudataType` / `playableItemProfile` in the index (kind, profile column,
//! loose tracks, link tracks, declared-vs-inferred, invalid combinations).

use plippa_noubin::{LibraryKind, LibraryRoot, LibraryWriter};
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn scan(root: &Path) -> Vec<plippa_noubin::PlayableRecord> {
    let mut w = LibraryWriter::open(&LibraryRoot::new(root)).unwrap();
    w.full_scan().unwrap();
    w.list_playables().unwrap()
}

fn find<'a>(
    recs: &'a [plippa_noubin::PlayableRecord],
    rel: &str,
) -> &'a plippa_noubin::PlayableRecord {
    recs.iter()
        .find(|r| r.noudata_relpath == rel)
        .unwrap_or_else(|| panic!("{rel} not indexed: {recs:?}"))
}

const ITEM: &str = r#"{"localFilePaths":["a.mp3"]}"#;

#[test]
fn every_type_is_indexed_with_the_right_kind_and_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for f in ["a.mp3", "b.mp3", "c.mp3", "d.mp3", "e.mp3"] {
        write(root, &format!("Music/{f}"), "");
    }
    write(
        root,
        "Music/r/_rel.noudata",
        &format!(
            r#"{{"noudataType":"release","playableItemTitle":"Rel","playableItemProfile":"audiobook",
            "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"Au"}}]}}}},
            "mediaList":[{{"localFilePaths":["../a.mp3"]}}]}}"#
        ),
    );
    write(
        root,
        "Music/r/link.noudata",
        r#"{"noudataType":"linkTrack","playableItemTitle":"Link",
        "referToReleaseNoudata":"_rel.noudata","mediaList":[{"localFilePaths":["../a.mp3"]}]}"#,
    );
    write(
        root,
        "Music/loose.noudata",
        r#"{"noudataType":"looseTrack","playableItemTitle":"Loose One",
        "playableItemProfile":"mix","mediaList":[{"localFilePaths":["b.mp3"]}]}"#,
    );
    write(
        root,
        "Playlists/pl.noudata",
        r#"{"noudataType":"userPlaylist","playableItemTitle":"PL",
        "playableItemProfile":"mix","mediaList":[{"localFilePaths":["../Music/c.mp3"]}]}"#,
    );
    write(
        root,
        "Music/dyn.noudata",
        r#"{"noudataType":"userPlaylist","playableItemTitle":"Dyn",
        "localPlaybackData":{"searchString":"x"},"mediaList":[]}"#,
    );
    let recs = scan(root);

    let rel = find(&recs, "Music/r/_rel.noudata");
    assert_eq!(rel.kind, LibraryKind::Audiobook);
    assert_eq!(rel.release_profile.as_deref(), Some("audiobook"));

    let link = find(&recs, "Music/r/link.noudata");
    assert_eq!(link.kind, LibraryKind::Track);
    assert_eq!(
        link.primary_artist.as_deref(),
        Some("Au"),
        "artist from the release"
    );
    assert_eq!(
        link.release_profile.as_deref(),
        Some("audiobook"),
        "profile inherited"
    );

    let loose = find(&recs, "Music/loose.noudata");
    assert_eq!(loose.kind, LibraryKind::Track);
    assert_eq!(loose.release_profile.as_deref(), Some("mix"));
    assert_eq!(loose.primary_artist, None);
    assert_eq!(loose.title.as_deref(), Some("Loose One"));

    let pl = find(&recs, "Playlists/pl.noudata");
    assert_eq!(pl.kind, LibraryKind::Playlist);
    assert_eq!(pl.release_profile, None, "profile is ignored on a playlist");
    let dynp = find(&recs, "Music/dyn.noudata");
    assert_eq!((dynp.kind, dynp.is_dynamic), (LibraryKind::Playlist, true));
}

#[test]
fn loose_track_item_title_comes_from_playable_item_title() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "a.mp3", "");
    write(
        root,
        "a.noudata",
        &format!(
            r#"{{"noudataType":"looseTrack","playableItemTitle":"Nice Title","mediaList":[{ITEM}]}}"#
        ),
    );
    let mut w = LibraryWriter::open(&LibraryRoot::new(root)).unwrap();
    w.full_scan().unwrap();
    let r = w.list_playables().unwrap();
    let items = w.items_of(find(&r, "a.noudata").id).unwrap();
    assert_eq!(items[0].title, "Nice Title");
}

#[test]
fn invalid_declared_type_is_inferred_and_unknown_type_is_inferred() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "a.mp3", "");
    write(root, "b.mp3", "");
    // release without releaseData ⇒ declared ignored ⇒ playlist
    write(
        root,
        "x.noudata",
        &format!(r#"{{"noudataType":"release","playableItemTitle":"X","mediaList":[{ITEM}]}}"#),
    );
    // unknown type + releaseData ⇒ inferred release
    write(
        root,
        "y.noudata",
        r#"{"noudataType":"hologram","playableItemTitle":"Y","releaseData":{},
        "mediaList":[{"localFilePaths":["b.mp3"]}]}"#,
    );
    let recs = scan(root);
    assert_eq!(find(&recs, "x.noudata").kind, LibraryKind::Playlist);
    assert_eq!(find(&recs, "y.noudata").kind, LibraryKind::Album);
}

#[test]
fn declared_link_track_survives_a_missing_release_but_inferred_one_becomes_a_playlist() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "a.mp3", "");
    write(root, "b.mp3", "");
    write(
        root,
        "decl.noudata",
        &format!(
            r#"{{"noudataType":"linkTrack","playableItemTitle":"D","referToReleaseNoudata":"_gone.noudata","mediaList":[{ITEM}]}}"#
        ),
    );
    write(
        root,
        "infer.noudata",
        r#"{"playableItemTitle":"I","referToReleaseNoudata":"_gone.noudata",
        "mediaList":[{"localFilePaths":["b.mp3"]}]}"#,
    );
    let recs = scan(root);
    assert_eq!(find(&recs, "decl.noudata").kind, LibraryKind::Track);
    assert_eq!(find(&recs, "infer.noudata").kind, LibraryKind::Playlist);
}

#[test]
fn loose_track_with_forbidden_release_data_keeps_declared_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "a.mp3", "");
    write(
        root,
        "a.noudata",
        &format!(
            r#"{{"noudataType":"looseTrack","playableItemTitle":"L","releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"Z"}}]}}}},"mediaList":[{ITEM}]}}"#
        ),
    );
    let recs = scan(root);
    assert_eq!(find(&recs, "a.noudata").kind, LibraryKind::Track);
}

// ---- XCHECK: classification vs grouping ---------------------------------------------------

/// A loose track whose profile is mix/audiobook/podcast is GROUPED under that tab, but it is
/// still classified as a loose *track* (kind `track`, type `looseTrack`): it is not a release.
/// Link tracks, `music`/`other` loose tracks and orphans stay under Tracks.
#[test]
fn loose_long_form_tracks_are_grouped_by_profile_but_classified_as_tracks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for f in ["a", "b", "c", "d", "e", "f", "g"] {
        write(root, &format!("Music/{f}.mp3"), "");
    }
    write(
        root,
        "Music/r/_rel.noudata",
        r#"{"noudataType":"release","playableItemTitle":"Rel","playableItemProfile":"mix",
            "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Dj"}]}},
            "mediaList":[{"localFilePaths":["../a.mp3"]}]}"#,
    );
    // link track of a MIX release: stays a Track (it is a pointer, not the long-form item)
    write(
        root,
        "Music/r/link.noudata",
        r#"{"noudataType":"linkTrack","playableItemTitle":"Link","referToReleaseNoudata":"_rel.noudata",
            "mediaList":[{"localFilePaths":["../a.mp3"]}]}"#,
    );
    for (f, title, profile) in [
        ("b", "Loose Mix", "mix"),
        ("c", "Loose Book", "audiobook"),
        ("d", "Loose Cast", "podcast"),
        ("e", "Loose Song", "music"),
        ("f", "Loose Other", "other"),
    ] {
        write(
            root,
            &format!("Music/{f}.noudata"),
            &format!(
                r#"{{"noudataType":"looseTrack","playableItemTitle":"{title}",
                   "playableItemProfile":"{profile}","mediaList":[{{"localFilePaths":["{f}.mp3"]}}]}}"#
            ),
        );
    }
    let mut w = LibraryWriter::open(&LibraryRoot::new(root)).unwrap();
    w.full_scan().unwrap();
    let recs = w.list_playables().unwrap();
    let by = |t: &str| recs.iter().find(|r| r.title.as_deref() == Some(t)).unwrap();

    for (title, group) in [
        ("Loose Mix", LibraryKind::Mix),
        ("Loose Book", LibraryKind::Audiobook),
        ("Loose Cast", LibraryKind::Podcast),
        ("Loose Song", LibraryKind::Track),
        ("Loose Other", LibraryKind::Track),
        ("Link", LibraryKind::Track),
        ("Rel", LibraryKind::Mix),
    ] {
        let r = by(title);
        assert_eq!(r.group, group, "{title}");
        if title.starts_with("Loose") || title == "Link" {
            // classification is unchanged: never a release
            assert_eq!(r.kind, LibraryKind::Track, "{title}");
            assert!(!r.kind.is_release(), "{title}");
        }
    }
    assert_eq!(by("Loose Mix").noudata_type.as_deref(), Some("looseTrack"));
    assert_eq!(by("Link").noudata_type.as_deref(), Some("linkTrack"));
    assert_eq!(by("Rel").noudata_type.as_deref(), Some("release"));

    // group listings/counts drive the tabs; kind listings stay the classification
    let titles = |v: Vec<plippa_noubin::PlayableRecord>| -> Vec<String> {
        v.into_iter().filter_map(|r| r.title).collect()
    };
    assert_eq!(titles(w.list_by_group(LibraryKind::Mix).unwrap()), ["Loose Mix", "Rel"]);
    assert_eq!(titles(w.list_by_group(LibraryKind::Audiobook).unwrap()), ["Loose Book"]);
    assert_eq!(titles(w.list_by_group(LibraryKind::Podcast).unwrap()), ["Loose Cast"]);
    assert_eq!(
        titles(w.list_by_group(LibraryKind::Track).unwrap()),
        ["g", "Link", "Loose Other", "Loose Song"],
        "the orphan file `g.mp3` is a Track too"
    );
    assert!(w.list_by_group(LibraryKind::Album).unwrap().is_empty());
    assert_eq!(
        titles(w.list_by_kind(LibraryKind::Track).unwrap()),
        ["g", "Link", "Loose Book", "Loose Cast", "Loose Mix", "Loose Other", "Loose Song"],
        "kind = classification: every loose and link track"
    );
    let n = |v: Vec<(LibraryKind, u64)>, k| v.iter().find(|(x, _)| *x == k).map_or(0, |(_, n)| *n);
    let groups = w.group_counts().unwrap();
    assert_eq!(n(groups.clone(), LibraryKind::Mix), 2);
    assert_eq!(n(groups.clone(), LibraryKind::Audiobook), 1);
    assert_eq!(n(groups.clone(), LibraryKind::Podcast), 1);
    assert_eq!(n(groups, LibraryKind::Track), 4);
    let kinds = w.kind_counts().unwrap();
    assert_eq!(n(kinds.clone(), LibraryKind::Mix), 1, "only the release is a Mix");
    assert_eq!(n(kinds, LibraryKind::Track), 7);
}
