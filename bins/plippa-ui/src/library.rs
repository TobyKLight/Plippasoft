//! Library list sourcing for the local UI.
//!
//! # Choice (MVP_11)
//!
//! UI uses a **read-only WAL** [`LibraryReader`] against the same
//! `.plippa/library.sqlite` as the engine. Server owns all writes; UI never
//! blocks on locks (WAL readers do not block writers, and we prefer slightly
//! stale rows over waiting).
//!
//! Refresh triggers:
//! - startup (best-effort open)
//! - [`IpcMessage::LibraryIndexUpdated`] from the hub
//!
//! Roots: `$PLIPPA_LIBRARY_ROOT` override when set; otherwise
//! [`resolve_library_roots`] USB / mount discovery (same as engine/server).
//! Playables from **all** open roots are merged and sorted A–Z by title.
//!
//! # NB_12 category browse
//!
//! [`LibrarySource::kind_counts`] (one grouped query per root) feeds the root category
//! screen, [`LibrarySource::list_kind`] the category lists (record fields only: no
//! `.noudata` read, no directory probing per row) and [`LibrarySource::detail_info`] the
//! track / chapter lists of one collection. The UI app never calls these itself: it queues
//! `BrowseRequest`s that the loop runner services with [`LibrarySource`] (see
//! `UiApp::service_browse`), so the render path stays free of I/O. Every string that came
//! from a file goes through [`screen_text`] (the shared `plippa_noubin::sanitise` stripper,
//! SEC-12) before it reaches the model.
//!
//! # UI_03 enrichment
//!
//! List is sorted **A–Z by title**. Optional artist / duration / local art path
//! are best-effort from the `.noudata` file; the art path is the first entry of the effective
//! cover list (`plippa_noubin::covers`, including the shared sibling lookup)
//! (no network art fetch, no audio decode for duration).

use crate::screens::{
    screen_text, ChapterRow, DetailCovers, DetailInfo, LibraryItem, TrackRow, MAX_BROWSE_ROWS,
};
use plippa_core::{audio_format_label, Capabilities};
use plippa_noubin::covers::CoverRef;
use plippa_noubin::{
    decode_playable_id, encode_playable_id, parse_noudata, resolve_library_roots, view_is_item_level, ItemRecord, LibraryError, LibraryKind, LibraryReader,
    LibraryRoot, MountIdentity, PlayableRecord, TrackRow as IndexTrackRow,
};
use std::cell::Cell;
use std::env;
use std::path::{Path, PathBuf};

/// Env var shared with engine / launcher.
pub const ENV_LIBRARY_ROOT: &str = "PLIPPA_LIBRARY_ROOT";

/// NB_18: what the index says about one `mediaList` position of a playable (see
/// [`LibrarySource::track_of`]).
#[derive(Debug, Clone, PartialEq)]
pub enum TrackLookup {
    /// The owning root, the track's media file (library-relative) and its title.
    Found(PathBuf, String, String),
    /// The playable exists but is not a release, so it has no tracks to associate one by one.
    NotARelease,
    /// The playable or that position is not in the library (or its index cannot be read).
    Missing,
}

/// Whether this player can decode the item: any of its `localFilePaths` alternates, judged by
/// the board's capabilities (the same call the engine's play gate makes). Computed per read and
/// never stored: the index is portable, the capabilities are this device's.
fn item_supported(it: &ItemRecord, caps: &Capabilities) -> bool {
    let paths: &[String] = if it.media_relpaths.is_empty() {
        std::slice::from_ref(&it.media_relpath)
    } else {
        &it.media_relpaths
    };
    paths.iter().any(|p| {
        Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| caps.supports_extension(&e.to_ascii_lowercase()))
    })
}

/// The chosen file's format as a person reads it (`M4A/AAC`); empty when it has no extension.
fn item_format(it: &ItemRecord) -> String {
    Path::new(&it.media_relpath)
        .extension()
        .and_then(|e| e.to_str())
        .map(audio_format_label)
        .unwrap_or_default()
}

/// Optional open of WAL indexes across discovered (or overridden) roots.
///
/// Failures leave an empty list rather than crashing the UI; index may appear
/// later via [`Self::reopen`] after [`IpcMessage::LibraryIndexUpdated`].
pub struct LibrarySource {
    /// When set, discovery is skipped and only this path is used (if a dir).
    override_root: Option<PathBuf>,
    roots: Vec<PathBuf>,
    /// Filesystem identity per root, captured when its reader was opened. A different
    /// identity means the stick was unplugged and remounted at the same path; the reader
    /// then refers to the old mount (reads fail / return stale rows) and must be reopened.
    mounts: Vec<Option<MountIdentity>>,
    readers: Vec<(PathBuf, LibraryReader)>,
    /// Set when an index read failed (stale connection after an unplug). Polled by the loop
    /// runner, which reopens the readers on its next ~1 s tick.
    broken: Cell<bool>,
}

impl LibrarySource {
    pub fn from_env() -> Self {
        let override_root = env::var_os(ENV_LIBRARY_ROOT).map(PathBuf::from);
        Self::open(override_root.as_deref())
    }

    pub fn open(override_root: Option<&Path>) -> Self {
        let mut src = Self {
            override_root: override_root.map(|p| p.to_path_buf()),
            roots: Vec::new(),
            mounts: Vec::new(),
            readers: Vec::new(),
            broken: Cell::new(false),
        };
        src.reopen();
        src
    }

    /// First discovered / configured root (compat for single-root callers).
    pub fn root(&self) -> Option<&Path> {
        self.roots.first().map(|p| p.as_path())
    }

