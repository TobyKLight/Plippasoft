//! NB_05: timestamp-paste cue import — preview, save (write-through), `add_media` and HTTP.
//!
//! Fixture: `crates/plippa-noubin/tests/fixtures/libraries/kinds` (copied to a temp dir).
//! Its `Mixes/Japanese/_Mix.noudata` has a `nav` list ("Chapters") and a `credits` list;
//! media files are empty placeholders unless a test writes a real WAV.

mod common;

use plippa_noubin::{
    parse_cue_paste, parse_noudata_with_warnings, CueListType, ImportKind, LibraryKind,
    NoudataDocument,
};
use plippa_server::{
    CuePreviewRequest, IdArg, RecordingIpc, SaveCuesRequest, ServerConfig, ServerError,
    ServerService,
};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MIX_REL: &str = "Mixes/Japanese/_Mix.noudata";

fn kinds_library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/plippa-noubin/tests/fixtures/libraries/kinds"),
        &root,
    );
    (tmp, root)
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

fn service(root: &Path) -> ServerService {
    ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )
}

fn id_of(svc: &mut ServerService, title: &str) -> u64 {
    svc.list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.title == title)
        .unwrap_or_else(|| panic!("no collection titled {title}"))
        .id
}

fn read_doc(root: &Path, rel: &str) -> NoudataDocument {
    let bytes = fs::read(root.join(rel)).unwrap();
    let (doc, warnings) = parse_noudata_with_warnings(&bytes).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    doc
}

fn save(text: &str) -> SaveCuesRequest {
    SaveCuesRequest {
        text: text.into(),
        item: None,
        cue_list_title: None,
        revision: None,
    }
}

/// Every file under `dir` with its bytes (to prove "nothing was written").
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    fn walk(d: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".plippa") {
                continue; // the derived index may change on its own
            }
            if e.file_type().unwrap().is_dir() {
                walk(&p, out);
            } else {
                out.push((p.clone(), fs::read(&p).unwrap()));
            }
        }
    }
    walk(dir, &mut out);
    out.sort();
    out
}

/// A valid mono 8-bit WAV of `secs` seconds (8 kHz), so the server can measure its length.
fn wav(secs: u32) -> Vec<u8> {
    let rate = 8000u32;
    let data = vec![128u8; (rate * secs) as usize];
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes()); // byte rate
    v.extend_from_slice(&1u16.to_le_bytes()); // block align
    v.extend_from_slice(&8u16.to_le_bytes()); // bits
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

const TRACKLIST: &str = "Tracklist:\n\
    0:00 Intro\n\
    03:45 - Wicked Babylon - Clippers\n\
    1:02:10 Closing set\n\
    thanks for listening\n";

// ---------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------

