//! Bandcamp's Subsonic profile.
//!
//! Everything measured about this API is a quirk of a server that lies with its status
//! codes, and each one is handled here rather than in a caller:
//!
//! - the API root is `/api/subsonic/rest/<endpoint>.view` — the `/view/<endpoint>`
//!   form that older client reports used answers "bad version" for *every* endpoint;
//! - authentication is the Subsonic token pair `s` (salt) + `t = md5(password + salt)`;
//!   the plain-password `p=` parameter is rejected by the store and never sent;
//! - bad credentials are an **empty HTTP 500**, not 401;
//! - `ping` answers `ok` with wrong credentials and with none, so
//!   [`StoreProvider::verify_credentials`] calls `getArtists`;
//! - unimplemented endpoints answer **HTTP 200** with
//!   `{"error":true,"error_message":"bad version"}`;
//! - a request without a User-Agent gets a Fastly "Client Challenge" HTML page at
//!   HTTP 200;
//! - `download` is a route stub, so [`StoreProvider::fetch_track_audio`] falls back to
//!   the lossy `stream` and says so;
//! - `getAlbumList2` pages at 200 items and ends with a short page.
//!
//! The provider is a pure request/parse layer: retries, timeouts, TLS, consent and
//! SSRF vetting belong to the [`Transport`] the caller supplies. It never logs, and
//! never puts a credential-bearing URL inside an error.
//!
//! ```
//! use std::sync::Arc;
//! use plippa_store::{Credentials, SubsonicProvider, Transport, HeaderMap, TransportResponse, StoreError};
//!
//! struct Offline;
//! impl Transport for Offline {
//!     fn get(&self, _url: &str, _headers: &HeaderMap, _max_bytes: u64) -> Result<TransportResponse, StoreError> {
//!         Err(StoreError::transport("no network in this example"))
//!     }
//! }
//!
//! let provider = SubsonicProvider::new(
//!     Arc::new(Offline),
//!     "https://bandcamp.com/api/subsonic",
//!     Credentials::new("fan", "generated-password"),
//!     "plippa",
//! )
//! .expect("the Bandcamp base URL is on the default allowlist");
//! assert_eq!(provider.base_url(), "https://bandcamp.com/api/subsonic");
//! ```

mod mapping;
mod response;
mod time;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::allowlist::AuthenticatedHosts;
use crate::credentials::{Credentials, SecretString};
use crate::error::{redact_url, StoreError};
use crate::md5::subsonic_token;
use crate::provider::{Capabilities, StoreProvider};
use crate::transport::{HeaderMap, Transport, TransportResponse};
use crate::types::{
    AudioFormat, AudioPreference, AudioQuality, AudioSource, Page, PageRequest, SourceUrl,
    StoreArtist, StoreArtistDetail, StoreArtistId, StoreCoverId, StoreId, StoreRelease,
    StoreReleaseDetail, StoreReleaseId, StoreSearchHit, StoreTrackRef,
};

pub use response::{parse_response, SubsonicPayload};
pub use time::parse_rfc1123_unix;

/// Protocol version sent as `v` unless the caller configures another.
pub const DEFAULT_API_VERSION: &str = "1.16.1";

/// Client name sent as `c` unless the caller configures another.
///
/// The store does not validate `c` at all (measured with an empty, an omitted and a
/// random name), so this is a courtesy, never a gate.
pub const DEFAULT_CLIENT_NAME: &str = "plippa";

/// Default cap for a cover-art body.
pub const DEFAULT_COVER_BYTE_CAP: u64 = 8 * 1024 * 1024;

/// Byte cap for a JSON metadata reply.
pub const JSON_BYTE_CAP: u64 = 4 * 1024 * 1024;

/// Bytes requested when probing `download`.
///
/// The store's answer today is a 44-byte JSON error, so nothing is transferred. A
/// server that does serve audio will start streaming and hit this cap, which the
/// transport reports as a [`StoreError::Transport`]; that is "could not confirm",
/// never "works".
pub const DOWNLOAD_PROBE_BYTES: u64 = 64 * 1024;

/// Hard ceiling on one crawl of the collection.
pub const MAX_CRAWL_RELEASES: u32 = 20_000;

/// Page size a crawl uses unless told otherwise (the store's own page size).
pub const DEFAULT_CRAWL_PAGE_SIZE: u32 = 200;

/// Fixed minimal User-Agent. Without one the store answers with an HTML challenge.
const USER_AGENT: &str = concat!("Plippa/", env!("CARGO_PKG_VERSION"));

