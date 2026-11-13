//! Whole-document sanitiser for untrusted `.noudata` (fetched `web.noudata`, imported files,
//! anything not written by the user's own player). `NBSecurityRequirements.md` §2.2 / §3.
//!
//! Runs on the **typed** parse result ([`crate::parse_noudata_with_warnings`]) so unknown
//! enums / invalid elements are already gone; it then walks every typed field **and** every
//! preserved `extra` map, because unknown properties are written back verbatim and would
//! otherwise be a side door. Nothing that has not passed through here may be stored, indexed,
//! written to disk or shown.
//!
//! Field rules (see the table in the security requirements):
//! * every string: control / bidi / zero-width stripped; titles and names NFC + single line +
//!   <= 256 chars; long text <= 64 KiB (multiline kept); cue text <= 2 KiB; tags <= 64 chars x
//!   <= 64; URLs via [`crate::sanitise_url`] (invalid URL: the element or field is dropped);
//! * relative paths must pass [`crate::is_safe_relative_path`] (one leading `../` is tolerated
//!   for `referToReleaseNoudata` only when local data is kept; the applier re-checks after
//!   joining), otherwise they are dropped;
//! * numbers are clamped to spec ranges; collections are capped (see the `MAX_*` constants);
//! * with [`SanitiseOptions::fetched`], local-only properties (`noubinKey*`,
//!   `referToReleaseNoudata`, `localPlaybackData`, `itemLocalPlaybackData`, `itemUserNotes`,
//!   `localFilePaths`, `imageLocalFilename`, `localCoverImage`) are removed with a warning: they
//!   MUST NOT come from a web linkhost.

use crate::netpolicy::sanitise_url;
use crate::noudata::{MediaListItem, NoudataDocument};
use crate::noudata_types::{
    Contributor, CoverImage, Credits, Cue, CueList, ImageEntry, NoudataWarning, PrimaryArtist,
    ReleaseData, ReleasePlatform, UniteData, UserNotes,
};
use crate::sanitise::{
    is_safe_relative_path, safe_path_segment, sanitise_key, sanitise_line, sanitise_multiline,
    sanitise_tag, sanitise_title, MAX_LONG_TEXT_BYTES, MAX_SEGMENT_BYTES, MAX_TAGS,
};
use crate::timestamp::NoubinTimestamp;
use serde_json::{Map, Value};

/// Most `mediaList` items kept.
pub const MAX_MEDIA_ITEMS: usize = 5000;
/// Most cue lists per item / cues per list / images per cover / platforms / links.
pub const MAX_CUE_LISTS: usize = 16;
pub const MAX_CUES_PER_LIST: usize = 5000;
pub const MAX_IMAGES: usize = 32;
pub const MAX_PLATFORMS: usize = 64;
pub const MAX_FURTHER_LINKS: usize = 64;
pub const MAX_CONTRIBUTORS: usize = 500;
pub const MAX_PRIMARY_ARTISTS: usize = 100;
/// Longest single-line string in a preserved unknown property (characters).
pub const MAX_EXTRA_STRING_CHARS: usize = 2048;
/// Deepest nesting kept inside a preserved unknown property.
pub const MAX_EXTRA_DEPTH: usize = 8;
/// Most entries kept in one preserved array / object.
pub const MAX_EXTRA_ENTRIES: usize = 500;
/// Longest cue text (bytes), all spans together.
pub const MAX_CUE_TEXT_BYTES_TOTAL: usize = 2048;
/// Longest `customVerb` (characters).
pub const MAX_VERB_CHARS: usize = 64;
/// Longest expected media length kept (100 hours), a sanity clamp for `expectedLength`.
pub const MAX_EXPECTED_LENGTH_MS: u64 = 100 * 3_600_000;

/// Property names whose strings are legitimately multi-line.
const MULTILINE_KEYS: &[&str] = &[
    "description",
    "legalNotice",
    "additionalCreditsText",
    "lyricsTranscript",
    "userComment",
];

/// What the sanitiser should assume about the source.
#[derive(Debug, Clone, Copy, Default)]
pub struct SanitiseOptions {
    /// Remove local-only properties (a document that came from the web).
    pub strip_local_data: bool,
}

