//! NB_08 whole-document sanitiser: hostile fixtures (`NBSecurityRequirements.md` §7).

use plippa_noubin::{
    check_json_object_limits, parse_noudata_with_warnings, sanitise_document, NoudataDocument,
    SanitiseOptions,
};
use serde_json::{json, Value};

fn parse(v: &Value) -> NoudataDocument {
    parse_noudata_with_warnings(v.to_string().as_bytes())
        .unwrap()
        .0
}

fn hostile() -> Value {
    let long = "x".repeat(100_000);
    json!({
        "_whatAmILookingAt": "hi",
        "noudataVersion": "0.1.0",
        "noubinKey": "https://evil.example/x",
        "noubinKeyNormalised": "evil.example/x/",
        "playableItemTitle": "  Ti\u{202E}tle\u{200B}\u{0007}\n with   junk ",
        "playableItemFolderSafeTitle": "../../etc/passwd",
        "referToReleaseNoudata": "../../../etc/passwd",
        "noudataType": "release",
        "playableItemProfile": "music",
        "releaseData": {
            "credits": {
                "primaryArtists": [
                    {"primaryArtistName": "Ar\u{202E}tist"},
                    {"primaryArtistName": "\u{200B}\u{0007}"}
                ],
                "primaryArtistFolderSafeName": "/abs/../bad",
                "contributors": [
                    {"contributorName": "Bob", "contributorURL": "javascript:alert(1)"},
                    {"contributorName": "Eve", "contributorURL": "http://EVE.example/a b"}
                ],
                "additionalCreditsText": long,
                "orgURL": "data:text/html,<script>alert(1)</script>"
            },
            "coverImage": {"images": [
                {"imageLocalFilename": "../../secret.jpg", "imageWebURL": "javascript:alert(1)"},
                {"imageWebURL": "http://cdn.example/cover.jpg?x=\"<b>"}
            ]},
            "releasePlatforms": [
                {"platformName": "Evil", "platformType": "store", "platformURL": "javascript:alert(document.cookie)"},
                {"platformName": "Good<img src=x onerror=alert(1)>", "platformType": "streaming",
                 "platformURL": "http://good.example/album", "customVerb": "Sp\u{202E}in"},
                {"platformName": "Creds", "platformURL": "https://user:pw@good.example/"}
            ],
            "furtherLinks": [
                {"linkTitle": "ok", "linkURL": "https://a.example/"},
                {"linkTitle": "bad", "linkURL": "file:///etc/passwd"}
            ],
            "legalNotice": long,
            "weird": {"deep": {"URLish": 1}, "someURL": "vbscript:x", "text": "a\u{202E}b"}
        },
        "localPlaybackData": {"shuffle": true, "searchString": "'; DROP TABLE playables;--"},
        "mediaList": [{
            "mediaTitle": "T1'); DROP TABLE items;--",
            "uniteData": {"expectedFilename": "../../evil", "freeDownloadURL": "ftp://x.example/a.mp3"},
            "localFilePaths": ["/etc/passwd", "C:\\x.mp3", "a/../../b.mp3", "ok/fine.mp3"],
            "itemUserNotes": {"userTags": ["a", "a", "\u{200B}", "x".repeat(200)], "userRating": 5},
            "lyricsTranscript": "l1\r\nl2\u{0007}",
            "cueLists": [{"cueListType": "nav", "cues": [
                {"startTime": "0:00", "cueTextSpans": [{"text": "Intro\u{202E}"}], "cueURL": "javascript:alert(1)"},
                {"startTime": "0:10", "cueTextSpans": [{"text": "y".repeat(5000)}]}
            ]}]
        }]
    })
}

