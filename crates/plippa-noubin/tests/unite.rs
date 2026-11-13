//! `unite` matching engine + local document builder (NB_07).
//!
//! Every fixture is built in memory: a `web.noudata` JSON parsed with the real NB_01 parser and
//! candidate files with declared durations/tags. Ground truth is written next to each fixture so
//! `assert_no_confident_wrong` can enforce "never wrong-but-confident".

use plippa_noubin::unite::{
    associate_existing, build_local_doc, detect_case_c, propose, CandidateFile, Confidence,
    ConfirmedItem, ConfirmedMapping, EmbeddedTags, FileOpKind, MatchReason, Placement, UniteError,
    UniteProposal, UniteWarningKind, AMBIGUITY_MARGIN, CONFIDENT_MIN, MIN_MATCH,
};
use plippa_noubin::{
    effective_type, is_safe_relative_path, normalise_noubin_key, parse_noudata_str,
    parse_noudata_with_warnings, LibraryRoot, LibraryWriter, NoudataDocument, NoudataType,
    PlayableItemProfile,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// fixture helpers
// ---------------------------------------------------------------------------

struct Item {
    title: &'static str,
    expected_name: Option<&'static str>,
    expected_secs: Option<f64>,
}

fn item(title: &'static str, name: Option<&'static str>, secs: Option<f64>) -> Item {
    Item {
        title,
        expected_name: name,
        expected_secs: secs,
    }
}

fn hms(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn web_json(title: &str, profile: &str, artist: &str, items: &[Item]) -> Value {
    let list: Vec<Value> = items
        .iter()
        .map(|i| {
            let mut m = json!({"mediaTitle": i.title});
            let mut u = serde_json::Map::new();
            if let Some(n) = i.expected_name {
                u.insert("expectedFilename".into(), json!(n));
            }
            if let Some(s) = i.expected_secs {
                u.insert("expectedLength".into(), json!(hms(s)));
            }
            if !u.is_empty() {
                m["uniteData"] = Value::Object(u);
            }
            m
        })
        .collect();
    json!({
        "playableItemTitle": title,
        "noudataType": "release",
        "playableItemProfile": profile,
        "releaseData": {
            "credits": {"primaryArtists": [{"primaryArtistName": artist}]}
        },
        "mediaList": list
    })
}

fn parse(v: &Value) -> NoudataDocument {
    let (doc, warnings) = parse_noudata_with_warnings(v.to_string().as_bytes()).unwrap();
    assert!(warnings.is_empty(), "fixture must be clean: {warnings:?}");
    doc
}

fn file(name: &str, secs: f64) -> CandidateFile {
    CandidateFile::new(format!("/import/{name}"), 4_000_000)
        .with_duration_ms((secs * 1000.0) as u64)
}

fn file_nodur(name: &str) -> CandidateFile {
    CandidateFile::new(format!("/import/{name}"), 4_000_000)
}

fn name_of(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}

/// `(item index, file names that belong to it)`.
type Truth<'a> = &'a [(usize, &'a [&'a str])];

/// Never wrong-but-confident: every match at `Confident` level is exactly the true one; a
/// wrong assignment may only ever appear as `Low`.
fn assert_no_confident_wrong(p: &UniteProposal, truth: Truth) {
    for m in &p.matches {
        if m.level != Confidence::Confident {
            continue;
        }
        let want: HashSet<&str> = truth
            .iter()
            .find(|(i, _)| *i == m.item_index)
            .map(|(_, f)| f.iter().copied().collect())
            .unwrap_or_default();
        let got: HashSet<String> = m.files.iter().map(|f| name_of(&f.path)).collect();
        let got: HashSet<&str> = got.iter().map(String::as_str).collect();
        // Alternate formats may be attached at confident level, but only true ones.
        assert!(
            got.is_subset(&want) && !got.is_empty(),
            "item {} confidently matched {got:?}, truth {want:?} ({:?})",
            m.item_index,
            m.reasons
        );
    }
}

fn find(p: &UniteProposal, item: usize) -> &plippa_noubin::unite::UniteMatch {
    p.matches
        .iter()
        .find(|m| m.item_index == item)
        .unwrap_or_else(|| panic!("item {item} not matched: {p:#?}"))
}

fn matched_names(p: &UniteProposal, item: usize) -> Vec<String> {
    find(p, item)
        .files
        .iter()
        .map(|f| name_of(&f.path))
        .collect()
}

// ---------------------------------------------------------------------------
// 10-track album with store-mangled names
// ---------------------------------------------------------------------------

fn album_items() -> Vec<Item> {
    vec![
        item("Intro", Some("01 - Intro.mp3"), Some(65.0)),
        item("What's Up?", Some("02 - What's Up?.mp3"), Some(213.0)),
        item("AC/DC Medley", Some("03 - AC_DC Medley.mp3"), Some(251.0)),
        item("Rock & Roll", Some("04 - Rock & Roll.mp3"), Some(199.0)),
        item("Fire!", Some("05 - Fire!.mp3"), Some(187.0)),
        item("Café del Mar", Some("06 - Café del Mar.mp3"), Some(305.0)),
        item("Interlude", Some("07 - Interlude.mp3"), Some(45.0)),
        item(
            "Midnight Train",
            Some("08 - Midnight Train.mp3"),
            Some(274.0),
        ),
        item(
            "Outro (Reprise)",
            Some("09 - Outro (Reprise).mp3"),
            Some(133.0),
        ),
        item("Hidden Track", Some("10 - Hidden Track.mp3"), Some(201.0)),
    ]
}

/// Store-mangled names (illegal characters dropped, separators changed, artist prefix) and an
/// mp3 length drift of a few hundred ms. The 10th file is garbled and 4 s off.
fn album_files() -> Vec<CandidateFile> {
    vec![
        file("01_Intro.mp3", 65.3),
        file("02 - Whats Up.mp3", 213.2),
        file("03 - ACDC Medley.mp3", 251.4),
        file("04 Rock and Roll.mp3", 199.1),
        file("05. Fire.mp3", 187.3),
        file("06 - Cafe del Mar.mp3", 305.2),
        file("07 - Interlude.mp3", 45.4),
        file("Artist Name - Midnight Train.mp3", 274.1),
        file("09-outro reprise.mp3", 133.2),
        file("trk10_final_v2.mp3", 205.0),
    ]
}

const ALBUM_TRUTH: [(usize, &[&str]); 10] = [
    (0, &["01_Intro.mp3"]),
    (1, &["02 - Whats Up.mp3"]),
    (2, &["03 - ACDC Medley.mp3"]),
    (3, &["04 Rock and Roll.mp3"]),
    (4, &["05. Fire.mp3"]),
    (5, &["06 - Cafe del Mar.mp3"]),
    (6, &["07 - Interlude.mp3"]),
    (7, &["Artist Name - Midnight Train.mp3"]),
    (8, &["09-outro reprise.mp3"]),
    (9, &["trk10_final_v2.mp3"]),
];

fn album_doc() -> NoudataDocument {
    parse(&web_json(
        "The Album",
        "music",
        "Artist Name",
        &album_items(),
    ))
}

#[test]
fn unite_album_with_mangled_names_matches_nine_of_ten_and_surfaces_the_tenth() {
    let doc = album_doc();
    let mut files = album_files();
    // Input order must not matter.
    files.reverse();
    let p = propose(&doc, &files, PlayableItemProfile::Music);

    assert_no_confident_wrong(&p, &ALBUM_TRUTH);
    let confident_correct = ALBUM_TRUTH[..9]
        .iter()
        .filter(|(i, f)| {
            let m = find(&p, *i);
            m.level == Confidence::Confident && matched_names(&p, *i) == f.to_vec()
        })
        .count();
    assert_eq!(confident_correct, 9, "{p:#?}");

    // The 10th is surfaced as low confidence (proposed, correct, needs the user) -- not
    // dropped silently and not confident.
    let tenth = find(&p, 9);
    assert_eq!(tenth.level, Confidence::Low);
    assert_eq!(matched_names(&p, 9), vec!["trk10_final_v2.mp3"]);
    assert!(tenth.confidence >= MIN_MATCH && tenth.confidence < CONFIDENT_MIN);
    assert!(tenth.reasons.contains(&MatchReason::OrderFallback));

    assert!(p.unmatched_items.is_empty() && p.unmatched_files.is_empty());
    assert!(p.is_complete() && p.needs_review());
}

#[test]
fn unite_album_confidence_is_separated() {
    let p = propose(&album_doc(), &album_files(), PlayableItemProfile::Music);
    for m in p
        .matches
        .iter()
        .filter(|m| m.level == Confidence::Confident)
    {
        assert!(m.confidence >= CONFIDENT_MIN, "{m:?}");
    }
    for m in p.matches.iter().filter(|m| m.level == Confidence::Low) {
        assert!(
            m.confidence < CONFIDENT_MIN
                || m.reasons
                    .iter()
                    .any(|r| matches!(r, MatchReason::Ambiguous { .. }))
        );
    }
    // Deterministic.
    assert_eq!(
        p,
        propose(&album_doc(), &album_files(), PlayableItemProfile::Music)
    );
}

#[test]
fn unite_album_shuffled_wrong_numbers_never_confident_wrong() {
    // Files right, but a store renumbered tracks 5 and 6 (swapped numbers, same titles).
    let mut files = album_files();
    files[4] = file("06. Fire.mp3", 187.3);
    files[5] = file("05 - Cafe del Mar.mp3", 305.2);
    let p = propose(&album_doc(), &files, PlayableItemProfile::Music);
    let mut truth: Vec<(usize, &[&str])> = ALBUM_TRUTH.to_vec();
    truth[4] = (4, &["06. Fire.mp3"]);
    truth[5] = (5, &["05 - Cafe del Mar.mp3"]);
    assert_no_confident_wrong(&p, &truth);
    assert_eq!(matched_names(&p, 4), vec!["06. Fire.mp3"]);
    assert_eq!(matched_names(&p, 5), vec!["05 - Cafe del Mar.mp3"]);
}

// ---------------------------------------------------------------------------
// MP3 + FLAC
// ---------------------------------------------------------------------------

fn ep_items() -> Vec<Item> {
    vec![
        item("One", Some("01 - One.mp3"), Some(100.0)),
        item("Two", Some("02 - Two.mp3"), Some(200.0)),
        item("Three", Some("03 - Three.mp3"), Some(300.0)),
    ]
}

#[test]
fn unite_mp3_and_flac_pair_both_go_into_one_item() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()));
    let files = vec![
        file("01 - One.mp3", 100.1),
        file("01 - One.flac", 100.0),
        file("02 - Two.mp3", 200.1),
        file("02 - Two (FLAC).flac", 200.0), // different name: attached as alternate
        file("03 - Three.mp3", 300.1),
        file("03 - Three.flac", 300.0),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    let truth: Vec<(usize, &[&str])> = vec![
        (0, &["01 - One.mp3", "01 - One.flac"]),
        (1, &["02 - Two.mp3", "02 - Two (FLAC).flac"]),
        (2, &["03 - Three.mp3", "03 - Three.flac"]),
    ];
    assert_no_confident_wrong(&p, &truth);
    for (i, want) in &truth {
        let mut got = matched_names(&p, *i);
        got.sort();
        let mut want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(find(&p, *i).level, Confidence::Confident);
    }
    assert!(find(&p, 1)
        .files
        .iter()
        .any(|f| f.reasons.contains(&MatchReason::AlternateFormat)));
    assert!(p.unmatched_files.is_empty() && p.is_complete());

    // ... and both land in localFilePaths.
    let plan = build_local_doc(
        &doc,
        &p.confirm_all(),
        None,
        &Placement::for_release(&doc, PlayableItemProfile::Music),
    )
    .unwrap();
    let mut lfp = plan.doc.media_list[0].local_file_paths.clone().unwrap();
    lfp.sort();
    assert_eq!(lfp, vec!["01 - One.flac", "01 - One.mp3"]);
}

