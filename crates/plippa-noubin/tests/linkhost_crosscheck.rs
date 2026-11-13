//! Cross-check of Noubin linkhost `web.noudata` files (standard v0.4.0) against the parser,
//! sanitiser and URL handling. The fixtures are a committed copy of generated linkhost cases (see
//! `tests/fixtures/linkhost-crosscheck/PROVENANCE.md`); nothing is fetched. This file covers parsing,
//! sanitising, URL and key handling and the unite proposal; the association journey is in
//! `plippa-server` (`xcheck_tests`).

use plippa_noubin::unite::{propose, CandidateFile, Confidence, Placement};
use plippa_noubin::{
    classify_type, effective_type, fetch_url_for_key, is_url_noubin_key, normalise_noubin_key,
    normalise_noubin_url, parse_noudata_with_warnings, sanitise_document, NoubinTimestamp,
    NoudataType, PlatformType, PlayableItemProfile, SanitiseOptions,
};
use serde_json::Value;
use std::path::PathBuf;

pub struct Case {
    pub id: String,
    pub bytes: Vec<u8>,
    pub web: Value,
    pub meta: Value,
}

impl Case {
    pub fn s(&self, key: &str) -> &str {
        self.meta[key]
            .as_str()
            .unwrap_or_else(|| panic!("{}: case.json has no string `{key}`", self.id))
    }
    pub fn published_files(&self) -> Vec<String> {
        self.meta["publishedFiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }
}

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linkhost-crosscheck")
}

pub fn cases() -> Vec<Case> {
    let dir = fixtures_dir();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let ids: Vec<String> = manifest["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 11, "the linkhost publishes 11 cross-check cases");
    ids.into_iter()
        .map(|id| {
            let bytes = std::fs::read(dir.join(&id).join("web.noudata")).unwrap();
            let web: Value = serde_json::from_slice(&bytes).unwrap();
            let meta: Value =
                serde_json::from_slice(&std::fs::read(dir.join(&id).join("case.json")).unwrap())
                    .unwrap();
            assert_eq!(meta["id"], id.as_str());
            Case {
                id,
                bytes,
                web,
                meta,
            }
        })
        .collect()
}

fn prof(s: &str) -> PlayableItemProfile {
    match s {
        "music" => PlayableItemProfile::Music,
        "podcast" => PlayableItemProfile::Podcast,
        "audiobook" => PlayableItemProfile::Audiobook,
        "mix" => PlayableItemProfile::Mix,
        _ => PlayableItemProfile::Other,
    }
}

/// (a) parse: effective type release, profile right, zero warnings, unknowns preserved.
#[test]
fn xcheck_a_parse_type_profile_and_unknown_properties() {
    for c in cases() {
        let (doc, w) = parse_noudata_with_warnings(&c.bytes).unwrap();
        assert!(w.is_empty(), "{}: unexpected parse warnings {w:?}", c.id);
        assert_eq!(doc.noudata_type, Some(NoudataType::Release), "{}", c.id);
        let cl = classify_type(&doc, |_| None);
        assert_eq!(cl.ty, NoudataType::Release, "{}", c.id);
        assert!(
            cl.declared && cl.warnings.is_empty(),
            "{}: {:?}",
            c.id,
            cl.warnings
        );
        assert_eq!(
            effective_type(&doc, |_| None),
            NoudataType::Release,
            "{}",
            c.id
        );
        assert!(
            doc.type_violations().is_empty(),
            "{}: {:?}",
            c.id,
            doc.type_violations()
        );
        let want = c.web["playableItemProfile"].as_str().unwrap();
        assert_eq!(doc.profile(), prof(want), "{}", c.id);
        assert_eq!(doc.playable_item_profile, Some(prof(want)), "{}", c.id);
        assert_eq!(doc.noudata_version.as_deref(), Some("0.4.0"), "{}", c.id);
        assert!(
            doc.noubin_key.is_none() && doc.noubin_key_normalised.is_none(),
            "{}",
            c.id
        );
        assert_eq!(
            doc.media_list.len(),
            c.web["mediaList"].as_array().unwrap().len(),
            "{}",
            c.id
        );
        // The linkhost now writes the standard's fixed `_whatAmILookingAt` text, first key.
        assert_eq!(
            doc.what_am_i_looking_at.as_deref(),
            Some(plippa_noubin::WHAT_AM_I_LOOKING_AT),
            "{}",
            c.id
        );
        assert_eq!(
            c.web["_whatAmILookingAt"].as_str(),
            Some(plippa_noubin::WHAT_AM_I_LOOKING_AT),
            "{}",
            c.id
        );
        assert_eq!(
            c.web.as_object().unwrap().keys().next().map(String::as_str),
            Some("_whatAmILookingAt"),
            "{}: first key",
            c.id
        );
        // ... and only standard properties: nothing is left over as an unknown (preserved)
        // property anywhere we model, and no empty `coverImage` object is emitted.
        let rd = doc.release_data.as_ref().unwrap();
        assert!(rd.extra.is_empty(), "{}: releaseData extras {:?}", c.id, rd.extra);
        for img in rd
            .cover_image
            .iter()
            .flat_map(|ci| ci.images.iter())
        {
            assert!(img.extra.is_empty(), "{}: cover image extras {:?}", c.id, img.extra);
        }
        let has_cover = c.published_files().iter().any(|f| f == "cover.webp");
        assert_eq!(
            c.web["releaseData"].get("coverImage").is_some(),
            has_cover,
            "{}: coverImage present exactly when a cover is published",
            c.id
        );
        for item in c.web["mediaList"].as_array().unwrap() {
            assert!(
                item.get("uniteData").is_some(),
                "{}: every item carries uniteData (standard 03)",
                c.id
            );
        }
        // Credits/description/dates/categories sit where the standard puts them (credits).
        let cr = rd.credits.as_ref().unwrap();
        for k in ["description", "releaseDate", "categories", "language"] {
            assert_eq!(
                cr.extra.get(k),
                c.web["releaseData"]["credits"].get(k),
                "{}: credits.{k}",
                c.id
            );
        }
    }
}

/// Semantic JSON diff: what the sanitiser is allowed to change for a linkhost document.
/// Returns the list of differences that are NOT on the allow-list.
fn unexplained_diffs(input: &Value, output: &Value, fetched: bool) -> Vec<String> {
    let mut raw = Vec::new();
    diff("", input, output, &mut raw);
    raw.into_iter()
        .filter(|d| {
            // Empty collections are dropped (no information lost).
            let empty_dropped = d.ends_with(" MISSING") && {
                let p = d.trim_end_matches(" MISSING");
                let mut cur = input;
                for seg in p.split('/').filter(|s| !s.is_empty()) {
                    cur = match cur {
                        Value::Object(m) => &m[seg],
                        Value::Array(a) => &a[seg.parse::<usize>().unwrap()],
                        _ => unreachable!(),
                    };
                }
                match cur {
                    Value::Array(a) => a.is_empty(),
                    Value::Object(m) => {
                        m.is_empty()
                            || (p.ends_with("coverImage")
                                && m.get("images").is_some_and(|i| i == &Value::Array(vec![])))
                    }
                    _ => false,
                }
            };
            // Lazy timestamps are re-written in canonical HH:MM:SS (same instant, checked below).
            let lazy_ts = d.contains("/uniteData/expectedLength CHANGED");
            // Fetched documents lose local-only `imageLocalFilename` (the local name is assigned
            // when the image is downloaded).
            let local_only = fetched && d.ends_with("/imageLocalFilename MISSING");
            !(empty_dropped || lazy_ts || local_only)
        })
        .collect()
}

/// (b) sanitise keeps every legitimate field; nothing mangled.
#[test]
fn xcheck_b_sanitise_keeps_every_legitimate_field() {
    for c in cases() {
        for fetched in [true, false] {
            let (mut doc, _) = parse_noudata_with_warnings(&c.bytes).unwrap();
            let opts = if fetched {
                SanitiseOptions::fetched()
            } else {
                SanitiseOptions::imported()
            };
            let warnings = sanitise_document(&mut doc, opts);
            assert!(
                warnings.is_empty(),
                "{} (fetched={fetched}): unexpected sanitise warnings {warnings:?}",
                c.id
            );
            let out: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
            let bad = unexplained_diffs(&c.web, &out, fetched);
            assert!(bad.is_empty(), "{} (fetched={fetched}): {bad:?}", c.id);
            // Lazy timestamps: same instant as the input string.
            for (i, item) in c.web["mediaList"].as_array().unwrap().iter().enumerate() {
                if let Some(l) = item["uniteData"]["expectedLength"].as_str() {
                    let want = NoubinTimestamp::parse(l).unwrap();
                    assert_eq!(
                        doc.media_list[i]
                            .unite_data
                            .as_ref()
                            .unwrap()
                            .expected_length,
                        Some(want),
                        "{} item {i}",
                        c.id
                    );
                }
                // expectedFilename / freeDownloadURL verbatim.
                for k in ["expectedFilename", "freeDownloadURL"] {
                    assert_eq!(
                        out["mediaList"][i]["uniteData"][k], item["uniteData"][k],
                        "{} {k}",
                        c.id
                    );
                }
                assert_eq!(
                    out["mediaList"][i].get("mediaTitle"),
                    item.get("mediaTitle"),
                    "{}",
                    c.id
                );
            }
            // Titles / names are byte-identical (the fixtures are NFC; emoji and CJK intact).
            for p in [
                "/playableItemTitle",
                "/playableItemURLSafeTitle",
                "/playableItemFolderSafeTitle",
                "/playableItemProfile",
                "/releaseData/credits/primaryArtists",
                "/releaseData/credits/primaryArtistURLSafeName",
                "/releaseData/credits/primaryArtistFolderSafeName",
                "/releaseData/credits/primaryArtistOverrideName",
                "/releaseData/credits/contributors",
                "/releaseData/credits/description",
                "/releaseData/releasePlatforms",
                "/releaseData/furtherLinks",
            ] {
                assert_eq!(
                    out.pointer(p),
                    c.web
                        .pointer(p)
                        .filter(|v| !matches!(v, Value::Array(a) if a.is_empty())),
                    "{} {p}",
                    c.id
                );
            }
            // The web URL of the cover is kept, with its query.
            if let Some(u) = c
                .web
                .pointer("/releaseData/coverImage/images/0/imageWebURL")
            {
                assert_eq!(
                    out.pointer("/releaseData/coverImage/images/0/imageWebURL"),
                    Some(u),
                    "{}",
                    c.id
                );
            }
            if fetched {
                // No local-only data appears in a fetched doc.
                assert!(out.get("noubinKey").is_none(), "{}", c.id);
            } else {
                assert_eq!(
                    out.pointer("/releaseData/coverImage/images/0/imageLocalFilename"),
                    c.web
                        .pointer("/releaseData/coverImage/images/0/imageLocalFilename"),
                    "{}",
                    c.id
                );
            }
        }
    }
}

/// Platform types and custom verb survive with the right enum values.
#[test]
fn xcheck_b_release_platform_types_and_links() {
    let all = cases();
    let c = all
        .iter()
        .find(|c| c.id == "release-platform-types-long")
        .unwrap();
    let (mut doc, _) = parse_noudata_with_warnings(&c.bytes).unwrap();
    assert!(sanitise_document(&mut doc, SanitiseOptions::fetched()).is_empty());
    let p = doc
        .release_data
        .as_ref()
        .unwrap()
        .release_platforms
        .as_ref()
        .unwrap();
    let types: Vec<_> = p.iter().map(|x| x.platform_type).collect();
    assert_eq!(
        types,
        [
            Some(PlatformType::Store),
            Some(PlatformType::DrmStore),
            Some(PlatformType::Streaming),
            Some(PlatformType::Other)
        ]
    );
    assert_eq!(p[3].custom_verb.as_deref(), Some("Order on"));
    assert_eq!(p[0].platform_url, "https://multi.bandcamp.com/album/all");
    let l = doc
        .release_data
        .as_ref()
        .unwrap()
        .further_links
        .as_ref()
        .unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(l[1].link_url, "https://video.example/bands/multi");
}

/// Unicode/emoji titles are intact (NFC only) after the round trip.
#[test]
fn xcheck_b_unicode_titles_are_intact() {
    let all = cases();
    let c = all
        .iter()
        .find(|c| c.id == "unicode-emoji-explicit-slugs-long")
        .unwrap();
    let (mut doc, _) = parse_noudata_with_warnings(&c.bytes).unwrap();
    assert!(sanitise_document(&mut doc, SanitiseOptions::fetched()).is_empty());
    assert_eq!(
        doc.playable_item_title.as_deref(),
        Some("봄날 🌸 Spring Day")
    );
    assert_eq!(
        doc.media_list[1].media_title.as_deref(),
        Some("Emoji Song 🎸🔥")
    );
    assert_eq!(
        doc.primary_artist_display().as_deref(),
        Some("Björk ✨ Guðmundsdóttir")
    );
    let c = all
        .iter()
        .find(|c| c.id == "unicode-title-derived-slug-long")
        .unwrap();
    let (mut doc, _) = parse_noudata_with_warnings(&c.bytes).unwrap();
    assert!(sanitise_document(&mut doc, SanitiseOptions::fetched()).is_empty());
    assert_eq!(
        doc.playable_item_title.as_deref(),
        Some("Café del Mar — Été 2026")
    );
    assert_eq!(
        doc.primary_artist_display().as_deref(),
        Some("José González")
    );
}

/// (c) URL handling: long and short forms and their variants.
#[test]
fn xcheck_c_url_normalisation_and_fetch_urls() {
    for c in cases() {
        let long = c.meta["longUrl"].as_str();
        let short = c.s("shortUrl");
        let metadata = c.s("metadataUrl");
        let code = c.s("code");
        let shape = c.s("publishedUrlShape");
        let published = if shape == "long" {
            long.unwrap()
        } else {
            short
        };
        // Both forms map to `<url>/web.noudata` on their own host.
        if let Some(l) = long {
            assert_eq!(
                normalise_noubin_url(l),
                format!("{l}web.noudata"),
                "{}",
                c.id
            );
        }
        assert_eq!(
            normalise_noubin_url(short),
            format!("{short}web.noudata"),
            "{}",
            c.id
        );
        // The form the linkhost published serves web.noudata at exactly that URL.
        assert_eq!(normalise_noubin_url(published), metadata, "{}", c.id);
        assert_eq!(
            fetch_url_for_key(published).unwrap().wire,
            metadata,
            "{}",
            c.id
        );
        assert_eq!(
            fetch_url_for_key(published).unwrap().display,
            metadata,
            "{}",
            c.id
        );

        let no_slash = published.trim_end_matches('/').to_string();
        let variants = [
            published.replace("https://", "http://"),
            published.replace("https://", "https://www."),
            published.replace("https://", "HTTPS://"),
            no_slash.clone(),
            format!("{published}?ref=nfc"),
            format!("{published}#track-3"),
            format!("{published}index.html"),
            format!("{published}web.noudata"),
            format!(" {published} "),
            {
                // Uppercase host only (path case is significant: the code is upper-case).
                let rest = published.strip_prefix("https://").unwrap();
                let (h, p) = rest.split_once('/').unwrap();
                format!("https://{}/{p}", h.to_ascii_uppercase())
            },
        ];
        for v in &variants {
            let n = normalise_noubin_url(v);
            let expect = if v.contains("?ref=nfc") {
                format!("{metadata}?ref=nfc")
            } else {
                metadata.to_string()
            };
            assert_eq!(n, expect, "{}: variant {v:?}", c.id);
            let f = fetch_url_for_key(v.trim()).unwrap();
            assert_eq!(f.wire, expect, "{}: wire of {v:?}", c.id);
            assert!(is_url_noubin_key(v), "{}: {v:?}", c.id);
        }

        // §6.2 Noubin Key normalisation of the tapped URL: stable across every variant and
        // idempotent. Code case is folded (keys are case-insensitive), the fetch URL keeps it.
        let key_norm = normalise_noubin_key(published);
        let rest = published.strip_prefix("https://").unwrap();
        assert_eq!(key_norm, rest.to_ascii_lowercase(), "{}", c.id);
        assert!(key_norm.contains(&code.to_ascii_lowercase()), "{}", c.id);
        // (§6.2 does not strip a trailing `index.html` / `web.noudata`: only the fetch URL does.)
        for v in variants
            .iter()
            .filter(|v| !v.ends_with("index.html") && !v.ends_with("web.noudata"))
        {
            let v = v.trim();
            let q = normalise_noubin_key(v);
            assert_eq!(q, key_norm, "{}: key of {v:?}", c.id);
        }
        assert_eq!(
            normalise_noubin_key(&key_norm),
            key_norm,
            "{}: idempotent",
            c.id
        );
        // The other URL form is a different key (different host): never conflated.
        let other = if shape == "long" {
            short
        } else {
            long.unwrap()
        };
        assert_ne!(normalise_noubin_key(other), key_norm, "{}", c.id);
    }
}

/// (e, part 1) Image URLs implied by `imageWebURL` and the §1.27 thumbnail naming match what
/// the linkhost publishes (see also the server-side download check).
#[test]
fn xcheck_e_published_files_match_cover_and_thumbnail_naming() {
    for c in cases() {
        let published = c.published_files();
        let images = c
            .web
            .pointer("/releaseData/coverImage/images")
            .and_then(Value::as_array);
        let first = images.and_then(|a| a.first());
        match first {
            None => {
                assert!(
                    !published
                        .iter()
                        .any(|f| f.starts_with("cover") || f.starts_with("thumbnails/")),
                    "{}: no cover entry but cover files are published",
                    c.id
                );
            }
            Some(img) => {
                let url = img["imageWebURL"].as_str().unwrap();
                let dir = c.s("publicUrl");
                let path = url
                    .strip_prefix(dir)
                    .unwrap_or_else(|| panic!("{}: {url} not under {dir}", c.id));
                let file = path.split('?').next().unwrap();
                assert!(
                    published.contains(&file.to_string()),
                    "{}: {file} not published",
                    c.id
                );
                assert_eq!(img["imageLocalFilename"].as_str(), Some(file), "{}", c.id);
                let stem = file.rsplit_once('.').unwrap().0;
                for tier in [128, 512, 1024] {
                    for ext in ["jpg", "webp"] {
                        let t = format!("thumbnails/{stem}-{tier}.{ext}");
                        assert!(published.contains(&t), "{}: {t} not published", c.id);
                    }
                }
            }
        }
    }
}

/// Paths present in `a` that are missing or different in `b`.
pub fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) => diff(&format!("{path}/{k}"), v, w, out),
                    None => out.push(format!("{path}/{k} MISSING")),
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!("{path} LEN {} -> {}", x.len(), y.len()));
            }
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}/{i}"), v, w, out);
            }
        }
        _ => {
            if a != b {
                out.push(format!("{path} CHANGED {a} -> {b}"));
            }
        }
    }
}

