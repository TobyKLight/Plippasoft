//! Typed subset of the Noubin metadata (`03 metadata-base.md`) used by Plippa.
//!
//! Parsing is deliberately forgiving (spec: "ignore the element, not the file"):
//! - unknown properties round-trip through each struct's flattened `extra`;
//! - a known property with the wrong type, or an unknown enum value, is *left in*
//!   `extra` (so it is written back untouched) and the typed field stays `None`;
//! - invalid array elements (bad cue, platform without URL, …) are dropped and a
//!   [`NoudataWarning`] is recorded.

use crate::timestamp::NoubinTimestamp;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use std::fmt;

/// A non-fatal problem found while parsing a `.noudata` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoudataWarning {
    /// JSON-path-ish location, e.g. `mediaList[0].cueLists[1].cues[3]`.
    pub path: String,
    pub message: String,
}

impl fmt::Display for NoudataWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

pub(crate) type Warnings = Vec<NoudataWarning>;

pub(crate) fn warn(w: &mut Warnings, path: &str, message: impl Into<String>) {
    w.push(NoudataWarning {
        path: path.to_string(),
        message: message.into(),
    });
}

// ---------------------------------------------------------------------------
// Field-taking helpers (remove from the map only when the value is usable)
// ---------------------------------------------------------------------------

fn drop_null(m: &mut Map<String, Value>, key: &str) -> bool {
    if matches!(m.get(key), Some(Value::Null)) {
        m.remove(key);
        return true;
    }
    false
}

pub(crate) fn take_string(
    m: &mut Map<String, Value>,
    key: &str,
    path: &str,
    w: &mut Warnings,
) -> Option<String> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(Value::String(_)) => match m.remove(key) {
            Some(Value::String(s)) => Some(s),
            _ => None,
        },
        Some(_) => {
            warn(w, &format!("{path}.{key}"), "expected a string; ignored");
            None
        }
    }
}

pub(crate) fn take_bool(
    m: &mut Map<String, Value>,
    key: &str,
    path: &str,
    w: &mut Warnings,
) -> Option<bool> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(Value::Bool(b)) => {
            let b = *b;
            m.remove(key);
            Some(b)
        }
        Some(_) => {
            warn(w, &format!("{path}.{key}"), "expected a boolean; ignored");
            None
        }
    }
}

/// Integer with a minimum of `min` (spec uses "integer: min 1" for 1-indexed values).
pub(crate) fn take_uint_min(
    m: &mut Map<String, Value>,
    key: &str,
    min: u64,
    path: &str,
    w: &mut Warnings,
) -> Option<u64> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(v) => match v.as_u64() {
            Some(n) if n >= min => {
                m.remove(key);
                Some(n)
            }
            _ => {
                warn(
                    w,
                    &format!("{path}.{key}"),
                    format!("expected an integer >= {min}; ignored"),
                );
                None
            }
        },
    }
}

pub(crate) fn take_timestamp(
    m: &mut Map<String, Value>,
    key: &str,
    path: &str,
    w: &mut Warnings,
) -> Option<NoubinTimestamp> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(Value::String(s)) => match NoubinTimestamp::parse(s) {
            Ok(t) => {
                m.remove(key);
                Some(t)
            }
            Err(e) => {
                warn(w, &format!("{path}.{key}"), format!("bad timestamp: {e}"));
                None
            }
        },
        Some(_) => {
            warn(
                w,
                &format!("{path}.{key}"),
                "expected a timestamp string; ignored",
            );
            None
        }
    }
}

/// Enum-like string. Unknown values are left in the map (never an error).
pub(crate) fn take_enum<T>(
    m: &mut Map<String, Value>,
    key: &str,
    path: &str,
    w: &mut Warnings,
    parse: fn(&str) -> Option<T>,
) -> Option<T> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(Value::String(s)) => {
            let parsed = parse(s)?;
            m.remove(key);
            Some(parsed)
        }
        Some(_) => {
            warn(w, &format!("{path}.{key}"), "expected a string; ignored");
            None
        }
    }
}

pub(crate) fn take_object(
    m: &mut Map<String, Value>,
    key: &str,
    path: &str,
    w: &mut Warnings,
) -> Option<Map<String, Value>> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(Value::Object(_)) => match m.remove(key) {
            Some(Value::Object(o)) => Some(o),
            _ => None,
        },
        Some(_) => {
            warn(w, &format!("{path}.{key}"), "expected an object; ignored");
            None
        }
    }
}

