//! NB_05: timestamp-paste parser (`parse_cue_paste`).

use plippa_noubin::{
    cue_list_to_paste_text, format_hms_ms, parse_cue_paste, parse_noudata_with_warnings,
    sanitise_cue_text, CuePaste, CueWarningKind, MediaListItem, NoudataDocument,
    MAX_CUE_TEXT_BYTES, MAX_PASTE_CUES,
};

fn starts(p: &CuePaste) -> Vec<u64> {
    p.cues.iter().map(|c| c.start.as_millis()).collect()
}
fn texts(p: &CuePaste) -> Vec<&str> {
    p.cues.iter().map(|c| c.text.as_str()).collect()
}
fn kinds(p: &CuePaste) -> Vec<CueWarningKind> {
    p.warnings.iter().map(|w| w.kind).collect()
}

#[test]
fn cue_paste_timestamped_tracklist() {
    let p = parse_cue_paste(
        "Tracklist:\n\
         0:00 Intro\n\
         03:45 - Wicked Babylon - Clippers\n\
         \n\
         1:02:10 Closing set\n\
         Thanks for listening!\n",
    );
    assert_eq!(starts(&p), vec![0, 225_000, 3_730_000]);
    assert_eq!(
        texts(&p),
        vec!["Intro", "Wicked Babylon - Clippers", "Closing set"]
    );
    // "Tracklist:" and the sign-off are ignored (the blank line is not reported).
    assert_eq!(p.ignored_lines.len(), 2);
    assert_eq!(p.ignored_lines[0].line, 1);
    assert_eq!(p.ignored_lines[0].text, "Tracklist:");
    assert_eq!(p.ignored_lines[1].line, 6);
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
}

#[test]
fn cue_paste_list_markers() {
    let p = parse_cue_paste(
        "- 0:00 One\n\
         * 1:00 Two\n\
         • 2:00 Three\n\
         – 3:00 Four\n\
         — 4:00 Five\n\
         1. 5:00 Six\n\
         12) 6:00 Seven\n\
         [ ] 7:00 Eight\n\
         [x] 8:00 Nine\n\
         - 3. 9:00 Ten\n\
         [10:00] Eleven\n\
         (11:00) Twelve\n",
    );
    assert_eq!(p.cues.len(), 12, "{:?}", p.ignored_lines);
    assert_eq!(
        texts(&p),
        vec![
            "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
            "Eleven", "Twelve"
        ]
    );
    assert_eq!(p.cues[11].start.as_millis(), 11 * 60_000);
    assert!(p.ignored_lines.is_empty());
}

#[test]
fn cue_paste_separators_and_internal_hyphens() {
    let p = parse_cue_paste(
        "0:00 - A - B\n\
         1:00 – En dash\n\
         2:00 — Em dash\n\
         3:00: Colon\n\
         4:00 | Pipe\n\
         5:00-Tight\n\
         6:00\u{2212}Minus sign\n\
         7:00 No separator - kept\n\
         8:00 - - double\n\
         9:00 Spider-Man: Origins\n",
    );
    assert_eq!(
        texts(&p),
        vec![
            "A - B",
            "En dash",
            "Em dash",
            "Colon",
            "Pipe",
            "Tight",
            "Minus sign",
            "No separator - kept",
            "- double",
            "Spider-Man: Origins"
        ]
    );
}

#[test]
fn cue_paste_bare_numbers_are_not_cues() {
    let p = parse_cue_paste("120 Song\n1 Intro\n2. Two\n0:00 Real\n");
    assert_eq!(texts(&p), vec!["Real"]);
    assert_eq!(p.ignored_lines.len(), 3);
    assert!(p.ignored_lines[0].reason.contains("without ':'"));
    // an explicit fraction *is* a timestamp
    let p = parse_cue_paste("1.5 Half\n90.250 Later\n");
    assert_eq!(starts(&p), vec![1500, 90_250]);
}

#[test]
fn cue_paste_lazy_timestamp_forms() {
    let p = parse_cue_paste("0:5 a\n1:23.1 b\n90:00 c\n1:00:00 d\n2:0:0.5 e\n3:00:00.1234 f\n");
    assert_eq!(
        starts(&p),
        vec![5000, 83_100, 3_600_000, 5_400_000, 7_200_500, 10_800_123]
    );
    // sorted output despite the source order (90:00 > 1:00:00 in the paste)
    let sorted: Vec<u64> = {
        let mut v = starts(&p);
        v.sort();
        v
    };
    assert_eq!(starts(&p), sorted);
    assert!(kinds(&p).contains(&CueWarningKind::Unsorted));
}