#[test]
fn unite_two_files_with_same_name_and_format_are_not_merged() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()[..1]));
    let files = vec![
        CandidateFile::new("/a/01 - One.mp3", 1).with_duration_ms(100_000),
        CandidateFile::new("/b/01 - One.mp3", 1).with_duration_ms(100_000),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    assert_eq!(find(&p, 0).files.len(), 1);
    assert_eq!(p.unmatched_files.len(), 1);
    assert!(p
        .warnings
        .iter()
        .any(|w| w.kind == UniteWarningKind::DuplicateFile));
    // An identical twin makes the pick a coin flip: must not be confident.
    assert_eq!(find(&p, 0).level, Confidence::Low);
}

// ---------------------------------------------------------------------------
// podcast / mix / audiobook
// ---------------------------------------------------------------------------

#[test]
fn unite_podcast_episode_set_uses_episode_numbers_not_positions() {
    let items = vec![
        item("Episode 101: The Beginning", None, Some(1800.0)),
        item("Episode 102: Guest Interview", None, Some(2400.0)),
        item("Episode 103: Listener Mail", None, Some(1500.0)),
        item("Episode 104: Q&A", None, Some(2000.0)),
    ];
    let doc = parse(&web_json("The Show", "podcast", "The Host", &items));
    let files = vec![
        file("episode 103 - listener mail.mp3", 1500.5),
        file("ep102 guest interview.mp3", 2399.9),
        file("Episode 101 - The Beginning.mp3", 1800.2),
        file("the-show-episode-104.mp3", 2000.4),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Podcast);
    let truth: Vec<(usize, &[&str])> = vec![
        (0, &["Episode 101 - The Beginning.mp3"]),
        (1, &["ep102 guest interview.mp3"]),
        (2, &["episode 103 - listener mail.mp3"]),
        (3, &["the-show-episode-104.mp3"]),
    ];
    assert_no_confident_wrong(&p, &truth);
    for (i, f) in &truth {
        assert_eq!(matched_names(&p, *i), f.to_vec(), "item {i}");
        assert_eq!(find(&p, *i).level, Confidence::Confident, "item {i}");
    }
}

