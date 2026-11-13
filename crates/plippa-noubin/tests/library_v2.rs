//! Index schema v2 + lazy scanning (NB_02).
//!
//! Fixture: `tests/fixtures/libraries/kinds` (album, per-track file, mix with cues,
//! audiobook, podcast, playlists incl. broken link + dynamic, orphan mp3, hidden dir).
//! Media files are empty placeholders: the index never decodes audio.

use plippa_noubin::{
    decode_playable_id, encode_playable_id, index_db_path, LibraryKind, LibraryReader, LibraryRoot,
    LibraryWriter, PlayableRecord, ScanReport, PLAYER_DB_DIR, SCHEMA_VERSION,
};
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/libraries")
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

fn kinds_library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    copy_dir(&fixture("kinds"), &root);
    (tmp, root)
}

fn empty_library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    (tmp, root)
}

fn set_mtime(path: &Path, when: SystemTime) {
    let f = fs::File::open(path).unwrap();
    f.set_modified(when).unwrap();
}

fn secs_ago(n: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(n)
}

/// Make every directory look old so the "too recent to trust" rule does not apply.
fn age_dirs(root: &Path, when: SystemTime) {
    for entry in fs::read_dir(root).unwrap().flatten() {
        if entry.file_type().unwrap().is_dir()
            && !entry.file_name().to_string_lossy().starts_with('.')
        {
            age_dirs(&entry.path(), when);
        }
    }
    set_mtime(root, when);
}

fn by_title<'a>(all: &'a [PlayableRecord], title: &str) -> &'a PlayableRecord {
    all.iter()
        .find(|r| r.title.as_deref() == Some(title))
        .unwrap_or_else(|| panic!("no playable titled {title:?} in {:?}", titles(all)))
}