    /// Every playable (across the open roots) whose `noubinKeyNormalised` is `norm`, with the
    /// root that holds it. Index only: callers trust the `.noudata` file before acting.
    pub fn holders_of_key(&self, norm: &str) -> Vec<(PathBuf, plippa_noubin::PlayableRecord)> {
        let mut out = Vec::new();
        for (root, reader) in &self.readers {
            if let Ok(records) = reader.find_by_key(norm) {
                out.extend(records.into_iter().map(|r| (root.clone(), r)));
            }
        }
        out
    }

    /// All library roots from the last successful discovery pass.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// True while a known root has no readable index (stick present, index not created yet
    /// or unreadable at the last attempt). The loop retries these instead of waiting for a
    /// `LibraryIndexUpdated` the UI may have missed (it can start before the server).
    pub fn has_unreadable_root(&self) -> bool {
        self.roots
            .iter()
            .any(|r| !self.readers.iter().any(|(p, _)| p == r))
    }

    /// Re-discover roots, then rebuild the readers only when something actually changed.
    ///
    /// Called on a ~1 s loop tick. This is what recovers from a USB stick that is unplugged
    /// and plugged back in at the same mount path: the path compares equal but the mount
    /// identity does not, so the stale SQLite readers are closed and reopened on the new
    /// filesystem. Returns true when the readers were rebuilt (callers refresh the screen).
    pub fn refresh_if_changed(&mut self) -> bool {
        let roots: Vec<PathBuf> = resolve_library_roots(self.override_root.as_deref())
            .into_iter()
            .map(|d| d.path)
            .collect();
        let mounts: Vec<Option<MountIdentity>> = roots
            .iter()
            .map(|root| MountIdentity::of(root))
            .collect();
        let unreadable = roots
            .iter()
            .any(|r| !self.readers.iter().any(|(p, _)| p == r));
        // A yanked stick can still leave its mount-point directory behind, so a failed read
        // (`broken`) is the signal there: reading it reports an I/O error. Reopening then
        // clears the reader, which the `unreadable` check above catches next tick.
        if self.broken.get()
            || roots != self.roots
            || mounts != self.mounts
            || unreadable
        {
            self.open_roots(&roots);
            return true;
        }
        false
    }

    /// Re-discover roots and re-open readers (after index rebuild / USB plug).
    pub fn reopen(&mut self) {
        let roots: Vec<PathBuf> = resolve_library_roots(self.override_root.as_deref())
            .into_iter()
            .map(|d| d.path)
            .collect();
        self.open_roots(&roots);
    }

    fn open_roots(&mut self, roots: &[PathBuf]) {
        self.readers.clear();
        self.broken.set(false);
        self.roots = roots.to_vec();
        self.mounts = roots.iter().map(|root| MountIdentity::of(root)).collect();
        for root in roots {
            match LibraryReader::open_with(&LibraryRoot::new(root), plippa_noubin::OpenWait::Interactive) {
                Ok(r) => self.readers.push((root.clone(), r)),
                Err(_) => {
                    // Soft-fail: stick present but index not ready yet.
                }
            }
        }
    }

