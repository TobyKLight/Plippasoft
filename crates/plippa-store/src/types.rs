//! Value types a store provider returns.
//!
//! Everything here is store-agnostic. A provider (for example a Subsonic/Bandcamp
//! profile) maps its own wire format into these types; nothing below knows about a
//! particular platform.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{redact_url, StoreError};

/// Longest accepted store slug in bytes.
pub const MAX_STORE_ID_BYTES: usize = 32;

/// Stable slug identifying a store implementation, for example `bandcamp`.
///
/// The slug is used as a file name by [`crate::CredentialStore`], so it is
/// restricted to `[a-z0-9-]+` (no dots, no separators, no traversal).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StoreId(String);

impl StoreId {
    /// Validate and construct a store slug.
    pub fn new(slug: &str) -> Result<Self, StoreError> {
        if is_valid_slug(slug) {
            Ok(Self(slug.to_string()))
        } else {
            Err(StoreError::policy(format!(
                "store id must match [a-z0-9-]+ and be 1..={MAX_STORE_ID_BYTES} bytes"
            )))
        }
    }

    /// The slug, as used in file names and stable codes.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StoreId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `true` when `slug` is a safe store id: `[a-z0-9-]+`, bounded length.
pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_STORE_ID_BYTES
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Declares an opaque provider-owned id newtype.
///
/// These deliberately carry **no structure**. Bandcamp mixes prefixes (`a:`, `b:`,
/// `t:`) and reuses a single value for both the album and the song of a single-track
/// release, so nothing may parse an id, infer its kind, or assume album and track ids
/// differ.
macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an id exactly as the provider returned it.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The id as the provider returned it.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id! {
    /// Opaque release (album / EP / single) id.
    StoreReleaseId
}

opaque_id! {
    /// Opaque track reference accepted by
    /// [`crate::StoreProvider::fetch_track_audio`].
    ///
    /// For a single-track release this may be the same string as the release id.
    StoreTrackRef
}

opaque_id! {
    /// Opaque cover-art id accepted by [`crate::StoreProvider::cover_art`].
    StoreCoverId
}

opaque_id! {
    /// Opaque artist id accepted by [`crate::StoreProvider::get_artist`].
    ///
    /// Artists exist in the protocol only so a caller can walk an artist's releases
    /// (the ownership-matching fallback); they are not a library grouping Plippa owns.
    StoreArtistId
}

/// One page request for a listing endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
    /// Items to skip.
    pub offset: u32,
    /// Maximum items to return, clamped to `1..=`[`PageRequest::MAX_LIMIT`].
    pub limit: u32,
}

impl PageRequest {
    /// Page size used when the caller has no preference.
    pub const DEFAULT_LIMIT: u32 = 200;
    /// Largest page a provider may be asked for.
    pub const MAX_LIMIT: u32 = 500;

    /// A request with `limit` clamped into range.
    pub fn new(offset: u32, limit: u32) -> Self {
        Self {
            offset,
            limit: limit.clamp(1, Self::MAX_LIMIT),
        }
    }

    /// The first page of size `limit`.
    pub fn first(limit: u32) -> Self {
        Self::new(0, limit)
    }
}

impl Default for PageRequest {
    fn default() -> Self {
        Self::first(Self::DEFAULT_LIMIT)
    }
}

/// One page of results.
///
/// `next_offset` is the provider's own decision (a short page usually ends the
/// crawl, but providers may signal it explicitly); `None` means "no more pages".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    /// Items on this page.
    pub items: Vec<T>,
    /// Offset these items started at.
    pub offset: u32,
    /// Offset to request next, or `None` at the end of the listing.
    pub next_offset: Option<u32>,
    /// Total item count when the provider knows it.
    pub total: Option<u32>,
}

impl<T> Page<T> {
    /// Build a page.
    pub fn new(
        items: Vec<T>,
        offset: u32,
        next_offset: Option<u32>,
        total: Option<u32>,
    ) -> Self {
        Self {
            items,
            offset,
            next_offset,
            total,
        }
    }

    /// `true` when this page carries no items.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Reference to a release's cover art.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreCover {
    /// Opaque provider id.
    pub id: StoreCoverId,
    /// Content type if the provider declares one (for example `image/jpeg`).
    pub mime: Option<String>,
}

/// A release as it appears in a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreRelease {
    /// Opaque provider id.
    pub id: StoreReleaseId,
    /// Release title as the provider reports it.
    ///
    /// Always the release's own title, never a track's `album` field: on this store
    /// family that field can hold the *artist* name.
    pub title: String,
    /// Primary artist name.
    pub artist: String,
    /// Release year when known.
    pub year: Option<u16>,
    /// Number of tracks when known.
    pub track_count: Option<u32>,
    /// Total duration in milliseconds when known.
    pub duration_ms: Option<u64>,
    /// Cover art reference when the release has one.
    pub cover: Option<StoreCover>,
    /// A human-openable page for this release, when the store publishes one.
    ///
    /// The Subsonic API exposes **no** URL for anything (`album_url_fields` was empty
    /// on every response measured), so [`crate::subsonic::SubsonicProvider`] always
    /// leaves this `None`. Displaying it is optional; never synthesise one.
    #[serde(default)]
    pub web_url: Option<String>,
    /// Release creation time as Unix seconds when the provider reports one.
    ///
    /// This store family reports RFC 1123 (`20 Aug 2026 06:48:08 GMT`), not ISO
    /// 8601. An unparsable value is [`None`], never an error: the date is decoration.
    #[serde(default)]
    pub created_unix: Option<i64>,
}

