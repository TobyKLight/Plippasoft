//! NB_01: typed `.noudata` schema — release/credits/cues/playback data, warnings,
//! round trips and §4.9 classification.

use plippa_noubin::{
    classify_type, effective_type, fs_release_resolver, is_dynamic_playlist, is_playlist,
    parse_noudata, parse_noudata_str, parse_noudata_with_warnings, CueListType, NoubinTimestamp,
    NoudataDocument, NoudataType, OverrideResumeBehaviour, PlatformType, PlayableItemProfile,
};
use serde_json::Value;
use std::path::PathBuf;

const MIXTAPE: &[u8] = include_bytes!("../fixtures/mixtape.noudata");
const AUDIOBOOK: &[u8] = include_bytes!("../fixtures/audiobook.noudata");
const PLAYLIST: &[u8] = include_bytes!("../fixtures/playlist.noudata");
const MESSY_TYPED: &[u8] = include_bytes!("../fixtures/messy_typed.noudata");
const MESSY_UNKNOWN: &[u8] = include_bytes!("../fixtures/messy_unknown.noudata");

fn ts(s: &str) -> NoubinTimestamp {
    NoubinTimestamp::parse(s).unwrap()
}

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

// ---- typed release / credits ------------------------------------------------

#[test]
fn mixtape_release_typed() {
    let (doc, warnings) = parse_noudata_with_warnings(MIXTAPE).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let rd = doc.release_data.as_ref().unwrap();
    assert_eq!(doc.noudata_type, Some(NoudataType::Release));
    assert_eq!(doc.profile(), PlayableItemProfile::Mix);
    assert_eq!(
        doc.playable_item_folder_safe_title.as_deref(),
        Some("Japanese Rocksteady Set Vol 3")
    );
    assert_eq!(
        doc.playable_item_url_safe_title.as_deref(),
        Some("japanese-rocksteady-vol-3")
    );
    // override name wins over the primaryArtists list
    assert_eq!(
        doc.primary_artist_display().as_deref(),
        Some("Japanese Rude Sounds b2b Friends")
    );
    let credits = rd.credits.as_ref().unwrap();
    assert_eq!(
        credits.contributors.as_ref().unwrap()[0].contributor_name,
        "Sam"
    );
    assert_eq!(doc.cover_local_filename(), Some("cover.jpg"));

    let platforms = rd.release_platforms.as_ref().unwrap();
    assert_eq!(platforms[0].effective_type(), PlatformType::Streaming);
    assert_eq!(platforms[1].custom_verb.as_deref(), Some("read about"));
    assert_eq!(
        rd.further_links.as_ref().unwrap()[0].link_url,
        "https://example.com/tracklist"
    );
    // legalNotice is not typed, but preserved
    assert!(rd.extra.contains_key("legalNotice"));
}

#[test]
fn primary_artists_joined_when_no_override() {
    let doc = parse_noudata(AUDIOBOOK).unwrap();
    assert_eq!(
        doc.primary_artist_display().as_deref(),
        Some("J.R.R. Tolkien, Andy Serkis")
    );
    assert_eq!(doc.profile(), PlayableItemProfile::Audiobook);
}

#[test]
fn legacy_flat_primary_artists_are_no_longer_interpreted_but_preserved() {
    let doc = parse_noudata_str(
        r#"{"releaseData":{"primaryArtists":[{"primaryArtistName":"Old"}]},
            "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();
    assert_eq!(doc.primary_artist_display(), None);
    assert!(doc
        .release_data
        .as_ref()
        .unwrap()
        .extra
        .contains_key("primaryArtists"));
}

// ---- cues ---------------------------------------------------------------------

#[test]
fn mixtape_cue_lists_typed() {
    let doc = parse_noudata(MIXTAPE).unwrap();
    let lists = &doc.media_list[0].cue_lists;
    assert_eq!(lists.len(), 3);

    let nav = &lists[0];
    assert_eq!(nav.list_type(), CueListType::Nav);
    assert!(nav.is_navigable(), "nav defaults to navigable");
    assert_eq!(nav.cue_list_title.as_deref(), Some("Chapters"));
    assert_eq!(nav.cues.len(), 3);
    assert_eq!(
        nav.cues[1].text(),
        "Peak time",
        "spans concatenate without spaces"
    );
    assert_eq!(nav.cues[1].start_time, ts("10:00"));

    let credits = &lists[1];
    assert_eq!(credits.list_type(), CueListType::Credits);
    assert!(credits.is_navigable(), "credits default to navigable");
    assert_eq!(credits.cue_list_language.as_deref(), Some("en"));
    assert_eq!(
        credits.cues[0].cue_url.as_deref(),
        Some("https://example.com/babylon")
    );

    let text = &lists[2];
    assert_eq!(text.list_type(), CueListType::Text);
    assert!(
        text.is_navigable(),
        "explicit cueListNavigable overrides the text default"
    );
}

#[test]
fn navigable_default_by_type() {
    let doc = parse_noudata_str(
        r#"{"mediaList":[{"cueLists":[
            {"cues":[{"startTime":"0","cueTextSpans":[{"text":"a"}]}]},
            {"cueListType":"text","cues":[{"startTime":"0","cueTextSpans":[{"text":"a"}]}]},
            {"cueListType":"nav","cueListNavigable":false,"cues":[{"startTime":"0","cueTextSpans":[{"text":"a"}]}]}
        ]}]}"#,
    )
    .unwrap();
    let l = &doc.media_list[0].cue_lists;
    assert_eq!(l[0].list_type(), CueListType::Other);
    assert!(!l[0].is_navigable());
    assert!(!l[1].is_navigable());
    assert!(!l[2].is_navigable(), "explicit false beats the nav default");
}