    /// Turn an index query result into `Option`, remembering a failed read as a stale
    /// connection so the loop reopens the readers instead of painting an empty library.
    fn index_read<T>(&self, result: Result<T, LibraryError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(e) => {
                if e.is_stale_connection() {
                    self.broken.set(true);
                }
                None
            }
        }
    }

    /// The covers of the playing item (D4: its own, its release's, the collection's) for the
    /// namespaced playable id `playable_id` (`SessionState::current_item`) and 0-based
    /// `item_position`, with the index of the root that owns it in [`Self::roots`]. `None`
    /// when no open root has that playable.
    pub fn item_covers(
        &self,
        playable_id: u64,
        item_position: u32,
    ) -> Option<(usize, Vec<CoverRef>)> {
        let (_, local) = decode_playable_id(playable_id);
        let (root, reader) = self
            .readers
            .iter()
            .find(|(root, _)| encode_playable_id(root, local) == playable_id)?;
        let idx = self.roots.iter().position(|r| r == root)?;
        let covers = self
            .index_read(reader.item_covers(local, item_position))
            .unwrap_or_default();
        Some((idx, covers))
    }

    /// Non-blocking list for Library screens, alphabetical by title.
    ///
    /// Merges playables from every open root (engine multi-root intent).
    pub fn list_items(&self) -> Vec<LibraryItem> {
        let mut items = Vec::new();
        for (root, reader) in &self.readers {
            let Some(rows) = self.index_read(reader.list_playables()) else {
                continue;
            };
            for r in rows {
                let art = reader
                    .covers_of(r.id)
                    .ok()
                    .and_then(|c| c.into_iter().next())
                    .map(|c| root.join(c.relpath));
                items.push(record_to_item(r, root, art));
            }
        }
        sort_library_items(&mut items);
        items
    }

    /// True while at least one library root is still a directory (stick plugged in).
    pub fn storage_present(&self) -> bool {
        self.roots.iter().any(|r| r.is_dir())
    }

    /// Entries per category (library VIEW, `libraryUIHeirarchy.md`): tracks for Tracks and
    /// Mixes, playables for the others, summed over every open root. Categories without
    /// content are absent; an unreadable / unplugged root contributes nothing.
    pub fn kind_counts(&self) -> Vec<(LibraryKind, u64)> {
        let mut totals: Vec<(LibraryKind, u64)> = Vec::new();
        for (_, reader) in &self.readers {
            let Some(counts) = self.index_read(reader.view_counts()) else {
                continue;
            };
            for (kind, n) in counts {
                match totals.iter_mut().find(|(k, _)| *k == kind) {
                    Some((_, t)) => *t += n,
                    None => totals.push((kind, n)),
                }
            }
        }
        totals
    }

    /// NB_18: `position` of the playable whose `.noudata` is `release_relpath` — the track's
    /// media file (library-relative) and its display title.
    ///
    /// `root` is the root that owns the item on screen (from [`LibraryItem`]); `None` searches
    /// every open root (the ids of two roots are not comparable, so the caller should pass it).
    /// [`TrackLookup::NotARelease`] is a playlist / loose track: a single item of those cannot
    /// be associated, exactly as the portal refuses it.
    pub fn track_of(
        &self,
        root: Option<&Path>,
        release_relpath: &str,
        position: u32,
    ) -> TrackLookup {
        for (reader_root, reader) in &self.readers {
            if root.is_some_and(|wanted| wanted != reader_root) {
                continue;
            }
            let Some(records) = self.index_read(reader.list_playables()) else {
                continue;
            };
            let Some(rec) = records
                .iter()
                .find(|r| r.noudata_relpath == release_relpath)
            else {
                continue;
            };
            if !plippa_noubin::is_release_kind(rec.kind) {
                return TrackLookup::NotARelease;
            }
            let Some(items) = self.index_read(reader.items_of(rec.id)) else {
                continue;
            };
            let Some(it) = items.iter().find(|i| i.position == position) else {
                continue;
            };
            let title = it
                .media_title
                .clone()
                .unwrap_or_else(|| it.title.clone());
            return TrackLookup::Found(reader_root.clone(), it.media_relpath.clone(), title);
        }
        TrackLookup::Missing
    }

    /// NB_18: the per-track `.noudata` (library-relative, in `root`) that already refers to
    /// `release_relpath` and lists `media_relpath` — the same predicate the portal uses, so a
    /// track associated from the portal is reused by the device and vice versa.
    pub fn track_file_for(
        &self,
        root: &Path,
        release_relpath: &str,
        media_relpath: &str,
    ) -> Option<String> {
        let (_, reader) = self.readers.iter().find(|(r, _)| r == root)?;
        let records = self.index_read(reader.list_by_kind(LibraryKind::Track))?;
        for rec in records {
            if rec.is_orphan
                || rec.refers_to_relpath.as_deref() != Some(release_relpath)
            {
                continue;
            }
            let items = self.index_read(reader.items_of(rec.id))?;
            if items.iter().any(|i| i.media_relpath == media_relpath) {
                return Some(rec.noudata_relpath);
            }
        }
        None
    }

    /// The playable whose `.noudata` is `relpath`, searched across every category (BCS_07's
    /// "Play" after a download). `None` when the scanner has not picked it up yet.
    pub fn find_by_relpath(&self, relpath: &str) -> Option<LibraryItem> {
        const KINDS: [LibraryKind; 6] = [
            LibraryKind::Track,
            LibraryKind::Album,
            LibraryKind::Playlist,
            LibraryKind::Mix,
            LibraryKind::Audiobook,
            LibraryKind::Podcast,
        ];
        KINDS
            .iter()
            .flat_map(|kind| self.list_kind(*kind))
            .find(|item| item.noudata_relpath == relpath)
    }

    /// Items of one category (see [`Self::kind_counts`]) from every root, A-Z by title.
    /// Record fields only. Tracks and Mixes list individual tracks (`item_position` set), the
    /// other categories list whole playables.
    pub fn list_kind(&self, kind: LibraryKind) -> Vec<LibraryItem> {
        let mut items = Vec::new();
        for (root, reader) in &self.readers {
            if view_is_item_level(kind) {
                let Some(rows) = self.index_read(reader.list_view_tracks(kind)) else {
                    continue;
                };
                items.extend(rows.into_iter().map(|t| track_row_to_item(t, root)));
                continue;
            }
            let Some(rows) = self.index_read(reader.list_by_view(kind)) else {
                continue;
            };
            items.extend(rows.into_iter().map(|r| record_to_item_light(r, root)));
        }
        sort_library_items(&mut items);
        items
    }

    /// The effective covers of the item on the detail screen (ART_09). A collection row gets
    /// the collection's list (`covers_of`); a single track of the Tracks / Mixes lists gets the
    /// item's own, else its release's, else the collection's (`item_covers`). The same
    /// functions the portal uses, so the sets agree. Empty on any error.
    pub fn detail_covers(&self, item: &LibraryItem) -> DetailCovers {
        let mut out = DetailCovers {
            playable_id: item.id,
            track_pos: item.item_position,
            ..DetailCovers::default()
        };
        let Some(root) = item.library_root.as_ref() else {
            return out;
        };
        let Some((_, reader)) = self.readers.iter().find(|(r, _)| r == root) else {
            return out;
        };
        let Some(idx) = self.roots.iter().position(|r| r == root) else {
            return out;
        };
        let covers = match item.item_position {
            Some(pos) => self.index_read(reader.item_covers(item.id, pos)),
            None => self.index_read(reader.covers_of(item.id)),
        };
        out.root_idx = idx;
        out.covers = covers.unwrap_or_default();
        out
    }

    /// Track and chapter (navigable cue) lists of one collection. Empty on any error
    /// (stick removed, item vanished).
    pub fn detail_info(
        &self,
        item: &LibraryItem,
        kind: Option<LibraryKind>,
        caps: &Capabilities,
    ) -> DetailInfo {
        let mut info = DetailInfo {
            playable_id: item.id,
            kind,
            ..DetailInfo::default()
        };
        let Some(root) = item.library_root.as_ref() else {
            return info;
        };
        let Some((_, reader)) = self.readers.iter().find(|(r, _)| r == root) else {
            return info;
        };
        let Some(items) = self.index_read(reader.items_of(item.id)) else {
            return info;
        };
        let has_cues = reader
            .get_playable(item.id)
            .ok()
            .flatten()
            .is_some_and(|r| r.has_navigable_cues);
        info.media_missing = items.iter().any(|i| !i.available);
        for it in items.iter().take(MAX_BROWSE_ROWS) {
            info.tracks.push(TrackRow {
                position: it.position,
                title: screen_text(&it.title),
                artist: it.artist.as_deref().map(screen_text).filter(|s| !s.is_empty()),
                duration_ms: it.duration_ms,
                available: it.available,
                supported: item_supported(it, caps),
                format: item_format(it),
            });
        }
        if has_cues {
            // One `.noudata` read per item; bounded so a hostile 100k-item file cannot stall.
            for it in items.iter().take(200) {
                // A single-track row only offers the chapters of its own track.
                if item.item_position.is_some_and(|p| p != it.position) {
                    continue;
                }
                let Some(cues) = self.index_read(reader.cues_for(item.id, it.position)) else {
                    continue;
                };
                for c in cues.into_iter().filter(|c| c.navigable) {
                    if info.chapters.len() >= MAX_BROWSE_ROWS {
                        break;
                    }
                    let title = screen_text(&c.text);
                    info.chapters.push(ChapterRow {
                        item_position: it.position,
                        start_ms: c.start_ms,
                        title: if title.is_empty() {
                            format!("Chapter {}", info.chapters.len() + 1)
                        } else {
                            title
                        },
                    });
                }
            }
            info.chapters.sort_by_key(|c| (c.item_position, c.start_ms));
            info.chapters
                .dedup_by_key(|c| (c.item_position, c.start_ms));
        }
        info
    }
}

