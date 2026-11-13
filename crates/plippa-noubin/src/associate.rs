//! NB_18: associating **one track** of a release (Noubin §4.5, a `linkTrack`).
//!
//! A single item of a release gets its own `.noudata` next to its media file; the release's
//! `.noudata` is not modified. This module decides *which* file that is and builds the document,
//! and it writes nothing: the tag write is finalised first, then the caller writes
//! [`TrackTarget::doc`] (new file) or read-modify-writes [`TrackTarget::relpath`] (existing
//! file), which keeps the crash-safe ordering of NB_06.
//!
//! Both entry points use this one implementation: the portal's
//! `POST /api/playables/{id}/associate?track={n}` and the device UI's track menu.

use crate::noudata::{
    MediaListItem, NoudataDocument, NOUDATA_VERSION, WHAT_AM_I_LOOKING_AT,
};
use crate::noudata_types::NoudataType;
use crate::playlist::relative_media_path;
use std::path::{Component, Path};

/// Longest media-file stem kept when naming the per-track file (bytes).
pub const MAX_TRACK_STEM_BYTES: usize = 200;
/// How many ` (n)` suffixes are tried before giving up.
pub const MAX_TRACK_NAME_ATTEMPTS: u32 = 1000;

/// Where a single track's key lives, and the document to write there.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackTarget {
    /// Library-relative, POSIX.
    pub relpath: String,
    /// Complete document, keys **not** set yet.
    pub doc: NoudataDocument,
    /// True when `relpath` does not exist yet (the caller creates it).
    pub created: bool,
}

/// True when `rel` is a non-empty, library-relative, POSIX path: no `..`, no root, no empty
/// component (`a//b` and a trailing `/` are malformed, not merely odd).
///
/// The one predicate for "is this path inside the library", shared by the associate paths.
pub fn is_clean_relpath(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.split('/').any(|s| s.is_empty())
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// True for the kinds whose `mediaList` items are tracks a person can associate one by one.
///
/// One predicate for both entry points: the portal refuses a single item of anything else, and
/// so must the device (a `linkTrack` that refers to a playlist is re-read as a playlist, so it
/// would be a stray row rather than a track).
pub fn is_release_kind(kind: crate::LibraryKind) -> bool {
    matches!(
        kind,
        crate::LibraryKind::Album
            | crate::LibraryKind::Mix
            | crate::LibraryKind::Audiobook
            | crate::LibraryKind::Podcast
    )
}

/// Decide the per-track target for `position` of a release. **Writes nothing.**
///
/// * `release_relpath` — the release's `.noudata`, library-relative.
/// * `media_relpath` — the track's media file, library-relative (from the caller's index).
/// * `track_title` — the track's own title.
/// * `existing` — a per-track `.noudata` the caller's scan already found that refers to
///   `release_relpath` and lists `media_relpath` (library-relative). Reused unchanged.
///
/// `Err` is a sentence for a human: never a path, an OS error or raw stderr.
pub fn track_associate_target(
    root: &Path,
    release_relpath: &str,
    media_relpath: &str,
    track_title: &str,
    existing: Option<&str>,
) -> Result<TrackTarget, String> {
    if !is_clean_relpath(release_relpath) {
        return Err("the release .noudata is outside the library".into());
    }
    if !is_clean_relpath(media_relpath) {
        return Err(
            "this track's media file is outside the library, so it cannot get its own .noudata"
                .into(),
        );
    }
    if let Some(rel) = existing {
        if !is_clean_relpath(rel) {
            return Err("the track's .noudata is outside the library".into());
        }
        return Ok(TrackTarget {
            relpath: rel.to_string(),
            doc: read_existing(&root.join(rel))?,
            created: false,
        });
    }

    let media_path = Path::new(media_relpath);
    let dir = rel_parent(media_relpath).to_string();
    let file_name = media_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "bad media file name".to_string())?
        .to_string();
    let stem = capped_stem(media_path);
    let relpath = free_sibling(root, &dir, &stem)?;
    let refer = relative_media_path(&dir, release_relpath)
        .ok_or_else(|| "the release .noudata is outside the library".to_string())?;
    let doc = NoudataDocument {
        what_am_i_looking_at: Some(WHAT_AM_I_LOOKING_AT.to_string()),
        noudata_version: Some(NOUDATA_VERSION.to_string()),
        // A linkTrack: own keys/notes only; release data, credits and profile come from the
        // referred release. `playableItemTitle` (not `mediaTitle`) is its title.
        noudata_type: Some(NoudataType::LinkTrack),
        playable_item_title: Some(track_title.to_string()),
        refer_to_release_noudata: Some(refer),
        media_list: vec![MediaListItem {
            local_file_paths: Some(vec![file_name]),
            ..Default::default()
        }],
        ..Default::default()
    };
    Ok(TrackTarget {
        relpath,
        doc,
        created: true,
    })
}