#[test]
fn cue_end_time_inference() {
    let doc = parse_noudata(MIXTAPE).unwrap();
    let nav = &doc.media_list[0].cue_lists[0];
    // explicit endTime
    assert_eq!(nav.resolved_end_ms(0, Some(4_000_000)), Some(600_000));
    // omitted ⇒ next cue's start
    assert_eq!(nav.resolved_end_ms(1, Some(4_000_000)), Some(45 * 60_000));
    // last cue, omitted ⇒ item end (or unknown)
    assert_eq!(nav.resolved_end_ms(2, Some(4_000_000)), Some(4_000_000));
    assert_eq!(nav.resolved_end_ms(2, None), None);
    assert_eq!(nav.resolved_end_ms(9, None), None);
}

#[test]
fn active_cue_from_media_position() {
    let doc = parse_noudata(MIXTAPE).unwrap();
    let credits = &doc.media_list[0].cue_lists[1];
    assert_eq!(credits.active_cue_index(0, Some(100_000_000)), Some(0));
    assert_eq!(
        credits.active_cue_index(392_999, Some(100_000_000)),
        Some(0)
    );
    assert_eq!(
        credits.active_cue_index(393_000, Some(100_000_000)),
        Some(1)
    );
    assert_eq!(
        credits.active_cue_index(200_000_000, Some(100_000_000)),
        None
    );
    // gap before the first cue of a list that starts later
    let nav = &doc.media_list[0].cue_lists[0];
    assert_eq!(
        nav.active_cue_index(30 * 60_000, Some(4_000_000_000)),
        Some(1)
    );
}

#[test]
fn cues_beyond_media_length_are_dropped_once_length_known() {
    let mut doc = parse_noudata(MIXTAPE).unwrap();
    let nav = &mut doc.media_list[0].cue_lists[0];
    let w = nav.drop_beyond_length(30 * 60_000, "nav");
    assert_eq!(nav.cues.len(), 2);
    assert_eq!(w.len(), 1);
}

#[test]
fn bad_cues_are_dropped_with_warnings_good_cues_survive() {
    let (doc, warnings) = parse_noudata_with_warnings(MESSY_TYPED).unwrap();
    let lists = &doc.media_list[0].cue_lists;
    // "nav" (empty cues) and "text" (only a cue with no spans) lists are dropped entirely.
    assert_eq!(lists.len(), 1);
    let cues = &lists[0].cues;
    let texts: Vec<String> = cues.iter().map(|c| c.text()).collect();
    assert_eq!(texts, ["good one", "second good"]);
    assert_eq!(
        lists[0].list_type(),
        CueListType::Other,
        "unknown cueListType ⇒ other"
    );
    assert!(cues[1].extra.contains_key("futureCueField"));

    let cue_warnings = warnings
        .iter()
        .filter(|w| w.path.contains("cueLists[0].cues["))
        .count();
    assert!(cue_warnings >= 5, "{warnings:#?}");
    assert!(warnings.iter().any(|w| w.message.contains("overlaps")));
    assert!(warnings.iter().any(|w| w.message.contains("after endTime")));
    assert!(warnings
        .iter()
        .any(|w| w.path == "mediaList[1].cueLists[1]"));
}

#[test]
fn single_bad_cue_still_yields_the_good_ones() {
    let (doc, warnings) = parse_noudata_with_warnings(
        br#"{"mediaList":[{"localFilePaths":["a.mp3"],"cueLists":[{"cueListType":"nav","cues":[
            {"startTime":"0","cueTextSpans":[{"text":"one"}]},
            {"startTime":"5"},
            {"startTime":"10","cueTextSpans":[{"text":"three"}]}
        ]}]}]}"#,
    )
    .unwrap();
    let cues = &doc.media_list[0].cue_lists[0].cues;
    assert_eq!(cues.len(), 2);
    assert_eq!(warnings.len(), 1);
}

