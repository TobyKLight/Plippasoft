//! The provider abstraction.
//!
//! A provider turns a store's published API into Plippa's own vocabulary. It covers
//! exactly what the product needs — authenticate, list a collection, read one
//! release, point at audio, fetch cover art — and nothing else. Protocol details
//! (paths, envelopes, auth schemes, pagination quirks) stay inside the implementation.

use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::types::{
    AudioPreference, AudioSource, Page, PageRequest, StoreArtist, StoreArtistDetail,
    StoreArtistId, StoreCoverId, StoreId, StoreRelease, StoreReleaseDetail, StoreReleaseId,
    StoreSearchHit, StoreTrackRef,
};

/// What a provider can actually do.
///
/// Consumers must branch on this rather than assuming parity: Bandcamp's Subsonic
/// beta advertises downloading but implements neither `download` nor playlists, so a
/// provider may report `stream: true, download: false, playlists: false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// The store can hand over an original file (not merely a stream).
    pub download: bool,
    /// `true` while [`Self::download`] is a placeholder rather than a measured fact.
    ///
    /// A provider that can only discover `download` by trying it starts with
    /// `download: false, download_unverified: true` and flips both after a probe
    /// succeeds. Consumers must not promise the user an original while this is set.
    pub download_unverified: bool,
    /// The store can stream audio.
    pub stream: bool,
    /// Cover art can be fetched by id.
    pub cover_art: bool,
    /// The store has playlists Plippa can read.
    pub playlists: bool,
}

/// A platform Plippa can sign in to and download from.
///
/// Implementations are `Send + Sync`: the server keeps one provider behind an
/// `Arc` and calls it from portal request threads and the background sync worker.
///
/// Every method returns [`StoreError`]; an implementation must validate the
/// protocol envelope rather than trust a status code, because store APIs in this
/// space answer `200` for authentication failures and unimplemented endpoints.
pub trait StoreProvider: Send + Sync {
    /// Stable slug used for credential files, logs and the portal.
    fn id(&self) -> StoreId;

    /// Human-readable name for the portal and the device screen.
    fn display_name(&self) -> &str;

    /// What this provider supports today.
    fn capabilities(&self) -> Capabilities;

    /// Prove the stored credentials work with a **real authenticated call**.
    ///
    /// Never a liveness ping: on this store family a ping succeeds with no
    /// credentials at all. `BadCredentials` and `InvalidCredentials` are distinct:
    /// the former means the server rejected a well-formed login, the latter means
    /// no request was made.
    fn verify_credentials(&self) -> Result<(), StoreError>;

    /// One page of the signed-in user's collection.
    fn list_releases(&self, page: PageRequest) -> Result<Page<StoreRelease>, StoreError>;

    /// A single release with its track list.
    fn get_release(&self, id: &StoreReleaseId) -> Result<StoreReleaseDetail, StoreError>;

    /// Every artist the signed-in user's collection contains.
    ///
    /// Used by the ownership-matching fallback: walk an artist's releases and compare
    /// titles. Not a library grouping Plippa owns.
    fn list_artists(&self) -> Result<Vec<StoreArtist>, StoreError>;

    /// One artist plus the releases the provider lists for it.
    fn get_artist(&self, id: &StoreArtistId) -> Result<StoreArtistDetail, StoreError>;

    /// Free-text track search.
    ///
    /// Best effort by contract: this store family's search misses titles it holds, so
    /// a caller must never treat a miss as "not owned". Implementations return an
    /// empty list for an empty result or an unimplemented search endpoint, and only
    /// error for a real failure (bad credentials, transport).
    fn search_tracks(&self, query: &str) -> Result<Vec<StoreSearchHit>, StoreError>;

    /// Point at audio for one track.
    ///
    /// Returns a URL, not bytes: the caller downloads it through its own hardened
    /// downloader so consent, SSRF vetting, byte caps and resume apply. `want` lets
    /// the caller refuse a lossy stream when it needs an original.
    fn fetch_track_audio(
        &self,
        track: &StoreTrackRef,
        want: AudioPreference,
    ) -> Result<AudioSource, StoreError>;

    /// Cover art bytes.
    fn cover_art(&self, id: &StoreCoverId) -> Result<Vec<u8>, StoreError>;
}
