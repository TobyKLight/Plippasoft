//! Build a [`NowPlayingList`] from a library playable (NB_04).
//!
//! Album / playlist / mix / audiobook / podcast / single track all go through
//! [`build_collection`]: the index's `items_of` supplies one [`ListTrack`] per
//! `mediaList` entry (titles/artists already resolved per track, so a playlist over
//! several releases shows each track's own metadata). The playable's own `.noudata`
//! is read once for `resumeAtItem` and per-item resume data / resume-behaviour
//! overrides (`localPlaybackData` no longer carries play-mode flags).
//!
//! Cues are loaded on demand, only when a track becomes current
//! ([`load_cues`]) — never on the audio thread and never for same-track seeks.

use crate::engine::{resolve_media_list_paths, EngineError};
use plippa_audio::OverspecPolicy;
use plippa_core::{
    Capabilities, CueSpan, ListTrack, NowPlayingList, NowPlayingListKind, PlayableItemId,
    ResumePolicy,
};
use plippa_noubin::{
    encode_playable_id, parse_noudata, ItemRecord, LibraryKind, LibraryReader,
    OverrideResumeBehaviour, PlayableRecord,
};
use std::path::{Path, PathBuf};

/// `localPlaybackData` flags of the loaded playable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollectionPrefs {
    /// `resumeAtItem` converted to a 0-based `mediaList` position.
    pub resume_at_item: Option<u32>,
}

pub struct BuiltCollection {
    pub list: NowPlayingList,
    pub prefs: CollectionPrefs,
}

/// List kind shown to UIs for a library record.
///
/// Follows the library GROUPING (`record.group`): a loose mix / audiobook / podcast track is
/// announced as a Mix / Audiobook / Podcast like its tab, although it is classified as a track.
pub fn list_kind_for(record: &PlayableRecord, track_count: usize) -> NowPlayingListKind {
    match record.group {
        LibraryKind::Track => NowPlayingListKind::Single,
        _ if record.is_orphan => NowPlayingListKind::Single,
        LibraryKind::Album | LibraryKind::Playlist if track_count <= 1 => {
            NowPlayingListKind::Single
        }
        LibraryKind::Album => NowPlayingListKind::Album,
        LibraryKind::Playlist => NowPlayingListKind::Playlist,
        LibraryKind::Mix => NowPlayingListKind::Mix,
        LibraryKind::Audiobook => NowPlayingListKind::Audiobook,
        LibraryKind::Podcast => NowPlayingListKind::Podcast,
    }
}

/// Resolve every entry of `record` into a track list (unshuffled, current = 0).
pub fn build_collection(
    root: &Path,
    reader: &LibraryReader,
    record: &PlayableRecord,
    caps: &Capabilities,
    policy: OverspecPolicy,
) -> Result<BuiltCollection, EngineError> {
    let doc = read_doc(root, record);
    let items = reader.items_of(record.id).unwrap_or_default();
    let fallback_artist = record.primary_artist.clone().unwrap_or_default();

    let mut tracks: Vec<ListTrack> = Vec::new();
    if items.is_empty() {
        // Index without item rows (v1 index, or nothing indexed): parse the `.noudata` directly.
        let paths = resolve_media_list_paths(root, record, caps, policy)?;
        for (i, p) in paths.into_iter().enumerate() {
            tracks.push(ListTrack {
                title: record.title.clone().unwrap_or_default(),
                artist: fallback_artist.clone(),
                duration_ms: record.duration_ms.filter(|_| i == 0),
                playable_id: Some(PlayableItemId::new(encode_playable_id(root, record.id))),
                item_position: i as u32,
                media_path: Some(p.clone()),
                media_candidates: vec![p],
                ..Default::default()
            });
        }
    } else {
        // Releases already asked for, to find cue sources of playlist entries.
        let mut release_items: Vec<(i64, Vec<ItemRecord>)> = Vec::new();
        let mut mix_profiles: Vec<(i64, bool)> = Vec::new();
        for item in &items {
            // Only plain regular files below the root (no symlink, FIFO, device, escape).
            let candidates: Vec<PathBuf> = item
                .media_relpaths
                .iter()
                .filter_map(|rel| {
                    plippa_noubin::confine::open_library_media(root, rel)
                        .ok()
                        .map(|m| root.join(m.rel))
                })
                .collect();
            let cue_source = cue_source_for(reader, record, item, &mut release_items);
            let (resume_policy, resume_from_ms) = item_resume(doc.as_ref(), item.position);
            let is_cued_mix = cue_source.is_some()
                && item_profile_is_mix(reader, record, item, &mut mix_profiles);
            let source = item.resolved_from.unwrap_or(record.id);
            // A lone untitled item (single track / one-file mix) shows the playable's title
            // rather than its file name.
            let title = match (&item.media_title, &record.title) {
                (None, Some(t)) if items.len() == 1 && item.resolved_from.is_none() => t.clone(),
                _ => item.title.clone(),
            };
            tracks.push(ListTrack {
                title,
                artist: item
                    .artist
                    .clone()
                    .unwrap_or_else(|| fallback_artist.clone()),
                duration_ms: item.duration_ms,
                playable_id: Some(PlayableItemId::new(encode_playable_id(root, source))),
                item_position: item.position,
                media_path: (candidates.len() == 1).then(|| candidates[0].clone()),
                media_candidates: candidates,
                cue_source,
                is_cued_mix,
                resume_policy,
                resume_from_ms,
            });
        }
    }

    let kind = list_kind_for(record, tracks.len());
    let list = NowPlayingList::new(kind, tracks);
    let mut prefs = CollectionPrefs::default();
    if let Some(l) = doc.as_ref().and_then(|d| d.local_playback_data.as_ref()) {
        // resumeAtItem is 1-indexed in the file (min 1).
        prefs.resume_at_item = l
            .resume_at_item
            .filter(|n| *n >= 1)
            .map(|n| (n - 1).min(u64::from(u32::MAX)) as u32);
    }
    Ok(BuiltCollection { list, prefs })
}