// ---- local playback data ------------------------------------------------------

#[test]
fn audiobook_playback_data_typed() {
    let doc = parse_noudata(AUDIOBOOK).unwrap();
    assert_eq!(
        doc.local_playback_data.as_ref().unwrap().resume_at_item,
        Some(2)
    );
    let item = &doc.media_list[0];
    let ilp = item.item_local_playback_data.as_ref().unwrap();
    assert_eq!(ilp.start_from, Some(ts("0:12")));
    assert_eq!(
        ilp.resume_from_time,
        Some(NoubinTimestamp::from_millis(5_000_500))
    );
    assert_eq!(
        ilp.effective_override_resume(),
        OverrideResumeBehaviour::AlwaysResume
    );
    assert_eq!(ilp.use_specific_alternate, Some(1));
    assert_eq!(ilp.repeat_item, Some(false));
    assert_eq!(item.lyrics_transcript.as_deref(), Some("Chapter one."));
    assert_eq!(doc.media_list[1].item_local_playback_data, None);
}

#[test]
fn playlist_local_playback_data_typed() {
    let doc = parse_noudata(PLAYLIST).unwrap();
    let l = doc.local_playback_data.as_ref().unwrap();
    // Noubin v0.4.1: the retired player keys are neither modelled nor written.
    assert!(l.extra.is_empty(), "{:?}", l.extra);
    let out: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
    for retired in ["shuffle", "repeatList", "stopAfterEach"] {
        assert!(
            out["localPlaybackData"].get(retired).is_none(),
            "{retired} written back"
        );
    }
    let notes = l.user_notes.as_ref().unwrap();
    assert_eq!(
        notes.user_rating,
        Some(0),
        "rating 0 is distinct from unrated"
    );
    assert_eq!(
        notes.user_tags.as_deref(),
        Some(&["chill".to_string(), "rain".to_string()][..])
    );
    assert_eq!(notes.created_by_user.as_deref(), Some("toby"));
    assert_eq!(doc.cover_local_filename(), Some("rain.png"));
    assert_eq!(
        doc.media_list[0]
            .item_user_notes
            .as_ref()
            .unwrap()
            .user_tags
            .as_deref(),
        Some(&["sad".to_string()][..])
    );
}

#[test]
fn retired_local_playback_keys_are_dropped_without_warnings() {
    let (doc, warnings) = parse_noudata_with_warnings(
        br#"{"playableItemTitle":"Legacy","localPlaybackData":{
            "shuffle":true,"repeatList":true,"stopAfterEach":true,
            "resumeAtItem":1,"userNotes":{"userComment":"keep me"}},
            "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();
    assert!(
        warnings.is_empty(),
        "an older player's settings are normal, not malformed: {warnings:?}"
    );
    let l = doc.local_playback_data.as_ref().unwrap();
    assert_eq!(l.resume_at_item, Some(1));
    assert!(
        l.extra.is_empty(),
        "retired keys must not be kept in `extra`: {:?}",
        l.extra
    );
    let out: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
    for retired in ["shuffle", "repeatList", "stopAfterEach"] {
        assert!(
            out["localPlaybackData"].get(retired).is_none(),
            "{retired} was written back"
        );
    }
    assert_eq!(out["localPlaybackData"]["userNotes"]["userComment"], "keep me");
}

/// Full-document diff: the only difference between the legacy input and what Plippa writes is
/// the three retired keys. `itemLocalPlaybackData.repeatItem` is a different key and stays.
const LEGACY_PLAYBACK: &str = r#"{
    "noudataVersion": "0.4.0",
    "noudataType": "userPlaylist",
    "playableItemTitle": "Legacy",
    "localPlaybackData": {
        "shuffle": true, "repeatList": true, "stopAfterEach": true,
        "resumeAtItem": 2, "searchString": "bpm>140",
        "userNotes": {"userComment": "c", "userRating": 3, "userTags": ["x"]},
        "unknownPlaybackFlag": "keep"
    },
    "mediaList": [
        {"mediaTitle": "One", "localFilePaths": ["a.mp3"],
         "itemLocalPlaybackData": {"repeatItem": true, "unknownItemFlag": "keep"}},
        {"mediaTitle": "Two", "localFilePaths": ["b.mp3"]}
    ]
}"#;