/// One track of the Tracks / Mixes lists.
fn track_row_to_item(t: IndexTrackRow, root: &Path) -> LibraryItem {
    let title = Some(screen_text(&t.title))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| screen_text(&t.collection_title));
    LibraryItem {
        id: t.playable_id,
        title,
        associated: t.associated,
        noudata_relpath: t.noudata_relpath,
        library_root: Some(root.to_path_buf()),
        artist: t
            .artist
            .as_deref()
            .map(screen_text)
            .filter(|s| !s.is_empty()),
        duration_ms: t.duration_ms,
        art_path: None,
        item_position: Some(t.position),
    }
}

/// Cheap row for category lists: everything comes from the index record.
fn record_to_item_light(r: PlayableRecord, root: &Path) -> LibraryItem {
    let title = r
        .title
        .as_deref()
        .map(screen_text)
        .filter(|t| !t.is_empty())
        .or_else(|| {
            // Orphan media file: the file name is the only title we have.
            r.media_relpaths.first().map(|m| {
                let name = m.rsplit('/').next().unwrap_or(m);
                screen_text(name.rsplit_once('.').map_or(name, |(stem, _)| stem))
            })
        })
        .unwrap_or_default();
    LibraryItem {
        id: r.id,
        title,
        associated: r.noubin_key_normalised.is_some(),
        noudata_relpath: r.noudata_relpath,
        library_root: Some(root.to_path_buf()),
        artist: r
            .primary_artist
            .as_deref()
            .map(screen_text)
            .filter(|s| !s.is_empty()),
        duration_ms: r.duration_ms,
        art_path: r.cover_relpath.map(|c| root.join(c)),
        item_position: None,
    }
}

/// Sort by title (case-insensitive); stable secondary by id.
pub fn sort_library_items(items: &mut [LibraryItem]) {
    items.sort_by(|a, b| {
        let ta = a.title.to_ascii_lowercase();
        let tb = b.title.to_ascii_lowercase();
        ta.cmp(&tb)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.item_position.cmp(&b.item_position))
    });
}

/// `art` is the first entry of the effective cover list ([`LibraryReader::covers_of`]: declared
/// covers that exist, else the shared sibling lookup).
fn record_to_item(r: PlayableRecord, root: &Path, art: Option<PathBuf>) -> LibraryItem {
    let title = r
        .title
        .clone()
        .unwrap_or_else(|| r.noudata_relpath.clone());
    let mut item = LibraryItem {
        id: r.id,
        title,
        associated: r.noubin_key_normalised.is_some(),
        noudata_relpath: r.noudata_relpath.clone(),
        library_root: Some(root.to_path_buf()),
        artist: None,
        duration_ms: None,
        art_path: art,
        item_position: None,
    };
    enrich_from_noudata(&mut item, root, &r.noudata_relpath);
    // SEC-12: file-derived text is stripped before it can reach the screen.
    item.title = screen_text(&item.title);
    item.artist = item.artist.as_deref().map(screen_text).filter(|s| !s.is_empty());
    item
}

fn enrich_from_noudata(item: &mut LibraryItem, root: &Path, relpath: &str) {
    let path = root.join(relpath);
    let Ok(bytes) = plippa_noubin::read_noudata_capped(&path) else {
        return;
    };
    let Ok(doc) = parse_noudata(&bytes) else {
        return;
    };
    if item.artist.is_none() {
        item.artist = extract_artist(&doc);
    }
    if item.duration_ms.is_none() {
        item.duration_ms = extract_duration_ms(&doc);
    }
}

/// Typed `releaseData` back to loose JSON for the legacy key lookups below.
fn release_data_value(doc: &plippa_noubin::NoudataDocument) -> Option<serde_json::Value> {
    doc.release_data
        .as_ref()
        .and_then(|r| serde_json::to_value(r).ok())
}

fn extract_artist(doc: &plippa_noubin::NoudataDocument) -> Option<String> {
    string_field(&doc.extra, &["playableItemArtist", "artist", "mainArtist"])
        .or_else(|| doc.primary_artist_display())
        .or_else(|| {
            release_data_value(doc)
                .and_then(|v| string_in_value(&v, &["artist", "mainArtist", "primaryArtist"]))
        })
        .filter(|s| !s.trim().is_empty())
}

fn extract_duration_ms(doc: &plippa_noubin::NoudataDocument) -> Option<u64> {
    u64_field(&doc.extra, &["durationMs", "duration_ms", "lengthMs"])
        .or_else(|| {
            doc.media_list.first().and_then(|m| {
                u64_field(&m.extra, &["durationMs", "duration_ms", "lengthMs"])
            })
        })
}