/// A track inside a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreTrack {
    /// Opaque provider reference, passed back to
    /// [`crate::StoreProvider::fetch_track_audio`].
    pub id: StoreTrackRef,
    /// Track title as the provider reports it.
    pub title: String,
    /// 1-based position within its disc.
    pub track_number: Option<u32>,
    /// 1-based disc number.
    pub disc_number: Option<u32>,
    /// Track duration in milliseconds when known.
    pub duration_ms: Option<u64>,
    /// Container as the provider names it (`mp3`, `flac`, …), when declared.
    ///
    /// This describes the file the store *advertises*, which on this store family is
    /// not the encode the `stream` endpoint actually serves.
    #[serde(default)]
    pub suffix: Option<String>,
    /// MIME type as the provider declares it (`audio/mpeg`, …).
    #[serde(default)]
    pub content_type: Option<String>,
    /// Declared size of the advertised file in bytes.
    ///
    /// Do not treat this as the size of a stream: the stream is a different encode
    /// and measured 0.89 % smaller than the declared file.
    #[serde(default)]
    pub declared_bytes: Option<u64>,
    /// Declared bitrate of the advertised file in kbps.
    ///
    /// Never report this as the quality of the stream.
    #[serde(default)]
    pub declared_bitrate_kbps: Option<u32>,
}

/// An artist and its releases.
///
/// Kept minimal on purpose: it exists for the ownership-matching fallback (walk the
/// artist's albums and compare titles), not as a library grouping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreArtist {
    /// Opaque provider id.
    pub id: StoreArtistId,
    /// Artist name as the provider reports it.
    pub name: String,
    /// Number of albums the provider says the artist has.
    pub album_count: Option<u32>,
    /// Cover art reference for the artist, when the provider has one.
    pub cover: Option<StoreCover>,
}

/// One artist plus the releases the provider lists for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreArtistDetail {
    /// The artist itself.
    pub artist: StoreArtist,
    /// Releases in provider order.
    pub releases: Vec<StoreRelease>,
}

/// One hit from a provider's free-text search.
///
/// Search on this store family is unreliable — it returned zero hits for a title it
/// definitely holds — so a hit is a *lead*, never proof of ownership. The release id
/// is carried so a caller can confirm with [`crate::StoreProvider::get_release`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreSearchHit {
    /// Release the track belongs to, when the provider says.
    pub release_id: Option<StoreReleaseId>,
    /// Artist name as the provider reports it.
    pub artist: Option<String>,
    /// The matched track.
    pub track: StoreTrack,
}

/// A release plus its track list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreReleaseDetail {
    /// The release itself.
    pub release: StoreRelease,
    /// Tracks in provider order.
    pub tracks: Vec<StoreTrack>,
}

/// Which kind of audio the caller is willing to accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AudioPreference {
    /// Only an original file is acceptable; a stream is not.
    PreferOriginal,
    /// Prefer an original, accept a stream.
    #[default]
    OriginalOrStream,
    /// Only a stream is acceptable (never request an original).
    StreamOnly,
}

/// Container of the audio the provider can point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat {
    /// MPEG-1/2 layer 3.
    Mp3,
    /// FLAC.
    Flac,
    /// RIFF WAVE.
    Wav,
    /// AAC in an MP4 container.
    Aac,
    /// Ogg Opus.
    Opus,
    /// Ogg Vorbis.
    Ogg,
    /// Anything not listed above.
    Unknown,
}

impl AudioFormat {
    /// Conventional file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Aac => "m4a",
            Self::Opus => "opus",
            Self::Ogg => "ogg",
            Self::Unknown => "bin",
        }
    }
}

/// Quality of a delivered audio source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioQuality {
    /// Bit-perfect relative to the store's master.
    Lossless,
    /// Lossy encode; bitrate when the provider declares one.
    Lossy {
        /// Kilobits per second.
        bitrate_kbps: Option<u32>,
    },
}

/// A URL that is a reusable credential (PAR-07).
///
/// The Subsonic token is derived from the password, and the server has no salt
/// history, so a captured URL works for the whole API and its `t=` allows offline
/// guessing of the password. Consequently this type:
///
/// * has **no** `Serialize` (a persisted download job or status JSON cannot contain
///   it) and **no** `Debug` (a derived or accidental print cannot include it);
/// * redacts on `Display`, so an error message can name the call, not the secret;
/// * is only reachable through [`Self::expose`], which exists for the one transport
///   call that must send it.
///
/// # Not serialisable
///
/// ```compile_fail
/// fn needs_serialise<T: serde::Serialize>(_: &T) {}
/// let url = plippa_store::SourceUrl::new("https://bandcamp.com/rest/stream.view?t=SECRET");
/// needs_serialise(&url); // PAR-07: there is no `Serialize` impl to reach
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct SourceUrl(String);