fn titles(all: &[PlayableRecord]) -> Vec<Option<String>> {
    all.iter().map(|r| r.title.clone()).collect()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn simple_noudata(title: &str, key: Option<&str>, media: &str) -> String {
    let key = key.map_or(String::new(), |k| {
        format!(r#""noubinKeyNormalised":"{k}","#)
    });
    format!(
        r#"{{{key}"playableItemTitle":"{title}","mediaList":[{{"localFilePaths":["{media}"]}}]}}"#
    )
}

/// Everything observable about the index, id-included, for equality checks.
fn dump(root: &Path) -> Vec<(PlayableRecord, Vec<plippa_noubin::ItemRecord>)> {
    let r = LibraryReader::open(&LibraryRoot::new(root)).unwrap();
    let mut out = Vec::new();
    for p in r.list_playables().unwrap() {
        let items = r.items_of(p.id).unwrap();
        out.push((p, items));
    }
    out
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

#[test]
fn fixture_library_indexes_into_the_right_kinds() {
    let (_tmp, root) = kinds_library();
    let lib = LibraryRoot::new(&root);
    let mut w = LibraryWriter::open(&lib).unwrap();
    let report = w.full_scan().unwrap();
    // 8 .noudata files outside the hidden dir; the hidden one must not be seen.
    assert_eq!(report.scanned_files, 8, "{report:?}");
    assert_eq!(report.ingested, 8);
    assert_eq!(
        report.orphans_added, 1,
        "only 'orphan song.mp3' is unclaimed"
    );

    let all = w.list_playables().unwrap();
    assert!(
        !titles(&all).contains(&Some("Hidden".into())),
        "hidden dir scanned"
    );

    let album = by_title(&all, "The Album");
    assert_eq!(album.kind, LibraryKind::Album);
    assert_eq!(album.release_profile.as_deref(), Some("music"));
    assert_eq!(album.primary_artist.as_deref(), Some("Band"));
    assert_eq!(
        album.cover_relpath.as_deref(),
        Some("Music/Band/Album/cover.jpg")
    );
    assert_eq!(album.media_relpaths.len(), 2);

    // Per-track file referring to the release: a track, inherits the artist.
    let track = by_title(&all, "Two");
    assert_eq!(track.kind, LibraryKind::Track);
    assert_eq!(track.primary_artist.as_deref(), Some("Band"));
    assert_eq!(
        track.refers_to_relpath.as_deref(),
        Some("Music/Band/Album/_Album.noudata")
    );

    let mix = by_title(&all, "Rocksteady Set");
    assert_eq!(mix.kind, LibraryKind::Mix);
    assert!(mix.has_navigable_cues);

    assert_eq!(by_title(&all, "The Hobbit").kind, LibraryKind::Audiobook);
    assert_eq!(by_title(&all, "The Show").kind, LibraryKind::Podcast);
    assert_eq!(by_title(&all, "Rainy Day").kind, LibraryKind::Playlist);

    // §3.7: an empty mediaList is fine for a dynamic playlist.
    let dynamic = by_title(&all, "Everything Loud");
    assert_eq!(dynamic.kind, LibraryKind::Playlist);
    assert!(dynamic.is_dynamic);
    assert!(w.items_of(dynamic.id).unwrap().is_empty());

    // Orphan: track titled by file name, no .noudata.
    let orphan = by_title(&all, "orphan song");
    assert_eq!(orphan.kind, LibraryKind::Track);
    assert!(orphan.is_orphan);
    assert_eq!(orphan.noudata_relpath, "");
    assert_eq!(orphan.media_relpaths, vec!["Loose/orphan song.mp3"]);

    // list_by_kind
    assert_eq!(w.list_by_kind(LibraryKind::Album).unwrap().len(), 1);
    assert_eq!(w.list_by_kind(LibraryKind::Track).unwrap().len(), 2); // per-track + orphan
    assert_eq!(w.list_by_kind(LibraryKind::Playlist).unwrap().len(), 3);
    assert_eq!(w.list_by_kind(LibraryKind::Mix).unwrap().len(), 1);

    // Reader sees the same.
    let r = LibraryReader::open(&lib).unwrap();
    assert_eq!(r.list_by_kind(LibraryKind::Audiobook).unwrap().len(), 1);
    assert_eq!(r.get_playable(album.id).unwrap().unwrap(), *album);
}

#[test]
fn classification_matrix() {
    let (_tmp, root) = empty_library();
    write(
        &root.join("rel.noudata"),
        r#"{"playableItemTitle":"Rel","playableItemProfile":"other","releaseData":{},
            "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    );
    write(
        &root.join("unknown_profile.noudata"),
        r#"{"playableItemTitle":"Weird","playableItemProfile":"symphony","releaseData":{},
            "mediaList":[{"localFilePaths":["a2.mp3"]}]}"#,
    );
    write(
        &root.join("track_of_rel.noudata"),
        r#"{"playableItemTitle":"TrackOfRel","referToReleaseNoudata":"rel.noudata",
            "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    );
    write(
        &root.join("refers_missing.noudata"),
        r#"{"playableItemTitle":"RefMissing","referToReleaseNoudata":"nope.noudata",
            "mediaList":[{"localFilePaths":["b.mp3"]}]}"#,
    );
    // Refers to a file that is itself not a release (a playlist) => playlist.
    write(
        &root.join("plain.noudata"),
        &simple_noudata("Plain", None, "c.mp3"),
    );
    write(
        &root.join("refers_plain.noudata"),
        r#"{"playableItemTitle":"RefPlain","referToReleaseNoudata":"plain.noudata",
            "mediaList":[{"localFilePaths":["c.mp3"]}]}"#,
    );
    for m in ["a.mp3", "a2.mp3", "b.mp3", "c.mp3"] {
        write(&root.join(m), "");
    }
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let all = w.list_playables().unwrap();
    assert_eq!(
        by_title(&all, "Rel").kind,
        LibraryKind::Album,
        "profile other => album"
    );
    assert_eq!(
        by_title(&all, "Weird").kind,
        LibraryKind::Album,
        "unknown profile => other => album"
    );
    assert_eq!(by_title(&all, "TrackOfRel").kind, LibraryKind::Track);
    assert_eq!(by_title(&all, "RefMissing").kind, LibraryKind::Playlist);
    assert_eq!(by_title(&all, "Plain").kind, LibraryKind::Playlist);
    assert_eq!(by_title(&all, "RefPlain").kind, LibraryKind::Playlist);
}

#[test]
fn referring_file_kind_follows_its_target() {
    let (_tmp, root) = empty_library();
    write(&root.join("a.mp3"), "");
    write(
        &root.join("t.noudata"),
        r#"{"playableItemTitle":"T","referToReleaseNoudata":"r.noudata",
            "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    );
    let lib = LibraryRoot::new(&root);
    let mut w = LibraryWriter::open(&lib).unwrap();
    w.full_scan().unwrap();
    assert_eq!(
        by_title(&w.list_playables().unwrap(), "T").kind,
        LibraryKind::Playlist
    );

    // Target appears later: the (unchanged) referring file becomes a track.
    write(
        &root.join("r.noudata"),
        r#"{"playableItemTitle":"R","playableItemProfile":"music","releaseData":{},
            "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    );
    let rep = w.lazy_rescan().unwrap();
    assert_eq!(
        rep.noudata_reads, 1,
        "only the new release is read: {rep:?}"
    );
    assert_eq!(
        by_title(&w.list_playables().unwrap(), "T").kind,
        LibraryKind::Track
    );

    // Target deleted: back to playlist.
    fs::remove_file(root.join("r.noudata")).unwrap();
    w.lazy_rescan().unwrap();
    assert_eq!(
        by_title(&w.list_playables().unwrap(), "T").kind,
        LibraryKind::Playlist
    );
}

// ---------------------------------------------------------------------------
// Items, playlist resolution, cues
// ---------------------------------------------------------------------------

#[test]
fn playlist_entries_resolve_to_authoritative_release_metadata() {
    let (_tmp, root) = kinds_library();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let all = w.list_playables().unwrap();
    let album = by_title(&all, "The Album");
    let mix = by_title(&all, "Rocksteady Set");
    let rainy = by_title(&all, "Rainy Day");

    let items = w.items_of(rainy.id).unwrap();
    assert_eq!(items.len(), 3);
    // ../ paths resolved against the playlist's folder.
    assert_eq!(items[0].media_relpath, "Music/Band/Album/01 - One.mp3");
    // The release's title wins over the playlist's own mediaTitle.
    assert_eq!(items[0].media_title.as_deref(), Some("not authoritative"));
    assert_eq!(items[0].title, "One");
    assert_eq!(items[0].artist.as_deref(), Some("Band"));
    assert_eq!(items[0].resolved_from, Some(album.id));
    assert_eq!(items[1].title, "Two");
    assert_eq!(items[2].title, "Rocksteady Set");
    assert_eq!(items[2].resolved_from, Some(mix.id));

    // Broken link: item unavailable, title falls back to own mediaTitle, no panic.
    let broken = by_title(&all, "Broken Link");
    let bi = w.items_of(broken.id).unwrap();
    assert!(!bi[0].available);
    assert_eq!(bi[0].title, "Gone");
    assert!(broken.media_missing);
    assert!(!rainy.media_missing);
}

#[test]
fn playlist_re_resolves_when_release_changes() {
    let (_tmp, root) = kinds_library();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let rainy = by_title(&w.list_playables().unwrap(), "Rainy Day").clone();

    let album_path = root.join("Music/Band/Album/_Album.noudata");
    let text = fs::read_to_string(&album_path)
        .unwrap()
        .replace("\"One\"", "\"Uno\"");
    fs::write(&album_path, text).unwrap();
    set_mtime(&album_path, secs_ago(0) + Duration::from_secs(5));

    let rep = w.lazy_rescan().unwrap();
    assert_eq!(rep.noudata_reads, 1);
    assert_eq!(w.items_of(rainy.id).unwrap()[0].title, "Uno");
}

#[test]
fn cues_are_read_on_demand_with_resolved_ends() {
    let (_tmp, root) = kinds_library();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let all = w.list_playables().unwrap();
    let mix = by_title(&all, "Rocksteady Set").clone();
    let mix = &mix;

    let items = w.items_of(mix.id).unwrap();
    assert!(items[0].has_navigable_cues);
    assert_eq!(items[0].cue_count, 4);

    let cues = w.cues_for(mix.id, 0).unwrap();
    assert_eq!(cues.len(), 4);
    let nav: Vec<_> = cues.iter().filter(|c| c.navigable).collect();
    assert_eq!(nav.len(), 4, "nav + credits lists are navigable by default");
    assert_eq!(cues[0].text, "Warm up");
    assert_eq!((cues[0].start_ms, cues[0].end_ms), (0, Some(600_000)));
    assert_eq!(cues[1].end_ms, Some(2_700_000), "end = next cue start");
    assert_eq!(cues[2].end_ms, None, "unknown item length");
    assert_eq!(cues[3].cue_list_index, 1);

    // Orphans and out-of-range items have no cues (and no error).
    let orphan = by_title(&all, "orphan song");
    assert!(w.cues_for(orphan.id, 0).unwrap().is_empty());
    assert!(w.cues_for(mix.id, 9).unwrap().is_empty());

    // Once the real length is known, cues beyond it are dropped and the last end resolves.
    let mut w = w;
    w.probe_durations(&mut |_| Some(50 * 60_000), 100).unwrap();
    let cues = w.cues_for(mix.id, 0).unwrap();
    assert_eq!(cues[2].end_ms, Some(50 * 60_000));
    let mix_path = root.join("Mixes/Japanese/_Mix.noudata");
    let text = fs::read_to_string(&mix_path).unwrap();
    assert!(text.contains("expectedLength"), "length written back");
    // Shorten: 20 minutes => "Cool down" (45:00) is beyond the media and is ignored.
    let mut only = plippa_noubin::parse_noudata(text.as_bytes()).unwrap();
    only.media_list[0]
        .unite_data
        .as_mut()
        .unwrap()
        .expected_length = Some(plippa_noubin::NoubinTimestamp::from_millis(20 * 60_000));
    let trimmed = plippa_noubin::cue_entries(&only, 0, Some(20 * 60_000));
    assert_eq!(trimmed.len(), 3, "cue beyond length dropped: {trimmed:?}");
}

// ---------------------------------------------------------------------------
// Lazy scan
// ---------------------------------------------------------------------------

#[test]
fn second_scan_reads_zero_noudata_and_writes_nothing() {
    let (_tmp, root) = kinds_library();
    let lib = LibraryRoot::new(&root);
    let mut w = LibraryWriter::open(&lib).unwrap();
    let first = w.lazy_rescan().unwrap();
    assert_eq!(
        first.noudata_reads, 8,
        "8 files; a declared linkTrack needs no read of its release to classify"
    );
    assert_eq!(first.ingested, 8);

    let probe = Connection::open(index_db_path(&lib)).unwrap();
    let version_before: i64 = probe
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();

    for _ in 0..2 {
        let second = w.lazy_rescan().unwrap();
        assert_eq!(second.noudata_reads, 0, "{second:?}");
        assert!(!second.changed(), "{second:?}");
        assert_eq!(second.scanned_files, 8);
    }
    let version_after: i64 = probe
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version_before, version_after,
        "no-change scan must not commit"
    );
}

#[test]
fn touching_one_noudata_rereads_only_that_file() {
    let (_tmp, root) = kinds_library();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.lazy_rescan().unwrap();

    let p = root.join("Podcasts/Show/_Show.noudata");
    let text = fs::read_to_string(&p)
        .unwrap()
        .replace("The Show", "The Show!");
    fs::write(&p, text).unwrap();
    // Same-second edits must still be seen: mtime is compared in ns, and set explicitly.
    set_mtime(&p, SystemTime::now() + Duration::from_millis(1500));

    let rep = w.lazy_rescan().unwrap();
    assert_eq!(rep.noudata_reads, 1, "{rep:?}");
    assert_eq!((rep.updated, rep.ingested, rep.removed), (1, 0, 0));
    assert!(rep.changed());
    assert!(w
        .list_playables()
        .unwrap()
        .iter()
        .any(|r| r.title.as_deref() == Some("The Show!")));

    let again = w.lazy_rescan().unwrap();
    assert_eq!(again.noudata_reads, 0);
    assert!(!again.changed());
}

#[test]
fn unchanged_directories_are_not_listed_again() {
    let (_tmp, root) = kinds_library();
    let old = secs_ago(3600);
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap(); // creates .plippa
    age_dirs(&root, old);
    let first = w.lazy_rescan().unwrap();
    assert!(first.dirs_listed >= 8);
    let second = w.lazy_rescan().unwrap();
    assert_eq!(second.dirs_listed, 0, "{second:?}");
    assert_eq!(second.noudata_reads, 0);

    // A new file appears and its folder's mtime changes: only that folder is relisted.
    write(
        &root.join("Playlists/New.noudata"),
        &simple_noudata("New", None, "../Mixes/Japanese/mix.mp3"),
    );
    set_mtime(&root.join("Playlists"), old - Duration::from_secs(60));
    let third = w.lazy_rescan().unwrap();
    assert_eq!(third.dirs_listed, 1, "{third:?}");
    assert_eq!(third.ingested, 1);
    assert_eq!(third.noudata_reads, 1);

    // full_scan ignores every cache.
    let full = w.full_scan().unwrap();
    assert!(full.dirs_listed >= 8);
    assert_eq!(
        full.noudata_reads, 9,
        "9 files; a declared linkTrack needs no read of its release to classify"
    );
}

#[test]
fn removing_a_noudata_keeps_media_and_orphans_it() {
    let (_tmp, root) = kinds_library();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.lazy_rescan().unwrap();
    let before = w.count_playables().unwrap();

    fs::remove_file(root.join("Podcasts/Show/_Show.noudata")).unwrap();
    let rep = w.lazy_rescan().unwrap();
    assert_eq!(rep.removed, 1);
    assert_eq!(
        rep.orphans_added, 1,
        "its media file is now an orphan track"
    );
    assert!(
        root.join("Podcasts/Show/ep1.mp3").is_file(),
        "media untouched"
    );
    assert_eq!(w.count_playables().unwrap(), before);
    let all = w.list_playables().unwrap();
    assert!(all
        .iter()
        .any(|r| r.is_orphan && r.title.as_deref() == Some("ep1")));

    // Restoring the .noudata takes the media back (orphan removed).
    write(
        &root.join("Podcasts/Show/_Show.noudata"),
        r#"{"playableItemTitle":"The Show","playableItemProfile":"podcast","releaseData":{},
            "mediaList":[{"localFilePaths":["ep1.mp3"]}]}"#,
    );
    let rep = w.lazy_rescan().unwrap();
    assert_eq!((rep.ingested, rep.orphans_removed), (1, 1), "{rep:?}");
}

#[test]
fn removing_media_marks_item_unavailable_and_leaves_noudata_alone() {
    let (_tmp, root) = kinds_library();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.lazy_rescan().unwrap();
    let album = by_title(&w.list_playables().unwrap(), "The Album").clone();
    let nou = root.join("Music/Band/Album/_Album.noudata");
    let bytes_before = fs::read(&nou).unwrap();

    fs::remove_file(root.join("Music/Band/Album/02 - Two.mp3")).unwrap();
    let rep = w.lazy_rescan().unwrap();
    assert!(rep.changed(), "{rep:?}");
    assert_eq!(
        rep.noudata_reads, 0,
        "no .noudata re-read for a missing media file"
    );
    assert!(rep.availability_changed >= 1);

    let items = w.items_of(album.id).unwrap();
    assert!(items[0].available);
    assert!(!items[1].available);
    assert!(w.get_playable(album.id).unwrap().unwrap().media_missing);
    assert_eq!(
        fs::read(&nou).unwrap(),
        bytes_before,
        ".noudata must be untouched"
    );
    // The record still lists both files (engine reports "media not found" for the missing one).
    assert_eq!(
        w.get_playable(album.id)
            .unwrap()
            .unwrap()
            .media_relpaths
            .len(),
        2
    );

    // Idempotent, and coming back is noticed.
    assert!(!w.lazy_rescan().unwrap().changed());
    write(&root.join("Music/Band/Album/02 - Two.mp3"), "");
    let rep = w.lazy_rescan().unwrap();
    assert!(rep.changed());
    assert!(w.items_of(album.id).unwrap()[1].available);
}

#[test]
fn alternates_pick_the_first_that_exists() {
    let (_tmp, root) = empty_library();
    write(&root.join("m.flac"), "");
    write(
        &root.join("m.noudata"),
        r#"{"playableItemTitle":"Alt","mediaList":[{"localFilePaths":["m.mp3","m.flac"]}]}"#,
    );
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let p = by_title(&w.list_playables().unwrap(), "Alt").clone();
    let it = &w.items_of(p.id).unwrap()[0];
    assert_eq!(it.media_relpath, "m.flac");
    assert!(it.available);
    assert_eq!(it.media_relpaths, vec!["m.mp3", "m.flac"]);
}

#[test]
fn deleting_plippa_dir_rebuilds_identical_index() {
    let (_tmp, root) = kinds_library();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    let before = dump(&root);
    assert_eq!(before.len(), 9, "8 .noudata + 1 orphan");

    fs::remove_dir_all(root.join(PLAYER_DB_DIR)).unwrap();
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    assert_eq!(dump(&root), before);
}

#[test]
fn unplayable_and_broken_noudata_are_read_once_and_do_not_crash() {
    let (_tmp, root) = empty_library();
    write(&root.join("broken.noudata"), "{ not json");
    write(
        &root.join("empty.noudata"),
        r#"{"playableItemTitle":"Empty","mediaList":[]}"#,
    );
    write(
        &root.join("ok.noudata"),
        &simple_noudata("Ok", None, "ok.mp3"),
    );
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let first = w.lazy_rescan().unwrap();
    assert_eq!(first.ingested, 1);
    assert_eq!(first.skipped, 2);
    assert_eq!(first.warnings.len(), 1, "{:?}", first.warnings);
    let second = w.lazy_rescan().unwrap();
    assert_eq!(
        second.noudata_reads, 0,
        "fingerprints of skipped files are cached"
    );
    assert!(second.warnings.is_empty());
}

// ---------------------------------------------------------------------------
// Duplicates, shared media
// ---------------------------------------------------------------------------

#[test]
fn duplicate_keys_are_all_indexed_returned_and_warned() {
    let (_tmp, root) = empty_library();
    write(
        &root.join("a.noudata"),
        &simple_noudata("A", Some("dup.key"), "a.mp3"),
    );
    write(
        &root.join("sub/b.noudata"),
        &simple_noudata("B", Some("dup.key"), "b.mp3"),
    );
    write(
        &root.join("c.noudata"),
        &simple_noudata("C", Some("other"), "c.mp3"),
    );
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let rep = w.full_scan().unwrap();
    assert_eq!(rep.ingested, 3);
    assert!(
        rep.warnings.iter().any(|m| m.contains("duplicate")
            && m.contains("dup.key")
            && m.contains("a.noudata")
            && m.contains("sub/b.noudata")),
        "{:?}",
        rep.warnings
    );

    let found = w.find_by_key("dup.key").unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].noudata_relpath, "a.noudata", "deterministic order");
    assert_eq!(w.find_by_key("other").unwrap().len(), 1);
    assert!(w.find_by_key("nope").unwrap().is_empty());

    let r = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    match r.lookup_by_normalised_key("dup.key").unwrap() {
        plippa_noubin::LookupResult::Found(h) => assert!(h.multi_match && h.records.len() == 2),
        _ => panic!("expected hit"),
    }
    assert_eq!(r.library_warnings().unwrap().len(), 1);
}