#[test]
fn cues_preview_writes_nothing_and_matches_parser() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let before = snapshot(&root);
    let gen = svc.status().generation;

    let p = svc
        .preview_cues(&CuePreviewRequest {
            text: TRACKLIST.into(),
            duration_ms: None,
            id: None,
            item: None,
        })
        .unwrap();
    assert_eq!(p.cue_count, 3);
    assert_eq!(p.cues[1].start, "00:03:45.000");
    assert_eq!(p.cues[1].text, "Wicked Babylon - Clippers");
    assert_eq!(p.cues[2].start, "01:02:10.000");
    assert_eq!(p.ignored_lines.len(), 2);
    assert_eq!(p.duration_ms, None);
    assert_eq!(snapshot(&root), before);
    assert_eq!(svc.status().generation, gen);

    // identical to the pure parser the save path uses
    let direct = parse_cue_paste(TRACKLIST);
    assert_eq!(
        p.cues
            .iter()
            .map(|c| (c.start_ms, c.text.as_str()))
            .collect::<Vec<_>>(),
        direct
            .cues
            .iter()
            .map(|c| (c.start.as_millis(), c.text.as_str()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn cues_preview_with_duration_drops_and_warns() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let p = svc
        .preview_cues(&CuePreviewRequest {
            text: TRACKLIST.into(),
            duration_ms: Some(600_000),
            id: None,
            item: None,
        })
        .unwrap();
    assert_eq!(p.cue_count, 2);
    assert_eq!(p.duration_ms, Some(600_000));
    assert!(p
        .warnings
        .iter()
        .any(|w| format!("{:?}", w.kind) == "BeyondLength" && w.line == Some(4)));
}

// ---------------------------------------------------------------------------
// Save / read back
// ---------------------------------------------------------------------------

#[test]
fn cues_put_replaces_only_the_nav_list() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = id_of(&mut svc, "Rocksteady Set");
    let gen = svc.status().generation;

    let got = svc.get_item_cues(id, None).unwrap();
    assert_eq!(got.cue_lists.len(), 2);
    assert_eq!(got.owned_index, Some(0));
    assert_eq!(got.cue_lists[0].title.as_deref(), Some("Chapters"));
    assert_eq!(got.cue_lists[1].cue_list_type, "credits");
    assert_eq!(got.kind, LibraryKind::Mix);

    let rep = svc.put_item_cues(id, save(TRACKLIST)).unwrap();
    assert!(rep.saved && rep.replaced && !rep.cleared);
    assert_eq!(rep.cue_list_index, Some(0));
    assert_eq!(rep.cue_count, 3);
    assert_eq!(rep.ignored_lines.len(), 2);
    assert!(svc.status().generation > gen, "index generation must bump");

    let doc = read_doc(&root, MIX_REL); // validates: no NB_01 warnings
    let lists = &doc.media_list[0].cue_lists;
    assert_eq!(lists.len(), 2);
    // nav replaced in place; its title survived
    assert_eq!(lists[0].list_type(), CueListType::Nav);
    assert_eq!(lists[0].cue_list_title.as_deref(), Some("Chapters"));
    assert!(lists[0].is_navigable());
    assert_eq!(lists[0].cues.len(), 3);
    assert_eq!(lists[0].cues[1].text(), "Wicked Babylon - Clippers");
    assert_eq!(lists[0].cues[1].start_time.as_millis(), 225_000);
    assert!(lists[0].cues.iter().all(|c| c.end_time.is_none()));
    // credits list untouched
    assert_eq!(lists[1].list_type(), CueListType::Credits);
    assert_eq!(lists[1].cues.len(), 1);
    assert_eq!(lists[1].cues[0].text(), "Wicked Babylon - Clippers");
    // the rest of the document survived
    assert_eq!(doc.playable_item_title.as_deref(), Some("Rocksteady Set"));
    assert_eq!(doc.media_list.len(), 1);

    // index sees the cues (reads them back through the reader API)
    let after = svc.get_item_cues(id, None).unwrap();
    assert_eq!(after.cue_lists[0].cues.len(), 3);
    assert_eq!(after.revision, rep.revision.clone().unwrap());
    // re-parse of the regenerated text = identical cues
    let again = parse_cue_paste(&after.text);
    assert_eq!(again.cues.len(), 3);
    assert_eq!(
        again
            .cues
            .iter()
            .map(|c| (c.start.as_millis(), c.text.clone()))
            .collect::<Vec<_>>(),
        parse_cue_paste(TRACKLIST)
            .cues
            .iter()
            .map(|c| (c.start.as_millis(), c.text.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn cues_put_appends_nav_and_keeps_lyrics_and_extra_fields() {
    let (_t, root) = kinds_library();
    let rel = "Podcasts/Show/_Show.noudata";
    // give the episode a lyrics list, a transcript and an unknown key
    let mut raw: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(rel)).unwrap()).unwrap();
    raw["mediaList"][0]["lyricsTranscript"] = "la la\r\nla".into();
    raw["mediaList"][0]["someFutureKey"] = serde_json::json!({"keep": true});
    raw["mediaList"][0]["cueLists"] = serde_json::json!([
        {"cueListType": "text", "cueListLanguage": "en", "cueListNavigable": false,
         "cues": [{"startTime": "0:05", "cueTextSpans": [{"text": "hello"}, {"text": "world"}]}]}
    ]);
    fs::write(root.join(rel), serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
    let mut svc = service(&root);
    svc.lazy_rescan().unwrap();
    let id = id_of(&mut svc, "The Show");
    assert!(svc.get_item_cues(id, None).unwrap().owned_index.is_none());

    let mut req = save("0:00 Cold open\n10:00 Interview\n");
    req.cue_list_title = Some("Chapters\u{202E}".into());
    let rep = svc.put_item_cues(id, req).unwrap();
    assert!(!rep.replaced);
    assert_eq!(rep.cue_list_index, Some(1));

    let doc = read_doc(&root, rel);
    let item = &doc.media_list[0];
    assert_eq!(item.cue_lists.len(), 2);
    assert_eq!(item.cue_lists[0].list_type(), CueListType::Text);
    assert_eq!(item.cue_lists[0].cue_list_language.as_deref(), Some("en"));
    assert_eq!(item.cue_lists[0].cues[0].cue_text_spans.len(), 2);
    assert_eq!(item.cue_lists[1].list_type(), CueListType::Nav);
    assert_eq!(
        item.cue_lists[1].cue_list_title.as_deref(),
        Some("Chapters")
    ); // bidi stripped
    assert_eq!(item.lyrics_transcript.as_deref(), Some("la la\nla")); // LF-normalised
    assert_eq!(item.extra["someFutureKey"]["keep"], true);

    // saving again replaces the appended list, not the text list
    svc.put_item_cues(id, save("0:00 Only\n")).unwrap();
    let doc = read_doc(&root, rel);
    assert_eq!(doc.media_list[0].cue_lists.len(), 2);
    assert_eq!(doc.media_list[0].cue_lists[1].cues.len(), 1);
    assert_eq!(
        doc.media_list[0].cue_lists[1].cue_list_title.as_deref(),
        Some("Chapters")
    );
}

#[test]
fn cues_put_blank_text_removes_only_the_owned_list() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = id_of(&mut svc, "Rocksteady Set");
    let rep = svc.put_item_cues(id, save("  \n")).unwrap();
    assert!(rep.cleared && rep.replaced);
    assert_eq!(rep.cue_count, 0);
    let doc = read_doc(&root, MIX_REL);
    assert_eq!(doc.media_list[0].cue_lists.len(), 1);
    assert_eq!(
        doc.media_list[0].cue_lists[0].list_type(),
        CueListType::Credits
    );
}

#[test]
fn cues_put_unusable_text_is_refused_and_changes_nothing() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = id_of(&mut svc, "Rocksteady Set");
    let before = snapshot(&root);
    for bad in ["just some words\nno stamps", "120 Song\n2. Two", "0:10 -"] {
        match svc.put_item_cues(id, save(bad)) {
            Err(ServerError::Invalid(m)) => assert!(m.contains("no cues"), "{m}"),
            other => panic!("expected Invalid for {bad:?}, got {other:?}"),
        }
    }
    let huge = format!("0:00 {}\n", "x".repeat(300_000));
    assert!(matches!(
        svc.put_item_cues(id, save(&huge)),
        Err(ServerError::Invalid(_))
    ));
    assert_eq!(snapshot(&root), before);
}

#[test]
fn cues_put_rejects_wrong_targets() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    // playlist
    let pl = id_of(&mut svc, "Rainy Day");
    assert!(matches!(
        svc.put_item_cues(pl, save("0:00 x")),
        Err(ServerError::Invalid(_))
    ));
    // orphan media file (no .noudata)
    let orphan = svc
        .list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.title.contains("orphan"))
        .expect("orphan fixture")
        .id;
    assert!(matches!(
        svc.put_item_cues(orphan, save("0:00 x")),
        Err(ServerError::Invalid(_))
    ));
    assert!(matches!(
        svc.get_item_cues(orphan, None),
        Err(ServerError::Invalid(_))
    ));
    // no such item / no such playable
    let mix = id_of(&mut svc, "Rocksteady Set");
    let mut r = save("0:00 x");
    r.item = Some(3);
    assert!(matches!(
        svc.put_item_cues(mix, r),
        Err(ServerError::NotFound(_))
    ));
    assert!(matches!(
        svc.get_item_cues(mix, Some(3)),
        Err(ServerError::NotFound(_))
    ));
    assert!(matches!(
        svc.put_item_cues(mix ^ 0xFFFF, save("0:00 x")),
        Err(ServerError::NotFound(_))
    ));
}