impl SourceUrl {
    /// Wrap a URL the provider just built.
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    /// The URL itself. **Only** a transport call may use this; never log or persist
    /// the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&redact_url(&self.0))
    }
}

/// Where the caller can download a track from.
///
/// The provider does **not** download the audio: it returns a URL and the caller
/// fetches it through its own hardened downloader (consent, SSRF vetting, byte cap,
/// resume). The URL is a bearer capability — it may carry signed query parameters —
/// so it is a [`SourceUrl`]: never printed, never serialised, reachable only at the
/// transport call.
///
/// # Not serialisable
///
/// ```compile_fail
/// fn needs_serialise<T: serde::Serialize>(_: &T) {}
/// let source = plippa_store::AudioSource {
///     url: plippa_store::SourceUrl::new("https://bandcamp.com/rest/stream.view?t=SECRET"),
///     expected_bytes: None,
///     format: plippa_store::AudioFormat::Mp3,
///     quality: plippa_store::AudioQuality::Lossless,
///     stream_fallback: false,
/// };
/// needs_serialise(&source); // PAR-07: a status or job payload cannot contain this
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct AudioSource {
    /// Absolute URL to GET. Treat as a secret.
    pub url: SourceUrl,
    /// Size the provider expects, when it knows one.
    pub expected_bytes: Option<u64>,
    /// Container of the delivered bytes.
    pub format: AudioFormat,
    /// Lossless or lossy, with bitrate if declared.
    pub quality: AudioQuality,
    /// The caller asked for an original and this is the store's stream instead.
    ///
    /// A product decision, not a footnote: the only audio this store will hand over
    /// today is a lossy, untagged transcode, so the caller must be able to say
    /// "substituted" rather than pretending the purchase was delivered. Set by a
    /// provider that fell back from `download` to `stream` (or when it could not
    /// verify `download`); `false` when the requested kind was returned.
    pub stream_fallback: bool,
}

impl fmt::Debug for AudioSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioSource")
            .field("url", &format_args!("{}", self.url))
            .field("expected_bytes", &self.expected_bytes)
            .field("format", &self.format)
            .field("quality", &self.quality)
            .field("stream_fallback", &self.stream_fallback)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_ids_accept_slugs_and_reject_path_like_input() {
        assert_eq!(StoreId::new("bandcamp").unwrap().as_str(), "bandcamp");
        assert!(StoreId::new("mock-store-2").is_ok());
        for bad in [
            "",
            "Bandcamp",
            "../bandcamp",
            "bandcamp/../x",
            "bandcamp.json",
            "band camp",
            "bandcamp\n",
            &"a".repeat(MAX_STORE_ID_BYTES + 1),
        ] {
            assert!(StoreId::new(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn page_request_clamps_the_limit() {
        assert_eq!(PageRequest::new(10, 0).limit, 1);
        assert_eq!(PageRequest::new(10, u32::MAX).limit, PageRequest::MAX_LIMIT);
        assert_eq!(PageRequest::default().offset, 0);
        assert_eq!(PageRequest::default().limit, PageRequest::DEFAULT_LIMIT);
    }

    #[test]
    fn audio_source_debug_never_shows_the_query() {
        let source = AudioSource {
            url: SourceUrl::new("https://t4.bcbits.com/stream/1?token=SECRETTOKEN&ts=99"),
            expected_bytes: Some(12),
            format: AudioFormat::Mp3,
            quality: AudioQuality::Lossy {
                bitrate_kbps: Some(254),
            },
            stream_fallback: false,
        };
        let shown = format!("{source:?}");
        assert!(!shown.contains("SECRETTOKEN"), "{shown}");
        assert!(!shown.contains("ts=99"), "{shown}");
        assert!(shown.contains("t4.bcbits.com"), "{shown}");
    }

    #[test]
    fn a_source_url_redacts_on_display() {
        let url = SourceUrl::new("https://bandcamp.com/rest/stream.view?u=fan&s=abc&t=SECRET&id=t:1");
        let shown = format!("{url}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(!shown.contains("u=fan"), "{shown}");
        assert!(shown.contains("bandcamp.com"), "{shown}");
        assert_eq!(
            url.expose(),
            "https://bandcamp.com/rest/stream.view?u=fan&s=abc&t=SECRET&id=t:1"
        );
    }

    #[test]
    fn opaque_ids_round_trip_through_json() {
        let id = StoreTrackRef::new("t:2970735116");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"t:2970735116\"");
        assert_eq!(serde_json::from_str::<StoreTrackRef>(&json).unwrap(), id);
        assert_eq!(id.as_str(), "t:2970735116");
    }
}