pub(crate) fn take_string_array(
    m: &mut Map<String, Value>,
    key: &str,
    path: &str,
    w: &mut Warnings,
) -> Option<Vec<String>> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => match m.remove(key) {
            Some(Value::Array(a)) => Some(
                a.into_iter()
                    .filter_map(|v| match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        },
        Some(_) => {
            warn(
                w,
                &format!("{path}.{key}"),
                "expected an array of strings; ignored",
            );
            None
        }
    }
}

/// Parse an array of objects, dropping invalid elements. `None` if the key is absent
/// or not an array (wrong-type arrays are left in `extra` with a warning).
pub(crate) fn take_elements<T>(
    m: &mut Map<String, Value>,
    key: &str,
    path: &str,
    w: &mut Warnings,
    mut parse: impl FnMut(Map<String, Value>, &str, &mut Warnings) -> Option<T>,
) -> Option<Vec<T>> {
    if drop_null(m, key) {
        return None;
    }
    match m.get(key) {
        None => None,
        Some(Value::Array(_)) => {
            let Some(Value::Array(items)) = m.remove(key) else {
                return None;
            };
            let mut out = Vec::new();
            for (i, item) in items.into_iter().enumerate() {
                let elem_path = format!("{path}.{key}[{i}]");
                match item {
                    Value::Object(obj) => {
                        if let Some(t) = parse(obj, &elem_path, w) {
                            out.push(t);
                        }
                    }
                    _ => warn(w, &elem_path, "expected an object; element ignored"),
                }
            }
            Some(out)
        }
        Some(_) => {
            warn(w, &format!("{path}.{key}"), "expected an array; ignored");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Enums (unknown strings fall back to the spec default via the `effective` helpers)
// ---------------------------------------------------------------------------

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($var:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($var),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$var => $s),+ }
            }
            /// `None` for values this version does not know.
            pub fn parse_known(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$var),)+ _ => None }
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    };
}

string_enum!(
    /// Root `playableItemProfile` (v0.4.0; replaces `releaseData.releaseProfile`).
    /// Missing/unknown ⇒ [`PlayableItemProfile::Other`] (see [`crate::NoudataDocument::profile`]).
    PlayableItemProfile {
        Music => "music",
        Podcast => "podcast",
        Audiobook => "audiobook",
        Mix => "mix",
        Other => "other",
    }
);

string_enum!(
    /// Root `noudataType` (v0.4.0). Missing/unknown ⇒ inferred per §4.9
    /// (see [`crate::effective_type`]); there is deliberately no `other` value.
    NoudataType {
        Release => "release",
        UserPlaylist => "userPlaylist",
        LooseTrack => "looseTrack",
        LinkTrack => "linkTrack",
    }
);

string_enum!(
    /// `platformType`. Missing/unknown ⇒ [`PlatformType::Other`].
    PlatformType {
        Store => "store",
        DrmStore => "DRMStore",
        Streaming => "streaming",
        Other => "other",
    }
);

string_enum!(
    /// `cueListType`. Missing/unknown ⇒ [`CueListType::Other`].
    CueListType {
        Nav => "nav",
        Credits => "credits",
        Text => "text",
        Other => "other",
    }
);

string_enum!(
    /// `itemLocalPlaybackData.overrideResumeBehaviour`. Missing/unknown ⇒ `Default`.
    OverrideResumeBehaviour {
        Default => "default",
        NeverResume => "neverResume",
        AlwaysResume => "alwaysResume",
    }
);

