//! A public, feature-free [`StoreProvider`] double for downstream tests.
//!
//! `MockProvider` is deliberately **not** behind `cfg(test)`: packages that consume
//! `plippa-store` need a provider that behaves like a real one without a network or
//! an account. It serves a small fixed collection, can be told to reject
//! authentication, and records what was asked of it.
//!
//! It makes no network requests and its URLs point at the reserved `.invalid` TLD,
//! so a test that accidentally feeds one to a real transport fails loudly instead of
//! reaching a third party.

use std::fmt;
use std::sync::Mutex;

use crate::error::StoreError;
use crate::provider::{Capabilities, StoreProvider};
use crate::types::{
    AudioFormat, AudioPreference, AudioQuality, AudioSource, Page, PageRequest, StoreArtist,
    StoreArtistDetail, StoreArtistId, StoreCover, StoreCoverId, StoreId, StoreRelease,
    StoreReleaseDetail, StoreReleaseId, StoreSearchHit, StoreTrack, StoreTrackRef,
};

/// Default host for mock audio and cover URLs (reserved TLD, never resolves).
pub const MOCK_AUDIO_HOST: &str = "https://mock.invalid";

/// One method call observed by [`MockProvider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockCall {
    /// Trait method name, for example `list_releases`.
    pub method: &'static str,
    /// Argument summary (a release id, a page request, a track reference).
    pub argument: String,
}

/// A provider backed by a fixed collection.
#[derive(Debug)]
pub struct MockProvider {
    id: StoreId,
    display_name: String,
    audio_base: String,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    authenticated: bool,
    releases: Vec<StoreReleaseDetail>,
    covers: Vec<(StoreCoverId, Vec<u8>)>,
    calls: Vec<MockCall>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MockProvider {
    /// A provider with two releases and valid credentials.
    pub fn new() -> Self {
        let id = StoreId::new("mock").expect("the literal `mock` is a valid store id");
        Self {
            id,
            display_name: "Mock Store".to_string(),
            audio_base: MOCK_AUDIO_HOST.to_string(),
            inner: Mutex::new(Inner {
                authenticated: true,
                releases: fake_collection(),
                covers: vec![(
                    StoreCoverId::new("mock-cover-1"),
                    b"\xff\xd8\xff\xe0mock-jpeg".to_vec(),
                )],
                calls: Vec::new(),
            }),
        }
    }

    /// A provider whose every call fails as a credential rejection.
    pub fn with_bad_credentials() -> Self {
        let provider = Self::new();
        provider.set_authenticated(false);
        provider
    }

    /// Change whether authenticated calls succeed.
    pub fn set_authenticated(&self, authenticated: bool) {
        self.lock().authenticated = authenticated;
    }

    /// Use a different base for audio URLs (for example a local test server).
    ///
    /// The query string is appended by the mock; the base should have no query.
    pub fn with_audio_base(mut self, base: impl Into<String>) -> Self {
        self.audio_base = base.into();
        self
    }

    /// Calls observed so far, oldest first.
    pub fn calls(&self) -> Vec<MockCall> {
        self.lock().calls.clone()
    }

    /// Number of calls observed so far.
    pub fn call_count(&self) -> usize {
        self.lock().calls.len()
    }

    /// Methods called so far, oldest first.
    pub fn called_methods(&self) -> Vec<&'static str> {
        self.lock().calls.iter().map(|call| call.method).collect()
    }

