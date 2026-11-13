//! Browsing a connected store (BCS_04): the collection list, one release, and its cover art.
//!
//! Everything here starts from what BCS_03 already cached. The two GET entry points
//! (`release_view`, `cover`) are **cache-only gates** (SRV-06): a release id that is not
//! in the cached collection is refused without a request, so a hostile page cannot fire
//! an authenticated store call with a guessed id. An uncached release is looked up through
//! [`StoreHub::lookup_release`] (a JSON POST) instead. Two rules shape the code:
//!
//! * **Search is Plippa's own.** The store's `search3` is unreliable (it misses titles it
//!   holds), so filtering happens in memory over the cached collection, case- and
//!   accent-insensitively across title and artist ([`fold_for_search`]).
//! * **Cover art is resolved, never relayed.** The client sends a *release* id; the id is
//!   looked up in the cache and the provider's own cover id is fetched. A cover request
//!   never carries a URL from the client, so this endpoint cannot be turned into an open
//!   proxy.
//!
//! Quality is reported as what it is: the only audio this store hands over today is a lossy
//! stream ([`QUALITY_NOTE`]). Nothing here can change that, so nothing here pretends.

use std::sync::Arc;

use plippa_store::{StoreRelease, StoreReleaseDetail};
use serde::Serialize;

use super::{now_secs, StoreApiError, StoreHub};

/// Page size the portal asks for when it does not say.
pub const DEFAULT_PAGE_SIZE: u32 = 50;
/// Largest page the collection endpoint returns.
pub const MAX_PAGE_SIZE: u32 = 200;
/// Cover bodies larger than this are refused (the provider's own cap is the same order).
pub const MAX_COVER_BYTES: u64 = 8 * 1024 * 1024;
/// Largest accepted `POST /…/release` body: one id.
pub const MAX_LOOKUP_BODY: usize = 512;
/// Short private cache: covers do not change, but a corrected cover should appear today.
pub const COVER_CACHE_CONTROL: &str = "private, max-age=300";
/// The honesty note that travels with every release view.
pub const QUALITY_NOTE: &str =
    "Bandcamp serves a lossy stream; purchases in FLAC are not available through the API";
/// Covers kept in memory for repeat requests (a page of thumbnails is one request each).
pub const COVER_CACHE_ENTRIES: usize = 48;
/// Upper bound on the bytes those covers may hold.
pub const COVER_CACHE_BYTES: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Fold `input` for a case- and accent-insensitive search.
///
/// Lowercases, folds the diacritics of the Latin letters that actually appear in release
/// titles (Latin-1 Supplement and Latin Extended-A), expands the ligatures, and collapses
/// whitespace runs. Characters outside that table are kept as they are, so a collection in
/// another script still matches itself. No dependency: the table is deliberate and small.
pub fn fold_for_search(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = false;
    for ch in input.chars() {
        for lower in ch.to_lowercase() {
            if lower.is_whitespace() {
                if !out.is_empty() && !last_space {
                    out.push(' ');
                    last_space = true;
                }
                continue;
            }
            last_space = false;
            match deaccent(lower) {
                Some(base) => out.push_str(base),
                None => out.push(lower),
            }
        }
    }
    out.trim_end().to_string()
}

/// The ASCII base of a lowercased Latin letter with a diacritic (or the letter itself).
fn deaccent(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'ǐ' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ǒ' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ǔ' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

/// `true` when `release` matches an already-folded `query` (empty query matches everything).
fn matches(release: &StoreRelease, folded_query: &str) -> bool {
    if folded_query.is_empty() {
        return true;
    }
    fold_for_search(&release.title).contains(folded_query)
        || fold_for_search(&release.artist).contains(folded_query)
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// One release as the collection list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseSummary {
    /// Provider id (URL-encoded by the client when it builds a detail URL).
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u16>,
    pub track_count: Option<u32>,
    /// Total length in whole seconds when the store declares one.
    pub duration_s: Option<u64>,
    /// The release has cover art (the client may request the cover endpoint).
    pub has_cover: bool,
}

impl From<&StoreRelease> for ReleaseSummary {
    fn from(r: &StoreRelease) -> Self {
        Self {
            id: r.id.as_str().to_string(),
            title: r.title.clone(),
            artist: r.artist.clone(),
            year: r.year,
            track_count: r.track_count,
            duration_s: r.duration_ms.map(secs_from_ms),
            has_cover: r.cover.is_some(),
        }
    }
}

/// `GET /api/stores/<store>/collection` answer: the matching slice plus how old it is.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionPage {
    /// Releases matching the query (not just those on this page).
    pub total: u32,
    pub offset: u32,
    pub size: u32,
    /// The cache is a day old, incomplete, or absent.
    pub stale: bool,
    /// When the cache was filled (Unix seconds), when known.
    pub synced_at: Option<u64>,
    pub releases: Vec<ReleaseSummary>,
}