impl SanitiseOptions {
    /// A document fetched from a linkhost.
    pub const fn fetched() -> Self {
        Self {
            strip_local_data: true,
        }
    }
    /// A `.noudata` that arrived on someone else's stick / by import: local data is kept
    /// (sanitised) because an imported library legitimately carries it.
    pub const fn imported() -> Self {
        Self {
            strip_local_data: false,
        }
    }
}

struct Ctx {
    opts: SanitiseOptions,
    warnings: Vec<NoudataWarning>,
}

impl Ctx {
    fn warn(&mut self, path: &str, message: impl Into<String>) {
        self.warnings.push(NoudataWarning {
            path: path.to_string(),
            message: message.into(),
        });
    }

    /// Sanitise a title-like string in place; `None` if nothing is left.
    fn title(&mut self, path: &str, s: Option<String>) -> Option<String> {
        let s = s?;
        let clean = sanitise_title(&s);
        if clean.is_empty() {
            self.warn(path, "empty after sanitising; dropped");
            return None;
        }
        if clean != s {
            self.warn(
                path,
                "text sanitised (control/bidi characters, whitespace or length)",
            );
        }
        Some(clean)
    }

    fn long_text(&mut self, path: &str, s: Option<String>) -> Option<String> {
        let s = s?;
        let clean = sanitise_multiline(&s, MAX_LONG_TEXT_BYTES);
        if clean != s {
            self.warn(path, "text sanitised");
        }
        Some(clean).filter(|c| !c.trim().is_empty())
    }

    fn url(&mut self, path: &str, s: Option<String>) -> Option<String> {
        let s = s?;
        match sanitise_url(&s) {
            Some(u) => Some(u),
            None => {
                self.warn(path, "URL is not an acceptable https URL; dropped");
                None
            }
        }
    }

    fn segment(&mut self, path: &str, s: Option<String>) -> Option<String> {
        let s = s?;
        let seg = safe_path_segment(&s, "", MAX_SEGMENT_BYTES);
        if seg.is_empty() {
            self.warn(path, "not a usable name; dropped");
            return None;
        }
        if seg != s {
            self.warn(path, "name sanitised");
        }
        Some(seg)
    }

    fn rel_path(&mut self, path: &str, s: String, allow_parent: bool) -> Option<String> {
        let mut rest = s.as_str();
        if allow_parent {
            if let Some(r) = rest.strip_prefix("../") {
                rest = r;
            }
        }
        if is_safe_relative_path(rest) && s.len() <= 4 * MAX_SEGMENT_BYTES {
            Some(s)
        } else {
            self.warn(path, "unsafe relative path; dropped");
            None
        }
    }
}