#[test]
fn unite_mix_single_huge_file_matches_on_expected_length_only() {
    let items = vec![item("Big Beach Mix", None, Some(3723.0))]; // 1:02:03, no filename
    let doc = parse(&web_json("Big Beach", "mix", "DJ Someone", &items));
    // 1 % of 62 min is 37 s: 20 s off is fine. The names say nothing.
    let files = vec![
        file("a8f3-91bc.mp3", 3743.0),
        file("cover.jpg", 0.0),
        file("notes.txt", 0.0),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Mix);
    let m = find(&p, 0);
    assert_eq!(m.level, Confidence::Confident);
    assert_eq!(matched_names(&p, 0), vec!["a8f3-91bc.mp3"]);
    assert!(m
        .reasons
        .iter()
        .any(|r| matches!(r, MatchReason::Length { unique: true, .. })));
    assert!(!m.reasons.contains(&MatchReason::ExpectedFilename));
    assert_eq!(p.ignored_files.len(), 2);
    assert!(p.is_complete());
}

#[test]
fn unite_mix_length_alone_picks_the_right_file_among_several() {
    let items = vec![item("Big Beach Mix", None, Some(3723.0))];
    let doc = parse(&web_json("Big Beach", "mix", "DJ Someone", &items));
    let files = vec![file("part-a.mp3", 3723.4), file("part-b.mp3", 1200.0)];
    let p = propose(&doc, &files, PlayableItemProfile::Mix);
    assert_eq!(matched_names(&p, 0), vec!["part-a.mp3"]);
    assert_eq!(find(&p, 0).level, Confidence::Confident);
    // The other file is not guessed onto anything.
    assert_eq!(p.unmatched_files.len(), 1);
    assert_eq!(name_of(&p.unmatched_files[0].path), "part-b.mp3");
    assert!(!p.is_complete());
}

#[test]
fn unite_mix_outside_one_percent_is_not_confident() {
    let items = vec![item("Big Beach Mix", None, Some(3723.0))];
    let doc = parse(&web_json("Big Beach", "mix", "DJ Someone", &items));
    // 3 minutes off a 62 minute mix (~5 %): never confident.
    let p = propose(
        &doc,
        &[file("xyz.mp3", 3903.0), file("other.mp3", 100.0)],
        PlayableItemProfile::Mix,
    );
    assert!(
        p.matches.iter().all(|m| m.level != Confidence::Confident),
        "{p:#?}"
    );
}

