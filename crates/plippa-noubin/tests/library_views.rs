//! Library VIEWS (the tabs of the web portal and the device UI), per
//! `devPlippaSW/libraryUIHeirarchy.md`: Tracks, Albums, Playlists, Podcasts, Audiobooks, Mixes.

use plippa_noubin::{view_is_item_level, LibraryKind, LibraryReader, LibraryRoot, LibraryWriter};
use std::fs;
use std::path::Path;

fn put(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

fn items(n: usize, prefix: &str) -> String {
    (1..=n)
        .map(|i| format!(r#"{{"mediaTitle":"{prefix} {i}","localFilePaths":["{prefix}{i}.mp3"]}}"#))
        .collect::<Vec<_>>()
        .join(",")
}

fn library() -> (tempfile::TempDir, LibraryReader) {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path();
    // Music release with 3 tracks.
    put(r, "Music/Band/Rec/_Rec.noudata", &format!(
        r#"{{"noudataType":"release","playableItemTitle":"Record","playableItemProfile":"music",
            "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"The Band"}}]}}}},
            "mediaList":[{}]}}"#, items(3, "Song")));
    // Legacy release (pre-0.4.0): the profile is only in releaseData.releaseProfile.
    put(r, "Music/Old/Legacy/_Legacy.noudata",
        r#"{"noudataType":"release","playableItemTitle":"Legacy Hit",
            "releaseData":{"releaseProfile":"music","primaryArtists":[{"primaryArtistName":"Old"}]},
            "mediaList":[{"localFilePaths":["x.mp3"]}]}"#);
    // Release with profile other: a Track, but not an Album.
    put(r, "Other/_Field.noudata",
        r#"{"noudataType":"release","playableItemTitle":"Field recording","playableItemProfile":"other",
            "releaseData":{},"mediaList":[{"localFilePaths":["field.wav"]}]}"#);
    // Loose tracks.
    put(r, "Loose/_loose.noudata",
        r#"{"noudataType":"looseTrack","playableItemTitle":"Loose music","playableItemProfile":"music",
            "mediaList":[{"localFilePaths":["loose.mp3"]}]}"#);
    put(r, "Loose/_lmix.noudata",
        r#"{"noudataType":"looseTrack","playableItemTitle":"Loose mix","playableItemProfile":"mix",
            "mediaList":[{"localFilePaths":["lmix.mp3"]}]}"#);
    put(r, "Loose/_pod.noudata",
        r#"{"noudataType":"looseTrack","playableItemTitle":"An episode","playableItemProfile":"podcast",
            "mediaList":[{"localFilePaths":["pod.mp3"]}]}"#);
    // Release mix, audiobook (2 files), podcast release.
    put(r, "Mixes/_RMix.noudata",
        r#"{"noudataType":"release","playableItemTitle":"Release mix","playableItemProfile":"mix",
            "releaseData":{},"mediaList":[{"localFilePaths":["rmix.mp3"]}]}"#);
    put(r, "Books/_Book.noudata", &format!(
        r#"{{"noudataType":"release","playableItemTitle":"The Book","playableItemProfile":"audiobook",
            "releaseData":{{}},"mediaList":[{}]}}"#, items(2, "Ch")));
    put(r, "Pods/_Show.noudata", &format!(
        r#"{{"noudataType":"release","playableItemTitle":"The Show","playableItemProfile":"podcast",
            "releaseData":{{}},"mediaList":[{}]}}"#, items(2, "Ep")));
    // Playlists: declared, with a profile (regardless of profile), and a legacy single-item
    // file with neither releaseData nor a reference (inferred playlist, §4.9).
    put(r, "Playlists/_PL.noudata",
        r#"{"noudataType":"userPlaylist","playableItemTitle":"My playlist","playableItemProfile":"podcast",
            "mediaList":[{"localFilePaths":["../Loose/loose.mp3"]}]}"#);
    put(r, "Imports/legacy.noudata",
        r#"{"playableItemTitle":"Legacy import","mediaList":[{"localFilePaths":["legacy.mp3"]}]}"#);
    // Link track: shown through its release, never a second Track.
    put(r, "Music/Band/Rec/02.noudata",
        r#"{"noudataType":"linkTrack","playableItemTitle":"Song 2","referToReleaseNoudata":"_Rec.noudata",
            "mediaList":[{"localFilePaths":["Song2.mp3"]}]}"#);
    // Orphan media with no .noudata.
    put(r, "stray.mp3", "");
    for f in ["Music/Band/Rec/Song1.mp3", "Music/Band/Rec/Song2.mp3", "Music/Band/Rec/Song3.mp3",
              "Music/Old/Legacy/x.mp3", "Other/field.wav", "Loose/loose.mp3", "Loose/lmix.mp3",
              "Loose/pod.mp3", "Mixes/rmix.mp3", "Books/Ch1.mp3", "Books/Ch2.mp3",
              "Pods/Ep1.mp3", "Pods/Ep2.mp3", "Imports/legacy.mp3"] {
        put(r, f, "");
    }
    LibraryWriter::open(&LibraryRoot::new(r)).unwrap().full_scan().unwrap();
    let reader = LibraryReader::open(&LibraryRoot::new(r)).unwrap();
    (tmp, reader)
}

fn titles(r: &LibraryReader, v: LibraryKind) -> Vec<String> {
    r.list_by_view(v).unwrap().into_iter().map(|p| p.title.unwrap_or_default()).collect()
}

fn track_titles(r: &LibraryReader, v: LibraryKind) -> Vec<String> {
    r.list_view_tracks(v).unwrap().into_iter().map(|t| t.title).collect()
}

#[test]
fn albums_are_music_releases_only_and_honour_the_legacy_profile_field() {
    let (_t, r) = library();
    assert_eq!(titles(&r, LibraryKind::Album), ["Legacy Hit", "Record"]);
}

#[test]
fn tracks_are_every_music_or_other_track_of_releases_and_loose_tracks_plus_orphans() {
    let (_t, r) = library();
    let rows = r.list_view_tracks(LibraryKind::Track).unwrap();
    let t: Vec<&str> = rows.iter().map(|x| x.title.as_str()).collect();
    assert_eq!(
        t,
        ["Field recording", "Legacy Hit", "Loose music", "Song 1", "Song 2", "Song 3", "stray"],
        "A-Z; release tracks AND loose tracks; no mix / podcast / audiobook; the link track is \
         not listed a second time; the orphan file is"
    );
    let song2 = rows.iter().find(|x| x.title == "Song 2").unwrap();
    assert_eq!((song2.position, song2.collection_items, song2.collection_title.as_str()), (1, 3, "Record"));
    let stray = rows.iter().find(|x| x.title == "stray").unwrap();
    assert!(stray.is_orphan);
}

#[test]
fn playlists_are_user_playlists_regardless_of_profile_and_inferred_legacy_files() {
    let (_t, r) = library();
    assert_eq!(titles(&r, LibraryKind::Playlist), ["Legacy import", "My playlist"]);
}

#[test]
fn mixes_are_tracks_of_profile_mix_loose_or_release() {
    let (_t, r) = library();
    assert_eq!(track_titles(&r, LibraryKind::Mix), ["Loose mix", "Release mix"]);
}

#[test]
fn audiobooks_and_podcasts_list_playables_of_that_profile() {
    let (_t, r) = library();
    assert_eq!(titles(&r, LibraryKind::Audiobook), ["The Book"]);
    assert_eq!(titles(&r, LibraryKind::Podcast), ["An episode", "The Show"]);
}

#[test]
fn view_counts_count_tracks_for_item_views_and_playables_for_the_rest() {
    let (_t, r) = library();
    let c = r.view_counts().unwrap();
    let n = |k| c.iter().find(|(kk, _)| *kk == k).map_or(0, |(_, n)| *n);
    assert_eq!(n(LibraryKind::Track), 7);
    assert_eq!(n(LibraryKind::Album), 2);
    assert_eq!(n(LibraryKind::Playlist), 2);
    assert_eq!(n(LibraryKind::Mix), 2);
    assert_eq!(n(LibraryKind::Audiobook), 1);
    assert_eq!(n(LibraryKind::Podcast), 2);
    assert!(view_is_item_level(LibraryKind::Track) && view_is_item_level(LibraryKind::Mix));
    assert!(!view_is_item_level(LibraryKind::Album));
}