/// Sanitise `doc` in place; returns one warning per repair or removal.
pub fn sanitise_document(doc: &mut NoudataDocument, opts: SanitiseOptions) -> Vec<NoudataWarning> {
    let mut cx = Ctx {
        opts,
        warnings: Vec::new(),
    };

    doc.what_am_i_looking_at = doc
        .what_am_i_looking_at
        .take()
        .map(|s| sanitise_line(&s, 256))
        .filter(|s| !s.is_empty());
    doc.noudata_version = doc
        .noudata_version
        .take()
        .map(|s| sanitise_line(&s, 32))
        .filter(|s| !s.is_empty());

    if opts.strip_local_data {
        for (name, present) in [
            ("noubinKey", doc.noubin_key.take().is_some()),
            (
                "noubinKeyNormalised",
                doc.noubin_key_normalised.take().is_some(),
            ),
            (
                "referToReleaseNoudata",
                doc.refer_to_release_noudata.take().is_some(),
            ),
            (
                "localPlaybackData",
                doc.local_playback_data.take().is_some(),
            ),
        ] {
            if present {
                cx.warn(name, "must not appear in web.noudata; removed");
            }
        }
    } else {
        doc.noubin_key = doc.noubin_key.take().and_then(|k| sanitise_key(&k));
        doc.noubin_key_normalised = doc
            .noubin_key_normalised
            .take()
            .and_then(|k| sanitise_key(&k));
        if let Some(r) = doc.refer_to_release_noudata.take() {
            doc.refer_to_release_noudata = cx.rel_path("referToReleaseNoudata", r, true);
        }
    }

    doc.playable_item_title = cx.title("playableItemTitle", doc.playable_item_title.take());
    doc.playable_item_folder_safe_title = cx.segment(
        "playableItemFolderSafeTitle",
        doc.playable_item_folder_safe_title.take(),
    );
    doc.playable_item_url_safe_title = cx.segment(
        "playableItemURLSafeTitle",
        doc.playable_item_url_safe_title.take(),
    );

    if let Some(rd) = doc.release_data.as_mut() {
        release_data(&mut cx, rd, "releaseData");
    }
    if let Some(lpd) = doc.local_playback_data.as_mut() {
        lpd.search_string = lpd
            .search_string
            .take()
            .map(|s| sanitise_line(&s, 256))
            .filter(|s| !s.is_empty());
        if let Some(n) = lpd.user_notes.as_mut() {
            user_notes(&mut cx, n, "localPlaybackData.userNotes");
        }
        if let Some(c) = lpd.local_cover_image.take() {
            lpd.local_cover_image =
                user_cover(&mut cx, c, "localPlaybackData.localCoverImage");
        }
        extra_map(&mut cx, &mut lpd.extra, "localPlaybackData");
    }

    if doc.media_list.len() > MAX_MEDIA_ITEMS {
        cx.warn(
            "mediaList",
            format!("more than {MAX_MEDIA_ITEMS} items; extra items dropped"),
        );
        doc.media_list.truncate(MAX_MEDIA_ITEMS);
    }
    for (i, item) in doc.media_list.iter_mut().enumerate() {
        media_item(&mut cx, item, &format!("mediaList[{i}]"));
    }

    extra_map(&mut cx, &mut doc.extra, "");
    // Guard the reserved keys even if they sneaked into `extra` under another spelling path.
    if opts.strip_local_data {
        for k in ["noubinKey", "noubinKeyNormalised"] {
            if doc.extra.remove(k).is_some() {
                cx.warn(k, "must not appear in web.noudata; removed");
            }
        }
    }
    cx.warnings
}

fn release_data(cx: &mut Ctx, rd: &mut ReleaseData, path: &str) {
    if let Some(c) = rd.credits.as_mut() {
        credits(cx, c, &format!("{path}.credits"));
    }
    if let Some(c) = rd.cover_image.take() {
        rd.cover_image = cover(cx, c, &format!("{path}.coverImage"));
    }
    if let Some(platforms) = rd.release_platforms.take() {
        let mut out = Vec::new();
        for (i, p) in platforms.into_iter().enumerate() {
            let pp = format!("{path}.releasePlatforms[{i}]");
            if out.len() >= MAX_PLATFORMS {
                cx.warn(&pp, "too many platforms; dropped");
                continue;
            }
            if let Some(p) = platform(cx, p, &pp) {
                out.push(p);
            }
        }
        rd.release_platforms = Some(out).filter(|v| !v.is_empty());
    }
    if let Some(links) = rd.further_links.take() {
        let mut out = Vec::new();
        for (i, mut l) in links.into_iter().enumerate() {
            let pp = format!("{path}.furtherLinks[{i}]");
            if out.len() >= MAX_FURTHER_LINKS {
                cx.warn(&pp, "too many links; dropped");
                continue;
            }
            let Some(url) = cx.url(&format!("{pp}.linkURL"), Some(l.link_url.clone())) else {
                cx.warn(&pp, "link dropped");
                continue;
            };
            l.link_url = url;
            l.link_title = cx.title(&format!("{pp}.linkTitle"), l.link_title.take());
            extra_map(cx, &mut l.extra, &pp);
            out.push(l);
        }
        rd.further_links = Some(out).filter(|v| !v.is_empty());
    }
    extra_map(cx, &mut rd.extra, path);
}

fn platform(cx: &mut Ctx, mut p: ReleasePlatform, path: &str) -> Option<ReleasePlatform> {
    let Some(url) = cx.url(&format!("{path}.platformURL"), Some(p.platform_url.clone())) else {
        cx.warn(path, "platform dropped (no acceptable URL)");
        return None;
    };
    let Some(name) = cx.title(
        &format!("{path}.platformName"),
        Some(p.platform_name.clone()),
    ) else {
        cx.warn(path, "platform dropped (no name)");
        return None;
    };
    p.platform_url = url;
    p.platform_name = name;
    p.custom_verb = p
        .custom_verb
        .take()
        .map(|v| sanitise_line(&v, MAX_VERB_CHARS))
        .filter(|v| !v.is_empty());
    extra_map(cx, &mut p.extra, path);
    Some(p)
}