/// (d, matcher) Files named like real downloads, with correct durations, are matched to the
/// linkhost's `uniteData` (expectedFilename / expectedLength) without panicking, completely
/// and with confidence. Includes keyword-only expected names ("Chapter 01").
#[test]
fn xcheck_d_unite_matches_real_download_names_for_every_case() {
    // (case id, [(file name, seconds)]) -- the same names the server journey uploads.
    let names: &[(&str, &[(&str, u64)])] = &[
        (
            "music-album-unitedata-long",
            &[
                ("01 Polar Night.mp3", 225),
                ("02 - Green Sky.flac", 245),
                ("03 Dawn.mp3", 3723),
            ],
        ),
        (
            "audiobook-long",
            &[
                ("Chapter 01.mp3", 2712),
                ("Chapter 02.mp3", 3090),
                ("Chapter 03.mp3", 3765),
            ],
        ),
        ("mix-single-file-long", &[("Midnight Drive Mix.mp3", 7200)]),
        (
            "unicode-emoji-explicit-slugs-long",
            &[("01 봄날.mp3", 274), ("02 Emoji Song.mp3", 179)],
        ),
        (
            "override-artist-various-short",
            &[("01 - Duet.flac", 213), ("02 - Solo.flac", 142)],
        ),
        (
            "release-platform-types-long",
            &[("01 Track A.mp3", 3), ("02 Track B.flac", 185)],
        ),
        (
            "music-album-free-download-short",
            &[("01 Session One.mp3", 150), ("02 Session Two.mp3", 190)],
        ),
        ("podcast-episode-short", &[("tape-talk-042.mp3", 3487)]),
    ];
    let all = cases();
    for (id, files) in names {
        let c = all.iter().find(|c| c.id == *id).unwrap();
        let (mut doc, _) = parse_noudata_with_warnings(&c.bytes).unwrap();
        assert!(sanitise_document(&mut doc, SanitiseOptions::fetched()).is_empty());
        let cands: Vec<CandidateFile> = files
            .iter()
            .map(|(n, secs)| {
                CandidateFile::new(PathBuf::from(n), 1000).with_duration_ms(secs * 1000)
            })
            .collect();
        let p = propose(&doc, &cands, doc.profile());
        assert!(p.is_complete(), "{id}: {p:?}");
        assert_eq!(p.matches.len(), doc.media_list.len(), "{id}");
        for m in &p.matches {
            assert_eq!(
                m.level,
                Confidence::Confident,
                "{id}: item {} {:?}",
                m.item_index,
                m.reasons
            );
            assert_eq!(
                m.files[0].path,
                PathBuf::from(files[m.item_index].0),
                "{id}: item {} got the wrong file",
                m.item_index
            );
        }
    }
}