#[test]
fn cues_put_stale_revision_conflicts() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = id_of(&mut svc, "Rocksteady Set");
    let seen = svc.get_item_cues(id, None).unwrap().revision;
    let mut r = save("0:00 A\n1:00 B\n");
    r.revision = Some(seen.clone());
    let rep = svc.put_item_cues(id, r).unwrap();
    // the same (now stale) revision again -> 409
    let mut r = save("0:00 C\n");
    r.revision = Some(seen);
    assert!(matches!(
        svc.put_item_cues(id, r),
        Err(ServerError::Conflict(_))
    ));
    let doc = read_doc(&root, MIX_REL);
    assert_eq!(doc.media_list[0].cue_lists[0].cues.len(), 2);
    // and with the fresh one it works
    let mut r = save("0:00 C\n");
    r.revision = rep.revision;
    svc.put_item_cues(id, r).unwrap();
}

#[test]
fn cues_put_validates_against_the_real_media_length() {
    let (_t, root) = kinds_library();
    // Real, measurable 60 s file for the mix.
    fs::write(root.join("Mixes/Japanese/mix.mp3"), []).unwrap();
    let raw = fs::read_to_string(root.join(MIX_REL))
        .unwrap()
        .replace("mix.mp3", "mix.wav");
    fs::write(root.join(MIX_REL), raw).unwrap();
    fs::write(root.join("Mixes/Japanese/mix.wav"), wav(60)).unwrap();
    let mut svc = service(&root);
    svc.lazy_rescan().unwrap();
    let id = id_of(&mut svc, "Rocksteady Set");

    let text = "0:00 A\n0:30 B\n1:30 C\n2:00 D\n";
    // preview by item id uses the real length (no duration_ms given) ...
    let prev = svc
        .preview_cues(&CuePreviewRequest {
            text: text.into(),
            duration_ms: None,
            id: Some(IdArg(id)),
            item: None,
        })
        .unwrap();
    assert_eq!(prev.duration_ms, Some(60_000));
    assert_eq!(prev.cue_count, 2);
    // ... and is exactly what is saved
    let rep = svc.put_item_cues(id, save(text)).unwrap();
    assert_eq!(rep.duration_ms, Some(60_000));
    assert_eq!(rep.cue_count, 2);
    assert_eq!(rep.cues.len(), prev.cues.len());
    for (a, b) in rep.cues.iter().zip(&prev.cues) {
        assert_eq!((a.start_ms, &a.text), (b.start_ms, &b.text));
    }
    assert_eq!(rep.warnings.len(), 2);
    let doc = read_doc(&root, MIX_REL);
    assert_eq!(doc.media_list[0].cue_lists[0].cues.len(), 2);
    // every cue lies inside the file
    assert!(doc.media_list[0].cue_lists[0]
        .cues
        .iter()
        .all(|c| c.start_time.as_millis() <= 60_000));
}