#[test]
fn a_write_never_emits_shuffle_repeatlist_or_stopaftereach() {
    let (mut before, mut after) = round_trip(LEGACY_PLAYBACK.as_bytes());
    for retired in ["shuffle", "repeatList", "stopAfterEach"] {
        assert!(
            after["localPlaybackData"].get(retired).is_none(),
            "{retired} was written back"
        );
        before["localPlaybackData"]
            .as_object_mut()
            .unwrap()
            .remove(retired);
    }
    strip_writer_fields(&mut before);
    strip_writer_fields(&mut after);
    assert_eq!(
        after, before,
        "dropping the retired keys must not touch anything else"
    );
    // repeatItem is a different key on a different object and still round-trips.
    assert_eq!(
        after["mediaList"][0]["itemLocalPlaybackData"]["repeatItem"],
        true
    );
}

#[test]
fn legacy_playlist_parses_and_still_reports_its_media_list() {
    let (doc, warnings) = parse_noudata_with_warnings(LEGACY_PLAYBACK.as_bytes()).unwrap();
    assert!(
        warnings.is_empty(),
        "dropping the retired keys must not reject the file: {warnings:?}"
    );
    assert_eq!(doc.playable_item_title.as_deref(), Some("Legacy"));
    let titles: Vec<&str> = doc
        .media_list
        .iter()
        .filter_map(|m| m.media_title.as_deref())
        .collect();
    assert_eq!(titles, ["One", "Two"]);
    assert!(is_playlist(&doc, |_| None));
}

#[test]
fn local_cover_overrides_release_cover() {
    let doc = parse_noudata_str(
        r#"{"releaseData":{"coverImage":{"images":[{"imageLocalFilename":"release.jpg"}]}},
            "localPlaybackData":{"localCoverImage":{"images":[{"imageLocalFilename":"mine.png"}]}},
            "mediaList":[]}"#,
    )
    .unwrap();
    assert_eq!(doc.cover_local_filename(), Some("mine.png"));
}

// ---- unite data ---------------------------------------------------------------

#[test]
fn unite_data_typed() {
    let doc = parse_noudata(MIXTAPE).unwrap();
    let u = doc.media_list[0].unite_data.as_ref().unwrap();
    assert_eq!(u.expected_filename.as_deref(), Some("mix"));
    assert_eq!(
        u.expected_length,
        Some(NoubinTimestamp::from_millis(3_723_125))
    );
    assert_eq!(u.free_download_url, None);
}

// ---- warnings / leniency --------------------------------------------------------

#[test]
fn messy_typed_never_errors_and_reports_what_it_ignored() {
    let (doc, warnings) = parse_noudata_with_warnings(MESSY_TYPED).unwrap();
    let rd = doc.release_data.as_ref().unwrap();

    // unknown playableItemProfile ⇒ other, raw value kept for write-through
    assert_eq!(doc.playable_item_profile, None);
    assert_eq!(doc.profile(), PlayableItemProfile::Other);
    assert_eq!(doc.extra["playableItemProfile"], "hologram");

    // invalid elements dropped, valid ones kept
    let credits = rd.credits.as_ref().unwrap();
    assert_eq!(credits.primary_artists.as_ref().unwrap().len(), 1);
    assert!(credits.contributors.as_ref().unwrap().is_empty());
    assert_eq!(credits.extra["futureCredit"], "keep me");
    let platforms = rd.release_platforms.as_ref().unwrap();
    assert_eq!(platforms.len(), 1);
    assert_eq!(platforms[0].platform_name, "Shop");
    assert_eq!(platforms[0].platform_type, None);
    assert_eq!(platforms[0].effective_type(), PlatformType::Other);
    assert!(rd.further_links.as_ref().unwrap().is_empty());
    assert!(
        rd.cover_image.is_none(),
        "coverImage without valid images is ignored"
    );

    // invalid local playback values ignored but preserved
    let l = doc.local_playback_data.as_ref().unwrap();
    assert_eq!(l.resume_at_item, None);
    assert_eq!(l.extra["resumeAtItem"], 0);
    // Noubin v0.4.1 retired keys are dropped, even when wrong-typed: no warning, no `extra`.
    assert!(
        l.extra.get("shuffle").is_none(),
        "retired key kept in extra: {:?}",
        l.extra
    );
    assert_eq!(
        l.extra.len(),
        2,
        "only the genuinely unknown / invalid keys survive: {:?}",
        l.extra
    );
    assert!(l.extra.contains_key("futureFlag"));
    assert!(l.extra.contains_key("resumeAtItem"));
    assert!(
        !warnings.iter().any(|w| w.path.contains("shuffle")),
        "an older player's settings are not malformed: {warnings:?}"
    );

    // non-object mediaList element dropped
    assert_eq!(doc.media_list.len(), 1);
    let item = &doc.media_list[0];
    assert!(item.unite_data.is_none());
    let ilp = item.item_local_playback_data.as_ref().unwrap();
    assert_eq!(ilp.start_from, None);
    assert_eq!(
        ilp.effective_override_resume(),
        OverrideResumeBehaviour::Default
    );
    assert_eq!(ilp.extra["overrideResumeBehaviour"], "sometimes");
    assert!(item.extra.contains_key("musicalData"));

    for needle in [
        "mediaList[0]",
        "releaseData.releasePlatforms[0]",
        "releaseData.credits.primaryArtists[1]",
        "releaseData.credits.contributors[0]",
        "releaseData.furtherLinks[0]",
        "releaseData.coverImage",
        "localPlaybackData.resumeAtItem",
        "mediaList[1].uniteData",
        "mediaList[1].itemLocalPlaybackData.startFrom",
    ] {
        assert!(
            warnings.iter().any(|w| w.path.starts_with(needle)),
            "no warning for {needle}: {warnings:#?}"
        );
    }
}