/// (d, placement) The artist's `playableItemFolderSafeTitle` / `primaryArtistFolderSafeName`
/// choose the local folder names (standard 03: "used for local library file paths").
#[test]
fn xcheck_d_placement_honours_folder_safe_names() {
    let all = cases();
    let c = all
        .iter()
        .find(|c| c.id == "podcast-episode-short")
        .unwrap();
    let (doc, _) = parse_noudata_with_warnings(&c.bytes).unwrap();
    // Title "Episode 42: Why Tape Is Not Dead", folder-safe "Episode 42 Why Tape Is Not Dead".
    let p = Placement::for_release(&doc, doc.profile());
    assert_eq!(
        p.dir,
        PathBuf::from("Podcasts/Episode 42 Why Tape Is Not Dead")
    );

    let doc = plippa_noubin::parse_noudata_str(
        r#"{"playableItemTitle":"HE??O","playableItemFolderSafeTitle":"HELLO","playableItemProfile":"music",
            "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"MYSTERY *"}],
                                      "primaryArtistFolderSafeName":"MYSTERY STAR"}},
            "mediaList":[{"mediaTitle":"a"},{"mediaTitle":"b"}]}"#,
    )
    .unwrap();
    let p = Placement::for_release(&doc, doc.profile());
    assert_eq!(p.dir, PathBuf::from("Music/MYSTERY STAR/HELLO"));
    // Without the hints the automatic conversion applies (and stays path-safe).
    let doc = plippa_noubin::parse_noudata_str(
        r#"{"playableItemTitle":"HE??O","playableItemProfile":"music",
            "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"MYSTERY *"}]}},
            "mediaList":[{"mediaTitle":"a"},{"mediaTitle":"b"}]}"#,
    )
    .unwrap();
    let p = Placement::for_release(&doc, doc.profile());
    assert!(p.dir.starts_with("Music") && !p.dir.to_string_lossy().contains('?'));
    // A hostile hint is cleaned, never trusted as a path.
    let doc = plippa_noubin::parse_noudata_str(
        r#"{"playableItemTitle":"T","playableItemFolderSafeTitle":"../../etc","playableItemProfile":"music",
            "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"A"}],
                                      "primaryArtistFolderSafeName":"/abs/x"}},
            "mediaList":[{},{}]}"#,
    )
    .unwrap();
    let p = Placement::for_release(&doc, doc.profile());
    assert!(
        p.dir
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
            && p.dir.starts_with("Music"),
        "{:?}",
        p.dir
    );
}

