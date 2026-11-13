//! Refresh (NB_09): fetch the latest metadata for an item that has a source URL and merge it
//! in **without touching local user data**.
//!
//! Source URL: the item's `noubinKey` when that is a URL; otherwise the recorded
//! `plippaSourceURL` root property (written by unite, spec has no field for this so it is a
//! preserved extra); otherwise the user supplies one, which is then recorded.
//!
//! Never overwritten by a refresh: `noubinKey`/`noubinKeyNormalised`, `referToReleaseNoudata`,
//! `playableItemFolderSafeTitle` (the folder is already named), `localPlaybackData` (resume,
//! user notes, own cover...; the v0.4.1-retired `shuffle` / `repeatList` / `stopAfterEach` are
//! no longer modelled, so they are dropped when the document is written), and per item
//! `localFilePaths`, `itemLocalPlaybackData`,
//! `itemUserNotes`, cue lists the item already has, local image file names, and any property
//! the fetched document does not mention.
//!
//! Two consents, both server-side: the metadata fetch (`consent`, checked by `WebHub::fetch`)
//! and, separately, the list of image URLs (`download/prepare` + `consentId` of the unite
//! session created here).

use super::hub::{UniteError, UniteHub, SOURCE_URL_KEY};
use super::images::plan_cover_targets;
use crate::service::ServerService;
use crate::web::fetch::WebError;
use plippa_noubin::{
    fetch_url_for_key, is_url_noubin_key, safe_path_segment, sanitise_key, sanitise_line,
    sanitise_url, write_noudata_atomic, AtomicWriteError, CoverImage, ImageEntry, NoudataDocument,
    WriteGuard, MAX_SEGMENT_BYTES,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use super::fsops::{LibraryFs, Transaction};

/// Body of `POST /api/playables/:id/refresh[/prepare]`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RefreshRequest {
    /// Noubin URL to use when the item has none recorded (or to override).
    pub url: Option<String>,
    pub consent: Option<bool>,
}

/// What a merge changed / could not do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub changes: Vec<String>,
    pub warnings: Vec<String>,
}