fn read_existing(path: &Path) -> Result<NoudataDocument, String> {
    let bytes = crate::read_noudata_capped(path)
        .map_err(|_| "the track's existing .noudata could not be read".to_string())?;
    crate::parse_noudata(&bytes)
        .map_err(|_| "the track's existing .noudata is not valid Noubin JSON".to_string())
}

/// Directory part of a POSIX relpath (`""` at the root).
fn rel_parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

/// The media file's stem, capped on a character boundary.
fn capped_stem(media_path: &Path) -> String {
    let mut stem = media_path
        .file_stem()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("track")
        .to_string();
    while stem.len() > MAX_TRACK_STEM_BYTES {
        if stem.pop().is_none() {
            break;
        }
    }
    if stem.is_empty() {
        "track".to_string()
    } else {
        stem
    }
}

/// `<dir>/<stem>.noudata`, then ` (2)`, ` (3)` … until the name is free.
fn free_sibling(root: &Path, dir: &str, stem: &str) -> Result<String, String> {
    let mut n = 1u32;
    loop {
        let name = if n == 1 {
            format!("{stem}.noudata")
        } else {
            format!("{stem} ({n}).noudata")
        };
        let rel = if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        };
        if !root.join(&rel).exists() {
            return Ok(rel);
        }
        n += 1;
        if n > MAX_TRACK_NAME_ATTEMPTS {
            return Err("could not find a free name for the track's .noudata".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nb18-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn a_new_track_target_is_a_sibling_link_track() {
        let root = tmp("new");
        write(&root.join("Music/Album/release.noudata"), "{}");
        write(&root.join("Music/Album/02 - Second.mp3"), "x");
        let t = track_associate_target(
            &root,
            "Music/Album/release.noudata",
            "Music/Album/02 - Second.mp3",
            "Second Track",
            None,
        )
        .unwrap();
        assert!(t.created);
        assert_eq!(t.relpath, "Music/Album/02 - Second.noudata");
        assert_eq!(t.doc.noudata_type, Some(NoudataType::LinkTrack));
        assert_eq!(t.doc.playable_item_title.as_deref(), Some("Second Track"));
        // Relative to the track file's own directory, not a bare file name.
        assert_eq!(
            t.doc.refer_to_release_noudata.as_deref(),
            Some("release.noudata")
        );
        assert_eq!(t.doc.media_list.len(), 1);
        assert_eq!(
            t.doc.media_list[0].local_file_paths.as_deref(),
            Some(&["02 - Second.mp3".to_string()][..])
        );
        assert!(!t.relpath.is_empty());
    }

    #[test]
    fn the_reference_is_relative_to_a_nested_track_directory() {
        let root = tmp("nested");
        write(&root.join("Music/release.noudata"), "{}");
        write(&root.join("Music/Disc 1/a.mp3"), "x");
        let t = track_associate_target(
            &root,
            "Music/release.noudata",
            "Music/Disc 1/a.mp3",
            "A",
            None,
        )
        .unwrap();
        assert_eq!(
            t.doc.refer_to_release_noudata.as_deref(),
            Some("../release.noudata"),
            "the reference must reach up out of the track's directory"
        );
        assert_eq!(t.relpath, "Music/Disc 1/a.noudata");
    }

    #[test]
    fn an_existing_track_file_is_reused_unchanged() {
        let root = tmp("existing");
        write(&root.join("Music/release.noudata"), "{}");
        write(
            &root.join("Music/song.noudata"),
            r#"{"noudataType":"linkTrack","playableItemTitle":"Different Title","noubinKey":"keep-me"}"#,
        );
        let t = track_associate_target(
            &root,
            "Music/release.noudata",
            "Music/song.mp3",
            "Song",
            Some("Music/song.noudata"),
        )
        .unwrap();
        assert!(!t.created);
        assert_eq!(t.relpath, "Music/song.noudata");
        assert_eq!(
            t.doc.playable_item_title.as_deref(),
            Some("Different Title"),
            "the existing title is not replaced"
        );
        assert_eq!(t.doc.noubin_key.as_deref(), Some("keep-me"));
    }

    #[test]
    fn a_name_collision_picks_the_next_free_suffix() {
        let root = tmp("collide");
        write(&root.join("Music/release.noudata"), "{}");
        write(&root.join("Music/song.mp3"), "x");
        write(&root.join("Music/song.noudata"), "{}");
        let t = track_associate_target(&root, "Music/release.noudata", "Music/song.mp3", "Song", None)
            .unwrap();
        assert_eq!(t.relpath, "Music/song (2).noudata");
        write(&root.join("Music/song (2).noudata"), "{}");
        let t = track_associate_target(&root, "Music/release.noudata", "Music/song.mp3", "Song", None)
            .unwrap();
        assert_eq!(t.relpath, "Music/song (3).noudata");
    }

    #[test]
    fn hostile_media_paths_are_refused() {
        let root = tmp("hostile");
        write(&root.join("Music/release.noudata"), "{}");
        for bad in ["../x.mp3", "/abs/x.mp3", "a//b.mp3", ""] {
            let r = track_associate_target(&root, "Music/release.noudata", bad, "T", None);
            assert!(r.is_err(), "{bad:?} was accepted");
        }
        for bad in ["../release.noudata", "/abs/release.noudata", ""] {
            let r = track_associate_target(&root, bad, "Music/x.mp3", "T", None);
            assert!(r.is_err(), "release {bad:?} was accepted");
        }
        // Nothing was created anywhere for the refused paths.
        assert!(!root.join("x.noudata").exists());
        assert!(!root.join("Music/x.noudata").exists());
        assert!(!root.join("Music/release (2).noudata").exists());
    }

    #[test]
    fn the_release_document_is_never_written() {
        let root = tmp("untouched");
        let release = root.join("Music/release.noudata");
        write(&release, r#"{"mediaList":[{"localFilePaths":["song.mp3"]}]}"#);
        let before = fs::read(&release).unwrap();
        let t = track_associate_target(&root, "Music/release.noudata", "Music/song.mp3", "Song", None)
            .unwrap();
        assert!(t.created);
        let after = fs::read(&release).unwrap();
        assert_eq!(before, after, "the release document must be byte-identical");
    }

    #[test]
    fn a_long_stem_is_capped() {
        let root = tmp("long");
        write(&root.join("Music/release.noudata"), "{}");
        let name = format!("{}.mp3", "a".repeat(400));
        let media = format!("Music/{name}");
        let t = track_associate_target(&root, "Music/release.noudata", &media, "T", None).unwrap();
        let file = t.relpath.rsplit('/').next().unwrap();
        assert!(file.len() <= MAX_TRACK_STEM_BYTES + ".noudata".len());
        assert!(file.ends_with(".noudata"));
    }

    #[test]
    fn a_path_ending_in_a_separator_is_refused() {
        let root = tmp("dirname");
        write(&root.join("Music/release.noudata"), "{}");
        let err = track_associate_target(&root, "Music/release.noudata", "Music/", "T", None)
            .unwrap_err();
        assert!(err.contains("media file is outside the library"), "{err}");
    }
}