/// One track of a release, in the shape the detail view needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackView {
    pub id: String,
    pub title: String,
    /// 1-based position in its disc, as the store reports it.
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub duration_s: Option<u64>,
    /// Container the store *advertises* (not necessarily what `stream` serves).
    pub suffix: Option<String>,
    /// Declared size of the advertised file (never the size of the stream).
    pub declared_bytes: Option<u64>,
    /// Declared bitrate of the advertised file (never the stream's bitrate).
    pub declared_bitrate: Option<u32>,
}

/// What the audio actually is, in the portal's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityNote {
    /// `lossy` for every source this store serves today.
    pub audio: &'static str,
    /// Unknown until the bytes are measured, so not claimed here.
    pub bitrate_kbps: Option<u32>,
    pub note: &'static str,
}

/// `GET /api/stores/<store>/release/<id>` answer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseView {
    pub release: ReleaseSummary,
    pub tracks: Vec<TrackView>,
    pub quality: QualityNote,
    pub capabilities: plippa_store::Capabilities,
    /// The release was already in the cached collection when the view was built.
    pub cached: bool,
}

/// Cover bytes, already identified by their magic bytes.
#[derive(Debug, Clone)]
pub struct CoverImage {
    pub bytes: Arc<Vec<u8>>,
    /// One of `image/jpeg`, `image/png`, `image/webp`, `image/gif`.
    pub content_type: &'static str,
}

/// A cached cover.
#[derive(Debug, Clone)]
pub struct CachedCover {
    pub cover_id: String,
    pub image: CoverImage,
}

fn secs_from_ms(ms: u64) -> u64 {
    (ms + 500) / 1000
}

pub(crate) fn quality() -> QualityNote {
    QualityNote {
        audio: "lossy",
        bitrate_kbps: None,
        note: QUALITY_NOTE,
    }
}

/// Content type from the first bytes, for the four image types a store may hand over.
pub fn cover_content_type(bytes: &[u8]) -> Option<&'static str> {
    if let Some(kind) = crate::unite::sniff::sniff_image_kind(bytes) {
        return Some(kind.content_type());
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    None
}

// ---------------------------------------------------------------------------
// StoreHub
// ---------------------------------------------------------------------------

impl StoreHub {
    /// One page of the cached collection, filtered by `query` (title + artist).
    ///
    /// Never touches the network: browsing works while offline, on the cache the last sync
    /// wrote. An absent cache is an empty, stale page rather than an error, so the portal can
    /// say "sync me" instead of showing a failure.
    pub fn collection(&self, query: &str, offset: u32, size: u32) -> CollectionPage {
        let size = size.clamp(1, MAX_PAGE_SIZE);
        let now = now_secs();
        let folded = fold_for_search(query);
        let cache = self.cache_snapshot();
        let (stale, synced_at) = match &cache {
            Some(cache) => (
                cache.is_stale(now),
                Some(cache.crawled_at).filter(|at| *at > 0),
            ),
            None => (true, None),
        };
        let releases = cache.map(|cache| cache.releases).unwrap_or_default();
        let matching: Vec<&StoreRelease> = releases
            .iter()
            .filter(|release| matches(release, &folded))
            .collect();
        let total = matching.len() as u32;
        let page: Vec<ReleaseSummary> = matching
            .into_iter()
            .skip(offset as usize)
            .take(size as usize)
            .map(ReleaseSummary::from)
            .collect();
        CollectionPage {
            total,
            offset,
            size,
            stale,
            synced_at,
            releases: page,
        }
    }