#[test]
fn unite_audiobook_chapters() {
    let items = vec![
        item("Chapter 1", None, Some(1200.0)),
        item("Chapter 2", None, Some(1300.0)),
        item("Chapter 10", None, Some(1250.0)),
    ];
    let doc = parse(&web_json(
        "Project Book",
        "audiobook",
        "Andy Author",
        &items,
    ));
    let files = vec![
        file_nodur("Chapter 10.m4b"),
        file_nodur("Chapter 01.m4b"),
        file_nodur("Chapter 02.m4b"),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Audiobook);
    let truth: Vec<(usize, &[&str])> = vec![
        (0, &["Chapter 01.m4b"]),
        (1, &["Chapter 02.m4b"]),
        (2, &["Chapter 10.m4b"]),
    ];
    assert_no_confident_wrong(&p, &truth);
    for (i, f) in &truth {
        assert_eq!(matched_names(&p, *i), f.to_vec());
        assert_eq!(find(&p, *i).level, Confidence::Confident);
    }
}

#[test]
fn unite_audiobook_single_huge_file() {
    let items = vec![item("The Whole Book", None, Some(36_000.0))]; // 10 h
    let doc = parse(&web_json("Old Book", "audiobook", "Someone Old", &items));
    let p = propose(
        &doc,
        &[file("Old Book (unabridged).m4b", 36_100.0)],
        PlayableItemProfile::Audiobook,
    );
    assert_eq!(find(&p, 0).level, Confidence::Confident);
}

// ---------------------------------------------------------------------------
// lengths, tags, order
// ---------------------------------------------------------------------------

#[test]
fn unite_mismatched_length_is_surfaced_not_confident() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()));
    let files = vec![
        file("01 - One.mp3", 100.2),
        file("02 - Two.mp3", 160.0), // 40 s short: a different edit?
        file("03 - Three.mp3", 300.1),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    let truth: Vec<(usize, &[&str])> = vec![
        (0, &["01 - One.mp3"]),
        (1, &["02 - Two.mp3"]),
        (2, &["03 - Three.mp3"]),
    ];
    assert_no_confident_wrong(&p, &truth);
    let m = find(&p, 1);
    assert_eq!(m.level, Confidence::Low);
    assert!(m
        .reasons
        .iter()
        .any(|r| matches!(r, MatchReason::LengthMismatch { diff_ms } if *diff_ms == 40_000)));
    assert!(p
        .warnings
        .iter()
        .any(|w| w.kind == UniteWarningKind::LengthMismatch && w.item_index == Some(1)));
    assert_eq!(find(&p, 0).level, Confidence::Confident);
}

#[test]
fn unite_length_tolerance_constants() {
    // Short item: +-2 s fits, 2.5 s is only partial evidence (never unique-length confident).
    let items = vec![item("Solo", None, Some(120.0))];
    let doc = parse(&web_json("Single", "music", "A", &items));
    let fits = propose(&doc, &[file("zzz.mp3", 121.9)], PlayableItemProfile::Music);
    assert!(find(&fits, 0)
        .reasons
        .iter()
        .any(|r| matches!(r, MatchReason::Length { unique: true, .. })));
    let near = propose(&doc, &[file("zzz.mp3", 122.5)], PlayableItemProfile::Music);
    assert!(near.matches.iter().all(|m| m
        .reasons
        .iter()
        .all(|r| !matches!(r, MatchReason::Length { unique: true, .. }))));

    // Long item (> 10 min): 1 % applies.
    let items = vec![item("Long", None, Some(1800.0))]; // 1 % = 18 s
    let doc = parse(&web_json("Long", "music", "A", &items));
    let inside = propose(&doc, &[file("zzz.mp3", 1815.0)], PlayableItemProfile::Music);
    assert!(find(&inside, 0)
        .reasons
        .iter()
        .any(|r| matches!(r, MatchReason::Length { unique: true, .. })));
    let outside = propose(&doc, &[file("zzz.mp3", 1830.0)], PlayableItemProfile::Music);
    assert!(outside.matches.iter().all(|m| m
        .reasons
        .iter()
        .all(|r| !matches!(r, MatchReason::Length { unique: true, .. }))));
}