const ENDPOINT_GET_ARTISTS: &str = "getArtists";
const ENDPOINT_GET_ARTIST: &str = "getArtist";
const ENDPOINT_GET_ALBUM_LIST_2: &str = "getAlbumList2";
const ENDPOINT_GET_ALBUM: &str = "getAlbum";
const ENDPOINT_GET_COVER_ART: &str = "getCoverArt";
const ENDPOINT_SEARCH_3: &str = "search3";
const ENDPOINT_PING: &str = "ping";
const ENDPOINT_STREAM: &str = "stream";
const ENDPOINT_DOWNLOAD: &str = "download";

/// What the server says about itself. Decorations only; nothing is gated on them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsonicServerInfo {
    /// Server implementation name (`BandcampServer`).
    pub server_type: Option<String>,
    /// Protocol version the server speaks (`1.16.1`).
    pub version: Option<String>,
    /// The server's own version string.
    pub server_version: Option<String>,
    /// Whether the server advertises the OpenSubsonic extensions.
    pub open_subsonic: bool,
}

/// How a crawl stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrawlStop {
    /// The collection ended with a short (or empty) page.
    EndOfCollection,
    /// The caller's progress callback asked to stop.
    Cancelled,
    /// `CrawlOptions::max_pages` was reached; there is more to fetch.
    PageCap,
    /// `CrawlOptions::max_releases` was reached; there is more to fetch.
    ReleaseCap,
}

/// One fetched page, as reported to a progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlPage {
    /// Offset this page started at.
    pub offset: u32,
    /// Items the page carried.
    pub items: u32,
}

/// Progress passed to the crawl callback after every page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlProgress {
    /// Pages fetched so far.
    pub pages_done: u32,
    /// Offset of the page just fetched.
    pub offset: u32,
    /// Items on the page just fetched.
    pub page_items: u32,
    /// Releases collected so far.
    pub releases_so_far: u32,
}

/// What the crawl callback wants next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrawlDecision {
    /// Fetch the next page.
    Continue,
    /// Stop here; the caller is cancelling.
    Stop,
}

/// Bounds on one crawl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlOptions {
    /// Items requested per page, clamped to `1..=`[`PageRequest::MAX_LIMIT`].
    pub page_size: u32,
    /// Stop after this many pages when set.
    pub max_pages: Option<u32>,
    /// Stop after this many releases (bounds a hostile or broken server).
    pub max_releases: u32,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_CRAWL_PAGE_SIZE,
            max_pages: None,
            max_releases: MAX_CRAWL_RELEASES,
        }
    }
}

/// The result of a crawl: what was collected, and whether it is the whole collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionCrawl {
    /// Releases in provider order.
    pub releases: Vec<StoreRelease>,
    /// One entry per fetched page, for progress reporting.
    pub pages: Vec<CrawlPage>,
    /// Why the crawl stopped.
    pub stop: CrawlStop,
}

impl CollectionCrawl {
    /// `true` only when the whole collection was read.
    pub fn is_complete(&self) -> bool {
        self.stop == CrawlStop::EndOfCollection
    }

    /// Number of releases collected.
    pub fn len(&self) -> usize {
        self.releases.len()
    }

    /// `true` when nothing was collected.
    pub fn is_empty(&self) -> bool {
        self.releases.is_empty()
    }
}

/// Whether `download` has been shown to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadState {
    /// Never tried. `capabilities().download` is false and marked unverified.
    Unknown,
    /// Tried and the server answered "unimplemented".
    Unavailable,
    /// Tried and audio came back; the container and size it reported are kept.
    Available {
        format: AudioFormat,
        expected_bytes: Option<u64>,
    },
}

/// A provider for a Subsonic server using Bandcamp's measured profile.
///
/// Only `https://…bandcamp.com` is accepted (the default
/// [`AuthenticatedHosts`] allowlist), because that is the profile this module
/// implements: another Subsonic server needs its own allowlist entry and its own
/// quirk review.
pub struct SubsonicProvider {
    transport: Arc<dyn Transport + Send + Sync>,
    base_url: String,
    api_version: String,
    client_name: String,
    host: AuthenticatedHosts,
    store_id: StoreId,
    secret: Secret,
    cover_byte_cap: u64,
    download: Mutex<DownloadState>,
    salt_counter: AtomicU64,
}

/// The login, kept out of `Debug` and out of every error.
struct Secret {
    user: String,
    /// Wiped when the provider is dropped.
    password: SecretString,
}