    /// One release with its tracks, shaped for the detail view.
    ///
    /// **Cache-only entry (SRV-06).** An id that is not in the cached collection is
    /// refused with [`StoreApiError::NotCached`] without touching the transport: a GET
    /// must never make an authenticated call for a client-chosen id (a hostile page can
    /// fire it with `<img src>`). A cached id may still need one track fetch, because the
    /// cache holds collection entries, not track lists; that fetch never writes the
    /// cache and is bounded by the ad-hoc limiter.
    pub fn release_view(&self, id: &str) -> Result<ReleaseView, StoreApiError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(StoreApiError::Store(plippa_store::StoreError::NotFound));
        }
        if self.cached_release(id).is_none() {
            return Err(StoreApiError::NotCached);
        }
        self.take_adhoc()?;
        let detail = self.release_detail_readonly(id)?;
        Ok(ReleaseView {
            release: ReleaseSummary::from(&detail.release),
            tracks: detail.tracks.iter().map(track_view).collect(),
            quality: quality(),
            capabilities: self.capabilities(),
            cached: true,
        })
    }

    /// Look up one release that is not cached any more (`POST /…/release`).
    ///
    /// This is the consent-shaped counterpart of the cache-only GET: the caller asked
    /// with a JSON body, the id is validated, and the answer is written to the cache
    /// exactly as the old refresh-on-miss path did. Bounded by the ad-hoc limiter.
    pub fn lookup_release(&self, id: &str) -> Result<ReleaseView, StoreApiError> {
        let id = id.trim();
        if id.is_empty() || id.len() > 256 {
            return Err(StoreApiError::InvalidRequest(
                "a release id must be 1..=256 bytes".to_string(),
            ));
        }
        self.take_adhoc()?;
        let detail = self.get_release(id)?;
        Ok(ReleaseView {
            release: ReleaseSummary::from(&detail.release),
            tracks: detail.tracks.iter().map(track_view).collect(),
            quality: quality(),
            capabilities: self.capabilities(),
            cached: false,
        })
    }

    /// A release's tracks fetched from the store **without writing the cache**.
    ///
    /// The download prepare step must be side-effect free (`BCS_04`: "nothing is downloaded
    /// and nothing is written"), so it looks a release up the normal way but never stores it.
    pub(crate) fn release_detail_readonly(
        &self,
        id: &str,
    ) -> Result<StoreReleaseDetail, StoreApiError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(StoreApiError::Store(plippa_store::StoreError::NotFound));
        }
        let creds = self.load_creds().ok_or(StoreApiError::NotConnected)?;
        let provider = self.provider(&creds)?;
        Ok(provider.get_release(&plippa_store::StoreReleaseId::new(id))?)
    }

    /// Cover art for a release.
    ///
    /// **Cache-only entry (SRV-06).** The release id must already be in the cached
    /// collection; only then is the provider's own **cover id** resolved (never a
    /// client-supplied URL) and fetched from the CDN. An uncached id is refused without
    /// an outbound call. The device Confirm screen depends on the cached path, so it
    /// keeps working; a CDN fetch is bounded by the cover bucket. Bodies are sniffed, the
    /// size is capped, and the `Content-Type` comes from the bytes.
    pub fn cover(&self, release_id: &str) -> Result<CoverImage, StoreApiError> {
        let release_id = release_id.trim();
        if release_id.is_empty() || release_id.len() > 256 {
            return Err(StoreApiError::NotCached);
        }
        let release = self.cached_release(release_id).ok_or(StoreApiError::NotCached)?;
        let Some(cover) = release.cover else {
            return Err(StoreApiError::NoCover);
        };
        let cover_id = cover.id.as_str().to_string();
        if let Some(image) = self.cached_cover(&cover_id) {
            return Ok(image);
        }
        self.take_cover()?;
        let creds = self.load_creds().ok_or(StoreApiError::NotConnected)?;
        let provider = self.provider(&creds)?;
        let bytes = provider.cover_art(&cover.id)?;
        if bytes.len() as u64 > MAX_COVER_BYTES {
            return Err(StoreApiError::BatchTooLarge(format!(
                "the cover is larger than {} MB",
                MAX_COVER_BYTES / (1024 * 1024)
            )));
        }
        let content_type = cover_content_type(&bytes).ok_or(StoreApiError::NoCover)?;
        let image = CoverImage {
            bytes: Arc::new(bytes),
            content_type,
        };
        self.remember_cover(&cover_id, &image);
        Ok(image)
    }

    /// A cover already fetched in this process, when it is still cached.
    fn cached_cover(&self, cover_id: &str) -> Option<CoverImage> {
        let mut cache = self
            .cover_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = cache.iter().position(|c| c.cover_id == cover_id)?;
        let entry = cache.remove(index).expect("just found");
        let image = entry.image.clone();
        cache.push_front(entry); // most recently used first
        Some(image)
    }

    /// Keep a cover, dropping the least recently used ones past the caps.
    fn remember_cover(&self, cover_id: &str, image: &CoverImage) {
        let mut cache = self
            .cover_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.retain(|c| c.cover_id != cover_id);
        cache.push_front(CachedCover {
            cover_id: cover_id.to_string(),
            image: image.clone(),
        });
        let mut total: usize = cache.iter().map(|c| c.image.bytes.len()).sum();
        while cache.len() > COVER_CACHE_ENTRIES || (total > COVER_CACHE_BYTES && cache.len() > 1) {
            if let Some(dropped) = cache.pop_back() {
                total -= dropped.image.bytes.len();
            }
        }
    }
}