#[test]
fn cue_paste_hour_long_and_over_24h() {
    let p =
        parse_cue_paste("0:00 Start\n59:59 Almost\n1:00:00 Hour\n10:00:00 Ten\n100:00:00 Long\n");
    assert_eq!(p.cues.len(), 5);
    assert_eq!(p.cues[4].start.as_millis(), 100 * 3600 * 1000);
    assert_eq!(format_hms_ms(p.cues[2].start.as_millis()), "01:00:00.000");
    assert_eq!(format_hms_ms(3_730_005), "01:02:10.005");
}

#[test]
fn cue_paste_boundary_rules() {
    let p = parse_cue_paste(
        "10:30am party\n\
         1.2.3 Section\n\
         12:34:56:78 four parts\n\
         0:00\n\
         0:10 -\n\
         0:20 ok\n\
         Intro 0:30\n\
         :30 no leading digit\n",
    );
    assert_eq!(texts(&p), vec!["ok"]);
    assert_eq!(p.ignored_lines.len(), 7);
    let reasons: Vec<&str> = p.ignored_lines.iter().map(|l| l.reason).collect();
    assert!(reasons.contains(&"timestamp without a title"));
    assert!(reasons.contains(&"not a valid timestamp"));
}

#[test]
fn cue_paste_duplicates_keep_first_and_warn() {
    let p = parse_cue_paste("0:00 A\n1:00 B\n1:00 C\n0:60 D\n2:00 E\n");
    assert_eq!(texts(&p), vec!["A", "B", "E"]);
    let dups: Vec<_> = p
        .warnings
        .iter()
        .filter(|w| w.kind == CueWarningKind::DuplicateTime)
        .collect();
    assert_eq!(dups.len(), 2);
    assert_eq!(dups[0].line, Some(3));
    assert_eq!(dups[1].line, Some(4));
}

#[test]
fn cue_paste_unsorted_input_is_sorted() {
    let p = parse_cue_paste("2:00 C\n0:00 A\n1:00 B\n");
    assert_eq!(texts(&p), vec!["A", "B", "C"]);
    assert_eq!(kinds(&p), vec![CueWarningKind::Unsorted]);
}

#[test]
fn cue_paste_first_cue_not_at_zero_is_noted() {
    let p = parse_cue_paste("0:30 First\n1:00 Second\n");
    assert_eq!(starts(&p), vec![30_000, 60_000]); // no synthetic cue
    assert_eq!(kinds(&p), vec![CueWarningKind::FirstCueNotAtZero]);
}

#[test]
fn cue_paste_junk_and_empty_input() {
    for t in ["", "\n\n  \n", "hello\nworld\n"] {
        let p = parse_cue_paste(t);
        assert!(p.cues.is_empty());
        assert!(p.warnings.is_empty());
    }
    assert_eq!(parse_cue_paste("hello\n\nworld").ignored_lines.len(), 2);
}

#[test]
fn cue_paste_line_endings_and_bom() {
    let p = parse_cue_paste("\u{FEFF}0:00 A\r\n1:00 B\r2:00 C\u{2028}3:00 D\u{85}4:00 E\n");
    assert_eq!(texts(&p), vec!["A", "B", "C", "D", "E"]);
    assert_eq!(p.cues[1].line, 2);
    assert_eq!(p.cues[4].line, 5);
    // No CR/LF ever survives into cue text (saved text is LF-normalised, single line).
    assert!(p.cues.iter().all(|c| !c.text.contains(['\r', '\n'])));
}

#[test]
fn cue_paste_unicode_titles() {
    let p = parse_cue_paste("0:00 日本語のタイトル\n1:00 Café — Ünïcode 🎵\n2:00 Ελληνικά\n");
    assert_eq!(
        texts(&p),
        vec!["日本語のタイトル", "Café — Ünïcode 🎵", "Ελληνικά"]
    );
}

#[test]
fn cue_paste_titles_are_sanitised() {
    let p = parse_cue_paste(
        "0:00 Clean\u{202E}evil\u{200B}\u{0007}\ttext\u{FEFF}\n\
         1:00 \u{200B}\u{202A}\n",
    );
    assert_eq!(texts(&p), vec!["Cleanevil text"]);
    assert!(kinds(&p).contains(&CueWarningKind::TextSanitised));
    assert_eq!(p.ignored_lines.len(), 1); // the title that was only invisible chars
    assert_eq!(sanitise_cue_text("  a\u{202E}b \u{0}c  "), "ab c");
}