fn read_doc(root: &Path, record: &PlayableRecord) -> Option<plippa_noubin::NoudataDocument> {
    let rel = plippa_noubin::confine::clean_relpath(&record.noudata_relpath)
        .filter(|p| p.ends_with(".noudata"))?;
    let bytes = plippa_noubin::read_noudata_capped(&root.join(rel)).ok()?;
    parse_noudata(&bytes).ok()
}

fn item_resume(
    doc: Option<&plippa_noubin::NoudataDocument>,
    position: u32,
) -> (ResumePolicy, Option<u64>) {
    let Some(data) = doc
        .and_then(|d| d.media_list.get(position as usize))
        .and_then(|i| i.item_local_playback_data.as_ref())
    else {
        return (ResumePolicy::Default, None);
    };
    let policy = match data.effective_override_resume() {
        OverrideResumeBehaviour::Default => ResumePolicy::Default,
        OverrideResumeBehaviour::NeverResume => ResumePolicy::Never,
        OverrideResumeBehaviour::AlwaysResume => ResumePolicy::Always,
    };
    (policy, data.resume_from_time.map(|t| t.as_millis()))
}

/// The item's own release has profile `mix` (a playlist entry takes the profile of the
/// release that supplied it).
fn item_profile_is_mix(
    reader: &LibraryReader,
    record: &PlayableRecord,
    item: &ItemRecord,
    profiles: &mut Vec<(i64, bool)>,
) -> bool {
    let id = item.resolved_from.unwrap_or(record.id);
    if id == record.id {
        return record.release_profile.as_deref() == Some("mix");
    }
    if let Some((_, is_mix)) = profiles.iter().find(|(i, _)| *i == id) {
        return *is_mix;
    }
    let is_mix = reader
        .get_playable(id)
        .ok()
        .flatten()
        .is_some_and(|r| r.release_profile.as_deref() == Some("mix"));
    profiles.push((id, is_mix));
    is_mix
}

/// Where the cues of `item` live: its own `.noudata` when it has navigable cues, else
/// (playlist entries) the release item that claims the same media file.
fn cue_source_for(
    reader: &LibraryReader,
    record: &PlayableRecord,
    item: &ItemRecord,
    release_items: &mut Vec<(i64, Vec<ItemRecord>)>,
) -> Option<(i64, u32)> {
    if item.has_navigable_cues {
        return Some((record.id, item.position));
    }
    let release = item.resolved_from?;
    if !release_items.iter().any(|(id, _)| *id == release) {
        release_items.push((release, reader.items_of(release).unwrap_or_default()));
    }
    let (_, items) = release_items.iter().find(|(id, _)| *id == release)?;
    items
        .iter()
        .find(|r| {
            r.has_navigable_cues
                && (r.media_relpath == item.media_relpath
                    || r.media_relpaths
                        .iter()
                        .any(|p| item.media_relpaths.contains(p)))
        })
        .map(|r| (release, r.position))
}

/// Navigable cues of one track (all enabled/disabled lists; the list filters by enable state).
/// One `.noudata` read; called when the track becomes current.
pub fn load_cues(reader: &LibraryReader, source: (i64, u32)) -> Vec<CueSpan> {
    reader
        .cues_for(source.0, source.1)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.navigable)
        .map(|c| CueSpan {
            list_index: c.cue_list_index,
            start_ms: c.start_ms,
            end_ms: c.end_ms,
            title: c.text,
        })
        .collect()
}

#[cfg(test)]
mod capped_read_tests {
    use super::*;
    use plippa_noubin::{LibraryKind, PlayableRecord};

    fn record(rel: &str) -> PlayableRecord {
        PlayableRecord {
            id: 1,
            noudata_relpath: rel.into(),
            noubin_key_normalised: None,
            title: None,
            media_relpaths: vec![],
            kind: LibraryKind::Album,
            group: LibraryKind::Album,
            noudata_type: Some("release".into()),
            release_profile: None,
            is_dynamic: false,
            is_orphan: false,
            primary_artist: None,
            duration_ms: None,
            cover_relpath: None,
            cover_count: 0,
            refers_to_relpath: None,
            has_navigable_cues: false,
            media_missing: false,
        }
    }

    /// NB_VERIFY NB_SEC_1 (SEC-03 follow-up): the engine never loads an over-size `.noudata`
    /// that grew after the scan (hostile stick on a 1 GB device).
    #[test]
    fn engine_does_not_read_oversized_noudata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ok.noudata"),
            br#"{"playableItemTitle":"Ok","mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
        )
        .unwrap();
        // Valid JSON, just huge: without the cap it would parse fine.
        let pad = "x".repeat(plippa_noubin::MAX_LOCAL_NOUDATA_BYTES as usize);
        std::fs::write(
            dir.path().join("big.noudata"),
            format!(r#"{{"playableItemTitle":"{pad}","mediaList":[{{"localFilePaths":["a.mp3"]}}]}}"#),
        )
        .unwrap();
        assert!(read_doc(dir.path(), &record("ok.noudata")).is_some());
        assert!(read_doc(dir.path(), &record("big.noudata")).is_none());
    }
}