/// Tolerance regression: older / third-party linkhosts may still publish properties the standard
/// does not define (`mediaDownloadPolicy`, `showDownloadFreeButton`, `imageWidth`/`imageHeight`), a
/// custom `_whatAmILookingAt`, and an empty `coverImage.images`. The Noubin linkhost no longer
/// does, but a player must keep coping: unknown properties are preserved, the custom hint text is
/// kept when fetching, the empty cover is silently ignored and nothing is dropped.
#[test]
fn xcheck_a_tolerates_non_standard_linkhost_output() {
    let legacy = serde_json::json!({
        "_whatAmILookingAt": "This is a Noubin release metadata file (web.noudata). Open it with a Noubin-compatible player.",
        "noudataVersion": "0.4.0",
        "noudataType": "release",
        "playableItemTitle": "Legacy Linkhost Release",
        "playableItemProfile": "music",
        "releaseData": {
            "credits": { "primaryArtists": [{ "primaryArtistName": "Old Host" }] },
            "coverImage": { "images": [] },
            "releasePlatforms": [
                { "platformName": "Bandcamp", "platformType": "store", "platformURL": "https://old.bandcamp.com/album/x" }
            ],
            "furtherLinks": [],
            "mediaDownloadPolicy": "mixed",
            "showDownloadFreeButton": true
        },
        "mediaList": [
            { "mediaTitle": "One", "uniteData": { "expectedFilename": "01 One", "expectedLength": "3:00" } }
        ]
    });
    let with_size = {
        let mut v = legacy.clone();
        v["releaseData"]["coverImage"] = serde_json::json!({
            "images": [{
                "imageLocalFilename": "cover.webp",
                "imageWebURL": "https://old.example/cover.webp",
                "imageWidth": 1500,
                "imageHeight": 1500
            }]
        });
        v
    };

    // Empty `coverImage.images` and every non-standard property: no parse warning at all.
    let (doc, w) = parse_noudata_with_warnings(legacy.to_string().as_bytes()).unwrap();
    assert!(w.is_empty(), "empty coverImage.images must be silent: {w:?}");
    assert_eq!(
        doc.what_am_i_looking_at.as_deref(),
        legacy["_whatAmILookingAt"].as_str(),
        "a custom hint text is tolerated as found"
    );
    let rd = doc.release_data.as_ref().unwrap();
    assert_eq!(rd.extra.get("mediaDownloadPolicy"), Some(&Value::from("mixed")));
    assert_eq!(rd.extra.get("showDownloadFreeButton"), Some(&Value::from(true)));
    assert_eq!(doc.media_list.len(), 1);
    assert_eq!(doc.profile(), PlayableItemProfile::Music);

    // Fetched + imported sanitising keeps the unknown properties and emits no warnings for them.
    for opts in [SanitiseOptions::fetched(), SanitiseOptions::imported()] {
        let (mut doc, _) = parse_noudata_with_warnings(legacy.to_string().as_bytes()).unwrap();
        let warnings = sanitise_document(&mut doc, opts);
        assert!(warnings.is_empty(), "{warnings:?}");
        let out: Value = serde_json::from_str(&doc.to_json_pretty().unwrap()).unwrap();
        assert_eq!(out["releaseData"]["mediaDownloadPolicy"], "mixed");
        assert_eq!(out["releaseData"]["showDownloadFreeButton"], true);
    }

    // Declared image size survives as unknown properties on the image.
    let (doc, w) = parse_noudata_with_warnings(with_size.to_string().as_bytes()).unwrap();
    assert!(w.is_empty(), "{w:?}");
    let img = &doc.release_data.as_ref().unwrap().cover_image.as_ref().unwrap().images[0];
    assert_eq!(img.extra.get("imageWidth"), Some(&Value::from(1500)));
    assert_eq!(img.extra.get("imageHeight"), Some(&Value::from(1500)));
}