fn track_view(track: &plippa_store::StoreTrack) -> TrackView {
    TrackView {
        id: track.id.as_str().to_string(),
        title: track.title.clone(),
        track: track.track_number,
        disc: track.disc_number,
        duration_s: track.duration_ms.map(secs_from_ms),
        suffix: track.suffix.clone(),
        declared_bytes: track.declared_bytes,
        declared_bitrate: track.declared_bitrate_kbps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_store::{StoreCover, StoreCoverId, StoreReleaseId};

    fn release(title: &str, artist: &str) -> StoreRelease {
        StoreRelease {
            id: StoreReleaseId::new("a:1"),
            title: title.to_string(),
            artist: artist.to_string(),
            year: Some(2026),
            track_count: Some(2),
            duration_ms: Some(200_000),
            cover: Some(StoreCover {
                id: StoreCoverId::new("c:1"),
                mime: Some("image/jpeg".to_string()),
            }),
            web_url: None,
            created_unix: None,
        }
    }

    #[test]
    fn store_search_folding_is_case_and_accent_insensitive() {
        assert_eq!(fold_for_search("Björk"), "bjork");
        assert_eq!(
            fold_for_search("  BJÖRK  Guðmundsdóttir "),
            "bjork gudmundsdottir"
        );
        assert_eq!(fold_for_search("Sigur Rós"), "sigur ros");
        assert_eq!(fold_for_search("Motörhead"), "motorhead");
        assert_eq!(fold_for_search("Æther"), "aether");
        assert_eq!(fold_for_search("Straße"), "strasse");
        assert_eq!(fold_for_search("naïve"), "naive");
        assert_eq!(fold_for_search("Łódź"), "lodz");
        assert_eq!(fold_for_search("Zoe"), "zoe");
        // Other scripts are preserved (so the collection can be searched in them).
        assert_eq!(fold_for_search("Мумий Тролль"), "мумий тролль");

        let r = release("Björk — Homogenic", "Sigur Rós");
        assert!(matches(&r, &fold_for_search("BJORK")), "title, upper case");
        assert!(matches(&r, &fold_for_search("sigur ros")), "artist, accents");
        assert!(!matches(&r, &fold_for_search("aphex")));
        assert!(matches(&r, ""), "an empty query lists everything");
    }

    #[test]
    fn store_release_summary_reports_whole_seconds_and_cover_presence() {
        let s = ReleaseSummary::from(&release("T", "A"));
        assert_eq!(s.duration_s, Some(200));
        assert!(s.has_cover);
        assert_eq!(s.track_count, Some(2));
    }

    #[test]
    fn store_cover_content_type_comes_from_the_bytes() {
        assert_eq!(
            cover_content_type(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 16]),
            Some("image/jpeg")
        );
        assert_eq!(
            cover_content_type(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPVP8X");
        assert_eq!(cover_content_type(&webp), Some("image/webp"));
        assert_eq!(cover_content_type(b"GIF89a......"), Some("image/gif"));
        assert_eq!(cover_content_type(b"<html>404</html>"), None);
        assert_eq!(cover_content_type(b""), None);
    }
}
