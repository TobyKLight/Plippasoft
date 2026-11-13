//! Cover images: which ones apply (D4), the index rows, thumbnails and the writer.
//! Image files are 1-byte stubs: nothing here decodes; the `.noudata` JSON is the point.

use plippa_noubin::covers::{
    apply_cover_edit, effective_collection_covers, effective_item_covers, resolve_thumbnail,
    sibling_candidates, thumbnail_relpath, CoverEdit, CoverImageSpec, CoverSource, CoverTarget,
    ThumbTier,
};
use plippa_noubin::{
    fs_release_resolver, index_db_path, parse_noudata_str, CoverEditError, LibraryReader,
    LibraryRoot, LibraryWriter, NoubinError, SCHEMA_VERSION,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn put(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn put_json(root: &Path, rel: &str, v: &Value) {
    put(root, rel, &serde_json::to_string_pretty(v).unwrap());
}

fn images(names: &[&str]) -> Value {
    json!({ "images": names.iter().map(|n| json!({"imageLocalFilename": n})).collect::<Vec<_>>() })
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    (t, root)
}

/// A release in `Album/` with a track, the artist's `coverImage`, and optional user list.
fn release(artist: Option<Value>, local: Option<Value>) -> Value {
    let mut v = json!({
        "noudataType": "release",
        "playableItemTitle": "Album",
        "playableItemProfile": "music",
        "releaseData": {},
        "mediaList": [{"mediaTitle": "One", "localFilePaths": ["01.mp3"]}]
    });
    if let Some(a) = artist {
        v["releaseData"]["coverImage"] = a;
    }
    if let Some(l) = local {
        v["localPlaybackData"] = json!({ "localCoverImage": l });
    }
    v
}

fn touch(root: &Path, rels: &[&str]) {
    for r in rels {
        put(root, r, "x");
    }
}

fn rels(covers: &[plippa_noubin::covers::CoverRef]) -> Vec<&str> {
    covers.iter().map(|c| c.relpath.as_str()).collect()
}

fn collection(root: &Path, rel: &str) -> Vec<plippa_noubin::covers::CoverRef> {
    let doc = parse_noudata_str(&fs::read_to_string(root.join(rel)).unwrap()).unwrap();
    effective_collection_covers(root, &doc, rel, &fs_release_resolver(&root.join(rel)))
}

fn index(root: &Path) -> LibraryWriter {
    let mut w = LibraryWriter::open(&LibraryRoot::new(root)).unwrap();
    w.full_scan().unwrap();
    w
}

fn id_of(w: &LibraryWriter, noudata: &str) -> i64 {
    w.list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.noudata_relpath == noudata)
        .unwrap_or_else(|| panic!("{noudata} not indexed"))
        .id
}

// ---------------------------------------------------------------------------
// Order
// ---------------------------------------------------------------------------

#[test]
fn local_list_wins_over_the_artists_and_artist_art_is_the_fallback() {
    let (_t, root) = tmp();
    touch(&root, &["Album/art.jpg", "Album/mine.png", "Album/01.mp3"]);
    put_json(
        &root,
        "Album/a.noudata",
        &release(Some(images(&["art.jpg"])), Some(images(&["mine.png"]))),
    );
    put_json(
        &root,
        "Album/b.noudata",
        &release(Some(images(&["art.jpg"])), None),
    );
    let a = collection(&root, "Album/a.noudata");
    assert_eq!(rels(&a), ["Album/mine.png"]);
    assert_eq!(a[0].source, CoverSource::Local);
    let b = collection(&root, "Album/b.noudata");
    assert_eq!(rels(&b), ["Album/art.jpg"]);
    assert_eq!(b[0].source, CoverSource::Release);
}

#[test]
fn an_explicitly_empty_local_list_hides_the_artists_art_and_the_sibling() {
    let (_t, root) = tmp();
    touch(&root, &["Album/art.jpg", "Album/cover.jpg", "Album/01.mp3"]);
    put_json(
        &root,
        "Album/a.noudata",
        &release(Some(images(&["art.jpg"])), Some(json!({"images": []}))),
    );
    assert!(collection(&root, "Album/a.noudata").is_empty());
    // The same holds in the index: nothing declared, nothing guessed.
    let w = index(&root);
    let id = id_of(&w, "Album/a.noudata");
    assert_eq!(w.get_playable(id).unwrap().unwrap().cover_count, 0);
    assert!(w.covers_of(id).unwrap().is_empty());
    assert_eq!(w.get_playable(id).unwrap().unwrap().cover_relpath, None);
}

#[test]
fn sibling_art_is_the_last_resort_and_primary_only() {
    let (_t, root) = tmp();
    touch(&root, &["Album/folder.jpg", "Album/cover.png", "Album/01.mp3"]);
    put_json(&root, "Album/a.noudata", &release(None, None));
    let c = collection(&root, "Album/a.noudata");
    assert_eq!(rels(&c), ["Album/cover.png"], "cover.* is preferred to folder.*");
    assert_eq!(c[0].source, CoverSource::Sibling);
    assert_eq!(
        sibling_candidates(&root.join("Album")),
        Some(root.join("Album/cover.png"))
    );
    assert_eq!(sibling_candidates(&root), None);
    // The index resolves it when asked, though nothing is declared.
    let w = index(&root);
    let id = id_of(&w, "Album/a.noudata");
    assert_eq!(w.get_playable(id).unwrap().unwrap().cover_count, 0);
    assert_eq!(rels(&w.covers_of(id).unwrap()), ["Album/cover.png"]);
}

#[test]
fn a_loose_tracks_item_cover_comes_first() {
    let (_t, root) = tmp();
    touch(&root, &["T/item.jpg", "T/mine.jpg", "T/s.mp3"]);
    put_json(
        &root,
        "T/t.noudata",
        &json!({
            "noudataType": "looseTrack",
            "playableItemTitle": "T",
            "localPlaybackData": {"localCoverImage": images(&["mine.jpg"])},
            "mediaList": [{"localFilePaths": ["s.mp3"], "itemCoverImage": images(&["item.jpg"])}]
        }),
    );
    let c = collection(&root, "T/t.noudata");
    assert_eq!(rels(&c), ["T/item.jpg"]);
    assert_eq!(c[0].source, CoverSource::Item);
}

#[test]
fn the_playing_item_prefers_its_own_cover_then_its_release_then_the_collection() {
    let (_t, root) = tmp();
    touch(
        &root,
        &["Album/art.jpg", "Album/t1.jpg", "Album/01.mp3", "Album/02.mp3", "PL/mine.jpg"],
    );
    put_json(
        &root,
        "Album/a.noudata",
        &json!({
            "noudataType": "release",
            "playableItemTitle": "Album",
            "releaseData": {"coverImage": images(&["art.jpg"])},
            "mediaList": [
                {"localFilePaths": ["01.mp3"], "itemCoverImage": images(&["t1.jpg"])},
                {"localFilePaths": ["02.mp3"]}
            ]
        }),
    );
    let rel = "Album/a.noudata";
    let doc = parse_noudata_str(&fs::read_to_string(root.join(rel)).unwrap()).unwrap();
    let res = fs_release_resolver(&root.join(rel));
    let item0 = effective_item_covers(&root, &doc, rel, 0, None, &res);
    assert_eq!(rels(&item0), ["Album/t1.jpg"]);
    assert_eq!(item0[0].source, CoverSource::Item);
    let item1 = effective_item_covers(&root, &doc, rel, 1, None, &res);
    assert_eq!(rels(&item1), ["Album/art.jpg"]);

    // A playlist entry: its release's art beats the playlist's own.
    let w = {
        put_json(
            &root,
            "PL/p.noudata",
            &json!({
                "noudataType": "userPlaylist",
                "playableItemTitle": "PL",
                "localPlaybackData": {"localCoverImage": images(&["mine.jpg"])},
                "mediaList": [
                    {"localFilePaths": ["../Album/02.mp3"]},
                    {"localFilePaths": ["../Album/missing-elsewhere.mp3"]}
                ]
            }),
        );
        index(&root)
    };
    let pid = id_of(&w, "PL/p.noudata");
    let items = w.items_of(pid).unwrap();
    assert!(items[0].resolved_from.is_some(), "entry resolves to the album");
    assert_eq!(rels(&w.item_covers(pid, 0).unwrap()), ["Album/art.jpg"]);
    assert_eq!(
        rels(&w.item_covers(pid, 1).unwrap()),
        ["PL/mine.jpg"],
        "no release for the entry: the playlist's cover"
    );
    let aid = id_of(&w, rel);
    assert_eq!(rels(&w.item_covers(aid, 0).unwrap()), ["Album/t1.jpg"]);
}

// ---------------------------------------------------------------------------
// Which entries are accepted
// ---------------------------------------------------------------------------

#[test]
fn missing_files_are_dropped_and_unsafe_or_remote_entries_are_skipped() {
    let (_t, root) = tmp();
    touch(&root, &["Album/a.jpg", "Album/b.webp", "outside.jpg", "Album/c.gif", "Album/01.mp3"]);
    let list = json!({"images": [
        {"imageLocalFilename": "gone.jpg"},
        {"imageLocalFilename": "../outside.jpg"},
        {"imageLocalFilename": "../../escape.jpg"},
        {"imageLocalFilename": "/etc/passwd.jpg"},
        {"imageLocalFilename": "C:\\x.jpg"},
        {"imageLocalFilename": "c.gif"},
        {"imageWebURL": "https://example.org/remote.jpg"},
        {"imageLocalFilename": "a.jpg"},
        {"imageLocalFilename": "a.jpg"},
        {"imageLocalFilename": "b.webp", "imageWebURL": "https://example.org/b.webp"}
    ]});
    put_json(&root, "Album/a.noudata", &release(None, Some(list)));
    let c = collection(&root, "Album/a.noudata");
    assert_eq!(
        rels(&c),
        ["outside.jpg", "Album/a.jpg", "Album/b.webp"],
        "`..` inside the library is fine, anything else is out; duplicates collapse"
    );
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

#[test]
fn a_release_with_three_images_indexes_three_rows_in_order() {
    let (_t, root) = tmp();
    touch(&root, &["Album/c.jpg", "Album/a.jpg", "Album/b.png", "Album/01.mp3"]);
    put_json(
        &root,
        "Album/a.noudata",
        &release(None, Some(images(&["c.jpg", "a.jpg", "b.png"]))),
    );
    let w = index(&root);
    let id = id_of(&w, "Album/a.noudata");
    let rec = w.get_playable(id).unwrap().unwrap();
    assert_eq!(rec.cover_count, 3);
    assert_eq!(rec.cover_relpath.as_deref(), Some("Album/c.jpg"));
    let covers = w.covers_of(id).unwrap();
    assert_eq!(rels(&covers), ["Album/c.jpg", "Album/a.jpg", "Album/b.png"]);
    assert!(covers.iter().all(|c| c.source == CoverSource::Local));

    // Removing one image from disk drops it from the list, not from the declared count.
    fs::remove_file(root.join("Album/a.jpg")).unwrap();
    assert_eq!(rels(&w.covers_of(id).unwrap()), ["Album/c.jpg", "Album/b.png"]);
}

#[test]
fn rewriting_the_noudata_rebuilds_the_rows_and_forgetting_it_removes_them() {
    let (_t, root) = tmp();
    touch(&root, &["Album/a.jpg", "Album/b.jpg", "Album/01.mp3"]);
    put_json(&root, "Album/a.noudata", &release(None, Some(images(&["a.jpg", "b.jpg"]))));
    let mut w = index(&root);
    let id = id_of(&w, "Album/a.noudata");
    assert_eq!(w.get_playable(id).unwrap().unwrap().cover_count, 2);

    put_json(&root, "Album/a.noudata", &release(None, Some(images(&["b.jpg"]))));
    w.ingest_noudata("Album/a.noudata").unwrap();
    assert_eq!(rels(&w.covers_of(id).unwrap()), ["Album/b.jpg"]);
    assert_eq!(w.get_playable(id).unwrap().unwrap().cover_count, 1);

    w.forget_noudata("Album/a.noudata").unwrap();
    let rows: i64 = Connection::open(index_db_path(&LibraryRoot::new(&root)))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM playable_covers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

fn link_track(item_cover: Option<Value>, local: Option<Value>) -> Value {
    let mut item = json!({"localFilePaths": ["01.mp3"]});
    if let Some(c) = item_cover {
        item["itemCoverImage"] = c;
    }
    let mut v = json!({
        "noudataType": "linkTrack",
        "playableItemTitle": "One",
        "referToReleaseNoudata": "a.noudata",
        "mediaList": [item]
    });
    if let Some(l) = local {
        v["localPlaybackData"] = json!({ "localCoverImage": l });
    }
    v
}

#[test]
fn a_link_track_inherits_the_releases_list_unless_it_has_its_own() {
    let (_t, root) = tmp();
    touch(&root, &["Album/r1.jpg", "Album/r2.jpg", "Album/own.jpg", "Album/01.mp3"]);
    put_json(&root, "Album/a.noudata", &release(Some(images(&["r1.jpg", "r2.jpg"])), None));
    put_json(&root, "Album/inherit.noudata", &link_track(None, None));
    put_json(&root, "Album/own.noudata", &link_track(Some(images(&["own.jpg"])), None));
    put_json(
        &root,
        "Album/hidden.noudata",
        &link_track(None, Some(json!({"images": []}))),
    );
    let mut w = index(&root);

    let inherit = id_of(&w, "Album/inherit.noudata");
    assert_eq!(rels(&w.covers_of(inherit).unwrap()), ["Album/r1.jpg", "Album/r2.jpg"]);
    let rec = w.get_playable(inherit).unwrap().unwrap();
    assert_eq!(rec.cover_count, 2);
    assert_eq!(rec.cover_relpath.as_deref(), Some("Album/r1.jpg"));

    let own = id_of(&w, "Album/own.noudata");
    assert_eq!(rels(&w.covers_of(own).unwrap()), ["Album/own.jpg"], "item cover beats release");
    assert_eq!(w.get_playable(own).unwrap().unwrap().cover_count, 1);

    let hidden = id_of(&w, "Album/hidden.noudata");
    assert!(w.covers_of(hidden).unwrap().is_empty());
    assert_eq!(w.get_playable(hidden).unwrap().unwrap().cover_relpath, None);

    // The release changes: the link track follows on the next index update.
    put_json(&root, "Album/a.noudata", &release(Some(images(&["r2.jpg"])), None));
    w.ingest_noudata("Album/a.noudata").unwrap();
    assert_eq!(rels(&w.covers_of(inherit).unwrap()), ["Album/r2.jpg"]);
    assert_eq!(w.get_playable(inherit).unwrap().unwrap().cover_count, 1);

    // The pure resolver agrees with the index.
    let c = collection(&root, "Album/inherit.noudata");
    assert_eq!(rels(&c), ["Album/r2.jpg"]);
}

#[test]
fn referencing_noudata_finds_collection_and_item_level_references() {
    let (_t, root) = tmp();
    touch(&root, &["Album/art.jpg", "Album/solo.jpg", "Album/other.jpg", "Album/01.mp3"]);
    // The user's list hides the artist's `art.jpg`, but the artist's file is still referenced.
    put_json(
        &root,
        "Album/a.noudata",
        &release(Some(images(&["art.jpg"])), Some(images(&["other.jpg"]))),
    );
    put_json(&root, "Album/t.noudata", &link_track(Some(images(&["solo.jpg"])), None));
    let w = index(&root);
    assert_eq!(w.referencing_noudata("Album/art.jpg").unwrap(), ["Album/a.noudata"]);
    assert_eq!(w.referencing_noudata("Album/other.jpg").unwrap(), ["Album/a.noudata"]);
    assert_eq!(w.referencing_noudata("Album/solo.jpg").unwrap(), ["Album/t.noudata"]);
    assert!(w.referencing_noudata("Album/nobody.jpg").unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

/// Turn the current index into what a v3 writer left behind.
fn downgrade_to_v3(root: &Path) {
    let conn = Connection::open(index_db_path(&LibraryRoot::new(root))).unwrap();
    conn.execute_batch(
        "DROP TABLE playable_covers;
         ALTER TABLE playables DROP COLUMN covers_hidden;
         UPDATE meta SET value = '3' WHERE key = 'schema_version';",
    )
    .unwrap();
}

#[test]
fn a_v3_index_migrates_in_place_and_fills_the_covers_on_the_next_scan() {
    let (_t, root) = tmp();
    touch(&root, &["Album/a.jpg", "Album/b.jpg", "Album/01.mp3"]);
    put_json(&root, "Album/a.noudata", &release(None, Some(images(&["a.jpg", "b.jpg"]))));
    let before_id = {
        let w = index(&root);
        id_of(&w, "Album/a.noudata")
    };
    downgrade_to_v3(&root);

    // A reader opened on the old file works (no covers) until the writer has migrated it.
    let reader = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    let rec = reader.get_playable(before_id).unwrap().unwrap();
    assert_eq!(rec.cover_count, 0);
    assert!(reader.covers_of(before_id).unwrap().is_empty());

    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let ver: String = Connection::open(index_db_path(&LibraryRoot::new(&root)))
        .unwrap()
        .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, SCHEMA_VERSION.to_string());
    assert_eq!(SCHEMA_VERSION, 4);
    // Nothing else was lost.
    let migrated = w.get_playable(before_id).unwrap().unwrap();
    assert_eq!(migrated.title.as_deref(), Some("Album"));
    assert_eq!(w.items_of(before_id).unwrap().len(), 1);

    let rep = w.lazy_rescan().unwrap();
    assert_eq!(rep.updated, 1, "the migrated row is re-read: {rep:?}");
    assert_eq!(rels(&w.covers_of(before_id).unwrap()), ["Album/a.jpg", "Album/b.jpg"]);
    assert_eq!(w.get_playable(before_id).unwrap().unwrap().cover_count, 2);
    assert!(!w.lazy_rescan().unwrap().changed(), "steady state afterwards");

    // The reader that was open during the migration sees the new data too.
    assert_eq!(reader.get_playable(before_id).unwrap().unwrap().cover_count, 2);
    assert_eq!(reader.covers_of(before_id).unwrap().len(), 2);
}

#[test]
fn an_index_from_a_newer_schema_is_rebuilt_not_trusted() {
    let (_t, root) = tmp();
    touch(&root, &["Album/a.jpg", "Album/01.mp3"]);
    put_json(&root, "Album/a.noudata", &release(None, Some(images(&["a.jpg"]))));
    drop(index(&root));
    Connection::open(index_db_path(&LibraryRoot::new(&root)))
        .unwrap()
        .execute("UPDATE meta SET value = ?1", [(SCHEMA_VERSION + 1).to_string()])
        .unwrap();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    assert_eq!(w.count_playables().unwrap(), 0, "derived data starts over");
    assert_eq!(w.lazy_rescan().unwrap().ingested, 1);
    let id = id_of(&w, "Album/a.noudata");
    assert_eq!(rels(&w.covers_of(id).unwrap()), ["Album/a.jpg"]);
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

#[test]
fn thumbnail_paths_sit_next_to_the_image() {
    assert_eq!(
        thumbnail_relpath("Music/A/cover.jpg", ThumbTier::T512, "jpg"),
        "Music/A/thumbnails/cover-512.jpg"
    );
    assert_eq!(thumbnail_relpath("cover.png", ThumbTier::T128, "webp"), "thumbnails/cover-128.webp");
}

#[test]
fn lowercase_thumbnails_dir_beats_the_capitalised_one_and_jpeg_beats_webp() {
    let (_t, root) = tmp();
    touch(
        &root,
        &[
            "A/Thumbnails/cover-128.jpg",
            "A/thumbnails/cover-128.webp",
            "A/thumbnails/cover-128.jpg",
        ],
    );
    let t = resolve_thumbnail(&root, "A/cover.jpg", 128).unwrap();
    assert_eq!(t.relpath, "A/thumbnails/cover-128.jpg");
    fs::remove_file(root.join("A/thumbnails/cover-128.jpg")).unwrap();
    assert_eq!(
        resolve_thumbnail(&root, "A/cover.jpg", 128).unwrap().relpath,
        "A/thumbnails/cover-128.webp",
        "lowercase dir first, then the fallback spelling"
    );
    fs::remove_file(root.join("A/thumbnails/cover-128.webp")).unwrap();
    assert_eq!(
        resolve_thumbnail(&root, "A/cover.jpg", 128).unwrap().relpath,
        "A/Thumbnails/cover-128.jpg"
    );
    assert_eq!(resolve_thumbnail(&root, "A/other.jpg", 128), None);
}

#[test]
fn thumbnail_tier_is_the_smallest_that_is_big_enough_else_the_largest() {
    let (_t, root) = tmp();
    touch(
        &root,
        &[
            "cover-ignored.txt",
            "thumbnails/cover-128.jpg",
            "thumbnails/cover-512.jpg",
            "thumbnails/cover-1024.jpg",
        ],
    );
    let pick = |px| resolve_thumbnail(&root, "cover.jpg", px).map(|t| t.tier);
    assert_eq!(pick(100), Some(ThumbTier::T128));
    assert_eq!(pick(128), Some(ThumbTier::T128));
    assert_eq!(pick(300), Some(ThumbTier::T512));
    assert_eq!(pick(5000), Some(ThumbTier::T1024));

    fs::remove_file(root.join("thumbnails/cover-512.jpg")).unwrap();
    fs::remove_file(root.join("thumbnails/cover-1024.jpg")).unwrap();
    assert_eq!(pick(5000), Some(ThumbTier::T128), "only 128 present");
    assert_eq!(pick(300), Some(ThumbTier::T128));
    fs::remove_file(root.join("thumbnails/cover-128.jpg")).unwrap();
    assert_eq!(pick(100), None);
}

#[test]
fn thumbnail_lookup_refuses_an_unsafe_image_path() {
    let (_t, root) = tmp();
    touch(&root, &["thumbnails/cover-128.jpg"]);
    assert_eq!(resolve_thumbnail(&root, "../cover.jpg", 128), None);
    assert_eq!(resolve_thumbnail(&root, "/cover.jpg", 128), None);
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

fn spec(name: &str, alt: Option<&str>) -> CoverImageSpec {
    CoverImageSpec {
        relpath_from_noudata: name.to_string(),
        alt_text: alt.map(str::to_string),
    }
}

fn to_value(doc: &plippa_noubin::NoudataDocument) -> Value {
    serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap()
}

fn doc_of(v: &Value) -> plippa_noubin::NoudataDocument {
    parse_noudata_str(&v.to_string()).unwrap()
}

#[test]
fn edit_reorders_adds_and_removes_and_keeps_unknown_keys_and_alt_text() {
    let mut doc = doc_of(&release(
        Some(images(&["art.jpg"])),
        Some(json!({
            "images": [
                {"imageLocalFilename": "a.jpg", "altText": "front", "isImageOfNoubin": true,
                 "futureKey": {"x": 1}},
                {"imageLocalFilename": "b.png", "altText": "back"},
                {"imageLocalFilename": "gone.jpg"}
            ],
            "listFutureKey": 7
        })),
    ));
    apply_cover_edit(
        &mut doc,
        &CoverEdit {
            target: CoverTarget::Collection,
            images: vec![spec("b.png", None), spec("new.webp", Some("  fresh \n")), spec("a.jpg", None)],
        },
    )
    .unwrap();
    let v = to_value(&doc);
    let list = &v["localPlaybackData"]["localCoverImage"];
    assert_eq!(list["listFutureKey"], 7);
    let names: Vec<&str> = list["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["imageLocalFilename"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["b.png", "new.webp", "a.jpg"]);
    assert_eq!(list["images"][0]["altText"], "back");
    assert_eq!(list["images"][1]["altText"], "fresh");
    assert_eq!(list["images"][2]["altText"], "front");
    assert_eq!(list["images"][2]["isImageOfNoubin"], true);
    assert_eq!(list["images"][2]["futureKey"], json!({"x": 1}));
    // The artist's list is never touched.
    assert_eq!(v["releaseData"]["coverImage"], images(&["art.jpg"]));
    // And the result parses back to the same list.
    let again = doc_of(&v);
    assert_eq!(again.cover_local_filename(), Some("b.png"));
}

#[test]
fn an_explicit_empty_alt_text_clears_it() {
    let mut doc = doc_of(&release(
        None,
        Some(json!({"images": [{"imageLocalFilename": "a.jpg", "altText": "front"}]})),
    ));
    apply_cover_edit(
        &mut doc,
        &CoverEdit {
            target: CoverTarget::Collection,
            images: vec![spec("a.jpg", Some(""))],
        },
    )
    .unwrap();
    assert!(to_value(&doc)["localPlaybackData"]["localCoverImage"]["images"][0]
        .get("altText")
        .is_none());
}

#[test]
fn copy_on_write_keeps_the_artists_entry_data_when_it_is_re_listed() {
    let mut doc = doc_of(&release(
        Some(json!({"images": [{"imageLocalFilename": "art.jpg", "isImageOfNoubin": true}]})),
        None,
    ));
    apply_cover_edit(
        &mut doc,
        &CoverEdit {
            target: CoverTarget::Collection,
            images: vec![spec("art.jpg", None), spec("mine.jpg", None)],
        },
    )
    .unwrap();
    let v = to_value(&doc);
    assert_eq!(v["localPlaybackData"]["localCoverImage"]["images"][0]["isImageOfNoubin"], true);
    assert_eq!(v["releaseData"]["coverImage"]["images"][0]["isImageOfNoubin"], true);
}

#[test]
fn deleting_everything_writes_an_explicit_empty_list_that_still_hides_the_artists_art() {
    let (_t, root) = tmp();
    touch(&root, &["Album/art.jpg", "Album/mine.jpg", "Album/01.mp3"]);
    let mut doc = doc_of(&release(Some(images(&["art.jpg"])), Some(images(&["mine.jpg"]))));
    apply_cover_edit(
        &mut doc,
        &CoverEdit {
            target: CoverTarget::Collection,
            images: vec![],
        },
    )
    .unwrap();
    let v = to_value(&doc);
    assert_eq!(v["localPlaybackData"]["localCoverImage"], json!({"images": []}));
    // Written, re-read and resolved: still nothing, and the artist's entry is intact.
    put_json(&root, "Album/a.noudata", &v);
    assert!(collection(&root, "Album/a.noudata").is_empty());
    assert_eq!(v["releaseData"]["coverImage"], images(&["art.jpg"]));
}

#[test]
fn item_edit_targets_the_one_based_media_item() {
    let mut doc = doc_of(&json!({
        "noudataType": "looseTrack",
        "playableItemTitle": "T",
        "mediaList": [{"localFilePaths": ["s.mp3"]}]
    }));
    apply_cover_edit(
        &mut doc,
        &CoverEdit {
            target: CoverTarget::Item { position: 1 },
            images: vec![spec("t.jpg", Some("cover"))],
        },
    )
    .unwrap();
    let v = to_value(&doc);
    assert_eq!(v["mediaList"][0]["itemCoverImage"]["images"][0]["imageLocalFilename"], "t.jpg");
    assert!(v.get("localPlaybackData").is_none());

    for position in [0, 2] {
        let before = doc.clone();
        let err = apply_cover_edit(
            &mut doc,
            &CoverEdit {
                target: CoverTarget::Item { position },
                images: vec![spec("x.jpg", None)],
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            NoubinError::Cover(CoverEditError::NoSuchItem { position: p }) if p == position
        ));
        assert_eq!(doc, before);
    }
}

#[test]
fn a_bad_spec_fails_the_whole_edit_and_leaves_the_document_untouched() {
    let original = doc_of(&release(Some(images(&["art.jpg"])), Some(images(&["mine.jpg"]))));
    let bad = [
        ("../x.jpg", "unsafe"),
        ("/abs/x.jpg", "unsafe"),
        ("sub/../x.jpg", "unsafe"),
        ("a\0b.jpg", "unsafe"),
        ("c:\\x.jpg", "unsafe"),
        ("x.gif", "ext"),
        ("noext", "ext"),
        ("", "unsafe"),
    ];
    for (name, kind) in bad {
        let mut doc = original.clone();
        let err = apply_cover_edit(
            &mut doc,
            &CoverEdit {
                target: CoverTarget::Collection,
                images: vec![spec("fine.jpg", None), spec(name, None)],
            },
        )
        .unwrap_err();
        match (kind, &err) {
            ("unsafe", NoubinError::Cover(CoverEditError::UnsafePath { .. })) => {}
            ("ext", NoubinError::Cover(CoverEditError::UnsupportedExtension { .. })) => {}
            _ => panic!("{name:?}: unexpected {err:?}"),
        }
        assert_eq!(doc, original, "{name:?} left a partial edit behind");
    }

    let mut doc = original.clone();
    let too_many = (0..33).map(|i| spec(&format!("i{i}.jpg"), None)).collect();
    let err = apply_cover_edit(
        &mut doc,
        &CoverEdit {
            target: CoverTarget::Collection,
            images: too_many,
        },
    )
    .unwrap_err();
    assert!(matches!(err, NoubinError::Cover(CoverEditError::TooManyImages { got: 33, .. })));
    assert_eq!(doc, original);
}

#[test]
fn an_edited_document_is_indexed_with_the_new_list() {
    let (_t, root) = tmp();
    touch(&root, &["Album/a.jpg", "Album/b.jpg", "Album/01.mp3"]);
    put_json(&root, "Album/a.noudata", &release(None, None));
    let mut w = index(&root);
    let id = id_of(&w, "Album/a.noudata");
    assert_eq!(w.get_playable(id).unwrap().unwrap().cover_count, 0);

    let path = root.join("Album/a.noudata");
    let mut doc = parse_noudata_str(&fs::read_to_string(&path).unwrap()).unwrap();
    apply_cover_edit(
        &mut doc,
        &CoverEdit {
            target: CoverTarget::Collection,
            images: vec![spec("b.jpg", None), spec("a.jpg", None)],
        },
    )
    .unwrap();
    doc.write_to_path(&path).unwrap();
    w.ingest_noudata("Album/a.noudata").unwrap();
    assert_eq!(rels(&w.covers_of(id).unwrap()), ["Album/b.jpg", "Album/a.jpg"]);
}

#[test]
fn an_explicit_empty_list_survives_parsing_sanitising_and_writing() {
    let v = release(Some(images(&["art.jpg"])), Some(json!({"images": [], "k": 1})));
    let mut doc = doc_of(&v);
    plippa_noubin::sanitise_document(&mut doc, plippa_noubin::SanitiseOptions::default());
    let out = to_value(&doc);
    assert_eq!(out["localPlaybackData"]["localCoverImage"], json!({"images": [], "k": 1}));
    assert_eq!(doc.cover_local_filename(), None, "hidden, not the artist's art");
}

#[test]
fn referencing_noudata_fails_closed_when_a_document_cannot_be_read_or_parsed() {
    let (_t, root) = tmp();
    touch(&root, &["Album/art.jpg", "Album/01.mp3"]);
    put_json(&root, "Album/a.noudata", &release(Some(images(&["art.jpg"])), None));
    put_json(&root, "Album/t.noudata", &link_track(None, None));
    let w = index(&root);
    assert!(w.referencing_noudata("Album/art.jpg").unwrap().contains(&"Album/a.noudata".to_string()));

    // Unparseable after indexing (a torn read): the answer is an error, not "nobody".
    put(&root, "Album/t.noudata", "{ not json");
    assert!(w.referencing_noudata("Album/nobody.jpg").is_err());

    // Unreadable (vanished stick): also an error.
    put_json(&root, "Album/t.noudata", &link_track(None, None));
    fs::remove_file(root.join("Album/t.noudata")).unwrap();
    assert!(w.referencing_noudata("Album/nobody.jpg").is_err());
}

#[test]
fn referencing_noudata_sees_json_escaped_filenames() {
    let (_t, root) = tmp();
    touch(&root, &["Album/art.jpg", "Album/01.mp3"]);
    let doc = serde_json::to_string_pretty(&release(Some(images(&["art.jpg"])), None)).unwrap();
    // The same document with the file name spelt through JSON escapes: no raw "art.jpg" remains.
    assert!(doc.contains("art.jpg"));
    put(&root, "Album/a.noudata", &doc.replace("art.jpg", "\\u0061rt\\u002ejpg"));
    let w = index(&root);
    assert_eq!(w.referencing_noudata("Album/art.jpg").unwrap(), ["Album/a.noudata"]);
}