fn credits(cx: &mut Ctx, c: &mut Credits, path: &str) {
    if let Some(artists) = c.primary_artists.take() {
        let mut out: Vec<PrimaryArtist> = Vec::new();
        for (i, mut a) in artists.into_iter().enumerate() {
            let pp = format!("{path}.primaryArtists[{i}]");
            if out.len() >= MAX_PRIMARY_ARTISTS {
                cx.warn(&pp, "too many artists; dropped");
                continue;
            }
            match cx.title(
                &format!("{pp}.primaryArtistName"),
                Some(a.primary_artist_name.clone()),
            ) {
                Some(n) => {
                    a.primary_artist_name = n;
                    extra_map(cx, &mut a.extra, &pp);
                    out.push(a);
                }
                None => cx.warn(&pp, "artist dropped (no name)"),
            }
        }
        c.primary_artists = Some(out).filter(|v| !v.is_empty());
    }
    c.primary_artist_override_name = cx.title(
        &format!("{path}.primaryArtistOverrideName"),
        c.primary_artist_override_name.take(),
    );
    c.primary_artist_folder_safe_name = cx.segment(
        &format!("{path}.primaryArtistFolderSafeName"),
        c.primary_artist_folder_safe_name.take(),
    );
    c.primary_artist_url_safe_name = cx.segment(
        &format!("{path}.primaryArtistURLSafeName"),
        c.primary_artist_url_safe_name.take(),
    );
    if let Some(list) = c.contributors.take() {
        let mut out: Vec<Contributor> = Vec::new();
        for (i, mut k) in list.into_iter().enumerate() {
            let pp = format!("{path}.contributors[{i}]");
            if out.len() >= MAX_CONTRIBUTORS {
                cx.warn(&pp, "too many contributors; dropped");
                continue;
            }
            let Some(name) = cx.title(
                &format!("{pp}.contributorName"),
                Some(k.contributor_name.clone()),
            ) else {
                cx.warn(&pp, "contributor dropped (no name)");
                continue;
            };
            k.contributor_name = name;
            k.contributor_role =
                cx.title(&format!("{pp}.contributorRole"), k.contributor_role.take());
            k.contributor_url = cx.url(&format!("{pp}.contributorURL"), k.contributor_url.take());
            extra_map(cx, &mut k.extra, &pp);
            out.push(k);
        }
        c.contributors = Some(out).filter(|v| !v.is_empty());
    }
    extra_map(cx, &mut c.extra, path);
}

/// `localCoverImage` / `itemCoverImage`: an explicitly empty list ("show no cover") is kept.
fn user_cover(cx: &mut Ctx, mut c: CoverImage, path: &str) -> Option<CoverImage> {
    if c.images.is_empty() {
        extra_map(cx, &mut c.extra, path);
        return Some(c);
    }
    cover(cx, c, path)
}

fn cover(cx: &mut Ctx, mut c: CoverImage, path: &str) -> Option<CoverImage> {
    let mut out: Vec<ImageEntry> = Vec::new();
    for (i, mut e) in std::mem::take(&mut c.images).into_iter().enumerate() {
        let pp = format!("{path}.images[{i}]");
        if out.len() >= MAX_IMAGES {
            cx.warn(&pp, "too many images; dropped");
            continue;
        }
        // Linkhosts legitimately publish `imageLocalFilename` (the image's file name in the
        // release folder, standard 02 §3.5 / 03 coverImage); locally the name is assigned when
        // the image is downloaded, so the remote one is dropped silently (no warning).
        if cx.opts.strip_local_data {
            e.image_local_filename = None;
        }
        if let Some(f) = e.image_local_filename.take() {
            e.image_local_filename = cx.rel_path(&format!("{pp}.imageLocalFilename"), f, false);
        }
        e.image_web_url = cx.url(&format!("{pp}.imageWebURL"), e.image_web_url.take());
        e.alt_text = cx.title(&format!("{pp}.altText"), e.alt_text.take());
        if e.image_local_filename.is_none() && e.image_web_url.is_none() {
            cx.warn(&pp, "image dropped (no usable location)");
            continue;
        }
        extra_map(cx, &mut e.extra, &pp);
        out.push(e);
    }
    if out.is_empty() {
        return None;
    }
    c.images = out;
    extra_map(cx, &mut c.extra, path);
    Some(c)
}