fn norm_title(s: &Option<String>) -> String {
    s.as_deref()
        .unwrap_or("")
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Pair local item positions with fetched ones: by position when the counts agree, otherwise by
/// unique normalised title.
fn pair_items(local: &NoudataDocument, fetched: &NoudataDocument) -> Vec<(usize, usize)> {
    if local.media_list.len() == fetched.media_list.len() {
        return (0..local.media_list.len()).map(|i| (i, i)).collect();
    }
    let mut out = Vec::new();
    for (i, l) in local.media_list.iter().enumerate() {
        let t = norm_title(&l.media_title);
        if t.is_empty() {
            continue;
        }
        let hits: Vec<usize> = fetched
            .media_list
            .iter()
            .enumerate()
            .filter(|(_, f)| norm_title(&f.media_title) == t)
            .map(|(j, _)| j)
            .collect();
        let dup_local = local
            .media_list
            .iter()
            .filter(|x| norm_title(&x.media_title) == t)
            .count()
            > 1;
        if hits.len() == 1 && !dup_local {
            out.push((i, hits[0]));
        }
    }
    out
}

/// Fetched cover list, with local file names carried over for entries with the same web URL,
/// followed by local-only entries (purchased art, `-alt` files) that must not be lost.
fn merge_cover(local: Option<&CoverImage>, fetched: Option<CoverImage>) -> Option<CoverImage> {
    let Some(mut f) = fetched else {
        return local.cloned();
    };
    let mut used: HashSet<usize> = HashSet::new();
    if let Some(l) = local {
        for e in &mut f.images {
            if e.image_local_filename.is_some() {
                continue;
            }
            let hit = l.images.iter().enumerate().find(|(i, le)| {
                !used.contains(i)
                    && le.image_local_filename.is_some()
                    && le.image_web_url.is_some()
                    && le.image_web_url == e.image_web_url
            });
            if let Some((i, le)) = hit {
                used.insert(i);
                e.image_local_filename = le.image_local_filename.clone();
            }
        }
        for (i, le) in l.images.iter().enumerate() {
            let carried = f.images.iter().any(|e| {
                e.image_local_filename == le.image_local_filename
                    && le.image_local_filename.is_some()
            });
            if !used.contains(&i) && le.image_local_filename.is_some() && !carried {
                f.images.push(le.clone());
            }
        }
    }
    Some(f)
}

/// Merge the (already sanitised) fetched release into `local` (see the module docs for what is
/// protected). Returns what changed.
pub fn merge_refreshed(local: &mut NoudataDocument, fetched: &NoudataDocument) -> RefreshReport {
    let mut rep = RefreshReport::default();
    if fetched.playable_item_title.is_some()
        && fetched.playable_item_title != local.playable_item_title
    {
        rep.changes.push("title".into());
        local.playable_item_title = fetched.playable_item_title.clone();
    }
    if fetched.playable_item_url_safe_title.is_some() {
        local.playable_item_url_safe_title = fetched.playable_item_url_safe_title.clone();
    }
    if let Some(fr) = fetched.release_data.as_ref() {
        let old = local.release_data.take();
        let mut new = fr.clone();
        new.cover_image = merge_cover(
            old.as_ref().and_then(|o| o.cover_image.as_ref()),
            new.cover_image.take(),
        );
        if let Some(o) = &old {
            for (k, v) in &o.extra {
                new.extra.entry(k.clone()).or_insert_with(|| v.clone());
            }
            if new.credits.is_none() {
                new.credits = o.credits.clone();
            }
        }
        if let Some(o) = &old {
            let before = serde_json::to_value(o).unwrap_or(Value::Null);
            let after = serde_json::to_value(&new).unwrap_or(Value::Null);
            let strip = |mut v: Value| {
                if let Some(m) = v.as_object_mut() {
                    m.remove("coverImage");
                }
                v
            };
            if strip(before.clone()) != strip(after.clone()) {
                rep.changes
                    .push("release information (artists, platforms, links)".into());
            }
            if before.get("coverImage") != after.get("coverImage") {
                rep.changes.push("cover image list".into());
            }
        } else {
            rep.changes.push("release information".into());
        }
        local.release_data = Some(new);
    }

    let pairs = pair_items(local, fetched);
    if pairs.len() < fetched.media_list.len() {
        rep.warnings.push(format!(
            "{} track(s) in the online metadata have no counterpart here and were not added",
            fetched.media_list.len() - pairs.len()
        ));
    }
    if pairs.len() < local.media_list.len() {
        rep.warnings.push(format!(
            "{} local track(s) could not be matched to the online list and were left as they are",
            local.media_list.len() - pairs.len()
        ));
    }
    for (i, j) in pairs {
        let f = &fetched.media_list[j];
        let l = &mut local.media_list[i];
        let before = (
            l.media_title.clone(),
            serde_json::to_value(&l.item_credits).unwrap_or(Value::Null),
            serde_json::to_value(&l.unite_data).unwrap_or(Value::Null),
            l.lyrics_transcript.clone(),
        );
        if f.media_title.is_some() {
            l.media_title = f.media_title.clone();
        }
        if f.item_credits.is_some() {
            l.item_credits = f.item_credits.clone();
        }
        if f.lyrics_transcript.is_some() {
            l.lyrics_transcript = f.lyrics_transcript.clone();
        }
        if f.unite_data.is_some() {
            l.unite_data = f.unite_data.clone();
        }
        if f.item_cover_image.is_some() {
            l.item_cover_image =
                merge_cover(l.item_cover_image.as_ref(), f.item_cover_image.clone());
        }
        if l.cue_lists.is_empty() {
            l.cue_lists = f.cue_lists.clone();
        }
        for (k, v) in &f.extra {
            l.extra.insert(k.clone(), v.clone());
        }
        let after = (
            l.media_title.clone(),
            serde_json::to_value(&l.item_credits).unwrap_or(Value::Null),
            serde_json::to_value(&l.unite_data).unwrap_or(Value::Null),
            l.lyrics_transcript.clone(),
        );
        if before != after {
            rep.changes.push(format!("track {}", i + 1));
        }
    }
    rep
}

/// Which key/URL a refresh contacts: `(key, origin)`.
pub fn pick_source(
    doc: &NoudataDocument,
    explicit: Option<&str>,
) -> Result<(String, &'static str), UniteError> {
    if let Some(e) = explicit.map(str::trim).filter(|e| !e.is_empty()) {
        let k =
            sanitise_key(e).ok_or_else(|| UniteError::Invalid("the URL is not valid".into()))?;
        return Ok((k, "given"));
    }
    if let Some(k) = doc.noubin_key.as_deref().filter(|k| is_url_noubin_key(k)) {
        return Ok((k.to_string(), "noubinKey"));
    }
    if let Some(u) = doc.extra.get(SOURCE_URL_KEY).and_then(Value::as_str) {
        if let Some(k) = sanitise_key(u).filter(|k| is_url_noubin_key(k)) {
            return Ok((k, "recorded"));
        }
    }
    Err(UniteError::NeedsUrl)
}

fn server_err(e: crate::service::ServerError) -> UniteError {
    use crate::service::ServerError as S;
    match e {
        S::NotFound(m) => UniteError::NotFound(m),
        S::Unavailable(m) => UniteError::Storage(m),
        S::Conflict(m) => UniteError::NotReady(m),
        other => UniteError::Invalid(other.to_string()),
    }
}

/// Step 1: what a refresh would contact. Sends nothing.
pub fn refresh_prepare(
    svc: &Arc<Mutex<ServerService>>,
    id: u64,
    req: &RefreshRequest,
) -> Result<Value, UniteError> {
    let ctx = svc
        .lock()
        .map_err(|_| UniteError::Busy)?
        .refresh_context(id)
        .map_err(server_err)?;
    let (key, origin) = pick_source(&ctx.doc, req.url.as_deref())?;
    let u = fetch_url_for_key(&key).map_err(|e| match e {
        plippa_noubin::FetchUrlError::NotAUrl => WebError::NotAUrl,
        other => WebError::BadUrl(other.to_string()),
    })?;
    Ok(json!({
        "fetch_url": u.display,
        "host": u.host,
        "source": origin,
        "consent_message": format!(
            "This will contact {} over the internet and update this item's online metadata. \
             Your notes, resume points and file assignments are not changed. Continue?",
            u.host
        ),
    }))
}

/// Step 2: fetch (needs `consent: true`, checked before anything else and again by the web
/// hub), merge, write atomically, rescan. Returns what changed and, when the online metadata
/// has cover images, a unite session id for the separate image consent/download.
pub fn refresh(
    hub: &UniteHub,
    svc: &Arc<Mutex<ServerService>>,
    id: u64,
    req: RefreshRequest,
) -> Result<Value, UniteError> {
    crate::web::fetch::Consent::from_request(req.consent)?;
    let (ctx, web) = {
        let mut g = svc.lock().map_err(|_| UniteError::Busy)?;
        let ctx = g.refresh_context(id).map_err(server_err)?;
        (ctx, g.web())
    };
    let (key, origin) = pick_source(&ctx.doc, req.url.as_deref())?;
    let root = ctx.root.clone();
    drop(ctx);
    // Network, outside the service lock. The web hub re-checks consent, capability, URL rules.
    let fetched = web.fetch(&key, req.consent)?;
    let held = web.held(&fetched.token).ok_or(WebError::UnknownSession)?;
    web.discard(&fetched.token);

    let (report, source_recorded) = {
        let mut g = svc.lock().map_err(|_| UniteError::Busy)?;
        // Re-read: the file may have changed while we were on the network.
        let ctx = g.refresh_context(id).map_err(server_err)?;
        let mut doc = ctx.doc.clone();
        let report = merge_refreshed(&mut doc, &held.doc);
        let mut recorded = false;
        if origin != "noubinKey" {
            if let Some(u) = sanitise_url(&held.source_url) {
                doc.extra.insert(SOURCE_URL_KEY.into(), Value::String(u));
                recorded = true;
            }
        }
        write_noudata_atomic(&doc, &ctx.path, WriteGuard::Unchanged(ctx.rev)).map_err(
            |e| match e {
                AtomicWriteError::Conflict => {
                    UniteError::NotReady("the item changed while refreshing; try again".into())
                }
                other => UniteError::Io(other.to_string()),
            },
        )?;
        let _ = g.lazy_rescan();
        (report, recorded)
    };
    let images = if plan_cover_targets(&held.doc).0.is_empty() {
        Value::Null
    } else {
        let (sid, _) = hub.create_session(root, Some(held.clone()), None, Some(id))?;
        json!({
            "session": sid,
            "next": "POST /api/unite/<session>/download/prepare {\"media\":false,\"images\":true}, \
                     then /download with its consentId, then POST /api/playables/<id>/refresh/images",
        })
    };
    Ok(json!({
        "ok": true,
        "changed": report.changes,
        "warnings": report.warnings.iter().map(|w| sanitise_line(w, 300)).collect::<Vec<_>>(),
        "source_recorded": source_recorded,
        "images": images,
    }))
}

/// Step 3 (optional): install the images the refresh session downloaded. Only entries that
/// have no local file yet get one; existing files are never replaced.
pub fn refresh_install_images(
    hub: &UniteHub,
    svc: &Arc<Mutex<ServerService>>,
    id: u64,
    session: &str,
) -> Result<Value, UniteError> {
    let staged = hub.take_refresh_images(session, id)?;
    let mut g = svc.lock().map_err(|_| UniteError::Busy)?;
    let ctx = g.refresh_context(id).map_err(server_err)?;
    let fs = LibraryFs::new(&ctx.root)?;
    let dir = ctx.rel_dir.clone();
    let mut doc = ctx.doc.clone();
    let mut taken: HashSet<String> = fs
        .existing_names(&dir)
        .into_iter()
        .map(|n| n.to_lowercase())
        .collect();
    let thumbs_dir = dir.join("thumbnails");
    let mut thumb_taken: HashSet<String> = fs
        .existing_names(&thumbs_dir)
        .into_iter()
        .map(|n| n.to_lowercase())
        .collect();
    let dir_abs = fs.resolve(&dir)?;
    let rd = doc
        .release_data
        .as_mut()
        .ok_or_else(|| UniteError::Invalid("not a release".into()))?;
    let mut moves: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let (mut installed, mut skipped) = (0usize, 0usize);
    let mut cover = rd.cover_image.take().unwrap_or(CoverImage {
        images: Vec::new(),
        extra: Default::default(),
    });
    for c in &staged {
        let pos = cover
            .images
            .iter()
            .position(|e| e.image_web_url.as_deref() == Some(c.url.as_str()));
        let idx = match pos {
            Some(p) => p,
            None => {
                cover.images.push(ImageEntry {
                    image_web_url: Some(c.url.clone()),
                    ..Default::default()
                });
                cover.images.len() - 1
            }
        };
        let entry = &mut cover.images[idx];
        let have_local = entry
            .image_local_filename
            .as_deref()
            .is_some_and(|n| dir_abs.join(n).is_file());
        if have_local {
            skipped += 1;
            continue;
        }
        let want = safe_path_segment(&format!("{}.{}", c.stem, c.ext), "cover", MAX_SEGMENT_BYTES);
        let mut name = want.clone();
        let mut n = 2;
        while !taken.insert(name.to_lowercase()) {
            let (s, e) = want.rsplit_once('.').unwrap_or((&want, "jpg"));
            name = format!("{s} ({n}).{e}");
            n += 1;
            if n > 1000 {
                return Err(UniteError::Io("no free file name for the image".into()));
            }
        }
        entry.image_local_filename = Some(name.clone());
        moves.push((c.path.clone(), dir.join(&name)));
        let stem = name
            .rsplit_once('.')
            .map_or(name.as_str(), |x| x.0)
            .to_string();
        for (tier, ext, src) in &c.thumbs {
            let tn = format!("{stem}-{tier}.{ext}");
            if thumb_taken.insert(tn.to_lowercase()) {
                moves.push((src.clone(), thumbs_dir.join(tn)));
            }
        }
        installed += 1;
    }
    rd.cover_image = if cover.images.is_empty() {
        None
    } else {
        Some(cover)
    };
    if installed == 0 {
        hub.discard(session);
        return Ok(json!({"ok": true, "installed": 0, "skipped": skipped}));
    }
    let mut tx = Transaction::default();
    let stage = hub.stage_of(session)?;
    let res = (|| -> Result<(), UniteError> {
        for (src, dst) in &moves {
            fs.check_staged(&stage, src)?;
            if let Some(p) = dst.parent() {
                fs.ensure_dir(p, tx.dirs())?;
            }
            let dst_abs = fs.resolve(dst)?;
            tx.move_in(src, &dst_abs)?;
        }
        write_noudata_atomic(&doc, &ctx.path, WriteGuard::Unchanged(ctx.rev)).map_err(
            |e| match e {
                AtomicWriteError::Conflict => UniteError::NotReady(
                    "the item changed while installing images; try again".into(),
                ),
                other => UniteError::Io(other.to_string()),
            },
        )?;
        Ok(())
    })();
    match res {
        Ok(()) => tx.commit(),
        Err(e) => {
            tx.rollback();
            return Err(e);
        }
    }
    let _ = g.lazy_rescan();
    drop(g);
    hub.discard(session);
    Ok(json!({"ok": true, "installed": installed, "skipped": skipped}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_noubin::parse_noudata;

    fn doc(s: &str) -> NoudataDocument {
        parse_noudata(s.as_bytes()).unwrap()
    }

    const LOCAL: &str = r#"{
        "noubinKey": "https://a.example/noubin/x/", "noubinKeyNormalised": "a.example/noubin/x/",
        "playableItemTitle": "Old Title", "playableItemFolderSafeTitle": "Old Title",
        "playableItemProfile": "music", "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "Band"}]},
            "coverImage": {"images": [
                {"imageWebURL": "https://a.example/c.png", "imageLocalFilename": "cover.png"},
                {"imageLocalFilename": "cover-alt.jpg"}]}},
        "localPlaybackData": {"resumeAtItem": 2, "shuffle": true,
            "repeatList": true, "stopAfterEach": true,
            "userNotes": {"userComment": "my note", "userRating": 4, "userTags": ["fav"]}},
        "mediaList": [
            {"mediaTitle": "One", "localFilePaths": ["01.mp3"],
             "itemUserNotes": {"userComment": "track note"},
             "itemLocalPlaybackData": {"resumeFromTime": "1:30"},
             "cueLists": [{"cueListType": "chapters", "cues": [{"cueTimestamp": "0:00", "cueText": "mine"}]}]},
            {"mediaTitle": "Two", "localFilePaths": ["02.mp3"]}
        ]
    }"#;
    const FETCHED: &str = r#"{
        "playableItemTitle": "New Title",
        "playableItemProfile": "music", "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "Band"}]},
            "coverImage": {"images": [{"imageWebURL": "https://a.example/c.png"}, {"imageWebURL": "https://a.example/new.jpg"}]},
            "releasePlatforms": [{"platformName": "Shop", "platformType": "store", "platformURL": "https://shop.example/a"}]},
        "mediaList": [
            {"mediaTitle": "One (Remaster)", "uniteData": {"expectedLength": "3:00"},
             "cueLists": [{"cueListType": "chapters", "cues": [{"cueTimestamp": "0:10", "cueText": "theirs"}]}]},
            {"mediaTitle": "Two", "lyricsTranscript": "la la"}
        ]
    }"#;

    #[test]
    fn unite_refresh_merges_metadata_and_keeps_every_bit_of_local_data() {
        let mut local = doc(LOCAL);
        let before = local.clone();
        let rep = merge_refreshed(&mut local, &doc(FETCHED));
        assert_eq!(local.playable_item_title.as_deref(), Some("New Title"));
        assert_eq!(
            local.media_list[0].media_title.as_deref(),
            Some("One (Remaster)")
        );
        assert_eq!(
            local.media_list[1].lyrics_transcript.as_deref(),
            Some("la la")
        );
        assert!(local
            .release_data
            .as_ref()
            .unwrap()
            .release_platforms
            .is_some());
        assert!(rep.changes.iter().any(|c| c == "title"));
        // Protected data is byte-for-byte what it was.
        assert_eq!(local.noubin_key, before.noubin_key);
        assert_eq!(local.noubin_key_normalised, before.noubin_key_normalised);
        assert_eq!(
            local.playable_item_folder_safe_title,
            before.playable_item_folder_safe_title
        );
        assert_eq!(
            local.local_playback_data, before.local_playback_data,
            "notes, rating, tags, resume"
        );
        for (l, b) in local.media_list.iter().zip(&before.media_list) {
            assert_eq!(l.local_file_paths, b.local_file_paths);
            assert_eq!(l.item_user_notes, b.item_user_notes);
            assert_eq!(l.item_local_playback_data, b.item_local_playback_data);
        }
        // The user's own cue list is not replaced by the online one.
        assert_eq!(
            local.media_list[0].cue_lists,
            before.media_list[0].cue_lists
        );
        // Local image files survive: matched by URL, plus the local-only alt entry.
        let imgs = &local
            .release_data
            .as_ref()
            .unwrap()
            .cover_image
            .as_ref()
            .unwrap()
            .images;
        assert_eq!(imgs[0].image_local_filename.as_deref(), Some("cover.png"));
        assert_eq!(
            imgs[1].image_web_url.as_deref(),
            Some("https://a.example/new.jpg")
        );
        assert!(imgs
            .iter()
            .any(|i| i.image_local_filename.as_deref() == Some("cover-alt.jpg")));
    }

    #[test]
    fn unite_refresh_drops_retired_playback_keys() {
        let mut local = doc(LOCAL);
        let rep = merge_refreshed(&mut local, &doc(FETCHED));
        let text = local.to_json_pretty().unwrap();
        // Noubin v0.4.1: the legacy player settings are no longer modelled, so a refresh
        // write drops them instead of echoing them back.
        for retired in ["shuffle", "repeatList", "stopAfterEach"] {
            assert!(
                !text.contains(retired),
                "{retired} survived the refresh: {text}"
            );
        }
        assert!(rep.changes.iter().any(|c| c == "title"));
        let lp = local.local_playback_data.as_ref().unwrap();
        assert_eq!(lp.resume_at_item, Some(2), "resume is still preserved");
        let notes = lp.user_notes.as_ref().unwrap();
        assert_eq!(notes.user_comment.as_deref(), Some("my note"));
        assert_eq!(notes.user_rating, Some(4));
        assert_eq!(notes.user_tags.as_deref(), Some(&["fav".to_string()][..]));
    }

    #[test]
    fn unite_refresh_mismatched_track_counts_pair_by_title_and_warn() {
        let mut local = doc(LOCAL);
        let fetched = doc(
            r#"{"playableItemTitle":"T","playableItemProfile": "music", "releaseData":{},
                "mediaList":[{"mediaTitle":"two!"},{"mediaTitle":"Bonus"},{"mediaTitle":"Other"}]}"#,
        );
        let rep = merge_refreshed(&mut local, &fetched);
        assert_eq!(
            local.media_list.len(),
            2,
            "never adds tracks it has no file for"
        );
        assert_eq!(local.media_list[1].media_title.as_deref(), Some("two!"));
        assert_eq!(
            local.media_list[0].media_title.as_deref(),
            Some("One"),
            "unmatched item untouched"
        );
        assert!(rep.warnings.len() >= 2);
    }

    #[test]
    fn unite_refresh_source_url_precedence() {
        let mut d = doc(LOCAL);
        assert_eq!(
            pick_source(&d, None).unwrap(),
            ("https://a.example/noubin/x/".into(), "noubinKey")
        );
        assert_eq!(
            pick_source(&d, Some("https://b.example/y")).unwrap().1,
            "given"
        );
        d.noubin_key = Some("LOCAL-KEY-123".into());
        assert!(matches!(pick_source(&d, None), Err(UniteError::NeedsUrl)));
        d.extra.insert(
            SOURCE_URL_KEY.into(),
            json!("https://c.example/z/web.noudata"),
        );
        assert_eq!(pick_source(&d, None).unwrap().1, "recorded");
        d.extra
            .insert(SOURCE_URL_KEY.into(), json!("javascript:alert(1)"));
        assert!(matches!(pick_source(&d, None), Err(UniteError::NeedsUrl)));
        assert!(pick_source(&d, Some("bad\nkey")).is_err());
    }
}