#[test]
fn unite_tags_match_opaque_filenames() {
    let items = vec![
        item("Alpha", None, None),
        item("Beta", None, None),
        item("Gamma", None, None),
    ];
    let doc = parse(&web_json("Tagged", "music", "T", &items));
    let tag = |title: &str, n: u32| EmbeddedTags {
        title: Some(title.into()),
        track_number: Some(n),
        ..Default::default()
    };
    // Opaque names in a shuffled order.
    let files = vec![
        file_nodur("x1.mp3").with_tags(tag("Gamma", 3)),
        file_nodur("x2.mp3").with_tags(tag("Alpha", 1)),
        file_nodur("x3.mp3").with_tags(tag("Beta", 2)),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    let truth: Vec<(usize, &[&str])> = vec![(0, &["x2.mp3"]), (1, &["x3.mp3"]), (2, &["x1.mp3"])];
    assert_no_confident_wrong(&p, &truth);
    for (i, f) in &truth {
        assert_eq!(matched_names(&p, *i), f.to_vec());
        assert_eq!(find(&p, *i).level, Confidence::Confident);
        assert!(find(&p, *i)
            .reasons
            .iter()
            .any(|r| matches!(r, MatchReason::TagTitle { .. })));
    }
}

#[test]
fn unite_order_fallback_only_when_counts_match_and_never_confident() {
    let items = vec![
        item("Alpha", None, None),
        item("Beta", None, None),
        item("Gamma", None, None),
    ];
    let doc = parse(&web_json("Anon", "music", "T", &items));

    let same = propose(
        &doc,
        &[
            file_nodur("track b.mp3"),
            file_nodur("track a.mp3"),
            file_nodur("track c.mp3"),
        ],
        PlayableItemProfile::Music,
    );
    assert_eq!(same.matches.len(), 3);
    for m in &same.matches {
        assert_eq!(m.level, Confidence::Low);
        assert!(m.reasons.contains(&MatchReason::OrderFallback));
    }

    // One more file than items: order proves nothing, nothing is proposed.
    let extra = propose(
        &doc,
        &[
            file_nodur("q1.mp3"),
            file_nodur("q2.mp3"),
            file_nodur("q3.mp3"),
            file_nodur("q4.mp3"),
        ],
        PlayableItemProfile::Music,
    );
    assert!(extra.matches.is_empty());
    assert_eq!(extra.unmatched_items.len(), 3);
    assert_eq!(extra.unmatched_files.len(), 4);
    assert!(extra
        .warnings
        .iter()
        .any(|w| w.kind == UniteWarningKind::CountMismatch));
}

#[test]
fn unite_ambiguous_duplicates_are_not_confident() {
    // Two items with the same title and two equally named files.
    let items = vec![item("Intro", None, None), item("Intro", None, None)];
    let doc = parse(&web_json("Twins", "music", "T", &items));
    let p = propose(
        &doc,
        &[file_nodur("Intro.mp3"), file_nodur("Intro.flac")],
        PlayableItemProfile::Music,
    );
    // Same stem, different extension: one unit (two formats) against two items.
    assert!(p.matches.iter().all(|m| m.level != Confidence::Confident));
}

// ---------------------------------------------------------------------------
// extra / missing / unsupported / case C
// ---------------------------------------------------------------------------

#[test]
fn unite_extra_file_is_returned_unmatched() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()));
    let mut files = vec![
        file("01 - One.mp3", 100.0),
        file("02 - Two.mp3", 200.0),
        file("03 - Three.mp3", 300.0),
    ];
    files.push(file("bonus - live jam.mp3", 555.0));
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    assert_eq!(p.matches.len(), 3);
    assert_eq!(p.unmatched_files.len(), 1);
    assert_eq!(name_of(&p.unmatched_files[0].path), "bonus - live jam.mp3");
    assert!(!p.is_complete());
    // Completion rule: building refuses while a file is unaccounted for ...
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    let err = build_local_doc(&doc, &p.confirm_all(), None, &placement).unwrap_err();
    assert!(
        matches!(err, UniteError::Incomplete { ref leftover_files, ref missing_items } if leftover_files.len() == 1 && missing_items.is_empty())
    );
    // ... unless the user explicitly leaves it out.
    let mut mapping = p.confirm_all();
    mapping
        .ignored_files
        .push(p.unmatched_files[0].path.clone());
    let plan = build_local_doc(&doc, &mapping, None, &placement).unwrap();
    assert_eq!(plan.file_ops.len(), 3);
}

#[test]
fn unite_missing_file_leaves_item_unmatched() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()));
    let files = vec![file("01 - One.mp3", 100.0), file("03 - Three.mp3", 300.0)];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    assert_eq!(p.matches.len(), 2);
    assert_eq!(p.unmatched_items.len(), 1);
    assert_eq!(p.unmatched_items[0].item_index, 1);
    assert_eq!(p.unmatched_items[0].title.as_deref(), Some("Two"));
    assert!(p.unmatched_files.is_empty());
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    let err = build_local_doc(&doc, &p.confirm_all(), None, &placement).unwrap_err();
    assert!(
        matches!(err, UniteError::Incomplete { ref missing_items, .. } if missing_items == &vec![1])
    );
    // Manual assignment fixes it: add a file the matcher would not have chosen.
    let mut mapping = p.confirm_all();
    mapping.assignments.push(ConfirmedItem {
        item_index: 1,
        files: vec![PathBuf::from("/import/mystery.mp3")],
    });
    mapping
        .known_files
        .push(PathBuf::from("/import/mystery.mp3"));
    assert!(build_local_doc(&doc, &mapping, None, &placement).is_ok());
}

#[test]
fn unite_unsupported_media_is_flagged_not_matched() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()[..1]));
    let files = vec![
        file("01 - One.mp3", 100.0),
        file("01 - One.mkv", 100.0),
        file_nodur("clip.mp4"),
        file_nodur("cover.png"),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    assert_eq!(p.unsupported_files.len(), 2);
    assert_eq!(matched_names(&p, 0), vec!["01 - One.mp3"]);
    assert_eq!(p.ignored_files.len(), 1);
    assert_eq!(
        p.warnings
            .iter()
            .filter(|w| w.kind == UniteWarningKind::UnsupportedFile)
            .count(),
        2
    );
    assert!(p.needs_review());
}

#[test]
fn unite_case_c_only_associates() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()));
    let files = vec![
        file("01 - One.mp3", 100.0),
        file_nodur("_EP.noudata"),
        file_nodur("track.noudata"), // per-track file: not a release document
    ];
    assert_eq!(name_of(&detect_case_c(&files).unwrap().path), "_EP.noudata");
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    assert!(p.case_c.is_some());
    assert!(p.matches.is_empty() && p.unmatched_items.is_empty() && p.unmatched_files.is_empty());
    assert!(p.warnings.iter().any(|w| w.kind == UniteWarningKind::CaseC));
    assert!(!p.is_complete());

    let existing = parse(&web_json("EP", "music", "Band", &ep_items()));
    let with_key = associate_existing(existing.clone(), Some("  My.Key.123 ")).unwrap();
    assert_eq!(with_key.noubin_key.as_deref(), Some("My.Key.123"));
    assert_eq!(
        with_key.noubin_key_normalised.as_deref(),
        Some(normalise_noubin_key("My.Key.123").as_str())
    );
    assert_eq!(
        associate_existing(existing.clone(), None).unwrap(),
        existing
    );
    assert_eq!(
        associate_existing(existing, Some("")).unwrap_err(),
        UniteError::InvalidKey
    );
    assert!(detect_case_c(&[file("01.mp3", 1.0)]).is_none());
}