impl SubsonicProvider {
    /// Build a provider for `base_url` (for example
    /// `https://bandcamp.com/api/subsonic`).
    ///
    /// The base URL must be https, on the default [`AuthenticatedHosts`] allowlist,
    /// on port 443, without userinfo, query or fragment, and without a trailing
    /// `/rest` (the provider adds it). Credentials are trimmed here, because the
    /// store's copy button has been seen prepending whitespace.
    pub fn new(
        transport: Arc<dyn Transport + Send + Sync>,
        base_url: &str,
        credentials: Credentials,
        client_name: &str,
    ) -> Result<Self, StoreError> {
        let host = AuthenticatedHosts::default();
        let base_url = normalise_base_url(base_url, &host)?;
        let credentials = credentials.normalised()?;
        let client_name = client_name.trim();
        let client_name = if client_name.is_empty() {
            DEFAULT_CLIENT_NAME.to_string()
        } else {
            client_name.to_string()
        };
        Ok(Self {
            transport,
            base_url,
            api_version: DEFAULT_API_VERSION.to_string(),
            client_name,
            host,
            store_id: StoreId::new("bandcamp")
                .map_err(|_| StoreError::policy("bandcamp is not a valid store id"))?,
            secret: Secret {
                user: credentials.username,
                password: credentials.password,
            },
            cover_byte_cap: DEFAULT_COVER_BYTE_CAP,
            download: Mutex::new(DownloadState::Unknown),
            salt_counter: AtomicU64::new(0),
        })
    }

    /// Override the protocol version sent as `v`.
    pub fn with_api_version(mut self, version: &str) -> Result<Self, StoreError> {
        let version = version.trim();
        let valid = !version.is_empty()
            && version.len() <= 16
            && version.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.');
        if !valid {
            return Err(StoreError::policy("invalid Subsonic API version"));
        }
        self.api_version = version.to_string();
        Ok(self)
    }

    /// Override the byte cap for a cover-art body.
    pub fn with_cover_byte_cap(mut self, max_bytes: u64) -> Self {
        self.cover_byte_cap = max_bytes.max(1);
        self
    }

    /// The validated base URL (no query, so it is safe to log).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The protocol version sent as `v`.
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// The client name sent as `c`.
    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    /// The allowlist every request and redirect is checked against.
    pub fn allowlist(&self) -> &AuthenticatedHosts {
        &self.host
    }

    // ---------------------------------------------------------------- requests

    /// Build the URL for one endpoint.
    ///
    /// **This is the only place a credential-bearing URL is built.** The result carries
    /// the user name in `u`, the salt in `s` and the token in `t`. Treat it as a
    /// **reusable credential for the whole Subsonic API**, not as a per-request value:
    /// the server keeps no salt history, so replaying the `s`/`t` pair keeps working,
    /// and `t = md5(password + s)` allows offline guessing of the password. It therefore
    /// goes to the transport and nowhere else: never to a log, an error, a JSON body or
    /// a `Debug` string. `redact_url` is what a caller may print.
    pub fn request_url(
        &self,
        endpoint: &str,
        extra: &[(&str, &str)],
    ) -> Result<String, StoreError> {
        if !valid_endpoint_name(endpoint) {
            return Err(StoreError::policy(format!(
                "refusing an unsafe endpoint name: {endpoint:?}"
            )));
        }

        // A fresh salt per request: a repeated salt lets a captured token be replayed.
        // The password+salt buffer is wiped by `concat`/`subsonic_token`'s drop.
        let salt = self.fresh_salt();
        let token = subsonic_token(self.secret.password.expose(), &salt);

        let mut params: Vec<(String, String)> = vec![
            ("u".to_string(), self.secret.user.clone()),
            ("v".to_string(), self.api_version.clone()),
            ("c".to_string(), self.client_name.clone()),
            ("f".to_string(), "json".to_string()),
            ("s".to_string(), salt),
            ("t".to_string(), token),
        ];
        for (key, value) in extra {
            params.push(((*key).to_string(), (*value).to_string()));
        }

        let mut url = format!("{}/rest/{endpoint}.view?", self.base_url);
        for (index, (key, value)) in params.iter().enumerate() {
            if index > 0 {
                url.push('&');
            }
            url.push_str(&percent_encode(key));
            url.push('=');
            url.push_str(&percent_encode(value));
        }
        Ok(url)
    }

    /// A 16-hex-character salt, different for every call.
    ///
    /// No `rand` dependency: the standard library seeds a [`RandomState`] from the OS
    /// and advances it per instance, and mixing in a counter, the process id and the
    /// clock makes a repeat within a session impossible.
    ///
    /// [`RandomState`]: std::collections::hash_map::RandomState
    fn fresh_salt(&self) -> String {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        let counter = self.salt_counter.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        format!("{:016x}", hasher.finish())
    }