#[test]
fn cues_hostile_text_is_stored_as_inert_sanitised_text() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = id_of(&mut svc, "Rocksteady Set");
    let text = "0:00 <img src=x onerror=alert(1)>\u{202E}\u{200B}\n\
                1:00 ../../etc/passwd\n\
                2:00 '); DROP TABLE playables;--\n\
                3:00 line\u{0}with\u{7}control\u{1B}[31m\n";
    svc.put_item_cues(id, save(text)).unwrap();
    let doc = read_doc(&root, MIX_REL);
    let cues = &doc.media_list[0].cue_lists[0].cues;
    assert_eq!(cues[0].text(), "<img src=x onerror=alert(1)>"); // no bidi / zero-width
    assert_eq!(cues[1].text(), "../../etc/passwd"); // just text, never a path
    assert_eq!(cues[2].text(), "'); DROP TABLE playables;--");
    assert_eq!(cues[3].text(), "linewithcontrol[31m");
    assert!(cues
        .iter()
        .all(|c| !c.text().chars().any(|ch| ch.is_control())));
    // library intact and still listed
    assert_eq!(
        svc.list_collections(Some(LibraryKind::Mix)).unwrap().len(),
        1
    );
    assert!(root.join("Mixes/Japanese/mix.mp3").is_file());
    assert!(!root.parent().unwrap().join("etc").exists());
}

// ---------------------------------------------------------------------------
// add_media with cue_text
// ---------------------------------------------------------------------------