#[test]
fn two_releases_claiming_the_same_media_are_both_indexed_with_a_warning() {
    let (_tmp, root) = empty_library();
    write(&root.join("shared.mp3"), "");
    let rel = |t: &str| {
        format!(
            r#"{{"playableItemTitle":"{t}","playableItemProfile":"music","releaseData":{{}},
                "mediaList":[{{"localFilePaths":["shared.mp3"]}}]}}"#
        )
    };
    write(&root.join("one.noudata"), &rel("One"));
    write(&root.join("two.noudata"), &rel("Two"));
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let rep = w.full_scan().unwrap();
    assert_eq!(rep.ingested, 2);
    assert_eq!(rep.orphans_added, 0, "claimed media is never an orphan");
    assert!(rep.warnings.iter().any(|m| m.contains("shared.mp3")
        && m.contains("one.noudata")
        && m.contains("two.noudata")));
    // A playlist referencing a release's media is NOT a conflict.
    write(
        &root.join("pl.noudata"),
        &simple_noudata("PL", None, "shared.mp3"),
    );
    let rep = w.lazy_rescan().unwrap();
    assert_eq!(
        rep.warnings
            .iter()
            .filter(|m| m.contains("claimed"))
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// Duration probe
// ---------------------------------------------------------------------------

#[test]
fn duration_probe_writes_back_once_and_is_idempotent() {
    let (_tmp, root) = kinds_library();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.lazy_rescan().unwrap();
    let all = w.list_playables().unwrap();
    let hobbit = by_title(&all, "The Hobbit").clone();
    assert_eq!(hobbit.duration_ms, None);

    let probed_paths: std::cell::RefCell<Vec<PathBuf>> = Default::default();
    let mut probe = |p: &Path| {
        probed_paths.borrow_mut().push(p.to_path_buf());
        if p.file_name()
            .unwrap()
            .to_string_lossy()
            .contains("hobbit-1")
        {
            Some(3_723_500)
        } else if p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("hobbit-2")
        {
            Some(60_000)
        } else {
            None // unreadable media
        }
    };
    let rep = w.probe_durations(&mut probe, 100).unwrap();
    assert!(rep.probed >= 2 && rep.written >= 1, "{rep:?}");
    assert!(rep.failed >= 1, "unmeasurable media is remembered: {rep:?}");
    assert!(probed_paths.borrow().iter().all(|p| p.starts_with(&root)));
    // Missing media (broken link) is never probed.
    assert!(!probed_paths
        .borrow()
        .iter()
        .any(|p| p.ends_with("missing.mp3")));

    let items = w.items_of(hobbit.id).unwrap();
    assert_eq!(items[0].duration_ms, Some(3_723_500));
    assert_eq!(items[0].duration_src.as_deref(), Some("probed"));
    assert_eq!(
        w.get_playable(hobbit.id).unwrap().unwrap().duration_ms,
        Some(3_783_500)
    );

    // Written to the .noudata as uniteData.expectedLength (spec writer form).
    let text = fs::read_to_string(root.join("Audiobooks/Tolkien/Hobbit/_Hobbit.noudata")).unwrap();
    assert!(text.contains(r#""expectedLength": "01:02:03.5""#), "{text}");
    // Playlist entries pointing at probed media inherit its length.
    let rainy = by_title(&w.list_playables().unwrap(), "Rainy Day").clone();
    let ri = w.items_of(rainy.id).unwrap();
    assert_eq!(ri.len(), 3);

    // Idempotent: nothing left to do, nothing rewritten, no repeat of failures.
    let rep2 = w.probe_durations(&mut probe, 100).unwrap();
    assert_eq!(rep2, Default::default());

    // The write-back is not seen as an external change by the next scan.
    let scan = w.lazy_rescan().unwrap();
    assert_eq!(scan.noudata_reads, 0, "{scan:?}");
    assert!(!scan.changed());

    // ...and a genuine re-read keeps the measured value.
    let p = root.join("Audiobooks/Tolkien/Hobbit/_Hobbit.noudata");
    let t = fs::read_to_string(&p).unwrap();
    fs::write(&p, t.replace("Part 2", "Part Two")).unwrap();
    set_mtime(&p, SystemTime::now() + Duration::from_secs(5));
    let scan = w.lazy_rescan().unwrap();
    assert_eq!(scan.updated, 1);
    let items = w.items_of(hobbit.id).unwrap();
    assert_eq!(items[0].duration_src.as_deref(), Some("probed"));
    assert_eq!(items[1].title, "Part Two");
}

#[test]
fn reordering_items_keeps_probed_durations() {
    let (_tmp, root) = empty_library();
    write(&root.join("a.mp3"), "");
    write(&root.join("b.mp3"), "");
    write(
        &root.join("x.noudata"),
        r#"{"playableItemTitle":"X","mediaList":[
            {"localFilePaths":["a.mp3"]},
            {"localFilePaths":["b.mp3"]}]}"#,
    );
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let mut probe = |p: &Path| {
        Some(match p.file_name().unwrap().to_str().unwrap() {
            "a.mp3" => 10_000,
            _ => 20_000,
        })
    };
    w.probe_durations(&mut probe, 10).unwrap();
    let x = by_title(&w.list_playables().unwrap(), "X").clone();
    assert_eq!(w.items_of(x.id).unwrap()[0].duration_ms, Some(10_000));

    // Reorder [b, a] — what `PUT /api/collections/:id/order` writes — and ingest just that file.
    write(
        &root.join("x.noudata"),
        r#"{"playableItemTitle":"X","mediaList":[
            {"localFilePaths":["b.mp3"]},
            {"localFilePaths":["a.mp3"]}]}"#,
    );
    set_mtime(
        &root.join("x.noudata"),
        SystemTime::now() + Duration::from_secs(5),
    );
    let rep = w.ingest_noudata("x.noudata").unwrap();
    assert_eq!(rep.updated, 1, "{rep:?}");

    let items = w.items_of(x.id).unwrap();
    assert_eq!(items[0].media_relpath, "b.mp3");
    assert_eq!(
        (items[0].duration_ms, items[0].duration_src.as_deref()),
        (Some(20_000), Some("probed")),
        "the measured length follows the media, not the old position"
    );
    assert_eq!(items[1].duration_ms, Some(10_000));
    assert_eq!(
        w.get_playable(x.id).unwrap().unwrap().duration_ms,
        Some(30_000)
    );
}