    /// Release ids in collection order.
    pub fn release_ids(&self) -> Vec<StoreReleaseId> {
        self.lock()
            .releases
            .iter()
            .map(|detail| detail.release.id.clone())
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record(&self, method: &'static str, argument: impl Into<String>) -> Result<(), StoreError> {
        let mut inner = self.lock();
        inner.calls.push(MockCall {
            method,
            argument: argument.into(),
        });
        if inner.authenticated {
            Ok(())
        } else {
            Err(StoreError::BadCredentials)
        }
    }
}

fn capabilities() -> Capabilities {
    Capabilities {
        download: false,
        download_unverified: false,
        stream: true,
        cover_art: true,
        playlists: false,
    }
}

impl StoreProvider for MockProvider {
    fn id(&self) -> StoreId {
        self.id.clone()
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn verify_credentials(&self) -> Result<(), StoreError> {
        self.record("verify_credentials", "")
    }

    fn list_releases(&self, page: PageRequest) -> Result<Page<StoreRelease>, StoreError> {
        self.record(
            "list_releases",
            format!("offset={} limit={}", page.offset, page.limit),
        )?;
        let inner = self.lock();
        let total = inner.releases.len();
        let start = (page.offset as usize).min(total);
        let end = start.saturating_add(page.limit as usize).min(total);
        let items: Vec<StoreRelease> = inner.releases[start..end]
            .iter()
            .map(|detail| detail.release.clone())
            .collect();
        let next_offset = if end < total { Some(end as u32) } else { None };
        Ok(Page::new(
            items,
            page.offset,
            next_offset,
            Some(total as u32),
        ))
    }

    fn get_release(&self, id: &StoreReleaseId) -> Result<StoreReleaseDetail, StoreError> {
        self.record("get_release", id.as_str())?;
        self.lock()
            .releases
            .iter()
            .find(|detail| &detail.release.id == id)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    fn fetch_track_audio(
        &self,
        track: &StoreTrackRef,
        want: AudioPreference,
    ) -> Result<AudioSource, StoreError> {
        self.record("fetch_track_audio", track.as_str())?;
        let known = self
            .lock()
            .releases
            .iter()
            .any(|detail| detail.tracks.iter().any(|candidate| &candidate.id == track));
        if !known {
            return Err(StoreError::NotFound);
        }
        if want == AudioPreference::PreferOriginal && !capabilities().download {
            return Err(StoreError::Unimplemented {
                endpoint: "download".to_string(),
            });
        }
        Ok(AudioSource {
            url: crate::types::SourceUrl::new(format!(
                "{}/stream/{}?token=mock-token",
                self.audio_base, track
            )),
            expected_bytes: Some(262_144),
            format: AudioFormat::Mp3,
            quality: AudioQuality::Lossy {
                bitrate_kbps: Some(192),
            },
            stream_fallback: want != AudioPreference::StreamOnly,
        })
    }

    fn cover_art(&self, id: &StoreCoverId) -> Result<Vec<u8>, StoreError> {
        self.record("cover_art", id.as_str())?;
        self.lock()
            .covers
            .iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, bytes)| bytes.clone())
            .ok_or(StoreError::NotFound)
    }

    fn list_artists(&self) -> Result<Vec<StoreArtist>, StoreError> {
        self.record("list_artists", "")?;
        Ok(vec![mock_artist()])
    }

    fn get_artist(&self, id: &StoreArtistId) -> Result<StoreArtistDetail, StoreError> {
        self.record("get_artist", id.as_str())?;
        if id != &mock_artist().id {
            return Err(StoreError::NotFound);
        }
        let releases = self
            .lock()
            .releases
            .iter()
            .map(|detail| detail.release.clone())
            .collect();
        Ok(StoreArtistDetail {
            artist: mock_artist(),
            releases,
        })
    }

    fn search_tracks(&self, query: &str) -> Result<Vec<StoreSearchHit>, StoreError> {
        self.record("search_tracks", query)?;
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let inner = self.lock();
        Ok(inner
            .releases
            .iter()
            .flat_map(|detail| {
                detail.tracks.iter().map(move |track| StoreSearchHit {
                    release_id: Some(detail.release.id.clone()),
                    artist: Some(detail.release.artist.clone()),
                    track: track.clone(),
                })
            })
            .filter(|hit| hit.track.title.to_lowercase().contains(&needle))
            .collect())
    }
}

fn mock_artist() -> StoreArtist {
    StoreArtist {
        id: StoreArtistId::new("mock-artist-1"),
        name: "Mock Artist".to_string(),
        album_count: Some(2),
        cover: Some(StoreCover {
            id: StoreCoverId::new("mock-cover-1"),
            mime: Some("image/jpeg".to_string()),
        }),
    }
}

impl fmt::Display for MockProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_name)
    }
}