// ---------------------------------------------------------------------------
// Credits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrimaryArtist {
    #[serde(rename = "primaryArtistName")]
    pub primary_artist_name: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Contributor {
    #[serde(rename = "contributorRole", skip_serializing_if = "Option::is_none")]
    pub contributor_role: Option<String>,
    #[serde(rename = "contributorName")]
    pub contributor_name: String,
    #[serde(rename = "contributorURL", skip_serializing_if = "Option::is_none")]
    pub contributor_url: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `releaseData.credits` / `itemCredits` (typed subset; everything else preserved).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Credits {
    #[serde(rename = "primaryArtists", skip_serializing_if = "Option::is_none")]
    pub primary_artists: Option<Vec<PrimaryArtist>>,
    #[serde(
        rename = "primaryArtistOverrideName",
        skip_serializing_if = "Option::is_none"
    )]
    pub primary_artist_override_name: Option<String>,
    #[serde(
        rename = "primaryArtistFolderSafeName",
        skip_serializing_if = "Option::is_none"
    )]
    pub primary_artist_folder_safe_name: Option<String>,
    #[serde(
        rename = "primaryArtistURLSafeName",
        skip_serializing_if = "Option::is_none"
    )]
    pub primary_artist_url_safe_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributors: Option<Vec<Contributor>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Credits {
    /// Display string per spec: override name, else primary artists joined by `, `.
    pub fn primary_artist_display(&self) -> Option<String> {
        if let Some(o) = self.primary_artist_override_name.as_deref() {
            if !o.trim().is_empty() {
                return Some(o.to_string());
            }
        }
        let names: Vec<&str> = self
            .primary_artists
            .iter()
            .flatten()
            .map(|a| a.primary_artist_name.as_str())
            .filter(|n| !n.trim().is_empty())
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    }

    pub(crate) fn from_map(mut m: Map<String, Value>, path: &str, w: &mut Warnings) -> Self {
        let primary_artists = take_elements(&mut m, "primaryArtists", path, w, |mut o, p, w| {
            let Some(name) = take_string(&mut o, "primaryArtistName", p, w) else {
                warn(w, p, "missing primaryArtistName; element ignored");
                return None;
            };
            Some(PrimaryArtist {
                primary_artist_name: name,
                extra: o,
            })
        });
        let contributors = take_elements(&mut m, "contributors", path, w, |mut o, p, w| {
            let Some(name) = take_string(&mut o, "contributorName", p, w) else {
                warn(w, p, "missing contributorName; element ignored");
                return None;
            };
            Some(Contributor {
                contributor_role: take_string(&mut o, "contributorRole", p, w),
                contributor_name: name,
                contributor_url: take_string(&mut o, "contributorURL", p, w),
                extra: o,
            })
        });
        Credits {
            primary_artists,
            primary_artist_override_name: take_string(&mut m, "primaryArtistOverrideName", path, w),
            primary_artist_folder_safe_name: take_string(
                &mut m,
                "primaryArtistFolderSafeName",
                path,
                w,
            ),
            primary_artist_url_safe_name: take_string(&mut m, "primaryArtistURLSafeName", path, w),
            contributors,
            extra: m,
        }
    }
}