#[test]
fn cues_add_media_long_form_kinds_write_single_item_with_cues() {
    for (kind, profile) in [
        (ImportKind::Mix, "mix"),
        (ImportKind::Audiobook, "audiobook"),
        (ImportKind::Podcast, "podcast"),
    ] {
        let (_t, root) = kinds_library();
        let mut svc = service(&root);
        let out = svc
            .add_media_with_cues(kind, "long form.wav", &wav(300), Some(TRACKLIST))
            .unwrap();
        let rep = out.cues.expect("cue report");
        assert_eq!(rep.cue_count, 2, "1:02:10 is past the 5 min file: dropped");
        assert_eq!(rep.duration_ms, Some(300_000));
        assert!(rep
            .warnings
            .iter()
            .any(|w| format!("{:?}", w.kind) == "BeyondLength"));

        let doc = read_doc(&root, &out.summary.noudata_relpath);
        // valid single-item mediaList
        assert_eq!(doc.media_list.len(), 1);
        assert_eq!(
            doc.media_list[0].local_file_paths.as_ref().unwrap().len(),
            1
        );
        assert_eq!(doc.profile().as_str(), profile);
        let lists = &doc.media_list[0].cue_lists;
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].list_type(), CueListType::Nav);
        assert_eq!(lists[0].cues.len(), 2);
        assert_eq!(lists[0].cues[1].text(), "Wicked Babylon - Clippers");

        // it shows up as that kind with navigable cues, and GET returns them
        let got = svc.get_item_cues(out.summary.id, None).unwrap();
        assert_eq!(got.cue_lists[0].cues.len(), 2);
        assert_eq!(got.duration_ms, Some(300_000));
        let d = svc
            .list_collections(None)
            .unwrap()
            .into_iter()
            .find(|c| c.id == out.summary.id)
            .unwrap();
        // No tags ⇒ a looseTrack: listed under Tracks; the profile rides in the file.
        assert_eq!(d.kind.as_str(), "track");
        assert_eq!(doc.noudata_type, Some(plippa_noubin::NoudataType::LooseTrack));
    }
}

#[test]
fn cues_add_media_without_cue_text_is_unchanged() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let out = svc
        .add_media_with_cues(ImportKind::Mix, "m.wav", &wav(5), Some("  \n"))
        .unwrap();
    assert!(out.cues.is_none());
    let doc = read_doc(&root, &out.summary.noudata_relpath);
    assert_eq!(doc.media_list.len(), 1);
    assert!(doc.media_list[0].cue_lists.is_empty());
}