fn string_field(
    map: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<String> {
    for k in keys {
        if let Some(serde_json::Value::String(s)) = map.get(*k) {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

fn u64_field(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u64> {
    for k in keys {
        let Some(v) = map.get(*k) else {
            continue;
        };
        match v {
            serde_json::Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    return Some(u);
                }
                if let Some(i) = n.as_i64() {
                    if i >= 0 {
                        return Some(i as u64);
                    }
                }
            }
            serde_json::Value::String(s) => {
                if let Ok(u) = s.parse::<u64>() {
                    return Some(u);
                }
            }
            _ => {}
        }
    }
    None
}

fn string_in_value(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let obj = v.as_object()?;
    string_field(obj, keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_noubin::{normalise_noubin_key, LibraryWriter};
    use std::fs;

    #[test]
    fn wal_reader_lists_playables_alphabetically() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // (no unclaimed tone.wav here: the index lists unreferenced media as orphan tracks)
        fs::write(root.join("zebra.wav"), b"RIFF").unwrap();
        fs::write(root.join("alpha.wav"), b"RIFF").unwrap();
        write_noudata(root, "_z.noudata", "Zebra", "local:z", None, None);
        write_noudata(root, "_a.noudata", "Alpha", "local:a", Some("Artist A"), Some(90_000));
        write_noudata(root, "_m.noudata", "middle", "local:m", None, None);
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();

        let src = LibrarySource::open(Some(root));
        let items = src.list_items();
        assert_eq!(items.len(), 3);
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "middle", "Zebra"]
        );
        assert_eq!(items[0].artist.as_deref(), Some("Artist A"));
        assert_eq!(items[0].duration_ms, Some(90_000));
        assert!(items[0].associated);
        assert_eq!(
            items[0].library_root.as_deref(),
            Some(root),
            "each item carries its library root for P6"
        );
    }

    #[test]
    fn detail_fields_and_art_path_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("tone.wav"), b"RIFF").unwrap();
        fs::write(root.join("cover.jpg"), b"fake-jpeg").unwrap();
        write_noudata(
            root,
            "_tone.noudata",
            "Tone",
            "local:tone",
            Some("DJ"),
            Some(12_000),
        );
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();

        let src = LibrarySource::open(Some(root));
        let items = src.list_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Tone");
        assert_eq!(items[0].artist.as_deref(), Some("DJ"));
        assert_eq!(items[0].duration_ms, Some(12_000));
        assert!(
            items[0]
                .art_path
                .as_ref()
                .is_some_and(|p| p.ends_with("cover.jpg")),
            "expected sibling cover.jpg, got {:?}",
            items[0].art_path
        );
    }

    #[test]
    fn merges_playables_from_multiple_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("stick_a");
        let b = tmp.path().join("stick_b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("tone.wav"), b"RIFF").unwrap();
        fs::write(b.join("tone.wav"), b"RIFF").unwrap();
        write_noudata(&a, "_a.noudata", "Alpha", "local:a", None, None);
        write_noudata(&b, "_z.noudata", "Zebra", "local:z", None, None);
        LibraryWriter::open(&LibraryRoot::new(&a))
            .unwrap()
            .full_scan()
            .unwrap();
        LibraryWriter::open(&LibraryRoot::new(&b))
            .unwrap()
            .full_scan()
            .unwrap();

        let mut src = LibrarySource {
            override_root: None,
            roots: Vec::new(),
            mounts: Vec::new(),
            readers: Vec::new(),
            broken: Cell::new(false),
        };
        src.open_roots(&[a.clone(), b.clone()]);
        let items = src.list_items();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Zebra"]
        );
        assert_eq!(items[0].library_root.as_deref(), Some(a.as_path()));
        assert_eq!(items[1].library_root.as_deref(), Some(b.as_path()));
    }

    #[test]
    fn missing_override_yields_empty_list() {
        let src = LibrarySource::open(Some(Path::new("/no/such/plippa/usb")));
        assert!(src.roots().is_empty());
        assert!(src.list_items().is_empty());
    }

    #[test]
    fn sort_library_items_case_insensitive() {
        let mut items = vec![
            LibraryItem {
                id: 2,
                title: "banana".into(),
                associated: false,
                noudata_relpath: "b".into(),
                library_root: None,
                artist: None,
                duration_ms: None,
                art_path: None,
                item_position: None,
            },
            LibraryItem {
                id: 1,
                title: "Apple".into(),
                associated: false,
                noudata_relpath: "a".into(),
                library_root: None,
                artist: None,
                duration_ms: None,
                art_path: None,
                item_position: None,
            },
        ];
        sort_library_items(&mut items);
        assert_eq!(items[0].title, "Apple");
        assert_eq!(items[1].title, "banana");
    }

    /// Album (2 tracks, one with cues), mixtape with cues, playlist: enough for browse tests.
    fn browse_fixture(root: &Path) {
        fs::write(root.join("a1.wav"), b"RIFF").unwrap();
        fs::write(root.join("a2.wav"), b"RIFF").unwrap();
        fs::write(root.join("mix.wav"), b"RIFF").unwrap();
        fs::write(root.join("_album.noudata"), r#"{
          "noubinKey":"al.k","noubinKeyNormalised":"al.k","playableItemTitle":"Zed Album",
          "playableItemProfile": "music", "releaseData":{},
          "mediaList":[{"mediaTitle":"One","localFilePaths":["a1.wav"]},
                       {"mediaTitle":"Two","localFilePaths":["a2.wav"]}]}"#).unwrap();
        fs::write(root.join("_mix.noudata"), r#"{
          "noubinKey":"mx.k","noubinKeyNormalised":"mx.k","playableItemTitle":"A Mix",
          "playableItemProfile": "mix", "releaseData":{},
          "mediaList":[{"localFilePaths":["mix.wav"],"cueLists":[{"cueListType":"nav","cues":[
            {"startTime":"0","cueTextSpans":[{"text":"Intro"}]},
            {"startTime":"30","cueTextSpans":[{"text":"Main"}]}]},
            {"cueListType":"lyrics","cues":[{"startTime":"5","cueTextSpans":[{"text":"la"}]}]}]}]}"#).unwrap();
        LibraryWriter::open(&LibraryRoot::new(root)).unwrap().full_scan().unwrap();
    }

    /// The UI can start before the server has created the index; it must notice that the
    /// root has no readable index yet and recover once the writer creates it.
    #[test]
    fn unreadable_root_is_reported_and_recovers_after_the_index_appears() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.wav"), b"RIFF").unwrap();
        fs::write(
            tmp.path().join("_a.noudata"),
            r#"{"playableItemTitle":"A","playableItemProfile":"music","releaseData":{},
               "mediaList":[{"localFilePaths":["a.wav"]}]}"#,
        )
        .unwrap();
        let mut src = LibrarySource::open(Some(tmp.path()));
        assert!(src.storage_present());
        assert!(src.has_unreadable_root(), "no .plippa/ index yet");
        assert!(src.kind_counts().is_empty());

        LibraryWriter::open(&LibraryRoot::new(tmp.path()))
            .unwrap()
            .full_scan()
            .unwrap();
        src.reopen();
        assert!(!src.has_unreadable_root());
        assert_eq!(
            src.kind_counts()
                .iter()
                .find(|(k, _)| *k == LibraryKind::Album)
                .map(|(_, n)| *n),
            Some(1)
        );
    }

    /// A stick unplugged and plugged back in keeps its mount path but is a different
    /// filesystem. Readers opened before the unplug refer to the old mount, so every query
    /// fails and the library screen shows 0 tracks / 0 albums while `storage_present()` is
    /// still true. The loop's `refresh_if_changed` poll must reopen them.
    #[test]
    fn a_remounted_root_is_reopened_and_lists_its_new_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("USB DISK");
        fs::create_dir_all(&root).unwrap();
        seed_one_track(&root, "first", "First", "local:first");

        let mut src = LibrarySource::open(Some(&root));
        assert_eq!(titles(&src), vec!["First"]);

        // Simulate unplug + remount at the same path: fresh directory inode + fresh index.
        let old = MountIdentity::of(&root).expect("mounted stick");
        let parent = root.parent().expect("root has a parent");
        let mut remounted = false;
        for attempt in 0..32 {
            fs::remove_dir_all(&root).unwrap();
            fs::create_dir_all(parent.join(format!(".decoy-{attempt}"))).unwrap();
            fs::create_dir_all(&root).unwrap();
            if MountIdentity::of(&root).as_ref() != Some(&old) {
                remounted = true;
                break;
            }
        }
        assert!(remounted, "the filesystem kept reusing the same directory identity");
        seed_one_track(&root, "second", "Second", "local:second");

        assert!(src.refresh_if_changed(), "a remount must be noticed");
        assert_eq!(titles(&src), vec!["Second"]);
    }

    #[test]
    fn refresh_if_changed_is_quiet_while_the_mount_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        browse_fixture(tmp.path());
        let mut src = LibrarySource::open(Some(tmp.path()));
        assert!(!src.refresh_if_changed());
        assert!(!src.has_unreadable_root());
    }

    fn seed_one_track(root: &Path, stem: &str, title: &str, key: &str) {
        fs::write(root.join(format!("{stem}.wav")), b"RIFF").unwrap();
        fs::write(
            root.join(format!("_{stem}.noudata")),
            format!(
                r#"{{"noudataType":"looseTrack","playableItemProfile":"music",
                   "noubinKey":"{key}","noubinKeyNormalised":"{key}",
                   "playableItemTitle":"{title}",
                   "mediaList":[{{"localFilePaths":["{stem}.wav"]}}]}}"#
            ),
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();
    }

    fn titles(src: &LibrarySource) -> Vec<String> {
        src.list_kind(LibraryKind::Track)
            .into_iter()
            .map(|i| i.title)
            .collect()
    }

    #[test]
    fn kind_counts_and_list_kind_per_category() {
        let tmp = tempfile::tempdir().unwrap();
        browse_fixture(tmp.path());
        let src = LibrarySource::open(Some(tmp.path()));
        let counts = src.kind_counts();
        let n = |k| counts.iter().find(|(kk, _)| *kk == k).map_or(0, |(_, n)| *n);
        assert_eq!(n(LibraryKind::Album), 1);
        assert_eq!(n(LibraryKind::Mix), 1);
        assert_eq!(n(LibraryKind::Audiobook), 0);
        let albums = src.list_kind(LibraryKind::Album);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, "Zed Album");
        assert_eq!(albums[0].library_root.as_deref(), Some(tmp.path()));
        assert!(src.list_kind(LibraryKind::Podcast).is_empty());
        assert!(src.storage_present());
    }

    /// XCHECK: a tagless single-file mix / audiobook / podcast (a loose *track* with that
    /// profile) is listed under Mixtapes / Audiobooks / Podcasts on the device, not Tracks;
    /// music/other loose tracks and link tracks stay under Tracks.
    #[test]
    fn loose_long_form_tracks_are_listed_under_their_profile_category() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for f in ["m", "b", "p", "s", "o"] {
            fs::write(root.join(format!("{f}.wav")), b"RIFF").unwrap();
        }
        for (f, title, profile) in [
            ("m", "Loose Mix", "mix"),
            ("b", "Loose Book", "audiobook"),
            ("p", "Loose Cast", "podcast"),
            ("s", "Loose Song", "music"),
        ] {
            fs::write(
                root.join(format!("{f}.noudata")),
                format!(
                    r#"{{"noudataType":"looseTrack","playableItemTitle":"{title}","playableItemProfile":"{profile}",
                       "mediaList":[{{"localFilePaths":["{f}.wav"]}}]}}"#
                ),
            )
            .unwrap();
        }
        LibraryWriter::open(&LibraryRoot::new(root)).unwrap().full_scan().unwrap();
        let src = LibrarySource::open(Some(root));
        let counts = src.kind_counts();
        let n = |k| counts.iter().find(|(kk, _)| *kk == k).map_or(0, |(_, n)| *n);
        assert_eq!(n(LibraryKind::Mix), 1);
        assert_eq!(n(LibraryKind::Audiobook), 1);
        assert_eq!(n(LibraryKind::Podcast), 1);
        // the music loose track and the orphan `o.wav` stay in Tracks
        assert_eq!(n(LibraryKind::Track), 2);
        let titles = |k| {
            src.list_kind(k).into_iter().map(|i| i.title).collect::<Vec<_>>()
        };
        assert_eq!(titles(LibraryKind::Mix), ["Loose Mix"]);
        assert_eq!(titles(LibraryKind::Audiobook), ["Loose Book"]);
        assert_eq!(titles(LibraryKind::Podcast), ["Loose Cast"]);
        assert!(titles(LibraryKind::Track).contains(&"Loose Song".to_string()));
        assert!(!titles(LibraryKind::Track).contains(&"Loose Mix".to_string()));
        assert!(src.list_kind(LibraryKind::Album).is_empty());
    }

    #[test]
    fn detail_info_lists_tracks_and_only_navigable_chapters() {
        let tmp = tempfile::tempdir().unwrap();
        browse_fixture(tmp.path());
        let src = LibrarySource::open(Some(tmp.path()));
        let album = &src.list_kind(LibraryKind::Album)[0];
        let d = src.detail_info(album, Some(LibraryKind::Album), &Capabilities::desktop_mock());
        assert_eq!(
            d.tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            vec!["One", "Two"]
        );
        assert!(d.chapters.is_empty());
        assert!(!d.media_missing);
        let mix = &src.list_kind(LibraryKind::Mix)[0];
        let d = src.detail_info(mix, Some(LibraryKind::Mix), &Capabilities::desktop_mock());
        assert_eq!(d.tracks.len(), 1);
        // The "lyrics" cue list is not navigable and is not offered as a chapter.
        assert_eq!(
            d.chapters
                .iter()
                .map(|c| (c.start_ms, c.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "Intro"), (30_000, "Main")]
        );
        // A media file that vanished before the next scan is flagged once the index knows.
        fs::remove_file(tmp.path().join("a2.wav")).unwrap();
        LibraryWriter::open(&LibraryRoot::new(tmp.path()))
            .unwrap()
            .full_scan()
            .unwrap();
        let d = src.detail_info(album, Some(LibraryKind::Album), &Capabilities::desktop_mock());
        assert!(d.tracks[0].available && !d.tracks[1].available);
        assert!(d.media_missing);
    }

    /// MEDIA_01: a release this board cannot decode is still indexed and listed; its items exist
    /// (`available`) but are `!supported` on a board without AAC, and supported on one with it.
    /// The flag is computed per read from the capabilities, never stored in the index.
    #[test]
    fn scan_keeps_unplayable_media_and_marks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("take.m4a"), b"\0\0\0\x18ftypM4A ").unwrap();
        fs::write(root.join("ok.wav"), b"RIFF").unwrap();
        fs::write(
            root.join("_r.noudata"),
            r#"{"noubinKey":"r.k","noubinKeyNormalised":"r.k","playableItemTitle":"Mixed Formats",
               "playableItemProfile":"music","releaseData":{},
               "mediaList":[{"mediaTitle":"Take","localFilePaths":["take.m4a"]},
                            {"mediaTitle":"Ok","localFilePaths":["ok.wav"]}]}"#,
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(root)).unwrap().full_scan().unwrap();
        let src = LibrarySource::open(Some(root));
        let album = &src.list_kind(LibraryKind::Album)[0];

        let no_aac = Capabilities::desktop_mock();
        let d = src.detail_info(album, Some(LibraryKind::Album), &no_aac);
        assert_eq!(d.tracks.len(), 2, "the release is listed in full");
        assert!(d.tracks.iter().all(|t| t.available), "both files exist");
        assert!(!d.media_missing, "unplayable is not missing");
        assert!(!d.tracks[0].supported);
        assert_eq!(d.tracks[0].format, "M4A/AAC");
        assert!(d.tracks[1].supported);

        let mut with_aac = no_aac.clone();
        with_aac.audio_formats.aac = true;
        let d = src.detail_info(album, Some(LibraryKind::Album), &with_aac);
        assert!(d.tracks[0].supported, "the same index on a board with AAC");

        // Deleting the derived index and rescanning changes nothing.
        fs::remove_dir_all(root.join(".plippa")).unwrap();
        LibraryWriter::open(&LibraryRoot::new(root)).unwrap().full_scan().unwrap();
        let src = LibrarySource::open(Some(root));
        let album = &src.list_kind(LibraryKind::Album)[0];
        let d = src.detail_info(album, Some(LibraryKind::Album), &no_aac);
        assert!(d.tracks[0].available && !d.tracks[0].supported);
    }

    #[test]
    fn removed_stick_yields_empty_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        browse_fixture(&root);
        let src = LibrarySource::open(Some(&root));
        let album = src.list_kind(LibraryKind::Album)[0].clone();
        drop(tmp);
        assert!(!src.storage_present());
        assert!(src.kind_counts().is_empty());
        assert!(src.list_kind(LibraryKind::Album).is_empty());
        let d = src.detail_info(&album, Some(LibraryKind::Album), &Capabilities::desktop_mock());
        assert!(d.tracks.is_empty() && d.chapters.is_empty());
    }

    /// SEC-12: hostile strings inside a `.noudata` (title, artist, media title, cue text) reach
    /// the model only after the shared stripper.
    #[test]
    fn hostile_file_text_is_stripped_before_it_reaches_the_model() {
        use crate::screens::screen_text;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("h.wav"), b"RIFF").unwrap();
        // JSON escapes: RLO, zero-width space, BEL, ESC, LF, BOM, tag character.
        let evil = r"Ev\u202Eil\u200B\u0007\u001B\n\uFEFF\uDB40\uDC41x";
        fs::write(
            root.join("_h.noudata"),
            format!(
                r#"{{"noubinKey":"h.k","noubinKeyNormalised":"h.k","playableItemTitle":"{evil}",
                  "playableItemProfile": "mix", "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"{evil}"}}]}}}},
                  "mediaList":[{{"mediaTitle":"{evil}","localFilePaths":["h.wav"],
                    "cueLists":[{{"cueListType":"nav","cues":[{{"startTime":"0","cueTextSpans":[{{"text":"{evil}"}}]}}]}}]}}]}}"#
            ),
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(root)).unwrap().full_scan().unwrap();
        let src = LibrarySource::open(Some(root));
        let mut strings = Vec::new();
        for it in src.list_kind(LibraryKind::Mix).iter().chain(src.list_items().iter()) {
            strings.push(it.title.clone());
            strings.extend(it.artist.clone());
        }
        let it = src.list_kind(LibraryKind::Mix)[0].clone();
        let d = src.detail_info(&it, Some(LibraryKind::Mix), &Capabilities::desktop_mock());
        strings.extend(d.tracks.iter().map(|t| t.title.clone()));
        strings.extend(d.tracks.iter().filter_map(|t| t.artist.clone()));
        strings.extend(d.chapters.iter().map(|c| c.title.clone()));
        assert!(strings.len() >= 5, "{strings:?}");
        for s in &strings {
            assert_eq!(*s, screen_text(s), "already clean: {s:?}");
            assert!(
                !s.chars().any(|c| c.is_control()
                    || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{FEFF}')
                    || ('\u{E0000}'..='\u{E007F}').contains(&c)),
                "hostile char survived in {s:?}"
            );
        }
    }

    fn write_noudata(
        root: &Path,
        name: &str,
        title: &str,
        key: &str,
        artist: Option<&str>,
        duration_ms: Option<u64>,
    ) {
        let artist_json = artist
            .map(|a| format!(r#", "playableItemArtist": "{a}""#))
            .unwrap_or_default();
        let dur_json = duration_ms
            .map(|d| format!(r#", "durationMs": {d}"#))
            .unwrap_or_default();
        let media = name.trim_start_matches('_').replace(".noudata", ".wav");
        // Ensure a matching wav exists when name maps oddly — callers write files.
        let _ = media;
        let media_file = if name.contains("tone") {
            "tone.wav"
        } else if name.contains("_z") {
            "zebra.wav"
        } else if name.contains("_a") {
            "alpha.wav"
        } else {
            "tone.wav"
        };
        // For middle use a dedicated file if present.
        let media_file = if name.contains("_m") {
            if root.join("middle.wav").exists() {
                "middle.wav"
            } else {
                // reuse alpha/zebra not needed — write middle.wav inline
                let _ = fs::write(root.join("middle.wav"), b"RIFF");
                "middle.wav"
            }
        } else {
            media_file
        };
        // Multi-root merge test uses tone.wav on both sticks for _a / _z names.
        let media_file = if !root.join(media_file).exists() && root.join("tone.wav").exists() {
            "tone.wav"
        } else {
            media_file
        };
        let noudata = format!(
            r#"{{
              "noubinKey": "{key}",
              "noubinKeyNormalised": "{}",
              "playableItemTitle": "{title}"{artist_json}{dur_json},
              "mediaList": [{{"localFilePaths": ["{media_file}"]}}]
            }}"#,
            normalise_noubin_key(key)
        );
        fs::write(root.join(name), noudata).unwrap();
    }

    /// NB_18: the track lookup refuses a playlist (the portal does too) and only answers for
    /// the root the caller names — two sticks' index ids are not comparable.
    #[test]
    fn track_of_refuses_a_playlist_and_stays_on_the_given_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("Music/Album")).unwrap();
        fs::write(root.join("Music/Album/01.wav"), b"RIFF").unwrap();
        fs::write(root.join("Music/Album/02.wav"), b"RIFF").unwrap();
        fs::write(
            root.join("Music/Album/_a.noudata"),
            r#"{"playableItemTitle":"Album","playableItemProfile":"music","releaseData":{},
                "mediaList":[{"mediaTitle":"One","localFilePaths":["01.wav"]},
                             {"mediaTitle":"Two","localFilePaths":["02.wav"]}]}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("Playlists")).unwrap();
        fs::write(
            root.join("Playlists/_p.noudata"),
            r#"{"playableItemTitle":"List","mediaList":[{"localFilePaths":["../Music/Album/01.wav"]}]}"#,
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();
        let src = LibrarySource::open(Some(root));

        assert_eq!(
            src.track_of(None, "Playlists/_p.noudata", 0),
            TrackLookup::NotARelease,
            "a playlist's items are not release tracks"
        );
        match src.track_of(Some(root), "Music/Album/_a.noudata", 1) {
            TrackLookup::Found(owner, media, title) => {
                assert_eq!(owner.as_path(), root);
                assert_eq!(media, "Music/Album/02.wav");
                assert_eq!(title, "Two");
            }
            other => panic!("expected the album's second track, got {other:?}"),
        }
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(
            src.track_of(Some(elsewhere.path()), "Music/Album/_a.noudata", 1),
            TrackLookup::Missing,
            "a root that does not own the release must not answer"
        );
    }
}