// ---------------------------------------------------------------------------
// build_local_doc
// ---------------------------------------------------------------------------

#[test]
fn unite_build_local_doc_album() {
    let doc = album_doc();
    let p = propose(&doc, &album_files(), PlayableItemProfile::Music);
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    assert_eq!(placement.dir, PathBuf::from("Music/Artist Name/The Album"));

    let plan = build_local_doc(
        &doc,
        &p.confirm_all(),
        Some("The Album.noubin.k3Lx"),
        &placement,
    )
    .unwrap();
    assert_eq!(
        plan.noudata_path,
        PathBuf::from("Music/Artist Name/The Album/_The Album.noudata")
    );
    assert_eq!(
        plan.doc.noubin_key.as_deref(),
        Some("The Album.noubin.k3Lx")
    );
    assert_eq!(
        plan.doc.noubin_key_normalised.as_deref(),
        Some(normalise_noubin_key("The Album.noubin.k3Lx").as_str())
    );
    // Canonical metadata kept, uniteData retained, localFilePaths relative to the .noudata.
    assert_eq!(plan.doc.playable_item_title.as_deref(), Some("The Album"));
    assert_eq!(plan.doc.media_list.len(), 10);
    assert_eq!(plan.doc.media_list[0].media_title.as_deref(), Some("Intro"));
    assert!(plan.doc.media_list[0].unite_data.is_some());
    assert_eq!(
        plan.doc.media_list[0].local_file_paths,
        Some(vec!["01_Intro.mp3".to_string()])
    );
    assert!(plan
        .doc
        .media_list
        .iter()
        .all(|i| i.local_file_paths.as_ref().is_some_and(|l| l.len() == 1)));
    assert_eq!(plan.file_ops.len(), 10);
    assert!(plan.file_ops.iter().all(|o| o.op == FileOpKind::Move
        && o.target.starts_with("Music/Artist Name/The Album")
        && o.source.starts_with("/import")));
    assert!(plan.missing_items.is_empty());
    // No key without a tag.
    let nokey = build_local_doc(&doc, &p.confirm_all(), None, &placement).unwrap();
    assert!(nokey.doc.noubin_key.is_none() && nokey.doc.noubin_key_normalised.is_none());
}

#[test]
fn unite_build_never_keeps_a_key_from_the_web_document() {
    let mut v = web_json("EP", "music", "Band", &ep_items());
    v["noubinKey"] = json!("evil.key");
    v["noubinKeyNormalised"] = json!("evil.key");
    let doc = parse(&v);
    assert!(doc.noubin_key.is_some());
    let p = propose(
        &doc,
        &[
            file("01 - One.mp3", 100.0),
            file("02 - Two.mp3", 200.0),
            file("03 - Three.mp3", 300.0),
        ],
        PlayableItemProfile::Music,
    );
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    let plan = build_local_doc(&doc, &p.confirm_all(), None, &placement).unwrap();
    assert!(plan.doc.noubin_key.is_none() && plan.doc.noubin_key_normalised.is_none());
    let json = plan.doc.to_json_pretty().unwrap();
    assert!(!json.contains("noubinKey"));
    // Key length cap.
    let long = "k".repeat(513);
    assert_eq!(
        build_local_doc(&doc, &p.confirm_all(), Some(&long), &placement).unwrap_err(),
        UniteError::InvalidKey
    );
}

#[test]
fn unite_build_single_item_release_names_noudata_after_the_track() {
    let items = vec![item("Only Song", None, Some(180.0))];
    let doc = parse(&web_json("Only Song", "music", "Solo Artist", &items));
    let p = propose(&doc, &[file("whatever.mp3", 180.4)], PlayableItemProfile::Music);
    let plan = build_local_doc(
        &doc,
        &p.confirm_all(),
        None,
        &Placement::for_release(&doc, PlayableItemProfile::Music),
    )
    .unwrap();
    assert_eq!(
        plan.noudata_path,
        PathBuf::from("Music/Solo Artist/Only Song/Only Song.noudata")
    );
}

#[test]
fn unite_build_profile_placement() {
    for (profile, name, artist, dir) in [
        ("mix", "Big Beach", "DJ Someone", "Music/Mixes/DJ Someone"),
        (
            "audiobook",
            "Old Book",
            "Andy Weir",
            "Audiobooks/Weir, Andy/Old Book",
        ),
        ("podcast", "The Show", "The Host", "Podcasts/The Show"),
        ("other", "Field Rec", "Rec Person", "Other/Rec Person"),
    ] {
        let doc = parse(&web_json(name, profile, artist, &[item("x", None, None)]));
        let pr = doc.profile();
        assert_eq!(
            Placement::for_release(&doc, pr).dir,
            PathBuf::from(dir),
            "{profile}"
        );
    }
}