#[test]
fn hostile_document_is_neutralised() {
    let mut doc = parse(&hostile());
    let warnings = sanitise_document(&mut doc, SanitiseOptions::fetched());
    assert!(!warnings.is_empty());

    // Local-only properties never survive a fetch.
    assert!(doc.noubin_key.is_none() && doc.noubin_key_normalised.is_none());
    assert!(doc.refer_to_release_noudata.is_none() && doc.local_playback_data.is_none());
    assert!(doc.media_list[0].local_file_paths.is_none());
    assert!(doc.media_list[0].item_user_notes.is_none());

    assert_eq!(doc.playable_item_title.as_deref(), Some("Title with junk"));
    let folder = doc.playable_item_folder_safe_title.as_deref().unwrap();
    assert!(
        !folder.contains('/') && !folder.starts_with('.'),
        "{folder}"
    );

    let rd = doc.release_data.as_ref().unwrap();
    let credits = rd.credits.as_ref().unwrap();
    let artists = credits.primary_artists.as_ref().unwrap();
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0].primary_artist_name, "Artist");
    let folder = credits.primary_artist_folder_safe_name.as_deref().unwrap();
    assert!(!folder.contains('/'), "{folder}");
    let contributors = credits.contributors.as_ref().unwrap();
    assert_eq!(contributors[0].contributor_url, None);
    assert_eq!(
        contributors[1].contributor_url, None,
        "raw space in a URL is refused"
    );
    // Long text capped at 64 KiB.
    assert_eq!(
        credits.extra["additionalCreditsText"]
            .as_str()
            .unwrap()
            .len(),
        64 * 1024
    );
    assert_eq!(rd.extra["legalNotice"].as_str().unwrap().len(), 64 * 1024);
    // Unknown properties: URL-looking key with a bad scheme removed, text stripped.
    assert!(rd.extra["weird"].get("someURL").is_none());
    assert_eq!(rd.extra["weird"]["text"], "ab");
    assert!(credits.extra.get("orgURL").is_none());

    let imgs = &rd.cover_image.as_ref().unwrap().images;
    assert_eq!(imgs.len(), 1);
    assert!(imgs[0].image_local_filename.is_none());
    let u = imgs[0].image_web_url.as_deref().unwrap();
    assert!(
        u.starts_with("https://cdn.example/cover.jpg?x=") && !u.contains(['"', '<', '>']),
        "{u}"
    );

    let plats = rd.release_platforms.as_ref().unwrap();
    assert_eq!(
        plats.len(),
        1,
        "javascript: and userinfo platforms are dropped"
    );
    assert_eq!(plats[0].platform_url, "https://good.example/album");
    assert_eq!(
        plats[0].platform_name, "Good<img src=x onerror=alert(1)>",
        "text is kept as text (escaped by the renderer)"
    );
    assert_eq!(plats[0].custom_verb.as_deref(), Some("Spin"));
    let links = rd.further_links.as_ref().unwrap();
    assert_eq!(links.len(), 1);

    let item = &doc.media_list[0];
    assert_eq!(
        item.media_title.as_deref(),
        Some("T1'); DROP TABLE items;--"),
        "SQL text stays inert text"
    );
    assert!(
        item.unite_data.is_none(),
        "nothing usable left in uniteData"
    );
    assert_eq!(item.lyrics_transcript.as_deref(), Some("l1\nl2"));
    let cues = &item.cue_lists[0].cues;
    assert_eq!(cues[0].cue_text_spans[0].text, "Intro");
    assert_eq!(cues[0].cue_url, None);
    assert_eq!(cues[1].cue_text_spans[0].text.len(), 2048);
}

#[test]
fn imported_profile_keeps_local_data_but_sanitises_it() {
    let mut doc = parse(&hostile());
    sanitise_document(&mut doc, SanitiseOptions::imported());
    assert_eq!(doc.noubin_key.as_deref(), Some("https://evil.example/x"));
    assert!(
        doc.refer_to_release_noudata.is_none(),
        "../../../ is refused"
    );
    let paths = doc.media_list[0].local_file_paths.as_ref().unwrap();
    assert_eq!(paths, &vec!["ok/fine.mp3".to_string()]);
    let notes = doc.media_list[0].item_user_notes.as_ref().unwrap();
    assert_eq!(
        notes.user_tags.as_ref().unwrap().len(),
        2,
        "dupes/empty removed, long tag capped"
    );
    assert_eq!(notes.user_tags.as_ref().unwrap()[1].chars().count(), 64);
    assert_eq!(
        doc.local_playback_data
            .as_ref()
            .unwrap()
            .search_string
            .as_deref(),
        Some("'; DROP TABLE playables;--")
    );
    // Noubin v0.4.1: the legacy `shuffle` in the input is dropped, not echoed on write.
    let text = doc.to_json_pretty().unwrap();
    assert!(!text.contains("shuffle"), "retired key written back: {text}");
}

#[test]
fn a_single_leading_parent_is_tolerated_for_playlist_refs_when_imported() {
    let mut doc = parse(&json!({
        "referToReleaseNoudata": "../Music/Album/_Album.noudata",
        "mediaList": [{"mediaTitle": "x"}]
    }));
    sanitise_document(&mut doc, SanitiseOptions::imported());
    assert_eq!(
        doc.refer_to_release_noudata.as_deref(),
        Some("../Music/Album/_Album.noudata")
    );
}

#[test]
fn sanitised_document_serialises_as_valid_noudata() {
    let mut doc = parse(&hostile());
    sanitise_document(&mut doc, SanitiseOptions::fetched());
    let text = doc.to_json_pretty().unwrap();
    let (again, _) = parse_noudata_with_warnings(text.as_bytes()).unwrap();
    assert_eq!(again.media_list.len(), 1);
    // Idempotent: a second pass changes nothing.
    let mut twice = again.clone();
    let w = sanitise_document(&mut twice, SanitiseOptions::fetched());
    assert_eq!(twice, again, "warnings: {w:?}");
}

#[test]
fn deep_extras_are_trimmed() {
    let mut deep = json!("leaf");
    for _ in 0..50 {
        deep = json!({ "n": deep });
    }
    let mut doc = parse(&json!({"mediaList": [], "bomb": deep}));
    sanitise_document(&mut doc, SanitiseOptions::fetched());
    let mut d = 0;
    let mut v = &doc.extra["bomb"];
    while let Some(n) = v.get("n") {
        d += 1;
        v = n;
    }
    assert!(d <= 8, "depth {d}");
}

#[test]
fn json_guard_blocks_bombs_before_parsing() {
    let deep = format!("{{\"a\":{}0{}}}", "[".repeat(200), "]".repeat(200));
    assert!(check_json_object_limits(deep.as_bytes()).is_err());
    assert!(check_json_object_limits(b"<!doctype html><html>").is_err());
    assert!(check_json_object_limits(br#"{"mediaList":[]}"#).is_ok());
}