#[test]
fn cues_add_media_rejects_cue_text_for_music_and_other_and_junk() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let before = snapshot(&root);
    for kind in [ImportKind::Music, ImportKind::Other] {
        match svc.add_media_with_cues(kind, "x.wav", &wav(5), Some("0:00 A")) {
            Err(ServerError::Invalid(m)) => assert!(m.contains("only accepted"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
    assert!(matches!(
        svc.add_media_with_cues(ImportKind::Mix, "x.wav", &wav(5), Some("no stamps here")),
        Err(ServerError::Invalid(_))
    ));
    assert_eq!(
        snapshot(&root),
        before,
        "nothing may be written on rejection"
    );
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn http_raw(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let body = body.unwrap_or("");
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head, raw[split + 4..].to_vec())
}

const JSON: (&str, &str) = ("Content-Type", "application/json");

fn http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, serde_json::Value) {
    let text = body.map(|b| b.to_string());
    let (st, _, bytes) = http_raw(port, method, path, text.as_deref(), &[JSON]);
    (
        st,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn start_http(root: &Path) -> u16 {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let svc = Arc::new(Mutex::new(service(root)));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root.to_path_buf()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[test]
fn cues_http_preview_get_put_flow() {
    let (_t, root) = kinds_library();
    let port = start_http(&root);
    let before = snapshot(&root);

    let (st, v) = http(
        port,
        "POST",
        "/api/cues/preview",
        Some(&serde_json::json!({"text": TRACKLIST})),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["cue_count"], 3);
    assert_eq!(v["cues"][2]["start"], "01:02:10.000");
    assert_eq!(v["ignored_lines"].as_array().unwrap().len(), 2);
    assert_eq!(v["ignored_lines"][0]["line"], 1);
    assert!(v["duration_ms"].is_null());
    assert_eq!(snapshot(&root), before, "preview must not write");

    let (st, v) = http(
        port,
        "POST",
        "/api/cues/preview",
        Some(&serde_json::json!({"text": TRACKLIST, "duration_ms": 300000})),
    );
    assert_eq!(st, 200);
    assert_eq!(v["cue_count"], 2);
    assert_eq!(v["warnings"][0]["kind"], "beyond_length");

    let (_, list) = http(port, "GET", "/api/collections?kind=mix", None);
    let id = list["collections"][0]["id"].as_str().unwrap().to_string();

    let (st, v) = http(port, "GET", &format!("/api/items/{id}/cues"), None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["item"], 0);
    assert_eq!(v["owned_index"], 0);
    assert_eq!(v["cue_lists"].as_array().unwrap().len(), 2);
    assert_eq!(v["cue_lists"][0]["cues"][0]["text"], "Warm up");
    assert!(v["text"].as_str().unwrap().contains("Warm up"));
    let rev = v["revision"].as_str().unwrap().to_string();

    let (st, v) = http(
        port,
        "PUT",
        &format!("/api/items/{id}/cues"),
        Some(&serde_json::json!({"text": TRACKLIST, "revision": rev})),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["saved"], true);
    assert_eq!(v["cue_count"], 3);
    let (st, v) = http(
        port,
        "PUT",
        &format!("/api/items/{id}/cues"),
        Some(&serde_json::json!({"text": "0:00 again", "revision": rev})),
    );
    assert_eq!(st, 409, "stale revision: {v}");
    let (st, v) = http(
        port,
        "PUT",
        &format!("/api/items/{id}/cues"),
        Some(&serde_json::json!({"text": "junk"})),
    );
    assert_eq!(st, 400, "{v}");
    let (st, _) = http(port, "GET", "/api/items/999/cues", None);
    assert_eq!(st, 404);
    let (st, _) = http(port, "GET", "/api/items/abc/cues", None);
    assert_eq!(st, 400);
    let (st, _) = http(port, "GET", &format!("/api/items/{id}/cues?item=7"), None);
    assert_eq!(st, 404);

    let doc = read_doc(&root, MIX_REL);
    assert_eq!(doc.media_list[0].cue_lists.len(), 2);
    assert_eq!(doc.media_list[0].cue_lists[0].cues.len(), 3);
}

#[test]
fn cues_http_guards_content_type_origin_size_and_json() {
    let (_t, root) = kinds_library();
    let port = start_http(&root);
    let body = r#"{"text":"0:00 A"}"#;

    // no / wrong Content-Type (what a cross-site HTML form can send) -> 415
    for h in [
        &[][..],
        &[("Content-Type", "text/plain")][..],
        &[("Content-Type", "application/x-www-form-urlencoded")][..],
    ] {
        let (st, _, _) = http_raw(port, "POST", "/api/cues/preview", Some(body), h);
        assert_eq!(st, 415);
        let (st, _, _) = http_raw(port, "PUT", "/api/items/1/cues", Some(body), h);
        assert_eq!(st, 415);
    }
    // Origin that is not this portal -> 403; the same origin as Host -> allowed
    let (st, _, _) = http_raw(
        port,
        "POST",
        "/api/cues/preview",
        Some(body),
        &[JSON, ("Origin", "https://evil.example")],
    );
    assert_eq!(st, 403);
    let (st, _, _) = http_raw(
        port,
        "PUT",
        "/api/items/1/cues",
        Some(body),
        &[JSON, ("Origin", "http://evil.example")],
    );
    assert_eq!(st, 403);
    let (st, _, _) = http_raw(
        port,
        "POST",
        "/api/cues/preview",
        Some(body),
        &[JSON, ("Origin", "http://x")],
    );
    assert_eq!(st, 200);
    // charset parameter is fine
    let (st, _, _) = http_raw(
        port,
        "POST",
        "/api/cues/preview",
        Some(body),
        &[("Content-Type", "application/json; charset=utf-8")],
    );
    assert_eq!(st, 200);
    // malformed JSON -> 400, oversize -> 413 (no panic, server keeps serving)
    let (st, _, _) = http_raw(port, "POST", "/api/cues/preview", Some("{nope"), &[JSON]);
    assert_eq!(st, 400);
    let (st, _, _) = http_raw(
        port,
        "POST",
        "/api/cues/preview",
        Some(r#"{"text":5}"#),
        &[JSON],
    );
    assert_eq!(st, 400);
    let big = format!(r#"{{"text":"{}"}}"#, "a".repeat(700_000));
    let (st, _, _) = http_raw(port, "POST", "/api/cues/preview", Some(&big), &[JSON]);
    assert_eq!(st, 413);
    let (st, _, _) = http_raw(port, "POST", "/api/cues/preview", Some(body), &[JSON]);
    assert_eq!(st, 200);
    // hardening headers on every response
    let (_, head, _) = http_raw(port, "GET", "/api/status", None, &[]);
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("x-content-type-options: nosniff"), "{head}");
    assert!(lower.contains("referrer-policy: no-referrer"), "{head}");
}

#[test]
fn cues_http_add_media_with_percent_encoded_unicode_cue_text() {
    let (_t, root) = kinds_library();
    let port = start_http(&root);
    let text = "0:00 Intro\n1:00 – Café — 日本語\n";
    let path = format!(
        "/api/media?kind=mix&filename=set.wav&cue_text={}",
        pct(text)
    );
    let body = wav(120);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Plippa: 1\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(&body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    assert!(head.contains(" 200 "), "{head}");
    let v: serde_json::Value = serde_json::from_slice(&raw[split + 4..]).unwrap();
    assert!(v["id"].is_string(), "old response shape kept: {v}");
    assert_eq!(v["cues"]["cue_count"], 2);
    let rel = v["noudata_relpath"].as_str().unwrap();
    let doc = read_doc(&root, rel);
    assert_eq!(
        doc.media_list[0].cue_lists[0].cues[1].text(),
        "Café — 日本語"
    );

    // music + cue_text -> 400
    let path = format!(
        "/api/media?kind=music&filename=t.wav&cue_text={}",
        pct("0:00 A")
    );
    let (st, _, _) = http_raw(port, "POST", &path, Some("RIFF"), &[("X-Plippa", "1")]);
    assert_eq!(st, 400);
}

// ---------------------------------------------------------------------------
// NB_16: edit cues of existing items; orphan media get a `.noudata`
// ---------------------------------------------------------------------------

fn orphan_id(svc: &mut ServerService) -> u64 {
    svc.list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.is_orphan)
        .expect("orphan fixture")
        .id
}

#[test]
fn cues_orphan_gets_a_noudata_then_accepts_cues() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let orphan = orphan_id(&mut svc);
    let media_before = fs::read(root.join("Loose/orphan song.mp3")).unwrap();

    let made = svc
        .create_noudata_for_orphan(orphan, ImportKind::Mix, Some("  My\u{202E}Mix\u{200B} "))
        .unwrap();
    assert_eq!(
        made.title.as_deref(),
        Some("MyMix"),
        "control/bidi/zero-width stripped"
    );
    assert_eq!(
        svc.get_item_cues(made.id, None).unwrap().kind,
        LibraryKind::Track,
        "a tagless orphan becomes a looseTrack (profile mix)"
    );
    let doc = read_doc(&root, "Loose/orphan song.noudata");
    assert_eq!(doc.profile(), plippa_noubin::PlayableItemProfile::Mix);
    assert_eq!(doc.playable_item_title.as_deref(), Some("MyMix"));
    assert_eq!(doc.media_list.len(), 1);
    assert_eq!(
        doc.media_list[0].local_file_paths.as_deref(),
        Some(&["orphan song.mp3".to_string()][..])
    );
    assert_eq!(
        fs::read(root.join("Loose/orphan song.mp3")).unwrap(),
        media_before,
        "the media file is untouched"
    );
    // The orphan row is gone (same file, now owned by the new playable); no duplicates.
    let all = svc.list_collections(None).unwrap();
    assert!(!all
        .iter()
        .any(|c| c.is_orphan && c.title.contains("orphan")));
    assert_eq!(all.iter().filter(|c| c.title == "MyMix").count(), 1);

    // Now cues can be saved and read back.
    let got = svc.get_item_cues(made.id, None).unwrap();
    assert!(got.text.is_empty());
    let r = svc
        .put_item_cues(
            made.id,
            SaveCuesRequest {
                revision: Some(got.revision),
                ..save("0:00 A\n1:00 B")
            },
        )
        .unwrap();
    assert_eq!(r.cue_count, 2);
    let doc = read_doc(&root, "Loose/orphan song.noudata");
    assert_eq!(doc.media_list[0].cue_lists.len(), 1);

    // A second create is refused (it is not an orphan any more).
    assert!(matches!(
        svc.create_noudata_for_orphan(made.id, ImportKind::Music, None),
        Err(ServerError::Invalid(_))
    ));
}

#[test]
fn cues_orphan_noudata_never_replaces_an_existing_file_and_refuses_non_orphans() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let orphan = orphan_id(&mut svc);
    // Something already sits at the companion name (not indexed yet): it must survive.
    fs::write(root.join("Loose/orphan song.noudata"), b"not json at all").unwrap();
    assert!(matches!(
        svc.create_noudata_for_orphan(orphan, ImportKind::Music, None),
        Err(ServerError::Conflict(_))
    ));
    assert_eq!(
        fs::read(root.join("Loose/orphan song.noudata")).unwrap(),
        b"not json at all"
    );
    // Not an orphan: a mix with its own .noudata, and a playlist.
    let mix = id_of(&mut svc, "Rocksteady Set");
    assert!(matches!(
        svc.create_noudata_for_orphan(mix, ImportKind::Mix, None),
        Err(ServerError::Invalid(_))
    ));
    let pl = id_of(&mut svc, "Rainy Day");
    assert!(matches!(
        svc.create_noudata_for_orphan(pl, ImportKind::Mix, None),
        Err(ServerError::Invalid(_))
    ));
    assert!(matches!(
        svc.create_noudata_for_orphan(orphan ^ 0xFFFF, ImportKind::Mix, None),
        Err(ServerError::NotFound(_))
    ));
}

#[test]
fn cues_http_create_noudata_for_orphan_and_guards() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let orphan = orphan_id(&mut svc);
    drop(svc);
    let port = start_http(&root);
    let path = format!("/api/items/{orphan}/noudata");

    // Needs application/json (CSRF rule) and a same-origin request.
    let (st, _, _) = http_raw(port, "POST", &path, Some("{\"kind\":\"mix\"}"), &[]);
    assert_eq!(st, 415);
    let (st, _, _) = http_raw(
        port,
        "POST",
        &path,
        Some("{\"kind\":\"mix\"}"),
        &[JSON, ("Origin", "http://evil.example")],
    );
    assert_eq!(st, 403);
    let (st, _) = http(
        port,
        "POST",
        &path,
        Some(&serde_json::json!({"kind": "bogus"})),
    );
    assert_eq!(st, 400);
    let (st, _, _) = http_raw(port, "POST", &path, Some("{oops"), &[JSON]);
    assert_eq!(st, 400);
    assert!(!root.join("Loose/orphan song.noudata").exists());

    // The cue endpoints still refuse the orphan with the hint.
    let (st, v) = http(port, "GET", &format!("/api/items/{orphan}/cues"), None);
    assert_eq!(st, 400, "{v}");

    let (st, v) = http(
        port,
        "POST",
        &path,
        Some(&serde_json::json!({"kind": "audiobook", "title": "<img src=x onerror=alert(1)>"})),
    );
    assert_eq!(st, 200, "{v}");
    let new_id = v["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, orphan.to_string());
    // Title is stored as plain text (the portal renders it with textContent).
    assert_eq!(v["title"], "<img src=x onerror=alert(1)>");
    let (st, g) = http(port, "GET", &format!("/api/items/{new_id}/cues"), None);
    assert_eq!(st, 200, "{g}");
    assert_eq!(g["kind"], "track");
    // Replaying the request: the orphan id no longer exists as an orphan.
    let (st, _) = http(
        port,
        "POST",
        &path,
        Some(&serde_json::json!({"kind": "mix"})),
    );
    assert!(st == 404 || st == 400, "{st}");
}

/// SECFIX_05: the item length used by preview/save is read only from a confined regular file.
/// A symlink named in a `.noudata` is not followed (its target is a measurable 10 s file that
/// the old code would have probed), and a FIFO cannot block the call.
#[test]
fn cue_length_probe_refuses_symlinks_and_fifos() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (t, root) = kinds_library();
    fs::write(t.path().join("outside.wav"), wav(10)).unwrap();
    std::os::unix::fs::symlink(t.path().join("outside.wav"), root.join("Mixes/link.wav")).unwrap();
    extern "C" {
        fn mkfifo(path: *const std::os::raw::c_char, mode: u32) -> i32;
    }
    let fifo = root.join("Mixes/pipe.wav");
    let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0);
    for (title, media) in [("Linked", "link.wav"), ("Piped", "pipe.wav")] {
        fs::write(
            root.join(format!("Mixes/{title}.noudata")),
            format!(
                r#"{{"noudataType":"release","playableItemTitle":"{title}","releaseData":{{}},
                   "mediaList":[{{"localFilePaths":["{media}"]}}]}}"#
            ),
        )
        .unwrap();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut svc = service(&root);
        svc.lazy_rescan().unwrap();
        let mut out = Vec::new();
        for title in ["Linked", "Piped"] {
            let id = svc
                .list_playables()
                .unwrap()
                .into_iter()
                .find(|p| p.title.as_deref() == Some(title))
                .unwrap()
                .id;
            let prev = svc
                .preview_cues(&CuePreviewRequest {
                    text: "0:00 A\n0:30 B\n".into(),
                    duration_ms: None,
                    id: Some(IdArg(id)),
                    item: None,
                })
                .unwrap();
            out.push(prev.duration_ms);
        }
        let _ = tx.send(out);
    });
    let got = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the cue length probe blocked on a FIFO");
    assert_eq!(got, vec![None, None], "a symlink or FIFO was probed");
}