// ---------------------------------------------------------------------------
// Cover images
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ImageEntry {
    #[serde(rename = "imageLocalFilename", skip_serializing_if = "Option::is_none")]
    pub image_local_filename: Option<String>,
    #[serde(rename = "imageWebURL", skip_serializing_if = "Option::is_none")]
    pub image_web_url: Option<String>,
    #[serde(rename = "isImageOfNoubin", skip_serializing_if = "Option::is_none")]
    pub is_image_of_noubin: Option<bool>,
    #[serde(rename = "altText", skip_serializing_if = "Option::is_none")]
    pub alt_text: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `coverImage` / `localCoverImage` / `itemCoverImage`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct CoverImage {
    pub images: Vec<ImageEntry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CoverImage {
    /// First (primary) image's local filename, if any.
    pub fn primary_local_filename(&self) -> Option<&str> {
        self.images
            .first()
            .and_then(|i| i.image_local_filename.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// Like [`Self::from_map`], but a present-but-empty `images` array is kept as an empty
    /// list. For the user-level objects (`localCoverImage`, `itemCoverImage`) that means
    /// "show no cover" and hides the art that would otherwise apply.
    pub(crate) fn from_map_keep_empty(
        m: Map<String, Value>,
        path: &str,
        w: &mut Warnings,
    ) -> Option<Self> {
        if matches!(m.get("images"), Some(Value::Array(a)) if a.is_empty()) {
            return Some(CoverImage {
                images: Vec::new(),
                extra: m.into_iter().filter(|(k, _)| k != "images").collect(),
            });
        }
        Self::from_map(m, path, w)
    }

    /// `None` (with a warning) when `images` is missing or has no valid element —
    /// the spec says the whole object is then invalid and ignored.
    pub(crate) fn from_map(
        mut m: Map<String, Value>,
        path: &str,
        w: &mut Warnings,
    ) -> Option<Self> {
        // A present-but-empty `images` array is "no cover" (linkhosts publish it when the
        // artist uploaded no image): ignore the object without a warning.
        let empty_array = matches!(m.get("images"), Some(Value::Array(a)) if a.is_empty());
        let images = take_elements(&mut m, "images", path, w, |mut o, p, w| {
            let local = take_string(&mut o, "imageLocalFilename", p, w).filter(|s| !s.is_empty());
            let web = take_string(&mut o, "imageWebURL", p, w).filter(|s| !s.is_empty());
            if local.is_none() && web.is_none() {
                warn(
                    w,
                    p,
                    "needs imageLocalFilename or imageWebURL; element ignored",
                );
                return None;
            }
            Some(ImageEntry {
                image_local_filename: local,
                image_web_url: web,
                is_image_of_noubin: take_bool(&mut o, "isImageOfNoubin", p, w),
                alt_text: take_string(&mut o, "altText", p, w),
                extra: o,
            })
        })
        .unwrap_or_default();
        if images.is_empty() {
            if !empty_array {
                warn(w, path, "cover image has no valid images; ignored");
            }
            return None;
        }
        Some(CoverImage { images, extra: m })
    }
}

// ---------------------------------------------------------------------------
// releaseData
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReleasePlatform {
    #[serde(rename = "platformName")]
    pub platform_name: String,
    #[serde(rename = "platformType", skip_serializing_if = "Option::is_none")]
    pub platform_type: Option<PlatformType>,
    #[serde(rename = "platformURL")]
    pub platform_url: String,
    #[serde(rename = "customVerb", skip_serializing_if = "Option::is_none")]
    pub custom_verb: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ReleasePlatform {
    /// Missing/unknown type ⇒ `other`.
    pub fn effective_type(&self) -> PlatformType {
        self.platform_type.unwrap_or(PlatformType::Other)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FurtherLink {
    #[serde(rename = "linkTitle", skip_serializing_if = "Option::is_none")]
    pub link_title: Option<String>,
    #[serde(rename = "linkURL")]
    pub link_url: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Typed `releaseData`. A `release` MUST have it; `userPlaylist`/`looseTrack`/`linkTrack`
/// MUST NOT (but a foreign file that does keeps it on write, spec §3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ReleaseData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    #[serde(rename = "coverImage", skip_serializing_if = "Option::is_none")]
    pub cover_image: Option<CoverImage>,
    #[serde(rename = "releasePlatforms", skip_serializing_if = "Option::is_none")]
    pub release_platforms: Option<Vec<ReleasePlatform>>,
    #[serde(rename = "furtherLinks", skip_serializing_if = "Option::is_none")]
    pub further_links: Option<Vec<FurtherLink>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ReleaseData {
    pub(crate) fn from_map(mut m: Map<String, Value>, path: &str, w: &mut Warnings) -> Self {
        let credits = take_object(&mut m, "credits", path, w)
            .map(|o| Credits::from_map(o, &format!("{path}.credits"), w));
        let cover_image = take_object(&mut m, "coverImage", path, w)
            .and_then(|o| CoverImage::from_map(o, &format!("{path}.coverImage"), w));
        let release_platforms =
            take_elements(&mut m, "releasePlatforms", path, w, |mut o, p, w| {
                let name = take_string(&mut o, "platformName", p, w).filter(|s| !s.is_empty());
                let url = take_string(&mut o, "platformURL", p, w).filter(|s| !s.is_empty());
                let (Some(platform_name), Some(platform_url)) = (name, url) else {
                    warn(w, p, "needs platformName and platformURL; element ignored");
                    return None;
                };
                Some(ReleasePlatform {
                    platform_name,
                    platform_type: take_enum(
                        &mut o,
                        "platformType",
                        p,
                        w,
                        PlatformType::parse_known,
                    ),
                    platform_url,
                    custom_verb: take_string(&mut o, "customVerb", p, w),
                    extra: o,
                })
            });
        let further_links = take_elements(&mut m, "furtherLinks", path, w, |mut o, p, w| {
            let Some(link_url) = take_string(&mut o, "linkURL", p, w).filter(|s| !s.is_empty())
            else {
                warn(w, p, "missing linkURL; element ignored");
                return None;
            };
            Some(FurtherLink {
                link_title: take_string(&mut o, "linkTitle", p, w),
                link_url,
                extra: o,
            })
        });
        ReleaseData {
            credits,
            cover_image,
            release_platforms,
            further_links,
            extra: m,
        }
    }
}

// ---------------------------------------------------------------------------
// User notes / local playback data
// ---------------------------------------------------------------------------

/// `userNotes` / `itemUserNotes`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct UserNotes {
    #[serde(rename = "userComment", skip_serializing_if = "Option::is_none")]
    pub user_comment: Option<String>,
    /// 0..=5; absent means "not yet rated" (distinct from 0).
    #[serde(rename = "userRating", skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<u8>,
    #[serde(rename = "userTags", skip_serializing_if = "Option::is_none")]
    pub user_tags: Option<Vec<String>>,
    #[serde(rename = "createdByUser", skip_serializing_if = "Option::is_none")]
    pub created_by_user: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl UserNotes {
    pub(crate) fn from_map(mut m: Map<String, Value>, path: &str, w: &mut Warnings) -> Self {
        let user_rating = match m.get("userRating").and_then(Value::as_u64) {
            Some(n) if n <= 5 => {
                m.remove("userRating");
                Some(n as u8)
            }
            Some(_) => {
                warn(
                    w,
                    &format!("{path}.userRating"),
                    "expected integer 0..=5; ignored",
                );
                None
            }
            None => {
                if m.get("userRating").is_some_and(|v| !v.is_null()) {
                    warn(
                        w,
                        &format!("{path}.userRating"),
                        "expected integer 0..=5; ignored",
                    );
                } else {
                    m.remove("userRating");
                }
                None
            }
        };
        UserNotes {
            user_comment: take_string(&mut m, "userComment", path, w),
            user_rating,
            user_tags: take_string_array(&mut m, "userTags", path, w),
            created_by_user: take_string(&mut m, "createdByUser", path, w),
            extra: m,
        }
    }
}

/// Root-level `localPlaybackData` (+ preserved unknowns).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct LocalPlaybackData {
    /// 1-indexed (0 is invalid and ignored on read).
    #[serde(rename = "resumeAtItem", skip_serializing_if = "Option::is_none")]
    pub resume_at_item: Option<u64>,
    #[serde(rename = "searchString", skip_serializing_if = "Option::is_none")]
    pub search_string: Option<String>,
    #[serde(rename = "userNotes", skip_serializing_if = "Option::is_none")]
    pub user_notes: Option<UserNotes>,
    #[serde(rename = "localCoverImage", skip_serializing_if = "Option::is_none")]
    pub local_cover_image: Option<CoverImage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl LocalPlaybackData {
    pub(crate) fn from_map(mut m: Map<String, Value>, path: &str, w: &mut Warnings) -> Self {
        // Noubin v0.4.1: shuffle / repeatList / stopAfterEach are player settings, not
        // collection metadata. Removed from the map (not kept in `extra`) so a read-modify-write
        // does not echo them back into the file.
        for retired in ["shuffle", "repeatList", "stopAfterEach"] {
            m.remove(retired);
        }
        LocalPlaybackData {
            resume_at_item: take_uint_min(&mut m, "resumeAtItem", 1, path, w),
            search_string: take_string(&mut m, "searchString", path, w),
            user_notes: take_object(&mut m, "userNotes", path, w)
                .map(|o| UserNotes::from_map(o, &format!("{path}.userNotes"), w)),
            local_cover_image: take_object(&mut m, "localCoverImage", path, w).and_then(|o| {
                CoverImage::from_map_keep_empty(o, &format!("{path}.localCoverImage"), w)
            }),
            extra: m,
        }
    }
}

/// `mediaList[].itemLocalPlaybackData`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct ItemLocalPlaybackData {
    #[serde(rename = "repeatItem", skip_serializing_if = "Option::is_none")]
    pub repeat_item: Option<bool>,
    #[serde(rename = "startFrom", skip_serializing_if = "Option::is_none")]
    pub start_from: Option<NoubinTimestamp>,
    #[serde(rename = "resumeFromTime", skip_serializing_if = "Option::is_none")]
    pub resume_from_time: Option<NoubinTimestamp>,
    #[serde(
        rename = "overrideResumeBehaviour",
        skip_serializing_if = "Option::is_none"
    )]
    pub override_resume_behaviour: Option<OverrideResumeBehaviour>,
    /// 1-indexed alternate in `localFilePaths` the user pinned.
    #[serde(
        rename = "useSpecificAlternate",
        skip_serializing_if = "Option::is_none"
    )]
    pub use_specific_alternate: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ItemLocalPlaybackData {
    pub fn effective_override_resume(&self) -> OverrideResumeBehaviour {
        self.override_resume_behaviour
            .unwrap_or(OverrideResumeBehaviour::Default)
    }

    pub(crate) fn from_map(mut m: Map<String, Value>, path: &str, w: &mut Warnings) -> Self {
        ItemLocalPlaybackData {
            repeat_item: take_bool(&mut m, "repeatItem", path, w),
            start_from: take_timestamp(&mut m, "startFrom", path, w),
            resume_from_time: take_timestamp(&mut m, "resumeFromTime", path, w),
            override_resume_behaviour: take_enum(
                &mut m,
                "overrideResumeBehaviour",
                path,
                w,
                OverrideResumeBehaviour::parse_known,
            ),
            use_specific_alternate: take_uint_min(&mut m, "useSpecificAlternate", 1, path, w),
            extra: m,
        }
    }
}

// ---------------------------------------------------------------------------
// uniteData
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct UniteData {
    #[serde(rename = "expectedFilename", skip_serializing_if = "Option::is_none")]
    pub expected_filename: Option<String>,
    #[serde(rename = "expectedLength", skip_serializing_if = "Option::is_none")]
    pub expected_length: Option<NoubinTimestamp>,
    #[serde(rename = "freeDownloadURL", skip_serializing_if = "Option::is_none")]
    pub free_download_url: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl UniteData {
    /// `None` (with a warning) unless at least one of the three properties is valid.
    pub(crate) fn from_map(
        mut m: Map<String, Value>,
        path: &str,
        w: &mut Warnings,
    ) -> Option<Self> {
        let u = UniteData {
            expected_filename: take_string(&mut m, "expectedFilename", path, w)
                .filter(|s| !s.is_empty()),
            expected_length: take_timestamp(&mut m, "expectedLength", path, w),
            free_download_url: take_string(&mut m, "freeDownloadURL", path, w)
                .filter(|s| !s.is_empty()),
            extra: m,
        };
        if u.expected_filename.is_none()
            && u.expected_length.is_none()
            && u.free_download_url.is_none()
        {
            warn(
                w,
                path,
                "uniteData needs one of expectedFilename/expectedLength/freeDownloadURL; ignored",
            );
            return None;
        }
        Some(u)
    }
}

// ---------------------------------------------------------------------------
// cueLists
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CueTextSpan {
    pub text: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cue {
    #[serde(rename = "startTime")]
    pub start_time: NoubinTimestamp,
    #[serde(rename = "endTime", skip_serializing_if = "Option::is_none")]
    pub end_time: Option<NoubinTimestamp>,
    #[serde(rename = "cueTextSpans")]
    pub cue_text_spans: Vec<CueTextSpan>,
    #[serde(rename = "cueURL", skip_serializing_if = "Option::is_none")]
    pub cue_url: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Cue {
    /// Spans concatenated **without** spaces (base-standard display text).
    pub fn text(&self) -> String {
        self.cue_text_spans
            .iter()
            .map(|s| s.text.as_str())
            .collect()
    }

    fn from_map(mut m: Map<String, Value>, path: &str, w: &mut Warnings) -> Option<Self> {
        let Some(start_time) = take_timestamp(&mut m, "startTime", path, w) else {
            warn(w, path, "missing or invalid startTime; cue ignored");
            return None;
        };
        let end_time = take_timestamp(&mut m, "endTime", path, w);
        if m.contains_key("endTime") {
            warn(w, path, "invalid endTime; cue ignored");
            return None;
        }
        if end_time.is_some_and(|e| start_time > e) {
            warn(w, path, "startTime is after endTime; cue ignored");
            return None;
        }
        let spans = take_elements(&mut m, "cueTextSpans", path, w, |mut o, p, w| {
            let Some(text) = take_string(&mut o, "text", p, w) else {
                warn(w, p, "missing text; span ignored");
                return None;
            };
            Some(CueTextSpan { text, extra: o })
        })
        .unwrap_or_default();
        if spans.is_empty() {
            warn(w, path, "no valid cueTextSpans; cue ignored");
            return None;
        }
        Some(Cue {
            start_time,
            end_time,
            cue_text_spans: spans,
            cue_url: take_string(&mut m, "cueURL", path, w),
            extra: m,
        })
    }
}

/// One entry of `cueLists`: a time-ordered, non-overlapping list of cues.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct CueList {
    #[serde(rename = "cueListType", skip_serializing_if = "Option::is_none")]
    pub cue_list_type: Option<CueListType>,
    #[serde(rename = "cueListTitle", skip_serializing_if = "Option::is_none")]
    pub cue_list_title: Option<String>,
    #[serde(rename = "cueListLanguage", skip_serializing_if = "Option::is_none")]
    pub cue_list_language: Option<String>,
    #[serde(rename = "cueListNavigable", skip_serializing_if = "Option::is_none")]
    pub cue_list_navigable: Option<bool>,
    pub cues: Vec<Cue>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CueList {
    /// Missing/unknown type ⇒ `other`.
    pub fn list_type(&self) -> CueListType {
        self.cue_list_type.unwrap_or(CueListType::Other)
    }

    /// Explicit flag, else `true` for `nav`/`credits` and `false` otherwise.
    pub fn is_navigable(&self) -> bool {
        self.cue_list_navigable.unwrap_or(matches!(
            self.list_type(),
            CueListType::Nav | CueListType::Credits
        ))
    }

    /// Effective end of cue `index` in ms: its `endTime`, else the next cue's start,
    /// else `item_length_ms` (`None` if the item length is unknown).
    pub fn resolved_end_ms(&self, index: usize, item_length_ms: Option<u64>) -> Option<u64> {
        let cue = self.cues.get(index)?;
        if let Some(e) = cue.end_time {
            return Some(e.as_millis());
        }
        match self.cues.get(index + 1) {
            Some(next) => Some(next.start_time.as_millis()),
            None => item_length_ms,
        }
    }

    /// Index of the cue active at `position_ms` (start ≤ pos < resolved end).
    pub fn active_cue_index(&self, position_ms: u64, item_length_ms: Option<u64>) -> Option<usize> {
        (0..self.cues.len()).rev().find(|&i| {
            let c = &self.cues[i];
            position_ms >= c.start_time.as_millis()
                && self
                    .resolved_end_ms(i, item_length_ms)
                    .is_none_or(|end| position_ms < end)
        })
    }

    /// Drop cues that lie beyond the real media length (spec: invalid if
    /// `startTime`/`endTime` exceed the item length). Call once the length is known.
    pub fn drop_beyond_length(&mut self, item_length_ms: u64, path: &str) -> Vec<NoudataWarning> {
        let mut w = Vec::new();
        let mut i = 0usize;
        self.cues.retain(|c| {
            let bad = c.start_time.as_millis() > item_length_ms
                || c.end_time.is_some_and(|e| e.as_millis() > item_length_ms);
            if bad {
                warn(
                    &mut w,
                    &format!("{path}.cues[{i}]"),
                    "beyond media length; cue ignored",
                );
            }
            i += 1;
            !bad
        });
        w
    }

    /// `None` (with a warning) when no valid cue remains.
    pub(crate) fn from_map(
        mut m: Map<String, Value>,
        path: &str,
        w: &mut Warnings,
    ) -> Option<Self> {
        let raw = take_elements(&mut m, "cues", path, w, Cue::from_map).unwrap_or_default();
        // Non-overlap / ordering: each cue must start at or after the previous
        // cue's end (or start, when its end is implied by the next start).
        let mut limit = 0u64;
        let mut cues = Vec::with_capacity(raw.len());
        for (i, cue) in raw.into_iter().enumerate() {
            let start = cue.start_time.as_millis();
            if start < limit {
                warn(
                    w,
                    &format!("{path}.cues[{i}]"),
                    "overlaps or precedes the previous cue; cue ignored",
                );
                continue;
            }
            limit = cue.end_time.map_or(start, NoubinTimestamp::as_millis);
            cues.push(cue);
        }
        if cues.is_empty() {
            warn(w, path, "cue list has no valid cues; ignored");
            return None;
        }
        Some(CueList {
            cue_list_type: take_enum(&mut m, "cueListType", path, w, CueListType::parse_known),
            cue_list_title: take_string(&mut m, "cueListTitle", path, w),
            cue_list_language: take_string(&mut m, "cueListLanguage", path, w),
            cue_list_navigable: take_bool(&mut m, "cueListNavigable", path, w),
            cues,
            extra: m,
        })
    }
}
