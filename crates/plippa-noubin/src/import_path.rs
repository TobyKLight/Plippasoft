//! §4.4 preferred import placement + companion `.noudata` field mapping.

use crate::noudata_types::{NoudataType, PlayableItemProfile};
use crate::safe_name::{sanitize_filename, sanitize_path_segment};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// User-chosen import kind (portal Add dialog). Sets `playableItemProfile` and root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind {
    Music,
    Mix,
    Audiobook,
    Podcast,
    Other,
}

impl ImportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Music => "music",
            Self::Mix => "mix",
            Self::Audiobook => "audiobook",
            Self::Podcast => "podcast",
            Self::Other => "other",
        }
    }

    /// Parse dialog / API kind. Rejects unknown values (no silent `Imports/` fallback).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "music" => Some(Self::Music),
            "mix" => Some(Self::Mix),
            "audiobook" => Some(Self::Audiobook),
            "podcast" => Some(Self::Podcast),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// Noubin `playableItemProfile` for this kind.
    pub fn profile(self) -> PlayableItemProfile {
        match self {
            Self::Music => PlayableItemProfile::Music,
            Self::Mix => PlayableItemProfile::Mix,
            Self::Audiobook => PlayableItemProfile::Audiobook,
            Self::Podcast => PlayableItemProfile::Podcast,
            Self::Other => PlayableItemProfile::Other,
        }
    }

    /// Folder a *loose track* (no artist/album known) of this kind goes into, directly under
    /// the library root (the standard does not specify loose-track folders).
    pub fn loose_track_dir(self) -> &'static str {
        match self {
            Self::Music => "Music",
            Self::Mix => "Music/Mixes",
            Self::Audiobook => "Audiobooks",
            Self::Podcast => "Podcasts",
            Self::Other => "Other",
        }
    }
}

/// Fields extracted from embedded tags (or left empty for filename fallbacks).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedFields {
    pub title: Option<String>,
    pub album: Option<String>,
    pub artist: Option<String>,
    /// Optional author first/last when known separately (audiobooks).
    pub author_lastname: Option<String>,
    pub author_firstname: Option<String>,
}

impl ExtractedFields {
    /// The usable release-level tags: an artist (or audiobook author) and/or an album. A title
    /// alone (or nothing) is not enough to describe a release.
    pub fn has_release_tags(&self) -> bool {
        non_empty(self.artist.as_deref()).is_some()
            || non_empty(self.album.as_deref()).is_some()
            || non_empty(self.author_lastname.as_deref()).is_some()
            || non_empty(self.author_firstname.as_deref()).is_some()
    }
}

/// Result of computing where an imported file should land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlacement {
    /// Directory relative to the library root (POSIX separators, no trailing slash).
    pub relative_dir: PathBuf,
    /// Sanitized media filename (with extension).
    pub media_filename: String,
    /// Suggested `playableItemTitle` / release title.
    pub playable_item_title: String,
    /// Safe title for filesystem-facing metadata.
    pub playable_item_safe_title: String,
    /// Primary display name used for artist/DJ/author/podcast folder (folder fallbacks such
    /// as `Unknown Artist` are folder names only — never written as metadata).
    pub primary_name: String,
    /// The artist/author/host named by the tags, if any; `None` ⇒ write no credits.
    pub primary_artist: Option<String>,
    /// `release` when the tags name an artist and/or album, else `looseTrack`.
    pub noudata_type: NoudataType,
    pub profile: PlayableItemProfile,
}

