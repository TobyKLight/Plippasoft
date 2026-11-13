//! Store-agnostic platform download provider.
//!
//! This crate is the half of "download from a platform" that does not touch the
//! network. It defines:
//!
//! - [`StoreProvider`], the trait a platform client implements;
//! - the value types a provider returns ([`StoreRelease`], [`StoreTrack`],
//!   [`AudioSource`], …);
//! - [`CredentialStore`], the on-disk secrets store (0600 in a 0700 directory);
//! - [`AuthenticatedHosts`], the default-deny host allowlist for authenticated
//!   requests and redirects;
//! - [`StoreError`], the error model with stable codes and safe `Display`;
//! - [`Transport`], the seam the caller fills with its own hardened HTTP client;
//! - [`subsonic_token`], the one MD5 use the Subsonic protocol forces;
//! - [`MockProvider`], a provider double for downstream tests.
//!
//! ## What is deliberately absent
//!
//! There is no HTTP client, TLS, retry, timeout, DNS vetting or SSRF policy here.
//! `plippa-server` already owns that stack (consent, private-address blocking, byte
//! caps, redirect re-checks); duplicating it would mean two policy engines and one
//! of them would drift. A provider asks a [`Transport`] for bytes and the binary
//! that implements the trait decides whether the request may happen at all.
//!
//! The platform-specific half is [`subsonic::SubsonicProvider`], which implements the
//! Bandcamp Subsonic profile as observed against a live account.

pub mod allowlist;
pub mod credentials;
pub mod error;
pub(crate) mod md5;
pub mod mock;
pub mod provider;
pub mod subsonic;
pub mod transport;
pub mod types;

pub use allowlist::{check_authenticated_target, AuthenticatedHosts, MAX_URL_BYTES};
pub use credentials::{
    wipe_bytes, CredentialStore, Credentials, SecretString, DIR_MODE, FILE_MODE,
    MAX_CREDENTIALS_BYTES,
};
pub use error::{redact_url, StoreError};
pub use md5::subsonic_token;
pub use mock::{MockCall, MockProvider, MOCK_AUDIO_HOST};
pub use provider::{Capabilities, StoreProvider};
pub use subsonic::{
    parse_rfc1123_unix, parse_response, CollectionCrawl, CrawlDecision, CrawlOptions, CrawlPage,
    CrawlProgress, CrawlStop, SubsonicPayload, SubsonicProvider, SubsonicServerInfo,
    DEFAULT_API_VERSION, DEFAULT_CLIENT_NAME, DEFAULT_COVER_BYTE_CAP, DEFAULT_CRAWL_PAGE_SIZE,
    DOWNLOAD_PROBE_BYTES, JSON_BYTE_CAP, MAX_CRAWL_RELEASES,
};
pub use transport::{CountingTransport, HeaderMap, RecordedCall, Transport, TransportResponse};
pub use types::{
    is_valid_slug, AudioFormat, AudioPreference, AudioQuality, AudioSource, Page, PageRequest,
    SourceUrl, StoreArtist, StoreArtistDetail, StoreArtistId, StoreCover, StoreCoverId, StoreId,
    StoreRelease, StoreReleaseDetail, StoreReleaseId, StoreSearchHit, StoreTrack, StoreTrackRef,
    MAX_STORE_ID_BYTES,
};