#[test]
fn probe_is_throttled_and_corrects_wrong_declared_length() {
    let (_tmp, root) = empty_library();
    write(&root.join("a.mp3"), "");
    write(&root.join("b.mp3"), "");
    write(
        &root.join("x.noudata"),
        r#"{"playableItemTitle":"X","mediaList":[
            {"localFilePaths":["a.mp3"],"uniteData":{"expectedLength":"3:00"}},
            {"localFilePaths":["b.mp3"],"uniteData":{"expectedFilename":"b"}}]}"#,
    );
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let p = by_title(&w.list_playables().unwrap(), "X").clone();
    let items = w.items_of(p.id).unwrap();
    assert_eq!(items[0].duration_ms, Some(180_000));
    assert_eq!(items[0].duration_src.as_deref(), Some("unite"));
    assert_eq!(items[1].duration_ms, None);
    assert_eq!(
        p.duration_ms, None,
        "aggregate only when every item is known"
    );

    let mut calls = 0;
    let mut probe = |_: &Path| {
        calls += 1;
        Some(if calls == 1 { 200_000 } else { 90_500 })
    };
    let rep = w.probe_durations(&mut probe, 1).unwrap();
    assert_eq!(rep.probed, 1, "max_items respected");
    let rep = w.probe_durations(&mut probe, 1).unwrap();
    assert_eq!(rep.probed, 1);
    assert_eq!(w.probe_durations(&mut probe, 1).unwrap().probed, 0);

    let items = w.items_of(p.id).unwrap();
    assert_eq!(
        items[0].duration_ms,
        Some(200_000),
        "declared 3:00 is not authoritative"
    );
    assert_eq!(items[1].duration_ms, Some(90_500));
    assert_eq!(
        w.get_playable(p.id).unwrap().unwrap().duration_ms,
        Some(290_500)
    );
    let text = fs::read_to_string(root.join("x.noudata")).unwrap();
    assert!(text.contains("03:20") && text.contains("01:30.5"), "{text}");
    assert!(
        text.contains("expectedFilename"),
        "other uniteData fields preserved"
    );
}