/// `(kind, extracted fields, filename) → relative directory + titles` under the library root.
///
/// Tags naming an artist and/or album ⇒ a `release` under the §4.4 tree (folder fallbacks
/// `Unknown Artist` / `Unknown Author` / `Unknown DJ` / `Unknown Podcast` fill only the
/// missing half of the path, never the metadata). No usable tags (filename only, or just a
/// title tag) ⇒ a `looseTrack` placed in the kind's root folder
/// ([`ImportKind::loose_track_dir`]) titled from the tag or the file name.
/// Does **not** create directories.
pub fn import_placement(
    kind: ImportKind,
    fields: &ExtractedFields,
    filename: &str,
) -> ImportPlacement {
    let media_filename = sanitize_filename(filename, "import");
    let stem = Path::new(&media_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported")
        .to_string();
    let profile = kind.profile();

    if !fields.has_release_tags() {
        let title = non_empty(fields.title.as_deref()).unwrap_or(&stem);
        return ImportPlacement {
            relative_dir: PathBuf::from(kind.loose_track_dir()),
            media_filename,
            playable_item_title: title.to_string(),
            playable_item_safe_title: sanitize_path_segment(title, &stem),
            primary_name: String::new(),
            primary_artist: None,
            noudata_type: NoudataType::LooseTrack,
            profile,
        };
    }
    let primary_artist = match kind {
        ImportKind::Audiobook => (non_empty(fields.artist.as_deref()).is_some()
            || non_empty(fields.author_lastname.as_deref()).is_some()
            || non_empty(fields.author_firstname.as_deref()).is_some())
        .then(|| audiobook_author_segment(fields)),
        _ => non_empty(fields.artist.as_deref()).map(str::to_string),
    };

    match kind {
        ImportKind::Music => {
            let artist = non_empty(fields.artist.as_deref()).unwrap_or("Unknown Artist");
            let release = non_empty(fields.album.as_deref())
                .or_else(|| non_empty(fields.title.as_deref()))
                .unwrap_or(&stem);
            let title = non_empty(fields.title.as_deref())
                .or_else(|| non_empty(fields.album.as_deref()))
                .unwrap_or(&stem);
            let artist_seg = sanitize_path_segment(artist, "Unknown Artist");
            let release_seg = sanitize_path_segment(release, "Unknown Release");
            ImportPlacement {
                relative_dir: PathBuf::from("Music").join(&artist_seg).join(&release_seg),
                media_filename,
                playable_item_title: title.to_string(),
                playable_item_safe_title: sanitize_path_segment(title, &stem),
                primary_name: artist_seg,
                primary_artist: primary_artist.clone(),
                noudata_type: NoudataType::Release,
                profile,
            }
        }
        ImportKind::Mix => {
            let dj = non_empty(fields.artist.as_deref()).unwrap_or("Unknown DJ");
            let title = non_empty(fields.title.as_deref())
                .or_else(|| non_empty(fields.album.as_deref()))
                .unwrap_or(&stem);
            let dj_seg = sanitize_path_segment(dj, "Unknown DJ");
            ImportPlacement {
                relative_dir: PathBuf::from("Music").join("Mixes").join(&dj_seg),
                media_filename,
                playable_item_title: title.to_string(),
                playable_item_safe_title: sanitize_path_segment(title, &stem),
                primary_name: dj_seg,
                primary_artist: primary_artist.clone(),
                noudata_type: NoudataType::Release,
                profile,
            }
        }
        ImportKind::Audiobook => {
            let author_seg = audiobook_author_segment(fields);
            let book = non_empty(fields.album.as_deref())
                .or_else(|| non_empty(fields.title.as_deref()))
                .unwrap_or(&stem);
            let title = non_empty(fields.title.as_deref())
                .or_else(|| non_empty(fields.album.as_deref()))
                .unwrap_or(&stem);
            let book_seg = sanitize_path_segment(book, "Unknown Title");
            ImportPlacement {
                relative_dir: PathBuf::from("Audiobooks")
                    .join(&author_seg)
                    .join(&book_seg),
                media_filename,
                playable_item_title: title.to_string(),
                playable_item_safe_title: sanitize_path_segment(title, &stem),
                primary_name: author_seg,
                primary_artist: primary_artist.clone(),
                noudata_type: NoudataType::Release,
                profile,
            }
        }
        ImportKind::Podcast => {
            let show = non_empty(fields.album.as_deref())
                .or_else(|| non_empty(fields.artist.as_deref()))
                .unwrap_or("Unknown Podcast");
            let title = non_empty(fields.title.as_deref()).unwrap_or(&stem);
            let show_seg = sanitize_path_segment(show, "Unknown Podcast");
            ImportPlacement {
                relative_dir: PathBuf::from("Podcasts").join(&show_seg),
                media_filename,
                playable_item_title: title.to_string(),
                playable_item_safe_title: sanitize_path_segment(title, &stem),
                primary_name: show_seg,
                primary_artist: primary_artist.clone(),
                noudata_type: NoudataType::Release,
                profile,
            }
        }
        ImportKind::Other => {
            let group = non_empty(fields.artist.as_deref())
                .or_else(|| non_empty(fields.album.as_deref()))
                .or_else(|| non_empty(fields.title.as_deref()))
                .unwrap_or(&stem);
            let title = non_empty(fields.title.as_deref()).unwrap_or(&stem);
            let group_seg = sanitize_path_segment(group, "Unknown");
            ImportPlacement {
                relative_dir: PathBuf::from("Other").join(&group_seg),
                media_filename,
                playable_item_title: title.to_string(),
                playable_item_safe_title: sanitize_path_segment(title, &stem),
                primary_name: group_seg,
                primary_artist: primary_artist.clone(),
                noudata_type: NoudataType::Release,
                profile,
            }
        }
    }
}

/// Audiobook author folder: `Lastname, Firstname` when both available; else single string.
pub fn audiobook_author_segment(fields: &ExtractedFields) -> String {
    if let (Some(last), Some(first)) = (
        non_empty(fields.author_lastname.as_deref()),
        non_empty(fields.author_firstname.as_deref()),
    ) {
        return sanitize_path_segment(&format!("{last}, {first}"), "Unknown Author");
    }
    if let Some(artist) = non_empty(fields.artist.as_deref()) {
        if artist.contains(',') {
            return sanitize_path_segment(artist, "Unknown Author");
        }
        // "Firstname Lastname" → "Lastname, Firstname" when exactly two tokens.
        let parts: Vec<&str> = artist.split_whitespace().collect();
        if parts.len() == 2 {
            return sanitize_path_segment(&format!("{}, {}", parts[1], parts[0]), "Unknown Author");
        }
        return sanitize_path_segment(artist, "Unknown Author");
    }
    sanitize_path_segment("Unknown Author", "Unknown Author")
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_path_with_tags() {
        let p = import_placement(
            ImportKind::Music,
            &ExtractedFields {
                title: Some("When Doves Cry".into()),
                album: Some("Purple Rain".into()),
                artist: Some("Prince".into()),
                ..Default::default()
            },
            "01.flac",
        );
        assert_eq!(p.relative_dir, PathBuf::from("Music/Prince/Purple Rain"));
        assert_eq!(p.profile, PlayableItemProfile::Music);
        assert_eq!(p.noudata_type, NoudataType::Release);
        assert_eq!(p.primary_artist.as_deref(), Some("Prince"));
    }

    #[test]
    fn no_tags_is_a_loose_track_in_the_kind_root() {
        let p = import_placement(ImportKind::Music, &ExtractedFields::default(), "solo.mp3");
        assert_eq!(p.relative_dir, PathBuf::from("Music"));
        assert_eq!(p.noudata_type, NoudataType::LooseTrack);
        assert_eq!(p.playable_item_title, "solo");
        assert_eq!(p.primary_artist, None);
        for (kind, dir) in [
            (ImportKind::Mix, "Music/Mixes"),
            (ImportKind::Audiobook, "Audiobooks"),
            (ImportKind::Podcast, "Podcasts"),
            (ImportKind::Other, "Other"),
        ] {
            let p = import_placement(kind, &ExtractedFields::default(), "x.mp3");
            assert_eq!(p.relative_dir, PathBuf::from(dir));
            assert_eq!(p.noudata_type, NoudataType::LooseTrack);
            assert_eq!(p.profile, kind.profile());
        }
    }

    #[test]
    fn title_tag_only_is_still_loose() {
        let p = import_placement(
            ImportKind::Music,
            &ExtractedFields {
                title: Some("Tagged Title".into()),
                ..Default::default()
            },
            "file.mp3",
        );
        assert_eq!(p.noudata_type, NoudataType::LooseTrack);
        assert_eq!(p.playable_item_title, "Tagged Title");
    }

    #[test]
    fn album_only_is_a_release_without_invented_artist() {
        let p = import_placement(
            ImportKind::Music,
            &ExtractedFields {
                album: Some("Some Album".into()),
                ..Default::default()
            },
            "a.mp3",
        );
        assert_eq!(p.noudata_type, NoudataType::Release);
        assert_eq!(p.primary_artist, None);
        assert_eq!(
            p.relative_dir,
            PathBuf::from("Music/Unknown Artist/Some Album")
        );
    }

    #[test]
    fn mix_podcast_audiobook_other_prefixes() {
        let mix = import_placement(
            ImportKind::Mix,
            &ExtractedFields {
                artist: Some("Fatboy Slim".into()),
                title: Some("Big Beach".into()),
                ..Default::default()
            },
            "mix.mp3",
        );
        assert_eq!(mix.relative_dir, PathBuf::from("Music/Mixes/Fatboy Slim"));

        let pod = import_placement(
            ImportKind::Podcast,
            &ExtractedFields {
                album: Some("Freakonomics Radio".into()),
                title: Some("ep01".into()),
                ..Default::default()
            },
            "ep01.mp3",
        );
        assert_eq!(
            pod.relative_dir,
            PathBuf::from("Podcasts/Freakonomics Radio")
        );

        let book = import_placement(
            ImportKind::Audiobook,
            &ExtractedFields {
                artist: Some("Andy Weir".into()),
                album: Some("Project Hail Mary".into()),
                title: Some("Chapter 1".into()),
                ..Default::default()
            },
            "ch1.mp3",
        );
        assert_eq!(
            book.relative_dir,
            PathBuf::from("Audiobooks/Weir, Andy/Project Hail Mary")
        );

        let other = import_placement(
            ImportKind::Other,
            &ExtractedFields {
                artist: Some("Field Rec".into()),
                title: Some("Rain".into()),
                ..Default::default()
            },
            "rain.wav",
        );
        assert_eq!(other.relative_dir, PathBuf::from("Other/Field Rec"));
        assert!(!other.relative_dir.starts_with("Music"));
    }

    #[test]
    fn audiobook_lastname_firstname_fields() {
        let seg = audiobook_author_segment(&ExtractedFields {
            author_lastname: Some("Weir".into()),
            author_firstname: Some("Andy".into()),
            ..Default::default()
        });
        assert_eq!(seg, "Weir, Andy");
    }

    #[test]
    fn reject_unknown_kind() {
        assert!(ImportKind::parse("imports").is_none());
        assert_eq!(ImportKind::parse("MUSIC"), Some(ImportKind::Music));
    }
}