#[test]
fn cue_paste_long_titles_are_truncated_on_char_boundary() {
    let long = "é".repeat(2000); // 4000 bytes
    let p = parse_cue_paste(&format!("0:00 {long}\n"));
    assert!(p.cues[0].text.len() <= MAX_CUE_TEXT_BYTES);
    assert!(p.cues[0].text.chars().all(|c| c == 'é'));
    assert!(kinds(&p).contains(&CueWarningKind::TextTruncated));
    // hostile ignored-line echo is bounded too
    let p = parse_cue_paste(&"x".repeat(10_000));
    assert!(p.ignored_lines[0].text.chars().count() <= 201);
}

#[test]
fn cue_paste_cue_count_is_capped() {
    let mut t = String::new();
    for i in 0..(MAX_PASTE_CUES + 50) {
        t.push_str(&format!("{}:{:02} Track {i}\n", i / 60, i % 60));
    }
    let p = parse_cue_paste(&t);
    assert_eq!(p.cues.len(), MAX_PASTE_CUES);
    assert!(kinds(&p).contains(&CueWarningKind::TooManyCues));
}

#[test]
fn cue_paste_drop_beyond_length() {
    let mut p = parse_cue_paste("0:00 A\n1:00 B\n2:00 C\n");
    p.drop_beyond_length(60_000); // exactly at the end is kept (matches NB_01's rule)
    assert_eq!(texts(&p), vec!["A", "B"]);
    assert!(kinds(&p).contains(&CueWarningKind::BeyondLength));
    let bl = p
        .warnings
        .iter()
        .find(|w| w.kind == CueWarningKind::BeyondLength)
        .unwrap();
    assert_eq!(bl.line, Some(3));
}

#[test]
fn cue_paste_shell_and_sql_text_is_just_text() {
    let p = parse_cue_paste(
        "0:00 '); DROP TABLE playables;--\n1:00 $(rm -rf /) `x` <script>alert(1)</script>\n",
    );
    assert_eq!(p.cues.len(), 2);
    assert_eq!(p.cues[0].text, "'); DROP TABLE playables;--");
    assert!(p.cues[1].text.contains("<script>")); // stored verbatim; the UI must use textContent
}

fn doc_with(p: &CuePaste, title: Option<&str>) -> NoudataDocument {
    NoudataDocument {
        playable_item_title: Some("Mix".into()),
        media_list: vec![MediaListItem {
            local_file_paths: Some(vec!["mix.mp3".into()]),
            cue_lists: vec![p.to_cue_list(title)],
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn cue_paste_saved_cue_list_validates_and_reparses_identically() {
    let p = parse_cue_paste(
        "0:00 Intro\n3:45 - Wicked Babylon - Clippers\n1:02:10.5 Closing – set\n99:59:59.999 Last\n",
    );
    let doc = doc_with(&p, Some("Tracklist"));
    let json = doc.to_json_pretty().unwrap();
    let (back, warnings) = parse_noudata_with_warnings(json.as_bytes()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let list = &back.media_list[0].cue_lists[0];
    assert_eq!(list.list_type(), plippa_noubin::CueListType::Nav);
    assert!(list.is_navigable());
    assert_eq!(list.cue_list_title.as_deref(), Some("Tracklist"));
    assert_eq!(list.cues.len(), 4);
    assert!(list.cues.iter().all(|c| c.end_time.is_none()));
    let got: Vec<(u64, String)> = list
        .cues
        .iter()
        .map(|c| (c.start_time.as_millis(), c.text()))
        .collect();
    let want: Vec<(u64, String)> = p
        .cues
        .iter()
        .map(|c| (c.start.as_millis(), c.text.clone()))
        .collect();
    assert_eq!(got, want);
    // and the paste text regenerated from the saved list parses to the same cues
    let again = parse_cue_paste(&cue_list_to_paste_text(list));
    assert_eq!(again.cues.len(), 4);
    for (a, b) in again.cues.iter().zip(&p.cues) {
        assert_eq!((a.start, &a.text), (b.start, &b.text));
    }
    assert!(again.warnings.is_empty());
}

#[test]
fn cue_paste_text_roundtrip_with_tricky_titles() {
    let p = parse_cue_paste(
        "0:00 - - dash first\n1:00 : colon first\n2:00 [x] bracket\n3:00 12. numbered\n",
    );
    let list = p.to_cue_list(None);
    let again = parse_cue_paste(&cue_list_to_paste_text(&list));
    assert_eq!(texts(&again), texts(&p));
}