/// NB_VERIFY: the probe rewrites the `.noudata` (source of truth) from the scanner thread, so it
/// must be a temp-file + rename (a power cut mid-write must never leave a truncated file, and the
/// rename is what the playlist/resume writers' fingerprint guard can detect).
#[test]
fn probe_write_back_replaces_the_file_atomically() {
    use std::os::unix::fs::MetadataExt;
    let (_tmp, root) = empty_library();
    write(&root.join("a.mp3"), "");
    let nou = root.join("x.noudata");
    write(&nou, &simple_noudata("X", None, "a.mp3"));
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    let ino_before = fs::metadata(&nou).unwrap().ino();
    let rep = w.probe_durations(&mut |_| Some(61_000), 10).unwrap();
    assert_eq!(rep.written, 1, "{rep:?}");
    assert_ne!(
        fs::metadata(&nou).unwrap().ino(),
        ino_before,
        "write-back must go through a temp file + rename, not truncate in place"
    );
    let left: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("plippa-tmp"))
        .collect();
    assert!(left.is_empty(), "temp file left behind: {left:?}");
    assert!(fs::read_to_string(&nou)
        .unwrap()
        .contains(r#""expectedLength": "00:01:01""#));
    // Not an external change for the next scan.
    assert_eq!(w.lazy_rescan().unwrap().noudata_reads, 0);
}

#[test]
fn probe_updates_index_even_if_noudata_is_not_writable() {
    let (_tmp, root) = empty_library();
    write(&root.join("a.mp3"), "");
    write(&root.join("x.noudata"), &simple_noudata("X", None, "a.mp3"));
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    // Make the write-back fail: replace the file with a directory-like obstacle is hard;
    // instead corrupt it after indexing so the re-read for write-back fails to parse.
    fs::write(root.join("x.noudata"), "{ nope").unwrap();
    let rep = w.probe_durations(&mut |_| Some(1000), 10).unwrap();
    assert_eq!(rep.probed, 1);
    assert_eq!(rep.write_errors, 1);
    assert_eq!(
        fs::read_to_string(root.join("x.noudata")).unwrap(),
        "{ nope",
        "never clobber"
    );
    let p = by_title(&w.list_playables().unwrap(), "X").clone();
    assert_eq!(p.duration_ms, Some(1000));
}

// ---------------------------------------------------------------------------
// Schema migration, ids
// ---------------------------------------------------------------------------

fn make_v1_db(root: &Path, rows: &[(&str, &str, &str, &str)]) {
    let dir = root.join(PLAYER_DB_DIR);
    fs::create_dir_all(&dir).unwrap();
    let conn = Connection::open(dir.join("library.sqlite")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
         CREATE TABLE playables (
            id INTEGER PRIMARY KEY NOT NULL,
            noudata_relpath TEXT NOT NULL UNIQUE,
            noubin_key_normalised TEXT,
            title TEXT,
            media_relpaths_json TEXT NOT NULL,
            mtime_secs INTEGER NOT NULL,
            file_size INTEGER NOT NULL);
         CREATE INDEX idx_playables_key ON playables(noubin_key_normalised);
         INSERT INTO meta VALUES ('schema_version','1');",
    )
    .unwrap();
    for (i, (rel, key, title, media)) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO playables VALUES (?1, ?2, ?3, ?4, ?5, 1700000000, 10)",
            rusqlite::params![(i as i64 + 1) * 10, rel, key, title, media],
        )
        .unwrap();
    }
}