fn user_notes(cx: &mut Ctx, n: &mut UserNotes, path: &str) {
    n.user_comment = cx.long_text(&format!("{path}.userComment"), n.user_comment.take());
    n.user_rating = n.user_rating.map(|r| r.min(5));
    if let Some(tags) = n.user_tags.take() {
        let mut out: Vec<String> = Vec::new();
        for t in tags {
            if out.len() >= MAX_TAGS {
                cx.warn(&format!("{path}.userTags"), "too many tags; extra dropped");
                break;
            }
            if let Some(t) = sanitise_tag(&t) {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
        n.user_tags = Some(out).filter(|v| !v.is_empty());
    }
    n.created_by_user = cx.title(&format!("{path}.createdByUser"), n.created_by_user.take());
    extra_map(cx, &mut n.extra, path);
}

fn unite_data(cx: &mut Ctx, mut u: UniteData, path: &str) -> Option<UniteData> {
    if let Some(name) = u.expected_filename.take() {
        let clean = sanitise_line(&name, MAX_SEGMENT_BYTES);
        if clean.is_empty() || clean.contains(['/', '\\']) {
            cx.warn(
                &format!("{path}.expectedFilename"),
                "not a plain file name; dropped",
            );
        } else {
            u.expected_filename = Some(clean);
        }
    }
    u.expected_length = u
        .expected_length
        .map(|t| NoubinTimestamp::from_millis(t.as_millis().min(MAX_EXPECTED_LENGTH_MS)));
    u.free_download_url = cx.url(
        &format!("{path}.freeDownloadURL"),
        u.free_download_url.take(),
    );
    extra_map(cx, &mut u.extra, path);
    if u.expected_filename.is_none() && u.expected_length.is_none() && u.free_download_url.is_none()
    {
        cx.warn(path, "uniteData has nothing usable left; dropped");
        return None;
    }
    Some(u)
}

fn media_item(cx: &mut Ctx, item: &mut MediaListItem, path: &str) {
    if let Some(u) = item.unite_data.take() {
        item.unite_data = unite_data(cx, u, &format!("{path}.uniteData"));
    }
    if cx.opts.strip_local_data {
        if item.local_file_paths.take().is_some() {
            cx.warn(
                &format!("{path}.localFilePaths"),
                "local property in web.noudata; removed",
            );
        }
        if item.item_local_playback_data.take().is_some() {
            cx.warn(
                &format!("{path}.itemLocalPlaybackData"),
                "local property in web.noudata; removed",
            );
        }
        if item.item_user_notes.take().is_some() {
            cx.warn(
                &format!("{path}.itemUserNotes"),
                "local property in web.noudata; removed",
            );
        }
    } else {
        if let Some(paths) = item.local_file_paths.take() {
            let kept: Vec<String> = paths
                .into_iter()
                .enumerate()
                .filter_map(|(i, p)| cx.rel_path(&format!("{path}.localFilePaths[{i}]"), p, false))
                .collect();
            item.local_file_paths = Some(kept).filter(|v| !v.is_empty());
        }
        if let Some(n) = item.item_user_notes.as_mut() {
            user_notes(cx, n, &format!("{path}.itemUserNotes"));
        }
        if let Some(l) = item.item_local_playback_data.as_mut() {
            extra_map(cx, &mut l.extra, &format!("{path}.itemLocalPlaybackData"));
        }
    }
    item.media_title = cx.title(&format!("{path}.mediaTitle"), item.media_title.take());
    if let Some(c) = item.item_credits.as_mut() {
        credits(cx, c, &format!("{path}.itemCredits"));
    }
    if let Some(c) = item.item_cover_image.take() {
        item.item_cover_image = user_cover(cx, c, &format!("{path}.itemCoverImage"));
    }
    item.lyrics_transcript = cx.long_text(
        &format!("{path}.lyricsTranscript"),
        item.lyrics_transcript.take(),
    );

    let lists = std::mem::take(&mut item.cue_lists);
    for (i, l) in lists.into_iter().enumerate() {
        let pp = format!("{path}.cueLists[{i}]");
        if item.cue_lists.len() >= MAX_CUE_LISTS {
            cx.warn(&pp, "too many cue lists; dropped");
            continue;
        }
        if let Some(l) = cue_list(cx, l, &pp) {
            item.cue_lists.push(l);
        }
    }
    extra_map(cx, &mut item.extra, path);
}

fn cue_list(cx: &mut Ctx, mut l: CueList, path: &str) -> Option<CueList> {
    l.cue_list_title = cx.title(&format!("{path}.cueListTitle"), l.cue_list_title.take());
    l.cue_list_language = l
        .cue_list_language
        .take()
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(35)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());
    let cues = std::mem::take(&mut l.cues);
    for (i, c) in cues.into_iter().enumerate() {
        if l.cues.len() >= MAX_CUES_PER_LIST {
            cx.warn(path, "too many cues; extra dropped");
            break;
        }
        l.cues.push(cue(cx, c, &format!("{path}.cues[{i}]")));
    }
    if l.cues.is_empty() {
        cx.warn(path, "cue list has no cues left; dropped");
        return None;
    }
    extra_map(cx, &mut l.extra, path);
    Some(l)
}

fn cue(cx: &mut Ctx, mut c: Cue, path: &str) -> Cue {
    let mut budget = MAX_CUE_TEXT_BYTES_TOTAL;
    for span in &mut c.cue_text_spans {
        let clean = sanitise_multiline(&span.text, budget);
        // Cue text is display text: no newlines inside a span.
        let clean = clean.replace('\n', " ");
        budget = budget.saturating_sub(clean.len());
        if clean != span.text {
            cx.warn(path, "cue text sanitised");
        }
        span.text = clean;
        extra_map(cx, &mut span.extra, path);
    }
    c.cue_url = cx.url(&format!("{path}.cueURL"), c.cue_url.take());
    extra_map(cx, &mut c.extra, path);
    c
}

// ---------------------------------------------------------------------------
// Preserved unknown properties
// ---------------------------------------------------------------------------

fn extra_map(cx: &mut Ctx, m: &mut Map<String, Value>, path: &str) {
    let old = std::mem::take(m);
    let mut dropped = 0usize;
    for (k, v) in old {
        if m.len() >= MAX_EXTRA_ENTRIES {
            dropped += 1;
            continue;
        }
        let key = sanitise_line(&k, 128);
        if key.is_empty() || key != k {
            dropped += 1;
            continue;
        }
        match extra_value(&key, v, 0) {
            Some(v) => {
                m.insert(key, v);
            }
            None => dropped += 1,
        }
    }
    if dropped > 0 {
        cx.warn(
            path,
            format!("{dropped} unknown propert(y/ies) dropped or trimmed"),
        );
    }
}

fn extra_value(key: &str, v: Value, depth: usize) -> Option<Value> {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(v),
        Value::String(s) => {
            if key.ends_with("URL") || key.ends_with("Url") || key.ends_with("url") {
                return sanitise_url(&s).map(Value::String);
            }
            if MULTILINE_KEYS.contains(&key) {
                return Some(Value::String(sanitise_multiline(&s, MAX_LONG_TEXT_BYTES)));
            }
            Some(Value::String(sanitise_line(&s, MAX_EXTRA_STRING_CHARS)))
        }
        Value::Array(a) => {
            if depth >= MAX_EXTRA_DEPTH {
                return None;
            }
            Some(Value::Array(
                a.into_iter()
                    .take(MAX_EXTRA_ENTRIES)
                    .filter_map(|x| extra_value(key, x, depth + 1))
                    .collect(),
            ))
        }
        Value::Object(o) => {
            if depth >= MAX_EXTRA_DEPTH {
                return None;
            }
            let mut out = Map::new();
            for (k, x) in o.into_iter().take(MAX_EXTRA_ENTRIES) {
                let ck = sanitise_line(&k, 128);
                if ck.is_empty() {
                    continue;
                }
                if let Some(x) = extra_value(&ck, x, depth + 1) {
                    out.insert(ck, x);
                }
            }
            Some(Value::Object(out))
        }
    }
}