    fn json_headers(&self) -> HeaderMap {
        vec![
            ("User-Agent".to_string(), USER_AGENT.to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ]
    }

    fn binary_headers(&self, accept: &str) -> HeaderMap {
        vec![
            ("User-Agent".to_string(), USER_AGENT.to_string()),
            ("Accept".to_string(), accept.to_string()),
        ]
    }

    /// One metadata call, validated by its envelope.
    fn json_call(
        &self,
        endpoint: &str,
        extra: &[(&str, &str)],
    ) -> Result<SubsonicPayload, StoreError> {
        let url = self.request_url(endpoint, extra)?;
        let response = self
            .transport
            .get(&url, &self.json_headers(), JSON_BYTE_CAP)?;
        self.check_response_target(&response)?;
        parse_response(&response.body, response.status, endpoint)
    }

    /// Refuse a reply that came from anywhere the allowlist does not cover.
    ///
    /// The transport must already enforce this on every hop; checking again is cheap
    /// and keeps the guarantee true even for a transport that only checks the first
    /// URL. Only the redacted URL can reach the error.
    fn check_response_target(&self, response: &TransportResponse) -> Result<(), StoreError> {
        for url in std::iter::once(&response.final_url).chain(response.redirects.iter()) {
            if url.is_empty() {
                continue;
            }
            self.host.check_redirect(url)?;
        }
        Ok(())
    }

    fn download_state(&self) -> DownloadState {
        *self
            .download
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_download_state(&self, state: DownloadState) {
        *self
            .download
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
    }

    // -------------------------------------------------------------- metadata

    /// The server's self-description.
    ///
    /// Not a credential check: `ping` answers `ok` with a wrong password and with no
    /// credentials at all, so never gate sign-in on it.
    pub fn ping(&self) -> Result<SubsonicServerInfo, StoreError> {
        let payload = self.json_call(ENDPOINT_PING, &[])?;
        if payload.is_xml() {
            // Only `ping` answers XML, and it carries nothing Plippa needs.
            return Ok(SubsonicServerInfo::default());
        }
        Ok(SubsonicServerInfo {
            server_type: payload.string("type").map(str::to_string),
            version: payload.string("version").map(str::to_string),
            server_version: payload.string("serverVersion").map(str::to_string),
            open_subsonic: payload.boolean("openSubsonic").unwrap_or(false),
        })
    }

    /// One page of the collection.
    ///
    /// `getAlbumList2` is asked for `alphabeticalByName`; a page shorter than the
    /// requested size is the end of the collection (the store pages at exactly 200
    /// and has no `lastOffset`), signalled by `next_offset: None`.
    pub fn list_releases(&self, page: PageRequest) -> Result<Page<StoreRelease>, StoreError> {
        let limit = if page.limit == 0 {
            PageRequest::DEFAULT_LIMIT
        } else {
            page.limit.min(PageRequest::MAX_LIMIT)
        };
        let size = limit.to_string();
        let offset = page.offset.to_string();
        let payload = self.json_call(
            ENDPOINT_GET_ALBUM_LIST_2,
            &[
                ("type", "alphabeticalByName"),
                ("size", &size),
                ("offset", &offset),
            ],
        )?;

        // A server with no purchases may omit `albumList2`; that is an empty page, not
        // an error. A present but non-object value is a malformed reply.
        let items = match payload.get("albumList2") {
            None => Vec::new(),
            Some(value) if value.is_null() => Vec::new(),
            Some(value) if value.is_object() => mapping::as_list(value.get("album")),
            Some(_) => {
                return Err(StoreError::Malformed {
                    detail: "albumList2 was not an object".to_string(),
                })
            }
        };

        let mut releases = Vec::with_capacity(items.len());
        for album in items {
            releases.push(mapping::release(album)?);
        }

        let returned = releases.len() as u32;
        let next_offset = if returned < limit {
            None
        } else {
            Some(page.offset.saturating_add(returned))
        };
        Ok(Page::new(releases, page.offset, next_offset, None))
    }

    /// Walk the whole collection, one page at a time.
    ///
    /// Equals [`Self::crawl_releases`] with [`CrawlOptions::default`] and a callback
    /// that never cancels.
    pub fn crawl_all_releases(&self) -> Result<CollectionCrawl, StoreError> {
        self.crawl_releases(CrawlOptions::default(), |_| CrawlDecision::Continue)
    }

    /// Walk the collection, reporting each page and honouring a cancel request.
    ///
    /// `max_pages` and `max_releases` bound a hostile or broken server; the defaults
    /// stop at [`MAX_CRAWL_RELEASES`]. The result says why it stopped, so a caller can
    /// tell "that is everything" from "there is more" without guessing.
    pub fn crawl_releases<F>(
        &self,
        options: CrawlOptions,
        mut on_page: F,
    ) -> Result<CollectionCrawl, StoreError>
    where
        F: FnMut(&CrawlProgress) -> CrawlDecision,
    {
        let page_size = options.page_size.clamp(1, PageRequest::MAX_LIMIT);
        let max_releases = options.max_releases.max(1) as usize;

        let mut releases: Vec<StoreRelease> = Vec::new();
        let mut pages: Vec<CrawlPage> = Vec::new();
        let mut offset: u32 = 0;
        let stop;

        loop {
            if let Some(max_pages) = options.max_pages {
                if pages.len() >= max_pages as usize {
                    stop = CrawlStop::PageCap;
                    break;
                }
            }

            let page = self.list_releases(PageRequest::new(offset, page_size))?;
            let count = page.items.len() as u32;
            pages.push(CrawlPage {
                offset,
                items: count,
            });
            releases.extend(page.items);

            let progress = CrawlProgress {
                pages_done: pages.len() as u32,
                offset,
                page_items: count,
                releases_so_far: releases.len() as u32,
            };
            if on_page(&progress) == CrawlDecision::Stop {
                stop = CrawlStop::Cancelled;
                break;
            }

            // A short page — including an empty one — is the end of the collection.
            if count < page_size {
                if releases.len() > max_releases {
                    releases.truncate(max_releases);
                    stop = CrawlStop::ReleaseCap;
                } else {
                    stop = CrawlStop::EndOfCollection;
                }
                break;
            }
            if releases.len() >= max_releases {
                releases.truncate(max_releases);
                stop = CrawlStop::ReleaseCap;
                break;
            }

            // A full page, so the next one starts after it. `offset` always advances
            // here because `count == page_size >= 1`.
            offset = page
                .next_offset
                .unwrap_or_else(|| offset.saturating_add(count));
        }

        Ok(CollectionCrawl {
            releases,
            pages,
            stop,
        })
    }

    /// One release and its tracks.
    pub fn get_release(
        &self,
        id: &StoreReleaseId,
    ) -> Result<StoreReleaseDetail, StoreError> {
        let payload = self.json_call(ENDPOINT_GET_ALBUM, &[("id", id.as_str())])?;
        let album = payload.get("album").ok_or_else(|| StoreError::Malformed {
            detail: "getAlbum returned no album object".to_string(),
        })?;
        let mut release = mapping::release(album)?;

        let mut tracks = Vec::new();
        for song in mapping::as_list(album.get("song")) {
            tracks.push(mapping::track(song)?);
        }

        if release.track_count.is_none() {
            release.track_count = Some(tracks.len() as u32);
        }
        if release.duration_ms.is_none() {
            let total: u64 = tracks
                .iter()
                .filter_map(|track| track.duration_ms)
                .sum();
            if total > 0 {
                release.duration_ms = Some(total);
            }
        }

        Ok(StoreReleaseDetail { release, tracks })
    }

    /// Every artist in the collection (the ownership-matching fallback entry point).
    pub fn list_artists(&self) -> Result<Vec<StoreArtist>, StoreError> {
        let payload = self.json_call(ENDPOINT_GET_ARTISTS, &[])?;
        if payload.is_xml() {
            return Err(StoreError::Malformed {
                detail: "getArtists answered XML although JSON was requested".to_string(),
            });
        }
        let artists = payload.get("artists").ok_or_else(|| StoreError::Malformed {
            detail: "getArtists returned no artists object".to_string(),
        })?;
        let mut out = Vec::new();
        for index in mapping::as_list(artists.get("index")) {
            for artist in mapping::as_list(index.get("artist")) {
                out.push(mapping::artist(artist)?);
            }
        }
        Ok(out)
    }

    /// One artist and the releases the server lists for it.
    pub fn get_artist(&self, id: &StoreArtistId) -> Result<StoreArtistDetail, StoreError> {
        let payload = self.json_call(ENDPOINT_GET_ARTIST, &[("id", id.as_str())])?;
        if payload.is_xml() {
            return Err(StoreError::Malformed {
                detail: "getArtist answered XML although JSON was requested".to_string(),
            });
        }
        let artist = payload.get("artist").ok_or_else(|| StoreError::Malformed {
            detail: "getArtist returned no artist object".to_string(),
        })?;
        let artist = mapping::artist(artist)?;
        let mut releases = Vec::new();
        for album in mapping::as_list(
            payload
                .get("artist")
                .and_then(|value| value.get("album")),
        ) {
            releases.push(mapping::release(album)?);
        }
        Ok(StoreArtistDetail { artist, releases })
    }

    /// Best-effort track search.
    ///
    /// Measured unreliable: a real title returned zero hits. An empty result, or a
    /// server that does not implement `search3`, is an empty list — never an error
    /// that a caller could mistake for "not owned".
    pub fn search_tracks(&self, query: &str) -> Result<Vec<StoreSearchHit>, StoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let payload = match self.json_call(
            ENDPOINT_SEARCH_3,
            &[
                ("query", query),
                ("songCount", "50"),
                ("albumCount", "0"),
                ("artistCount", "0"),
            ],
        ) {
            Ok(payload) => payload,
            Err(StoreError::Unimplemented { .. }) | Err(StoreError::NotFound) => {
                return Ok(Vec::new())
            }
            Err(error) => return Err(error),
        };
        if payload.is_xml() {
            return Ok(Vec::new());
        }

        let mut hits = Vec::new();
        let result = payload.get("searchResult3");
        for song in mapping::as_list(result.and_then(|value| value.get("song"))) {
            hits.push(mapping::search_hit(song)?);
        }
        Ok(hits)
    }

    // ----------------------------------------------------------------- audio

    /// Point at audio for one track, with an optional container hint for `download`.
    ///
    /// `download` is tried first for [`AudioPreference::PreferOriginal`] and
    /// [`AudioPreference::OriginalOrStream`]; when the store answers "unimplemented"
    /// (as it does today) the lossy `stream` is returned with
    /// [`AudioSource::stream_fallback`] set. No request is made on the `stream` path:
    /// the URL is built and the caller's hardened downloader follows the redirect to
    /// the allowlisted CDN.
    pub fn fetch_track_audio_with(
        &self,
        track: &StoreTrackRef,
        want: AudioPreference,
        format: Option<AudioFormat>,
    ) -> Result<AudioSource, StoreError> {
        if want == AudioPreference::StreamOnly {
            return self.stream_source(track, false);
        }

        match self.download_state() {
            DownloadState::Available {
                format,
                expected_bytes,
            } => self.original_source(track, format, expected_bytes),
            DownloadState::Unavailable => self.stream_source(track, true),
            DownloadState::Unknown => {
                let hint = format.and_then(subsonic_format_name);
                match self.attempt_download(track, hint) {
                    Ok(confirmed) => {
                        self.set_download_state(DownloadState::Available {
                            format: confirmed.format,
                            expected_bytes: confirmed.expected_bytes,
                        });
                        self.original_source(track, confirmed.format, confirmed.expected_bytes)
                    }
                    // The measured answer: `download` is a route stub.
                    Err(StoreError::Unimplemented { .. }) => {
                        self.set_download_state(DownloadState::Unavailable);
                        self.stream_source(track, true)
                    }
                    // `download` answered with something that is not audio, so it is
                    // not usable here. Fall back rather than fail the track.
                    Err(StoreError::Malformed { .. }) => {
                        self.set_download_state(DownloadState::Unavailable);
                        self.stream_source(track, true)
                    }
                    // The probe hit a byte cap or a network failure, so whether
                    // `download` works is still unknown. Fall back to the stream
                    // without claiming anything about `download`.
                    Err(StoreError::Transport { .. }) => self.stream_source(track, true),
                    // A challenge, a rejected login, a rate limit or a policy refusal
                    // means the whole connection is unusable: report it.
                    Err(other) => Err(other),
                }
            }
        }
    }

    /// Try one `download` to learn whether this server implements it.
    ///
    /// Discovers a track by reading one page of the collection and the first release
    /// on it. `Ok(false)` means the server answered "unimplemented" (as Bandcamp does
    /// today) or there is no track to try; an empty collection cannot prove anything.
    pub fn probe_download(&self) -> Result<bool, StoreError> {
        let page = self.list_releases(PageRequest::new(0, 1))?;
        let Some(release) = page.items.first() else {
            return Ok(false);
        };
        let detail = self.get_release(&release.id)?;
        let Some(track) = detail.tracks.first() else {
            return Ok(false);
        };
        self.probe_download_track(&track.id)
    }

    /// The same probe for a known track (cheaper: no listing calls).
    pub fn probe_download_track(&self, track: &StoreTrackRef) -> Result<bool, StoreError> {
        match self.attempt_download(track, None) {
            Ok(confirmed) => {
                self.set_download_state(DownloadState::Available {
                    format: confirmed.format,
                    expected_bytes: confirmed.expected_bytes,
                });
                Ok(true)
            }
            Err(StoreError::Unimplemented { .. }) => {
                self.set_download_state(DownloadState::Unavailable);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// One `download` request, classified.
    fn attempt_download(
        &self,
        track: &StoreTrackRef,
        format: Option<&str>,
    ) -> Result<ConfirmedDownload, StoreError> {
        let mut extra: Vec<(&str, &str)> = vec![("id", track.as_str())];
        if let Some(format) = format {
            extra.push(("format", format));
        }
        let url = self.request_url(ENDPOINT_DOWNLOAD, &extra)?;
        let response = self.transport.get(
            &url,
            &self.binary_headers("audio/*, application/json;q=0.5"),
            DOWNLOAD_PROBE_BYTES,
        )?;
        self.check_response_target(&response)?;

        if let Some(format) = mapping::sniff_audio_format(&response.body) {
            let expected_bytes = response
                .header("content-length")
                .and_then(|value| value.trim().parse::<u64>().ok());
            return Ok(ConfirmedDownload {
                format,
                expected_bytes,
            });
        }

        // Not audio: the body must be the server's error envelope. `parse_response`
        // turns "bad version" into `Unimplemented` and an empty 500 into
        // `BadCredentials`; a successful envelope here means the endpoint answered
        // with metadata instead of a file, which is not usable audio.
        match parse_response(&response.body, response.status, ENDPOINT_DOWNLOAD) {
            Ok(_) => Err(StoreError::Malformed {
                detail: "download answered with metadata instead of audio".to_string(),
            }),
            Err(error) => Err(error),
        }
    }

    /// The `download` URL, for a caller whose own downloader fetches it.
    ///
    /// The URL is a credit: capturing it (log, crash dump, portal JSON, a persisted job)
    /// gives working access to the whole API, and its `t=` allows offline password
    /// guessing. It never leaves this process except as the request line.
    fn original_source(
        &self,
        track: &StoreTrackRef,
        format: AudioFormat,
        expected_bytes: Option<u64>,
    ) -> Result<AudioSource, StoreError> {
        // The URL is built, not fetched: the caller's downloader applies caps,
        // consent and vetting. It is a reusable credential (PAR-07): `SourceUrl` keeps
        // it out of `Debug`, `Display` redacts it, and nothing may serialise it.
        let url = self.request_url(ENDPOINT_DOWNLOAD, &[("id", track.as_str())])?;
        Ok(AudioSource {
            url: SourceUrl::new(url),
            expected_bytes,
            format,
            quality: match format {
                AudioFormat::Flac | AudioFormat::Wav => AudioQuality::Lossless,
                _ => AudioQuality::Lossy { bitrate_kbps: None },
            },
            stream_fallback: false,
        })
    }

    /// The `stream` URL.
    ///
    /// Same credential caveat as the `download` URL: it is not a link to a public file,
    /// it is the password by another name.
    ///
    /// Reported as lossy with **no** bitrate: the only bitrate the server declares
    /// (`bitRate`, kept on [`crate::StoreTrack::declared_bitrate_kbps`]) describes a
    /// different encode than the one `stream` serves, and the stream's real average is
    /// only knowable after downloading it.
    fn stream_source(
        &self,
        track: &StoreTrackRef,
        fallback: bool,
    ) -> Result<AudioSource, StoreError> {
        let url = self.request_url(ENDPOINT_STREAM, &[("id", track.as_str())])?;
        Ok(AudioSource {
            url: SourceUrl::new(url),
            // The declared `size` belongs to the advertised file, which measured
            // 0.89 % larger than the stream; do not present it as this body's size.
            expected_bytes: None,
            format: AudioFormat::Mp3,
            quality: AudioQuality::Lossy {
                bitrate_kbps: None,
            },
            stream_fallback: fallback,
        })
    }

    // ----------------------------------------------------------------- covers

    /// Cover art bytes, capped and sniffed.
    ///
    /// Only JPEG, PNG, GIF and WebP bodies are returned; anything else — including
    /// HTML served at HTTP 200 — is an error, so a caller can serve these bytes with
    /// the type this method guarantees.
    pub fn cover_art(&self, id: &StoreCoverId) -> Result<Vec<u8>, StoreError> {
        let url = self.request_url(ENDPOINT_GET_COVER_ART, &[("id", id.as_str())])?;
        let response =
            self.transport
                .get(&url, &self.binary_headers("image/*"), self.cover_byte_cap)?;
        self.check_response_target(&response)?;

        if mapping::sniff_image_mime(&response.body).is_some() {
            return Ok(response.body);
        }

        // Not an image. Classify the body so a rejected login or an unimplemented
        // endpoint reports itself instead of "not an image".
        match parse_response(&response.body, response.status, ENDPOINT_GET_COVER_ART) {
            Ok(_) => Err(StoreError::Malformed {
                detail: "getCoverArt answered with no image".to_string(),
            }),
            Err(error) => Err(error),
        }
    }
}

/// The container and size a successful `download` probe reported.
struct ConfirmedDownload {
    format: AudioFormat,
    expected_bytes: Option<u64>,
}

impl fmt::Debug for SubsonicProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubsonicProvider")
            .field("base_url", &redact_url(&self.base_url))
            .field("api_version", &self.api_version)
            .field("client_name", &self.client_name)
            .field("username", &"<set>")
            .field("password", &"<redacted>")
            .field("cover_byte_cap", &self.cover_byte_cap)
            .field("download", &self.download_state())
            .finish()
    }
}

impl StoreProvider for SubsonicProvider {
    fn id(&self) -> StoreId {
        self.store_id.clone()
    }

    fn display_name(&self) -> &str {
        "Bandcamp"
    }

    /// `download` starts false and marked unverified: the only honest answer until
    /// [`Self::probe_download`] has been tried. `stream` and `cover_art` are measured.
    fn capabilities(&self) -> Capabilities {
        let available = matches!(self.download_state(), DownloadState::Available { .. });
        Capabilities {
            download: available,
            download_unverified: !available,
            stream: true,
            cover_art: true,
            // The beta's playlist endpoints exist but this provider does not read them.
            playlists: false,
        }
    }

    /// A real authenticated call (`getArtists`), never `ping`.
    fn verify_credentials(&self) -> Result<(), StoreError> {
        let payload = self.json_call(ENDPOINT_GET_ARTISTS, &[])?;
        if payload.is_xml() {
            return Err(StoreError::Malformed {
                detail: "getArtists answered XML although JSON was requested".to_string(),
            });
        }
        // Requiring the container is what makes this a credential check: a ping-shaped
        // `status: ok` body has no `artists` object and must not pass.
        if payload.get("artists").is_none() {
            return Err(StoreError::Malformed {
                detail: "getArtists returned no artists object".to_string(),
            });
        }
        Ok(())
    }

    // Each of these delegates to the inherent method of the same name; method
    // resolution prefers the inherent implementation, so there is no recursion.
    fn list_releases(&self, page: PageRequest) -> Result<Page<StoreRelease>, StoreError> {
        self.list_releases(page)
    }

    fn get_release(&self, id: &StoreReleaseId) -> Result<StoreReleaseDetail, StoreError> {
        self.get_release(id)
    }

    fn list_artists(&self) -> Result<Vec<StoreArtist>, StoreError> {
        self.list_artists()
    }

    fn get_artist(&self, id: &StoreArtistId) -> Result<StoreArtistDetail, StoreError> {
        self.get_artist(id)
    }

    fn search_tracks(&self, query: &str) -> Result<Vec<StoreSearchHit>, StoreError> {
        self.search_tracks(query)
    }

    fn fetch_track_audio(
        &self,
        track: &StoreTrackRef,
        want: AudioPreference,
    ) -> Result<AudioSource, StoreError> {
        self.fetch_track_audio_with(track, want, None)
    }

    fn cover_art(&self, id: &StoreCoverId) -> Result<Vec<u8>, StoreError> {
        self.cover_art(id)
    }
}

/// The `format=` value for a container, or `None` when the protocol has no name for
/// it (in which case the parameter is simply not sent).
fn subsonic_format_name(format: AudioFormat) -> Option<&'static str> {
    match format {
        AudioFormat::Mp3 => Some("mp3"),
        AudioFormat::Flac => Some("flac"),
        AudioFormat::Wav => Some("wav"),
        AudioFormat::Aac => Some("aac"),
        AudioFormat::Opus => Some("opus"),
        AudioFormat::Ogg => Some("ogg"),
        AudioFormat::Unknown => None,
    }
}

/// Validate and normalise the base URL.
fn normalise_base_url(base_url: &str, host: &AuthenticatedHosts) -> Result<String, StoreError> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(StoreError::policy("a store base url is required"));
    }
    if base.contains('?') || base.contains('#') {
        return Err(StoreError::policy(
            "a store base url must not carry a query or fragment",
        ));
    }
    // https only, port 443 only, no userinfo, host on the allowlist.
    host.check_authenticated(base)?;

    // The provider appends `/rest/<endpoint>.view`; a base that already ends in
    // `/rest` would build `/rest/rest/…`, which this store answers "bad version" for.
    if base
        .rsplit('/')
        .next()
        .is_some_and(|last| last.eq_ignore_ascii_case("rest"))
    {
        return Err(StoreError::policy(
            "a store base url must not include the /rest path",
        ));
    }
    Ok(base.to_string())
}

fn valid_endpoint_name(endpoint: &str) -> bool {
    !endpoint.is_empty()
        && endpoint.len() <= 32
        && endpoint.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Percent-encode one query component (RFC 3986 unreserved characters only).
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*byte));
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[(byte >> 4) as usize]));
                out.push(char::from(HEX[(byte & 0x0f) as usize]));
            }
        }
    }
    out
}