#[test]
fn unite_build_folder_safe_title_and_file_renames() {
    let items = vec![
        item("Why? / Because", None, Some(100.0)),
        item("Second", None, Some(200.0)),
    ];
    let doc = parse(&web_json("Why? / Because: The LP", "music", "A|B", &items));
    let files = vec![
        CandidateFile::new("/import/01 - Why*Because.mp3", 1).with_duration_ms(100_000),
        CandidateFile::new("/import/02 - Second.mp3", 1).with_duration_ms(200_000),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    assert_eq!(
        placement.dir,
        PathBuf::from("Music/A_B/Why_ _ Because_ The LP")
    );
    let plan = build_local_doc(&doc, &p.confirm_all(), None, &placement).unwrap();
    // Title untouched, folder-safe form derived (§1.28).
    assert_eq!(
        plan.doc.playable_item_title.as_deref(),
        Some("Why? / Because: The LP")
    );
    assert_eq!(
        plan.doc.playable_item_folder_safe_title.as_deref(),
        Some("Why_ _ Because_ The LP")
    );
    assert_eq!(
        plan.noudata_path,
        PathBuf::from("Music/A_B/Why_ _ Because_ The LP/_Why_ _ Because_ The LP.noudata")
    );
    // The illegal file name is renamed and localFilePaths uses the new name.
    let op = &plan.file_ops[0];
    assert_eq!(op.op, FileOpKind::MoveAndRename);
    assert_eq!(name_of(&op.target), "01 - Why_Because.mp3");
    assert_eq!(
        plan.doc.media_list[0].local_file_paths,
        Some(vec!["01 - Why_Because.mp3".to_string()])
    );
    assert_eq!(plan.file_ops[1].op, FileOpKind::Move);
}

#[test]
fn unite_build_collisions_get_numbered_names_never_overwrite() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()[..1]));
    let p = propose(&doc, &[file("01 - One.mp3", 100.0)], PlayableItemProfile::Music);
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music)
        .with_existing_names(["01 - ONE.mp3".to_string(), "01 - One.noudata".to_string()]);
    let plan = build_local_doc(&doc, &p.confirm_all(), None, &placement).unwrap();
    assert_eq!(name_of(&plan.file_ops[0].target), "01 - One (2).mp3");
    assert_eq!(plan.file_ops[0].op, FileOpKind::MoveAndRename);
    assert_eq!(name_of(&plan.noudata_path), "EP.noudata"); // stem is the release title
    let placement2 = Placement::for_release(&doc, PlayableItemProfile::Music)
        .with_existing_names(["ep.noudata".to_string()]);
    let plan2 = build_local_doc(&doc, &p.confirm_all(), None, &placement2).unwrap();
    assert_eq!(name_of(&plan2.noudata_path), "EP (2).noudata");
    assert!(plan2
        .warnings
        .iter()
        .any(|w| w.kind == UniteWarningKind::Plan));

    // A file that is already where it should be is kept, not renamed onto itself.
    let files = vec![CandidateFile::new("Music/Band/EP/01 - One.mp3", 1).with_duration_ms(100_000)];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    let placement3 = Placement::for_release(&doc, PlayableItemProfile::Music)
        .with_existing_names(["01 - One.mp3".to_string()]);
    let plan3 = build_local_doc(&doc, &p.confirm_all(), None, &placement3).unwrap();
    assert_eq!(plan3.file_ops[0].op, FileOpKind::Keep);
    assert_eq!(name_of(&plan3.file_ops[0].target), "01 - One.mp3");
}

#[test]
fn unite_build_rejects_bad_mappings() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()[..2]));
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    let m = |a: Vec<(usize, &str)>| ConfirmedMapping {
        assignments: a
            .into_iter()
            .map(|(i, f)| ConfirmedItem {
                item_index: i,
                files: vec![PathBuf::from(f)],
            })
            .collect(),
        ..Default::default()
    };
    assert_eq!(
        build_local_doc(&doc, &m(vec![(5, "/a.mp3")]), None, &placement).unwrap_err(),
        UniteError::UnknownItem(5)
    );
    assert_eq!(
        build_local_doc(
            &doc,
            &m(vec![(0, "/a.mp3"), (0, "/b.mp3")]),
            None,
            &placement
        )
        .unwrap_err(),
        UniteError::DuplicateItem(0)
    );
    assert_eq!(
        build_local_doc(
            &doc,
            &m(vec![(0, "/a.mp3"), (1, "/a.mp3")]),
            None,
            &placement
        )
        .unwrap_err(),
        UniteError::DuplicateFile(PathBuf::from("/a.mp3"))
    );
    // Partial builds only on request.
    let mut partial = m(vec![(0, "/a.mp3")]);
    assert!(matches!(
        build_local_doc(&doc, &partial, None, &placement),
        Err(UniteError::Incomplete { .. })
    ));
    partial.allow_incomplete = true;
    let plan = build_local_doc(&doc, &partial, None, &placement).unwrap();
    assert_eq!(plan.missing_items, vec![1]);
}