#[test]
fn non_object_root_is_an_error_but_bad_json_types_are_not() {
    assert!(parse_noudata(b"[1,2]").is_err());
    assert!(parse_noudata(b"not json").is_err());
    let (doc, w) =
        parse_noudata_with_warnings(br#"{"playableItemTitle": 5, "mediaList": "nope"}"#).unwrap();
    assert_eq!(doc.playable_item_title, None);
    assert!(doc.media_list.is_empty());
    assert!(!w.is_empty());
}

#[test]
fn utf8_bom_accepted() {
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(br#"{"playableItemTitle":"Bom","mediaList":[]}"#);
    assert_eq!(
        parse_noudata(&bytes)
            .unwrap()
            .playable_item_title
            .as_deref(),
        Some("Bom")
    );
}

// ---- legacy safe title (0.1.0 name: no longer interpreted) ----------------------

#[test]
fn legacy_safe_title_is_just_an_unknown_property() {
    let doc = parse_noudata(MESSY_TYPED).unwrap();
    assert_eq!(doc.playable_item_folder_safe_title, None);
    let json: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
    assert_eq!(json["playableItemSafeTitle"], "Messy Legacy");
}

// ---- round trips ----------------------------------------------------------------

/// JSON-equal after removing what the writer is allowed to normalise.
fn round_trip(bytes: &[u8]) -> (Value, Value) {
    let before: Value = serde_json::from_slice(bytes).unwrap();
    let doc = parse_noudata(bytes).unwrap();
    let after: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
    (before, after)
}

fn strip_writer_fields(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.remove("_whatAmILookingAt");
        o.remove("noudataVersion");
        o.remove("noudataType"); // writers always declare it
    }
}

#[test]
fn messy_unknown_round_trips_json_equal() {
    let (mut before, mut after) = round_trip(MESSY_UNKNOWN);
    // Noubin v0.4.1: a legacy `localPlaybackData.shuffle` is dropped on the way through and
    // reported nowhere (an older player's setting is normal, not malformed).
    assert_eq!(
        before["localPlaybackData"].get("shuffle"),
        Some(&Value::Bool(false)),
        "fixture is legacy input"
    );
    assert!(
        after["localPlaybackData"].get("shuffle").is_none(),
        "the retired key survives a write: {after}"
    );
    before["localPlaybackData"]
        .as_object_mut()
        .unwrap()
        .remove("shuffle");
    strip_writer_fields(&mut before);
    strip_writer_fields(&mut after);
    assert_eq!(before, after);
}

#[test]
fn clean_fixtures_round_trip_json_equal_except_normalised_values() {
    // playlist / audiobook contain nothing the writer normalises.
    for bytes in [PLAYLIST, AUDIOBOOK] {
        let (mut before, mut after) = round_trip(bytes);
        strip_writer_fields(&mut before);
        strip_writer_fields(&mut after);
        // audiobook timestamps are re-written in canonical form
        if bytes == AUDIOBOOK {
            let ilp = &mut before["mediaList"][0]["itemLocalPlaybackData"];
            ilp["startFrom"] = "00:00:12".into();
            ilp["resumeFromTime"] = "01:23:20.5".into();
            for cue in before["mediaList"][0]["cueLists"][0]["cues"]
                .as_array_mut()
                .unwrap()
            {
                let s = cue["startTime"].as_str().unwrap().to_string();
                cue["startTime"] = NoubinTimestamp::parse(&s).unwrap().to_string().into();
            }
        }
        assert_eq!(before, after);
    }
}

#[test]
fn every_unknown_field_survives_write_through() {
    let doc = parse_noudata(MESSY_TYPED).unwrap();
    let out: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
    assert_eq!(out["futureRoot"], serde_json::json!([1, 2, {"a": null}]));
    assert_eq!(
        out["releaseData"]["futureReleaseThing"],
        serde_json::json!({"deep": true})
    );
    assert_eq!(out["releaseData"]["credits"]["futureCredit"], "keep me");
    // unknown enum values are written back verbatim, not collapsed to "other"
    assert_eq!(out["playableItemProfile"], "hologram");
    assert_eq!(
        out["releaseData"]["releasePlatforms"][0]["platformType"],
        "teleporter"
    );
    assert_eq!(out["localPlaybackData"]["futureFlag"], true);
    assert_eq!(out["mediaList"][0]["unknownItemField"], 123);
    assert_eq!(out["mediaList"][0]["musicalData"]["musicalKey"], "F#m");
    assert_eq!(
        out["mediaList"][0]["cueLists"][0]["cueListType"],
        "hologramCues"
    );
    assert_eq!(
        out["mediaList"][0]["cueLists"][0]["cues"][1]["futureCueField"],
        "x"
    );
}

#[test]
fn writer_output_is_stable_after_second_round_trip() {
    for bytes in [MIXTAPE, AUDIOBOOK, PLAYLIST, MESSY_TYPED, MESSY_UNKNOWN] {
        let once = parse_noudata(bytes).unwrap();
        let twice = parse_noudata_str(&once.to_json_pretty().unwrap()).unwrap();
        let mut a = once.clone();
        a.prepare_for_write();
        assert_eq!(a, twice);
        assert_eq!(
            once.to_json_pretty().unwrap(),
            twice.to_json_pretty().unwrap()
        );
    }
}

// ---- writer rules --------------------------------------------------------------

#[test]
fn writer_sets_what_am_i_first_and_version_and_normalises_newlines() {
    let doc = parse_noudata(MIXTAPE).unwrap();
    let mut stripped = doc.clone();
    stripped.what_am_i_looking_at = None;
    stripped.noudata_version = None;
    let json = stripped.to_json_pretty().unwrap();
    let first_key = json.lines().nth(1).unwrap().trim();
    assert!(
        first_key.starts_with("\"_whatAmILookingAt\""),
        "{first_key}"
    );
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["noudataVersion"], "0.4.0");
    assert_eq!(v["noudataType"], "release");
    assert_eq!(v["releaseData"]["legalNotice"], "Line one\nLine two");
    assert_eq!(
        v["mediaList"][0]["cueLists"][2]["cues"][0]["cueTextSpans"][0]["text"],
        "hello\nworld"
    );
    assert!(!json.contains("\\r"));
}