#[test]
fn v1_index_is_migrated_in_place_keeping_ids_and_rescanned() {
    let (_tmp, root) = empty_library();
    write(&root.join("a.mp3"), "");
    write(
        &root.join("a.noudata"),
        &simple_noudata("A", Some("ka"), "a.mp3"),
    );
    write(
        &root.join("gone.noudata"),
        &simple_noudata("Gone", Some("kg"), "a.mp3"),
    );
    fs::remove_file(root.join("gone.noudata")).unwrap();
    make_v1_db(
        &root,
        &[
            ("a.noudata", "ka", "OLD TITLE", r#"["a.mp3"]"#),
            ("gone.noudata", "kg", "Gone", r#"["a.mp3"]"#),
        ],
    );
    let lib = LibraryRoot::new(&root);

    // A v1 index is still readable (stale) before the writer has migrated it.
    let stale = LibraryReader::open(&lib).unwrap();
    assert_eq!(stale.list_playables().unwrap().len(), 2);
    assert!(stale.items_of(10).unwrap().is_empty());
    assert!(stale.list_by_kind(LibraryKind::Album).unwrap().is_empty());
    assert!(stale.library_warnings().unwrap().is_empty());
    assert_eq!(
        stale.find_by_key("ka").unwrap()[0].title.as_deref(),
        Some("OLD TITLE")
    );
    drop(stale);

    let mut w = LibraryWriter::open(&lib).unwrap();
    let ver: String = Connection::open(index_db_path(&lib))
        .unwrap()
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ver, SCHEMA_VERSION.to_string());
    // Migrated rows keep their ids and stay playable until the scan catches up.
    assert_eq!(
        w.get_playable(10).unwrap().unwrap().title.as_deref(),
        Some("OLD TITLE")
    );
    assert!(w.get_playable(20).unwrap().is_some());

    let rep = w.lazy_rescan().unwrap();
    assert_eq!(rep.updated, 1, "the migrated row is re-read: {rep:?}");
    assert_eq!(rep.removed, 1, "and the vanished file's row dropped");
    let a = w.get_playable(10).unwrap().unwrap();
    assert_eq!(a.title.as_deref(), Some("A"));
    assert_eq!(a.kind, LibraryKind::Playlist);
    assert_eq!(w.items_of(10).unwrap().len(), 1);
    // Reader (already v2 now) sees it too.
    let r = LibraryReader::open(&lib).unwrap();
    assert_eq!(r.items_of(10).unwrap().len(), 1);
    // Steady state afterwards.
    assert!(!w.lazy_rescan().unwrap().changed());
}

/// Device bug (first run after the v1 -> v3 upgrade): the UI opened its reader while the
/// index was still v1, the server then migrated it, and the long-lived reader kept answering
/// "empty" (category counts / lists) until some later index update made it reopen. A reader
/// must notice the migration on its own.
#[test]
fn reader_opened_before_the_migration_serves_v2_data_afterwards() {
    let (_tmp, root) = empty_library();
    write(&root.join("a.mp3"), "");
    write(
        &root.join("a.noudata"),
        &simple_noudata("A", Some("ka"), "a.mp3"),
    );
    make_v1_db(&root, &[("a.noudata", "ka", "OLD TITLE", r#"["a.mp3"]"#)]);
    let lib = LibraryRoot::new(&root);

    // Long-lived reader (the UI / engine hold one for the whole process lifetime).
    let reader = LibraryReader::open(&lib).unwrap();
    assert!(reader.group_counts().unwrap().is_empty(), "v1: no kinds yet");

    let mut w = LibraryWriter::open(&lib).unwrap(); // migrates v1 -> current
    w.lazy_rescan().unwrap();

    assert_eq!(
        reader.group_counts().unwrap(),
        vec![(LibraryKind::Playlist, 1)],
        "the same reader must see the migrated + rescanned index"
    );
    assert_eq!(reader.list_by_group(LibraryKind::Playlist).unwrap().len(), 1);
    assert_eq!(reader.items_of(10).unwrap().len(), 1);
    assert_eq!(reader.kind_counts().unwrap(), vec![(LibraryKind::Playlist, 1)]);
}

#[test]
fn unknown_schema_version_is_rebuilt_not_fatal() {
    let (_tmp, root) = empty_library();
    write(&root.join("a.noudata"), &simple_noudata("A", None, "a.mp3"));
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    Connection::open(index_db_path(&lib))
        .unwrap()
        .execute("UPDATE meta SET value = '99'", [])
        .unwrap();
    let mut w = LibraryWriter::open(&lib).unwrap();
    assert_eq!(w.count_playables().unwrap(), 0);
    assert_eq!(w.lazy_rescan().unwrap().ingested, 1);
}

#[test]
fn multi_root_id_namespacing_is_unaffected() {
    let (_t1, a) = kinds_library();
    let (_t2, b) = kinds_library();
    let mut wa = LibraryWriter::open(&LibraryRoot::new(&a)).unwrap();
    let mut wb = LibraryWriter::open(&LibraryRoot::new(&b)).unwrap();
    wa.full_scan().unwrap();
    wb.full_scan().unwrap();
    let ra = wa.list_playables().unwrap();
    let rb = wb.list_playables().unwrap();
    // Identical libraries => identical local ids; the namespace keeps global ids apart.
    assert_eq!(ra[0].id, rb[0].id);
    let ga = encode_playable_id(&a, ra[0].id);
    let gb = encode_playable_id(&b, rb[0].id);
    assert_ne!(ga, gb);
    assert_eq!(decode_playable_id(ga).1, ra[0].id);
    assert_eq!(decode_playable_id(gb).1, rb[0].id);
}

#[test]
fn scan_report_default_is_unchanged() {
    assert!(!ScanReport::default().changed());
}

/// A long-lived reader (the engine opens one per library generation) must observe a key that
/// the writer binds *after* the reader was opened. Association writes the key into the
/// `.noudata` and then rescans; if that rescan does not bump the index generation the engine is
/// never told, so the only thing that can save it is the reader seeing the committed row.
#[test]
fn a_reader_opened_before_a_key_was_bound_sees_the_bound_key() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("album.noudata"),
        simple_noudata("An Album", None, "a.mp3"),
    )
    .unwrap();
    fs::write(root.join("a.mp3"), b"x").unwrap();
    let lib = LibraryRoot::new(&root);
    let mut w = LibraryWriter::open(&lib).unwrap();
    w.full_scan().unwrap();
    assert!(
        w.find_by_key("local:boundlater").unwrap().is_empty(),
        "no key before the association"
    );

    // The engine's reader, opened while the key is still absent.
    let reader = LibraryReader::open(&lib).unwrap();
    assert_eq!(
        reader.lookup_by_normalised_key("local:boundlater").unwrap(),
        plippa_noubin::LookupResult::NotFound,
        "not associated yet"
    );

    // The association rewrites the `.noudata` with the key and rescans (the writer's own
    // connection commits it).
    fs::write(
        root.join("album.noudata"),
        simple_noudata("An Album", Some("local:boundlater"), "a.mp3"),
    )
    .unwrap();
    w.lazy_rescan().unwrap();
    drop(w);

    // The same reader must see it without being reopened.
    let hit = reader.lookup_by_normalised_key("local:boundlater").unwrap();
    assert!(
        matches!(hit, plippa_noubin::LookupResult::Found(_)),
        "a long-lived reader must see a key bound after it opened: {hit:?}"
    );
}