#[test]
fn unite_output_is_valid_noudata_and_indexable_after_apply() {
    let doc = parse(&web_json("EP", "music", "Band", &ep_items()));
    let files = vec![
        file("01 - One.mp3", 100.1),
        file("01 - One.flac", 100.0),
        file("02 - Two.mp3", 200.1),
        file("03 - Three.mp3", 300.1),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    assert!(p.is_complete());
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    let plan =
        build_local_doc(&doc, &p.confirm_all(), Some("EP.noubin.abc123"), &placement).unwrap();

    // Round-trips through the NB_01 parser without warnings and classifies as a release.
    let json = plan.doc.to_json_pretty().unwrap();
    let (back, warnings) = parse_noudata_with_warnings(json.as_bytes()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(back.noubin_key.as_deref(), Some("EP.noubin.abc123"));
    assert_eq!(effective_type(&back, |_| None), NoudataType::Release);
    // v0.4.0: declared release, root profile from the web document, typed rules satisfied.
    assert_eq!(back.noudata_type, Some(NoudataType::Release));
    assert_eq!(back.profile(), PlayableItemProfile::Music);
    assert!(back.type_violations().is_empty());
    assert!(parse_noudata_str(&json).is_ok());

    // Apply the plan in a temp library (the applier is NB_09; this is the plan's contract).
    let tmp = tempfile::tempdir().unwrap();
    let import = tmp.path().join("import");
    let lib = tmp.path().join("lib");
    fs::create_dir_all(&import).unwrap();
    fs::create_dir_all(&lib).unwrap();
    for f in &files {
        fs::write(import.join(name_of(&f.path)), b"x").unwrap();
    }
    for op in &plan.file_ops {
        let from = import.join(name_of(&op.source));
        let to = lib.join(&op.target);
        assert!(is_safe_relative_path(&op.target.to_string_lossy()));
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(from, to).unwrap();
    }
    let noudata = lib.join(&plan.noudata_path);
    plan.doc.write_to_path(&noudata).unwrap();

    let mut writer = LibraryWriter::open(&LibraryRoot::new(&lib)).unwrap();
    writer.full_scan().unwrap();
    let all = writer.list_playables().unwrap();
    assert_eq!(all.len(), 1, "{all:#?}");
    let rec = &all[0];
    assert_eq!(rec.noudata_relpath, "Music/Band/EP/_EP.noudata");
    assert!(!rec.media_missing);
    let items = writer.items_of(rec.id).unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(
        writer
            .find_by_key(&normalise_noubin_key("EP.noubin.abc123"))
            .unwrap()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// hostile input
// ---------------------------------------------------------------------------

fn assert_plan_paths_safe(plan: &plippa_noubin::unite::LocalDocPlan) {
    let mut all: Vec<&Path> = vec![&plan.noudata_path];
    all.extend(plan.file_ops.iter().map(|o| o.target.as_path()));
    for p in all {
        assert!(p.is_relative(), "{p:?}");
        assert!(
            p.components().all(|c| matches!(c, Component::Normal(_))),
            "{p:?}"
        );
        assert!(is_safe_relative_path(&p.to_string_lossy()), "{p:?}");
        assert!(p.to_string_lossy().len() <= 260);
        for c in p.components() {
            assert!(c.as_os_str().len() <= 200);
            assert!(!c.as_os_str().to_string_lossy().starts_with('.'));
        }
    }
    for it in &plan.doc.media_list {
        for l in it.local_file_paths.iter().flatten() {
            assert!(is_safe_relative_path(l) && !l.contains('/'), "{l:?}");
        }
    }
}

#[test]
fn unite_hostile_web_document_and_filenames_stay_inside_the_library() {
    let long = "T".repeat(600);
    let v = json!({
        "playableItemTitle": format!("../../etc/passwd\u{202E}\u{0007}{long}"),
        "playableItemFolderSafeTitle": "../../../root",
        "referToReleaseNoudata": "../../outside.noudata",
        "noubinKey": "stolen",
        "noudataType": "release",
        "playableItemProfile": "music",
        "releaseData": {
            "credits": {"primaryArtists": [{"primaryArtistName": ".plippa"}]},
            "coverImage": {"images": [{"imageLocalFilename": "../../evil.jpg", "imageWebURL": "https://example.com/a.jpg"}]}
        },
        "mediaList": [
            {
                "mediaTitle": "Track\u{0000}One\u{200B}\u{202E}'; DROP TABLE playables;--",
                "localFilePaths": ["/etc/passwd", "../../x.mp3"],
                "uniteData": {"expectedFilename": "../../../etc/passwd", "expectedLength": "0:01:40"}
            },
            {
                "mediaTitle": "CON",
                "uniteData": {"expectedFilename": "C:\\Windows\\system32\\evil.mp3", "expectedLength": "0:03:20"}
            }
        ]
    });
    let doc = parse(&v);
    let files = vec![
        CandidateFile::new("/import/../../etc/shadow.mp3", 1).with_duration_ms(100_000),
        CandidateFile::new("/import/NUL.mp3", 1).with_duration_ms(200_000),
        CandidateFile::new(format!("/import/{}.mp3", "é".repeat(300)), 1),
    ];
    let p = propose(&doc, &files, PlayableItemProfile::Music);
    let mut mapping = p.confirm_all();
    mapping.ignored_files = p.unmatched_files.iter().map(|f| f.path.clone()).collect();
    let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
    let plan = build_local_doc(&doc, &mapping, Some("legit.key"), &placement).unwrap();
    assert_plan_paths_safe(&plan);

    let d = &plan.doc;
    assert_eq!(d.noubin_key.as_deref(), Some("legit.key"));
    assert!(d.refer_to_release_noudata.is_none());
    let title = d.playable_item_title.as_deref().unwrap();
    assert!(title.chars().count() <= 256);
    assert!(!title.contains(['\u{202E}', '\u{0007}']));
    assert!(!d
        .playable_item_folder_safe_title
        .as_deref()
        .unwrap()
        .contains('/'));
    for it in &d.media_list {
        let t = it.media_title.as_deref().unwrap();
        assert!(!t.contains(['\0', '\u{200B}', '\u{202E}']));
        let e = it
            .unite_data
            .as_ref()
            .unwrap()
            .expected_filename
            .as_deref()
            .unwrap();
        assert!(!e.contains(['/', '\\']) && is_safe_relative_path(e), "{e}");
    }
    let cover = &d
        .release_data
        .as_ref()
        .unwrap()
        .cover_image
        .as_ref()
        .unwrap()
        .images[0];
    assert!(cover.image_local_filename.is_none());
    // Serialised JSON never contains the traversal strings as a path field.
    let json = d.to_json_pretty().unwrap();
    assert!(!json.contains("evil.jpg") && !json.contains("outside.noudata"));
    assert!(!json.contains("stolen") && !json.contains("../../../root"));
    // The hostile artist did not create a hidden folder.
    assert!(!plan.noudata_path.to_string_lossy().contains("/."));
}

#[test]
fn unite_constants_are_sane() {
    const { assert!(MIN_MATCH < CONFIDENT_MIN) };
    const { assert!(AMBIGUITY_MARGIN > 0.0) };
}