#[test]
fn writer_does_not_overwrite_existing_version() {
    let mut doc = parse_noudata_str(r#"{"noudataVersion":"0.3.1","mediaList":[]}"#).unwrap();
    doc.playable_item_title = Some("x".into());
    let v: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
    assert_eq!(v["noudataVersion"], "0.3.1");
}

#[test]
fn default_document_is_writable() {
    let doc = NoudataDocument::default();
    let v: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
    assert_eq!(v["mediaList"], serde_json::json!([]));
}

// ---- §4.9 classification --------------------------------------------------------

#[test]
fn classification_release_playlist_and_track_reference() {
    let none = |_: &str| -> Option<NoudataDocument> { None };

    let mix = parse_noudata(MIXTAPE).unwrap();
    assert_eq!(effective_type(&mix, none), NoudataType::Release);
    assert!(!is_playlist(&mix, none));

    let pl = parse_noudata(PLAYLIST).unwrap();
    assert_eq!(effective_type(&pl, none), NoudataType::UserPlaylist);
    assert!(is_playlist(&pl, none));

    let dir = fixture_dir("track_ref");
    let track_path = dir.join("02_Two.noudata");
    let track = parse_noudata(&std::fs::read(&track_path).unwrap()).unwrap();
    assert!(track.release_data.is_none());
    assert_eq!(track.noudata_type, Some(NoudataType::LinkTrack));
    assert_eq!(
        effective_type(&track, fs_release_resolver(&track_path)),
        NoudataType::LinkTrack
    );
    assert!(!is_playlist(&track, fs_release_resolver(&track_path)));
    // Profile of a link track comes from the referred release.
    assert_eq!(
        track.effective_profile(fs_release_resolver(&track_path)),
        PlayableItemProfile::Music
    );
}

#[test]
fn reference_to_missing_or_non_release_file_is_a_playlist() {
    let dir = fixture_dir("track_ref");
    let orphan_path = dir.join("orphan_ref.noudata");
    let orphan = parse_noudata(&std::fs::read(&orphan_path).unwrap()).unwrap();
    assert_eq!(
        effective_type(&orphan, fs_release_resolver(&orphan_path)),
        NoudataType::UserPlaylist
    );

    // refers to a playlist (no releaseData) ⇒ still a playlist
    let refers_to_playlist = parse_noudata_str(
        r#"{"referToReleaseNoudata":"p.noudata","mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();
    let pl = parse_noudata(PLAYLIST).unwrap();
    assert!(is_playlist(&refers_to_playlist, |_| Some(pl.clone())));
}

#[test]
fn dynamic_playlist_needs_search_string_and_empty_media_list() {
    let dynamic = parse_noudata_str(
        r#"{"playableItemTitle":"Fast","localPlaybackData":{"searchString":"bpm>140","shuffle":true},"mediaList":[]}"#,
    )
    .unwrap();
    assert!(is_dynamic_playlist(&dynamic));
    assert_eq!(
        effective_type(&dynamic, |_| None),
        NoudataType::UserPlaylist
    );
    assert!(is_playlist(&dynamic, |_| None));

    let has_items = parse_noudata_str(
        r#"{"localPlaybackData":{"searchString":"x"},"mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();
    assert!(!is_dynamic_playlist(&has_items));

    // a dynamic playlist is valid without a mediaList — no "will not be playable" warning
    let (_, w) = parse_noudata_with_warnings(
        br#"{"localPlaybackData":{"searchString":"x"},"mediaList":[]}"#,
    )
    .unwrap();
    assert!(w.is_empty(), "{w:?}");
}

// ---- v0.4.0 noudataType / playableItemProfile ------------------------------------

fn parse(s: &str) -> NoudataDocument {
    parse_noudata_str(s).unwrap()
}

const ITEM: &str = r#"{"localFilePaths":["a.mp3"]}"#;

#[test]
fn each_declared_type_is_used_when_valid() {
    let rel = parse(&format!(
        r#"{{"noudataType":"release","releaseData":{{}},"mediaList":[{ITEM}]}}"#
    ));
    let pl = parse(&format!(
        r#"{{"noudataType":"userPlaylist","mediaList":[{ITEM}]}}"#
    ));
    let lo = parse(&format!(
        r#"{{"noudataType":"looseTrack","mediaList":[{ITEM}]}}"#
    ));
    let li = parse(&format!(
        r#"{{"noudataType":"linkTrack","referToReleaseNoudata":"r.noudata","mediaList":[{ITEM}]}}"#
    ));
    for (d, t) in [
        (&rel, NoudataType::Release),
        (&pl, NoudataType::UserPlaylist),
        (&lo, NoudataType::LooseTrack),
        (&li, NoudataType::LinkTrack),
    ] {
        let c = classify_type(d, |_| None);
        assert_eq!((c.ty, c.declared), (t, true));
        assert!(c.warnings.is_empty());
        assert!(d.type_violations().is_empty(), "{t:?}");
    }
}

#[test]
fn omitted_or_unknown_type_is_inferred_and_unknown_is_preserved() {
    let omitted = parse(&format!(r#"{{"releaseData":{{}},"mediaList":[{ITEM}]}}"#));
    assert_eq!(effective_type(&omitted, |_| None), NoudataType::Release);
    let unknown = parse(&format!(
        r#"{{"noudataType":"podcastFeed","mediaList":[{ITEM}]}}"#
    ));
    assert_eq!(unknown.noudata_type, None);
    let c = classify_type(&unknown, |_| None);
    assert_eq!((c.ty, c.declared), (NoudataType::UserPlaylist, false));
    // never overwritten nor duplicated on write
    let out = unknown.to_json_pretty().unwrap();
    assert_eq!(out.matches("\"noudataType\"").count(), 1);
    assert!(out.contains("podcastFeed"));
    // a document without any type gets one when written
    let v: Value = serde_json::from_str(&omitted.to_json_pretty().unwrap()).unwrap();
    assert_eq!(v["noudataType"], "release");
}

#[test]
fn invalid_combinations_ignore_the_declared_type_with_a_warning() {
    for doc in [
        // release without releaseData
        format!(r#"{{"noudataType":"release","mediaList":[{ITEM}]}}"#),
        // loose / link track with two items
        format!(r#"{{"noudataType":"looseTrack","mediaList":[{ITEM},{ITEM}]}}"#),
        format!(
            r#"{{"noudataType":"linkTrack","referToReleaseNoudata":"r","mediaList":[{ITEM},{ITEM}]}}"#
        ),
        // linkTrack without referToReleaseNoudata
        format!(r#"{{"noudataType":"linkTrack","mediaList":[{ITEM}]}}"#),
    ] {
        let d = parse(&doc);
        let c = classify_type(&d, |_| None);
        assert!(!c.declared, "{doc}");
        assert_eq!(c.ty, NoudataType::UserPlaylist, "{doc}");
        assert!(!c.warnings.is_empty(), "{doc}");
    }
}

#[test]
fn declared_type_wins_when_it_has_forbidden_release_data_and_it_is_preserved() {
    let d = parse(&format!(
        r#"{{"noudataType":"looseTrack","releaseData":{{"futureThing":1}},"mediaList":[{ITEM}]}}"#
    ));
    assert_eq!(effective_type(&d, |_| None), NoudataType::LooseTrack);
    assert!(!d.type_violations().is_empty(), "flagged for editors");
    let v: Value = serde_json::from_str(&d.to_json_pretty().unwrap()).unwrap();
    assert_eq!(v["releaseData"]["futureThing"], 1);
    // …but Plippa refuses to *create* such a file.
    assert!(d.to_json_pretty_checked().is_err());
}

#[test]
fn profile_matrix_on_every_type() {
    let resolver = |_: &str| {
        Some(parse(&format!(
            r#"{{"noudataType":"release","playableItemProfile":"audiobook","releaseData":{{}},"mediaList":[{ITEM}]}}"#
        )))
    };
    let cases = [
        (
            r#""noudataType":"release","releaseData":{}"#,
            PlayableItemProfile::Mix,
        ),
        (r#""noudataType":"looseTrack""#, PlayableItemProfile::Mix),
        // link track: the stored value is ignored, the release's applies
        (
            r#""noudataType":"linkTrack","referToReleaseNoudata":"r.noudata""#,
            PlayableItemProfile::Audiobook,
        ),
        // playlist: ignored
        (
            r#""noudataType":"userPlaylist""#,
            PlayableItemProfile::Other,
        ),
    ];
    for (head, want) in cases {
        let d = parse(&format!(
            r#"{{{head},"playableItemProfile":"mix","mediaList":[{ITEM}]}}"#
        ));
        assert_eq!(d.effective_profile(resolver), want, "{head}");
    }
    // missing / unknown profile ⇒ other
    let d = parse(&format!(
        r#"{{"noudataType":"looseTrack","mediaList":[{ITEM}]}}"#
    ));
    assert_eq!(d.profile(), PlayableItemProfile::Other);
}

#[test]
fn loose_track_cover_comes_from_item_cover_and_upgrade_keeps_data() {
    let mut d = parse(
        r#"{"noudataType":"looseTrack","playableItemTitle":"T","playableItemProfile":"mix",
            "noubinKey":"k.noubin.x",
            "localPlaybackData":{"userNotes":{"userComment":"hi"}},
            "mediaList":[{"localFilePaths":["a.mp3"],
              "itemCoverImage":{"images":[{"imageLocalFilename":"c.jpg"}]},
              "cueLists":[{"cueListType":"nav","cues":[{"startTime":"0:10","cueTextSpans":[{"text":"x"}]}]}]}]}"#,
    );
    assert_eq!(d.cover_local_filename(), Some("c.jpg"));
    assert!(d.upgrade_loose_to_release());
    assert!(!d.upgrade_loose_to_release(), "only a looseTrack upgrades");
    assert!(d.set_release_primary_artist("Artist"));
    assert!(d.type_violations().is_empty());
    let v: Value = serde_json::from_str(&d.to_json_pretty().unwrap()).unwrap();
    assert_eq!(v["noudataType"], "release");
    assert_eq!(v["playableItemProfile"], "mix");
    assert_eq!(v["noubinKey"], "k.noubin.x");
    assert_eq!(
        v["releaseData"]["credits"]["primaryArtists"][0]["primaryArtistName"],
        "Artist"
    );
    assert_eq!(d.media_list[0].cue_lists.len(), 1);
    assert_eq!(v["localPlaybackData"]["userNotes"]["userComment"], "hi");
}

#[test]
fn creating_loose_or_link_tracks_with_credits_is_refused() {
    let d = parse(&format!(
        r#"{{"noudataType":"looseTrack","mediaList":[{{"localFilePaths":["a.mp3"],"itemCredits":{{"primaryArtists":[{{"primaryArtistName":"X"}}]}}}}]}}"#
    ));
    assert!(d.to_json_pretty_checked().is_err());
    let d = parse(&format!(
        r#"{{"noudataType":"linkTrack","referToReleaseNoudata":"r","playableItemProfile":"music","mediaList":[{ITEM}]}}"#
    ));
    assert!(
        d.to_json_pretty_checked().is_err(),
        "linkTrack omits the profile"
    );
}