/// The fixed collection: an album with three tracks, and a single whose release id
/// and track reference are the same string (matching how single-track releases
/// behave on the real store, so downstream code cannot accidentally assume they
/// differ).
fn fake_collection() -> Vec<StoreReleaseDetail> {
    let album = StoreRelease {
        id: StoreReleaseId::new("mock-rel-1"),
        title: "Mock Album".to_string(),
        artist: "Mock Artist".to_string(),
        year: Some(2026),
        track_count: Some(3),
        duration_ms: Some(540_000),
        cover: Some(StoreCover {
            id: StoreCoverId::new("mock-cover-1"),
            mime: Some("image/jpeg".to_string()),
        }),
        web_url: None,
        created_unix: Some(1_780_000_000),
    };
    let album_tracks = ["First Light", "Second Wind", "Third Rail"]
        .iter()
        .enumerate()
        .map(|(index, title)| StoreTrack {
            id: StoreTrackRef::new(format!("mock-trk-{}", index + 1)),
            title: (*title).to_string(),
            track_number: Some(index as u32 + 1),
            disc_number: Some(1),
            duration_ms: Some(180_000),
            suffix: Some("mp3".to_string()),
            content_type: Some("audio/mpeg".to_string()),
            declared_bytes: Some(7_200_000),
            declared_bitrate_kbps: Some(256),
        })
        .collect();

    let single = StoreRelease {
        id: StoreReleaseId::new("mock-single-1"),
        title: "Mock Single".to_string(),
        artist: "Mock Artist".to_string(),
        year: None,
        track_count: Some(1),
        duration_ms: Some(200_000),
        cover: None,
        web_url: None,
        created_unix: None,
    };
    let single_tracks = vec![StoreTrack {
        id: StoreTrackRef::new("mock-single-1"),
        title: "Only Track".to_string(),
        track_number: Some(1),
        disc_number: Some(1),
        duration_ms: Some(200_000),
        suffix: Some("flac".to_string()),
        content_type: Some("audio/flac".to_string()),
        declared_bytes: Some(40_000_000),
        declared_bitrate_kbps: None,
    }];

    vec![
        StoreReleaseDetail {
            release: album,
            tracks: album_tracks,
        },
        StoreReleaseDetail {
            release: single,
            tracks: single_tracks,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> MockProvider {
        MockProvider::new()
    }

    #[test]
    fn advertises_stream_but_not_download() {
        let provider = provider();
        assert_eq!(provider.id().as_str(), "mock");
        assert_eq!(provider.display_name(), "Mock Store");
        let caps = provider.capabilities();
        assert!(caps.stream);
        assert!(!caps.download);
        assert!(caps.cover_art);
        assert!(!caps.playlists);
    }

    #[test]
    fn lists_the_collection_with_pagination() {
        let provider = provider();
        let page = provider
            .list_releases(PageRequest::first(1))
            .expect("authenticated");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "Mock Album");
        assert_eq!(page.next_offset, Some(1));
        assert_eq!(page.total, Some(2));
        assert_eq!(page.offset, 0);

        let next = provider
            .list_releases(PageRequest::new(1, 1))
            .expect("authenticated");
        assert_eq!(next.items.len(), 1);
        assert_eq!(next.items[0].title, "Mock Single");
        assert_eq!(next.next_offset, None);

        let past_end = provider
            .list_releases(PageRequest::new(99, 10))
            .expect("authenticated");
        assert!(past_end.is_empty());
        assert_eq!(past_end.next_offset, None);
    }

    #[test]
    fn get_release_returns_tracks_and_not_found_for_unknown_ids() {
        let provider = provider();
        let detail = provider
            .get_release(&StoreReleaseId::new("mock-rel-1"))
            .expect("known release");
        assert_eq!(detail.tracks.len(), 3);
        assert_eq!(detail.tracks[2].id.as_str(), "mock-trk-3");

        let single = provider
            .get_release(&StoreReleaseId::new("mock-single-1"))
            .expect("known single");
        assert_eq!(single.tracks.len(), 1);
        assert_eq!(
            single.tracks[0].id.as_str(),
            single.release.id.as_str(),
            "a single's track reference equals its release id"
        );

        assert_eq!(
            provider.get_release(&StoreReleaseId::new("nope")),
            Err(StoreError::NotFound)
        );
    }

    #[test]
    fn bad_credentials_fail_every_authenticated_call() {
        let provider = MockProvider::with_bad_credentials();
        assert_eq!(provider.verify_credentials(), Err(StoreError::BadCredentials));
        assert_eq!(
            provider.list_releases(PageRequest::default()),
            Err(StoreError::BadCredentials)
        );
        assert_eq!(
            provider.get_release(&StoreReleaseId::new("mock-rel-1")),
            Err(StoreError::BadCredentials)
        );
        assert_eq!(
            provider.fetch_track_audio(&StoreTrackRef::new("mock-trk-1"), AudioPreference::default()),
            Err(StoreError::BadCredentials)
        );
        assert_eq!(
            provider.cover_art(&StoreCoverId::new("mock-cover-1")),
            Err(StoreError::BadCredentials)
        );

        // Calls are still recorded, so a test can prove it tried.
        assert_eq!(provider.call_count(), 5);

        provider.set_authenticated(true);
        assert_eq!(provider.verify_credentials(), Ok(()));
    }

    #[test]
    fn audio_preferences_are_honoured() {
        let provider = provider();
        let track = StoreTrackRef::new("mock-trk-2");

        let stream = provider
            .fetch_track_audio(&track, AudioPreference::OriginalOrStream)
            .expect("stream fallback allowed");
        assert_eq!(stream.format, AudioFormat::Mp3);
        assert_eq!(
            stream.quality,
            AudioQuality::Lossy {
                bitrate_kbps: Some(192)
            }
        );
        assert_eq!(stream.expected_bytes, Some(262_144));
        assert!(stream.url.expose().starts_with(MOCK_AUDIO_HOST));
        assert!(!format!("{stream:?}").contains("mock-token"));

        assert!(provider
            .fetch_track_audio(&track, AudioPreference::StreamOnly)
            .is_ok());
        assert_eq!(
            provider.fetch_track_audio(&track, AudioPreference::PreferOriginal),
            Err(StoreError::Unimplemented {
                endpoint: "download".to_string()
            })
        );
        assert_eq!(
            provider.fetch_track_audio(&StoreTrackRef::new("nope"), AudioPreference::default()),
            Err(StoreError::NotFound)
        );
    }

    #[test]
    fn cover_art_round_trips_and_records_calls() {
        let provider = provider();
        let bytes = provider
            .cover_art(&StoreCoverId::new("mock-cover-1"))
            .expect("known cover");
        assert_eq!(bytes, b"\xff\xd8\xff\xe0mock-jpeg");
        assert_eq!(
            provider.cover_art(&StoreCoverId::new("mock-cover-2")),
            Err(StoreError::NotFound)
        );

        assert_eq!(
            provider.called_methods(),
            vec!["cover_art", "cover_art"],
            "every call is recorded in order"
        );
        assert_eq!(provider.calls()[0].argument, "mock-cover-1");
    }

    #[test]
    fn with_audio_base_moves_the_url_without_a_network_call() {
        let provider = MockProvider::new().with_audio_base("http://127.0.0.1:9");
        let source = provider
            .fetch_track_audio(&StoreTrackRef::new("mock-trk-1"), AudioPreference::default())
            .expect("source");
        assert!(source.url.expose().starts_with("http://127.0.0.1:9/stream/mock-trk-1"));
    }
}
