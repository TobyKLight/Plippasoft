//! The Subsonic provider against recorded fixtures: no network, no credentials.
//!
//! Every row of the verified-behaviour table for the Subsonic provider has a test here,
//! by name. The fixtures under `tests/fixtures/subsonic/` reproduce
//! the field lists and shapes measured against a live account, with every
//! account-identifying value replaced.

use std::sync::Arc;

use plippa_store::{
    parse_rfc1123_unix, AudioFormat, AudioPreference, AudioQuality, AuthenticatedHosts,
    CountingTransport, CrawlDecision, CrawlOptions, CrawlPage, CrawlStop, Credentials, PageRequest,
    StoreArtistId, StoreCoverId, StoreError, StoreProvider, StoreReleaseId, StoreTrackRef,
    SubsonicProvider, SubsonicServerInfo, Transport, TransportResponse, subsonic_token,
};

const BASE: &str = "https://bandcamp.com/api/subsonic";
const USER: &str = "fixture-user";
const PASSWORD: &str = "fixture-password";
const TRACK: &str = "t:2000000001";

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/subsonic")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("fixture {name}: {error}"))
}

/// A JSON reply from the API base.
fn json(name: &str) -> TransportResponse {
    TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: fixture(name),
        final_url: format!("{BASE}/rest/x.view"),
        redirects: Vec::new(),
    }
}

/// A reply with an arbitrary status (an empty body, a challenge page, …).
fn reply(status: u16, body: Vec<u8>) -> TransportResponse {
    TransportResponse {
        status,
        headers: Vec::new(),
        body,
        final_url: format!("{BASE}/rest/x.view"),
        redirects: Vec::new(),
    }
}

fn provider(transport: &Arc<CountingTransport>) -> SubsonicProvider {
    let transport: Arc<dyn Transport + Send + Sync> = Arc::clone(transport) as Arc<_>;
    SubsonicProvider::new(
        transport,
        BASE,
        Credentials::new(USER, PASSWORD),
        "plippa",
    )
    .expect("the Bandcamp base url is on the default allowlist")
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| percent_decode(value))
    })
}

/// Read a query value the way the server does, so assertions can name the real
/// opaque id (`t:2000000001`) rather than its wire encoding.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Request building and auth
// ---------------------------------------------------------------------------

#[test]
fn the_api_root_is_the_rest_form_and_never_the_view_form() {
    let transport = Arc::new(CountingTransport::new());
    let provider = provider(&transport);
    let url = provider.request_url("getArtists", &[]).expect("url");

    assert!(
        url.starts_with("https://bandcamp.com/api/subsonic/rest/getArtists.view?"),
        "{url}"
    );
    assert!(!url.contains("/view/"), "the `/view/<ep>` form must never be built");
    assert_eq!(query_param(&url, "f").as_deref(), Some("json"));
    assert_eq!(query_param(&url, "v").as_deref(), Some("1.16.1"));
    assert_eq!(query_param(&url, "c").as_deref(), Some("plippa"));
    // A hostile endpoint name cannot be pasted into the path.
    assert!(provider.request_url("getArtists?u=x", &[]).is_err());
    assert!(provider.request_url("../admin", &[]).is_err());
}

#[test]
fn token_auth_uses_a_fresh_salt_per_request_and_never_the_plain_password() {
    let transport = Arc::new(CountingTransport::new());
    let provider = provider(&transport);

    let first = provider.request_url("ping", &[]).expect("url");
    let second = provider.request_url("ping", &[]).expect("url");

    let salt = query_param(&first, "s").expect("salt");
    let token = query_param(&first, "t").expect("token");
    let other_salt = query_param(&second, "s").expect("salt");
    let other_token = query_param(&second, "t").expect("token");

    assert_eq!(salt.len(), 16, "salt is 16 hex characters: {salt}");
    assert!(salt.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(salt, other_salt, "each request must use a fresh salt");
    assert_ne!(token, other_token);
    assert_eq!(token, subsonic_token(PASSWORD, &salt));
    assert_eq!(
        other_token,
        subsonic_token(PASSWORD, &other_salt)
    );

    assert_eq!(query_param(&first, "u").as_deref(), Some(USER));
    // `p=` (the plain password) is rejected by the store, so it must never be sent.
    assert!(!first.contains("&p=") && !first.contains("?p="), "{first}");
    assert!(!first.contains(PASSWORD));
}

#[test]
fn credentials_are_trimmed_and_empty_ones_are_refused_before_any_request() {
    let transport = Arc::new(CountingTransport::new());
    let transport: Arc<dyn Transport + Send + Sync> = Arc::clone(&transport) as Arc<_>;
    // The copy button has been measured prepending a space.
    let provider = SubsonicProvider::new(
        transport,
        BASE,
        Credentials::new("  fixture-user ", " fixture-password\n"),
        "plippa",
    )
    .expect("provider");
    let url = provider.request_url("ping", &[]).expect("url");
    assert_eq!(query_param(&url, "u").as_deref(), Some(USER));

    let empty: Arc<dyn Transport + Send + Sync> = Arc::new(CountingTransport::new());
    assert_eq!(
        SubsonicProvider::new(empty, BASE, Credentials::new(" ", "x"), "plippa").unwrap_err(),
        StoreError::InvalidCredentials
    );
}

#[test]
fn the_base_url_must_be_https_and_on_the_default_allowlist() {
    fn build(base: &str) -> Result<SubsonicProvider, StoreError> {
        let transport: Arc<dyn Transport + Send + Sync> = Arc::new(CountingTransport::new());
        SubsonicProvider::new(
            transport,
            base,
            Credentials::new(USER, PASSWORD),
            "plippa",
        )
    }

    for bad in [
        "",
        "http://bandcamp.com/api/subsonic",
        "https://evil.example/api/subsonic",
        "https://bandcamp.com.evil.example/api/subsonic",
        "https://evilbandcamp.com/api/subsonic",
        "https://bandcamp.com:8443/api/subsonic",
        "https://u:p@bandcamp.com/api/subsonic",
        "https://bandcamp.com/api/subsonic?x=1",
        "https://bandcamp.com/api/subsonic#x",
        "https://bandcamp.com/api/subsonic/rest",
    ] {
        let error = build(bad).expect_err("must refuse {bad}");
        assert_eq!(error.code(), "policy", "{bad}");
    }

    let provider = build("https://bandcamp.com/api/subsonic/").expect("trailing slash");
    assert_eq!(provider.base_url(), BASE);
    assert!(provider.allowlist().authenticated(BASE));
}

#[test]
fn the_version_and_client_name_are_configurable_but_validated() {
    let transport: Arc<dyn Transport + Send + Sync> = Arc::new(CountingTransport::new());
    let provider = SubsonicProvider::new(
        transport,
        BASE,
        Credentials::new(USER, PASSWORD),
        "Plippa Test Client",
    )
    .expect("provider")
    .with_api_version("1.16.1")
    .expect("valid version");
    let url = provider.request_url("ping", &[]).expect("url");
    assert_eq!(query_param(&url, "v").as_deref(), Some("1.16.1"));
    // The client name is never validated by the store, but it is still sent.
    assert_eq!(
        query_param(&url, "c").as_deref(),
        Some("Plippa Test Client")
    );

    let transport: Arc<dyn Transport + Send + Sync> = Arc::new(CountingTransport::new());
    let build = || {
        SubsonicProvider::new(
            transport.clone(),
            BASE,
            Credentials::new(USER, PASSWORD),
            "plippa",
        )
        .expect("provider")
    };
    assert!(build().with_api_version("1.16.1-beta").is_err());
    assert!(build().with_api_version("").is_err());
}

// ---------------------------------------------------------------------------
// Envelope handling
// ---------------------------------------------------------------------------

#[test]
fn an_empty_http_500_means_bad_credentials_not_malformed() {
    let transport = Arc::new(CountingTransport::new());
    // The recorded capture: status 500 with a zero-byte body.
    transport.push_response(reply(500, fixture("empty_body.bin")));
    let provider = provider(&transport);

    assert_eq!(
        provider.verify_credentials(),
        Err(StoreError::BadCredentials)
    );
    assert_eq!(transport.call_count(), 1);
    assert!(transport.urls()[0].contains("getArtists.view"));
}

#[test]
fn credentials_are_verified_with_get_artists_and_not_with_ping() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getArtists.json"));
    let provider = provider(&transport);

    assert_eq!(provider.verify_credentials(), Ok(()));
    let urls = transport.urls();
    assert_eq!(urls.len(), 1);
    assert!(urls[0].contains("getArtists.view"), "{}", urls[0]);
    assert!(!urls[0].contains("ping.view"), "{}", urls[0]);
}

#[test]
fn a_server_that_answers_only_ping_does_not_authenticate() {
    // `ping` returns `status: ok` with wrong credentials and with none at all, so a
    // transport that answers every call with the ping envelope must still fail.
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("ping.json"));
    transport.push_response(json("ping.json"));
    let provider = provider(&transport);

    assert_eq!(
        provider.ping(),
        Ok(SubsonicServerInfo {
            server_type: Some("BandcampServer".to_string()),
            version: Some("1.16.1".to_string()),
            server_version: Some("1.0".to_string()),
            open_subsonic: true,
        })
    );
    assert!(
        matches!(
            provider.verify_credentials(),
            Err(StoreError::Malformed { .. })
        ),
        "a ping-shaped `status: ok` body must not pass a credential check"
    );
    assert!(transport.urls()[1].contains("getArtists.view"));
}

#[test]
fn a_fastly_client_challenge_at_http_200_is_challenged() {
    let transport = Arc::new(CountingTransport::new());
    for _ in 0..2 {
        transport.push_response(TransportResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html; charset=UTF-8".into())],
            body: fixture("fastly_challenge.html"),
            final_url: format!("{BASE}/rest/x.view"),
            redirects: Vec::new(),
        });
    }
    let provider = provider(&transport);

    assert_eq!(provider.verify_credentials(), Err(StoreError::Challenged));
    assert_eq!(
        provider.list_releases(PageRequest::default()),
        Err(StoreError::Challenged)
    );
}

#[test]
fn an_unimplemented_endpoint_is_unimplemented_not_malformed() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("error_bad_version.json"));
    transport.push_response(json("error_bad_version.json"));
    let provider = provider(&transport);

    // HTTP 200 with a `bad version` body names the endpoint and is never `Malformed`.
    assert_eq!(
        provider.get_artist(&StoreArtistId::new("b:1")),
        Err(StoreError::Unimplemented {
            endpoint: "getArtist".to_string()
        })
    );
    assert_eq!(
        provider.probe_download_track(&StoreTrackRef::new(TRACK)),
        Ok(false)
    );
    assert!(transport.urls()[0].contains("getArtist.view"));
    assert!(transport.urls()[1].contains("download.view"));
}

#[test]
fn a_failed_envelope_with_code_40_is_bad_credentials() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("error_failed_auth.json"));
    let provider = provider(&transport);

    assert_eq!(provider.verify_credentials(), Err(StoreError::BadCredentials));
}

#[test]
fn http_429_is_rate_limited() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(reply(429, fixture("error_rate_limited.json")));
    let provider = provider(&transport);

    assert_eq!(
        provider.list_releases(PageRequest::default()),
        Err(StoreError::RateLimited)
    );
}

#[test]
fn a_ping_that_answers_xml_is_tolerated_and_a_data_endpoint_that_does_is_not() {
    let transport = Arc::new(CountingTransport::new());
    let xml = || TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "application/xml".into())],
        body: fixture("ping.xml"),
        final_url: format!("{BASE}/rest/x.view"),
        redirects: Vec::new(),
    };
    transport.push_response(xml());
    transport.push_response(xml());
    let provider = provider(&transport);

    // Only `ping` is expected to answer XML; it carries nothing, which is fine.
    assert_eq!(provider.ping(), Ok(SubsonicServerInfo::default()));
    // A credential check that gets XML learned nothing about the credentials.
    assert!(matches!(
        provider.verify_credentials(),
        Err(StoreError::Malformed { .. })
    ));
}

#[test]
fn an_album_without_an_id_is_a_malformed_reply_not_a_silent_drop() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("malformed.json"));
    let provider = provider(&transport);

    assert!(matches!(
        provider.list_releases(PageRequest::default()),
        Err(StoreError::Malformed { .. })
    ));
}

// ---------------------------------------------------------------------------
// Collection listing
// ---------------------------------------------------------------------------

#[test]
fn the_listing_is_alphabetical_by_name_and_the_page_size_is_clamped() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_page1.json"));
    transport.push_response(json("getAlbumList2_page1.json"));
    let provider = provider(&transport);

    let page = provider
        .list_releases(PageRequest {
            offset: 400,
            limit: 9_000,
        })
        .expect("page");
    let first = &transport.urls()[0];
    assert_eq!(query_param(first, "type").as_deref(), Some("alphabeticalByName"));
    assert_eq!(query_param(first, "size").as_deref(), Some("500"));
    assert_eq!(query_param(first, "offset").as_deref(), Some("400"));
    assert!(first.contains("getAlbumList2.view"));
    // The fixture has fewer items than the requested page, so the crawl would stop.
    assert_eq!(page.next_offset, None);

    provider
        .list_releases(PageRequest {
            offset: 0,
            limit: 0,
        })
        .expect("page");
    assert_eq!(
        query_param(&transport.urls()[1], "size").as_deref(),
        Some("200"),
        "an unspecified size uses the store's own page size"
    );
}

#[test]
fn an_album_maps_to_a_release_with_no_invented_url() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_page1.json"));
    let provider = provider(&transport);
    let page = provider
        .list_releases(PageRequest::first(3))
        .expect("page");

    assert_eq!(page.items.len(), 3);
    let first = &page.items[0];
    assert_eq!(first.id, StoreReleaseId::new("a:1000000001"));
    assert_eq!(first.title, "Fixture Album One");
    assert_eq!(first.artist, "Fixture Artist");
    assert_eq!(first.year, None, "`year: null` stays unknown, not 0");
    assert_eq!(first.track_count, Some(2));
    assert_eq!(first.duration_ms, Some(1_389_000), "seconds become milliseconds");
    assert_eq!(
        first.cover.as_ref().map(|cover| cover.id.as_str()),
        Some("ca:3000000001")
    );
    // No Subsonic response carries a Bandcamp URL, so nothing may be synthesised.
    assert_eq!(first.web_url, None);

    assert_eq!(page.items[2].year, Some(2019));
    assert_eq!(page.items[1].cover, None);
    assert_eq!(page.next_offset, Some(3), "a full page continues the crawl");
}

#[test]
fn a_release_created_date_is_parsed_as_rfc_1123_and_an_invalid_one_is_none() {
    assert_eq!(
        parse_rfc1123_unix("20 Aug 2026 06:48:08 GMT"),
        Some(1_787_208_488)
    );

    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_page1.json"));
    let provider = provider(&transport);
    let page = provider
        .list_releases(PageRequest::first(3))
        .expect("page");

    assert_eq!(page.items[0].created_unix, Some(1_787_208_488));
    // The second fixture album carries ISO 8601, which this store does not send. It
    // must be `None` and must not fail the listing.
    assert_eq!(page.items[1].created_unix, None);
}

#[test]
fn the_crawl_stops_on_a_short_page_and_reports_each_page() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_page1.json"));
    transport.push_response(json("getAlbumList2_page2.json"));
    let provider = provider(&transport);

    let mut seen = Vec::new();
    let crawl = provider
        .crawl_releases(
            CrawlOptions {
                page_size: 3,
                ..CrawlOptions::default()
            },
            |progress| {
                seen.push(*progress);
                CrawlDecision::Continue
            },
        )
        .expect("crawl");

    assert_eq!(crawl.stop, CrawlStop::EndOfCollection);
    assert!(crawl.is_complete());
    assert_eq!(crawl.releases.len(), 4);
    assert_eq!(
        crawl.pages,
        vec![
            CrawlPage {
                offset: 0,
                items: 3
            },
            CrawlPage {
                offset: 3,
                items: 1
            },
        ]
    );
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].releases_so_far, 4);
    assert_eq!(seen[1].offset, 3);
    assert_eq!(transport.call_count(), 2);
    assert_eq!(query_param(&transport.urls()[1], "offset").as_deref(), Some("3"));
}

#[test]
fn an_empty_collection_is_a_complete_empty_crawl() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_empty.json"));
    let provider = provider(&transport);

    let crawl = provider.crawl_all_releases().expect("crawl");
    assert!(crawl.is_complete());
    assert!(crawl.is_empty());
    assert_eq!(transport.call_count(), 1);
}

#[test]
fn the_crawl_respects_the_page_cap_and_the_release_cap() {
    // A full page every time: only the caps can stop this.
    let transport = Arc::new(CountingTransport::new());
    for _ in 0..2 {
        transport.push_response(json("getAlbumList2_page1.json"));
    }
    let provider = provider(&transport);

    let page_capped = provider
        .crawl_releases(
            CrawlOptions {
                page_size: 3,
                max_pages: Some(1),
                max_releases: 20_000,
            },
            |_| CrawlDecision::Continue,
        )
        .expect("crawl");
    assert_eq!(page_capped.stop, CrawlStop::PageCap);
    assert!(!page_capped.is_complete());
    assert_eq!(page_capped.releases.len(), 3);
    assert_eq!(transport.call_count(), 1);

    let release_capped = provider
        .crawl_releases(
            CrawlOptions {
                page_size: 3,
                max_pages: None,
                max_releases: 2,
            },
            |_| CrawlDecision::Continue,
        )
        .expect("crawl");
    assert_eq!(release_capped.stop, CrawlStop::ReleaseCap);
    assert_eq!(release_capped.releases.len(), 2);
    assert_eq!(transport.call_count(), 2);
}

#[test]
fn the_crawl_can_be_cancelled_from_the_callback() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_page1.json"));
    let provider = provider(&transport);

    let crawl = provider
        .crawl_releases(
            CrawlOptions {
                page_size: 3,
                ..CrawlOptions::default()
            },
            |_| CrawlDecision::Stop,
        )
        .expect("crawl");
    assert_eq!(crawl.stop, CrawlStop::Cancelled);
    assert!(!crawl.is_complete());
    assert_eq!(transport.call_count(), 1);
}

#[test]
fn an_album_maps_to_tracks_with_the_declared_fields_kept_apart() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbum_album.json"));
    let provider = provider(&transport);

    let detail = provider
        .get_release(&StoreReleaseId::new("a:1000000001"))
        .expect("release");
    assert_eq!(detail.release.title, "Fixture Album One");
    assert_eq!(detail.release.artist, "Fixture Artist");
    assert_eq!(detail.release.web_url, None);
    assert_eq!(detail.tracks.len(), 2);

    let mp3 = &detail.tracks[0];
    assert_eq!(mp3.id, StoreTrackRef::new("t:2000000001"));
    assert_eq!(mp3.title, "Fixture Track A");
    assert_eq!(mp3.track_number, Some(1));
    assert_eq!(mp3.disc_number, Some(1), "an absent disc means one disc");
    assert_eq!(mp3.duration_ms, Some(600_000));
    assert_eq!(mp3.suffix.as_deref(), Some("mp3"));
    assert_eq!(mp3.content_type.as_deref(), Some("audio/mpeg"));
    assert_eq!(mp3.declared_bytes, Some(19_200_000));
    assert_eq!(mp3.declared_bitrate_kbps, Some(320));

    let flac = &detail.tracks[1];
    assert_eq!(flac.suffix.as_deref(), Some("flac"));
    assert_eq!(flac.declared_bitrate_kbps, None);

    assert_eq!(
        query_param(&transport.urls()[0], "id").as_deref(),
        Some("a:1000000001")
    );
    assert!(transport.urls()[0].contains("getAlbum.view"));
}

#[test]
fn a_single_track_release_keeps_ids_opaque_and_ignores_song_album() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbum_single.json"));
    let provider = provider(&transport);

    let single_id = StoreReleaseId::new("t:2000000009");
    let detail = provider.get_release(&single_id).expect("single");
    // The same value is both the release id and the track reference; nothing may
    // branch on a prefix or assume they differ.
    assert_eq!(detail.release.id, single_id);
    assert_eq!(detail.tracks.len(), 1);
    assert_eq!(detail.tracks[0].id.as_str(), single_id.as_str());
    // `song.album` holds the ARTIST name in this fixture, as measured on the real
    // account; the release title must come from the album object instead.
    assert_eq!(detail.release.title, "Fixture Single");
    assert_ne!(detail.release.title, detail.release.artist);
    assert_eq!(detail.release.track_count, Some(1));
    // Both ids are usable for their own endpoints.
    assert_eq!(
        query_param(&transport.urls()[0], "id").as_deref(),
        Some("t:2000000009")
    );
}

#[test]
fn a_release_web_url_is_always_none_on_every_path() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_page1.json"));
    transport.push_response(json("getAlbum_album.json"));
    transport.push_response(json("getArtist.json"));
    let provider = provider(&transport);

    let page = provider
        .list_releases(PageRequest::first(3))
        .expect("page");
    assert!(page.items.iter().all(|release| release.web_url.is_none()));

    let detail = provider
        .get_release(&StoreReleaseId::new("a:1000000001"))
        .expect("release");
    assert!(detail.release.web_url.is_none());

    let artist = provider
        .get_artist(&StoreArtistId::new("b:4000000001"))
        .expect("artist");
    assert!(artist.releases.iter().all(|release| release.web_url.is_none()));
    assert_eq!(artist.releases.len(), 2);
    assert_eq!(artist.artist.name, "Fixture Artist");
    assert_eq!(artist.artist.album_count, Some(2));
}

#[test]
fn artists_are_listed_from_the_index_and_carry_their_releases() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getArtists.json"));
    transport.push_response(json("getArtist.json"));
    let provider = provider(&transport);

    let artists = provider.list_artists().expect("artists");
    assert_eq!(artists.len(), 2);
    assert_eq!(artists[0].id, StoreArtistId::new("b:4000000002"));
    assert_eq!(artists[1].id, StoreArtistId::new("b:4000000001"));
    assert_eq!(artists[1].name, "Fixture Artist");
    assert_eq!(artists[1].album_count, Some(2));
    assert_eq!(
        artists[1].cover.as_ref().map(|cover| cover.id.as_str()),
        Some("ca:3000000001")
    );

    assert!(transport.urls()[0].contains("getArtists.view"));
    assert_eq!(transport.call_count(), 1, "one listing request, no per-artist calls");

    let detail = provider.get_artist(&artists[1].id).expect("artist");
    assert_eq!(detail.artist.id, artists[1].id);
    assert_eq!(detail.releases.len(), 2);
    assert!(transport.urls()[1].contains("getArtist.view"));
    assert_eq!(
        query_param(&transport.urls()[1], "id").as_deref(),
        Some("b:4000000001")
    );
}

#[test]
fn search_is_best_effort_and_an_empty_result_is_not_an_error() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("search3_empty.json"));
    transport.push_response(json("error_bad_version.json"));
    transport.push_response(json("search3_hits.json"));
    let provider = provider(&transport);

    // An empty result: a miss, never "not owned".
    assert_eq!(provider.search_tracks("We're Getting Older Baby"), Ok(vec![]));
    // A server without search3 is still not an error.
    assert_eq!(provider.search_tracks("Sable"), Ok(vec![]));

    let hits = provider.search_tracks("Fixture Track").expect("hits");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].track.title, "Fixture Track A");
    assert_eq!(
        hits[0].release_id.as_ref().map(|id| id.as_str()),
        Some("a:1000000001")
    );
    assert_eq!(hits[0].artist.as_deref(), Some("Fixture Artist"));
    // An empty query makes no request at all.
    assert_eq!(provider.search_tracks("   "), Ok(vec![]));
    assert_eq!(transport.call_count(), 3);
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

#[test]
fn download_bad_version_falls_back_to_stream_and_says_so() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("error_bad_version.json"));
    let provider = provider(&transport);

    let source = provider
        .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::PreferOriginal)
        .expect("falls back to the stream");
    assert!(
        source.stream_fallback,
        "the caller must be able to see that the original was substituted"
    );
    assert!(source.url.expose().contains("/rest/stream.view?"), "{}", source.url);
    assert_eq!(query_param(source.url.expose(), "id").as_deref(), Some(TRACK));
    assert_eq!(source.format, AudioFormat::Mp3);
    // The stream's real bitrate is only knowable after downloading it, and the
    // declared `bitRate` belongs to a different encode.
    assert_eq!(source.quality, AudioQuality::Lossy { bitrate_kbps: None });
    assert_eq!(source.expected_bytes, None);
    assert!(!format!("{source:?}").contains("t="), "{source:?}");

    // `download` was tried first, exactly once, and the answer was cached.
    let calls = transport.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].url.contains("/rest/download.view?"));
    assert_eq!(query_param(&calls[0].url, "id").as_deref(), Some(TRACK));

    let again = provider
        .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::PreferOriginal)
        .expect("cached");
    assert!(again.stream_fallback);
    assert_eq!(transport.call_count(), 1, "the capability answer is remembered");
}

#[test]
fn original_or_stream_falls_back_without_an_error() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("error_bad_version.json"));
    let provider = provider(&transport);

    let source = provider
        .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::OriginalOrStream)
        .expect("stream is acceptable");
    assert!(source.stream_fallback);
    assert!(source.url.expose().contains("/rest/stream.view?"));
}

#[test]
fn stream_only_never_tries_download() {
    let transport = Arc::new(CountingTransport::new());
    let provider = provider(&transport);

    let source = provider
        .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::StreamOnly)
        .expect("stream");
    assert!(!source.stream_fallback);
    assert!(source.url.expose().contains("/rest/stream.view?"));
    assert_eq!(source.quality, AudioQuality::Lossy { bitrate_kbps: None });
    assert_eq!(transport.call_count(), 0, "no request is needed to point at a stream");
}

#[test]
fn a_working_download_is_reported_as_the_original() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "audio/flac".into()),
            ("Content-Length".into(), "45000000".into()),
        ],
        body: b"fLaC\x00\x00\x00\x22".to_vec(),
        final_url: format!("{BASE}/rest/download.view"),
        redirects: Vec::new(),
    });
    let provider = provider(&transport);

    let source = provider
        .fetch_track_audio(&StoreTrackRef::new("t:2000000002"), AudioPreference::PreferOriginal)
        .expect("original");
    assert!(!source.stream_fallback);
    assert_eq!(source.format, AudioFormat::Flac);
    assert_eq!(source.quality, AudioQuality::Lossless);
    assert_eq!(source.expected_bytes, Some(45_000_000));
    assert!(source.url.expose().contains("/rest/download.view?"));

    let caps = provider.capabilities();
    assert!(caps.download);
    assert!(!caps.download_unverified);

    // The capability is now known, so a later call needs no probe request.
    let _ = provider
        .fetch_track_audio(&StoreTrackRef::new("t:2000000002"), AudioPreference::PreferOriginal)
        .expect("cached");
    assert_eq!(transport.call_count(), 1);
}

#[test]
fn a_format_hint_is_passed_through_to_download() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("error_bad_version.json"));
    let provider = provider(&transport);

    provider
        .fetch_track_audio_with(
            &StoreTrackRef::new(TRACK),
            AudioPreference::PreferOriginal,
            Some(AudioFormat::Flac),
        )
        .expect("falls back");
    assert_eq!(
        query_param(&transport.urls()[0], "format").as_deref(),
        Some("flac")
    );
}

#[test]
fn capabilities_start_with_download_false_and_marked_unverified() {
    let transport = Arc::new(CountingTransport::new());
    let provider = provider(&transport);
    let caps = provider.capabilities();
    assert!(!caps.download, "never claim download before it works");
    assert!(caps.download_unverified);
    assert!(caps.stream);
    assert!(caps.cover_art);
    assert!(!caps.playlists);
    assert_eq!(provider.id().as_str(), "bandcamp");
    assert_eq!(provider.display_name(), "Bandcamp");
}

#[test]
fn probe_download_reports_false_against_the_bad_version_fixture() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getAlbumList2_page1.json"));
    transport.push_response(json("getAlbum_album.json"));
    transport.push_response(json("error_bad_version.json"));
    let provider = provider(&transport);

    // No track or album id is hard-coded: the probe finds its own sample track.
    assert_eq!(provider.probe_download(), Ok(false));
    assert!(!provider.capabilities().download);
    assert!(provider.capabilities().download_unverified);
    let urls = transport.urls();
    assert_eq!(urls.len(), 3, "list, album detail, then one download attempt");
    assert!(urls[0].contains("getAlbumList2.view"));
    assert!(urls[1].contains("getAlbum.view"));
    assert!(urls[2].contains("download.view"));
    assert_eq!(
        query_param(&urls[2], "id").as_deref(),
        Some("t:2000000001"),
        "the probe uses a track from the collection"
    );

    // The answer is remembered: a later preference for originals costs no request.
    let source = provider
        .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::PreferOriginal)
        .expect("stream");
    assert!(source.stream_fallback);
    assert_eq!(transport.call_count(), 3);
}

#[test]
fn probe_download_flips_the_capability_only_when_audio_comes_back() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "audio/mpeg".into())],
        body: fixture("download.mp3"),
        final_url: format!("{BASE}/rest/download.view"),
        redirects: Vec::new(),
    });
    let provider = provider(&transport);

    assert_eq!(
        provider.probe_download_track(&StoreTrackRef::new(TRACK)),
        Ok(true)
    );
    let caps = provider.capabilities();
    assert!(caps.download, "only a real audio body proves download");
    assert!(!caps.download_unverified);

    // A confirmed `download` is then used directly by a caller preferring originals.
    let source = provider
        .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::PreferOriginal)
        .expect("original");
    assert!(!source.stream_fallback);
    assert!(source.url.expose().contains("/rest/download.view?"));
    assert_eq!(source.format, AudioFormat::Mp3);
    assert_eq!(transport.call_count(), 1, "the probe answer is reused");
}


// ---------------------------------------------------------------------------
// Cover art
// ---------------------------------------------------------------------------

#[test]
fn cover_art_returns_sniffed_image_bytes_and_rejects_anything_else() {
    let transport = Arc::new(CountingTransport::new());
    let image = fixture("cover.jpg");
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "image/jpeg".into())],
        body: image.clone(),
        final_url: format!("{BASE}/rest/getCoverArt.view"),
        redirects: Vec::new(),
    });
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "text/html".into())],
        body: fixture("fastly_challenge.html"),
        final_url: format!("{BASE}/rest/getCoverArt.view"),
        redirects: Vec::new(),
    });
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: fixture("error_bad_version.json"),
        final_url: format!("{BASE}/rest/getCoverArt.view"),
        redirects: Vec::new(),
    });
    let provider = provider(&transport);
    let id = StoreCoverId::new("ca:3000000001");

    assert_eq!(provider.cover_art(&id), Ok(image));
    assert_eq!(provider.cover_art(&id), Err(StoreError::Challenged));
    assert_eq!(
        provider.cover_art(&id),
        Err(StoreError::Unimplemented {
            endpoint: "getCoverArt".to_string()
        })
    );
    assert!(transport.urls()[0].contains("getCoverArt.view"));
    assert_eq!(
        query_param(&transport.urls()[0], "id").as_deref(),
        Some("ca:3000000001")
    );
}

#[test]
fn the_cover_byte_cap_is_the_callers() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "image/jpeg".into())],
        body: fixture("cover.jpg"),
        final_url: format!("{BASE}/rest/getCoverArt.view"),
        redirects: Vec::new(),
    });
    let provider = provider(&transport).with_cover_byte_cap(4096);
    provider
        .cover_art(&StoreCoverId::new("ca:3000000001"))
        .expect("cover");
    assert_eq!(transport.calls()[0].max_bytes, 4096);
}

// ---------------------------------------------------------------------------
// Redaction, User-Agent and allowlist
// ---------------------------------------------------------------------------

#[test]
fn no_error_ever_renders_a_credential_bearing_url() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(reply(500, Vec::new()));
    transport.push_response(reply(200, fixture("fastly_challenge.html")));
    transport.push_response(reply(500, Vec::new()));
    transport.push_response(json("error_failed_auth.json"));
    transport.push_response(reply(200, b"not json".to_vec()));
    transport.push_response(reply(429, fixture("error_rate_limited.json")));
    let provider = provider(&transport);

    let errors = vec![
        provider.verify_credentials().expect_err("500"),
        provider.verify_credentials().expect_err("challenge"),
        provider
            .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::PreferOriginal)
            .expect_err("a rejected login is not a fallback"),
        provider
            .list_releases(PageRequest::default())
            .expect_err("failed envelope"),
        provider
            .get_release(&StoreReleaseId::new("a:1"))
            .expect_err("not json"),
        provider
            .get_artist(&StoreArtistId::new("b:1"))
            .expect_err("rate limited"),
        // The queue is empty now: a transport failure that names the URL.
        provider
            .cover_art(&StoreCoverId::new("ca:1"))
            .expect_err("no queued response"),
    ];
    assert_eq!(
        errors.iter().map(StoreError::code).collect::<Vec<_>>(),
        vec![
            "bad_credentials",
            "challenged",
            "bad_credentials",
            "bad_credentials",
            "malformed",
            "rate_limited",
            "transport",
        ]
    );

    // Every value that must never be rendered: the login and each request's own
    // salt and token (read from the recorded URLs, which is where they live).
    let mut secrets: Vec<String> = vec![USER.to_string(), PASSWORD.to_string()];
    for url in transport.urls() {
        for name in ["u", "s", "t"] {
            if let Some(value) = query_param(&url, name) {
                assert!(!value.is_empty());
                secrets.push(value);
            }
        }
    }
    assert!(secrets.len() >= 21, "the requests really carried credentials");

    for error in &errors {
        let shown = error.to_string();
        assert!(!shown.contains(USER), "{shown}");
        assert!(!shown.contains(PASSWORD), "{shown}");
        for secret in &secrets {
            assert!(!shown.contains(secret.as_str()), "error leaked a secret: {shown}");
        }
        assert!(
            !shown.contains("<redacted>") || shown.contains("/rest/"),
            "only the path may survive redaction: {shown}"
        );
        for fragment in ["u=", "s=", "t="] {
            assert!(
                !shown.contains(fragment),
                "error rendered URL material {fragment:?}: {shown}"
            );
        }
    }
}

#[test]
fn every_request_sends_a_user_agent_and_stays_on_the_allowlist() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getArtists.json"));
    transport.push_response(json("getAlbumList2_page1.json"));
    transport.push_response(json("getAlbum_album.json"));
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "image/jpeg".into())],
        body: fixture("cover.jpg"),
        final_url: format!("{BASE}/rest/getCoverArt.view"),
        redirects: Vec::new(),
    });
    let provider = provider(&transport);

    provider.verify_credentials().expect("credentials");
    provider
        .list_releases(PageRequest::first(3))
        .expect("listing");
    provider
        .get_release(&StoreReleaseId::new("a:1000000001"))
        .expect("release");
    provider
        .cover_art(&StoreCoverId::new("ca:3000000001"))
        .expect("cover");
    provider
        .fetch_track_audio(&StoreTrackRef::new(TRACK), AudioPreference::StreamOnly)
        .expect("stream");

    let hosts = AuthenticatedHosts::default();
    let calls = transport.calls();
    assert_eq!(calls.len(), 4, "the stream path makes no request");
    for call in &calls {
        let user_agent = call
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
            .map(|(_, value)| value.as_str());
        assert!(
            user_agent.is_some_and(|value| value.starts_with("Plippa/")),
            "no User-Agent on {}",
            plippa_store::redact_url(&call.url)
        );
        assert!(call.max_bytes > 0);
        assert!(
            hosts.authenticated(&call.url),
            "request left the allowlist: {}",
            plippa_store::redact_url(&call.url)
        );
    }
}

#[test]
fn a_redirect_off_the_allowlist_is_refused() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(TransportResponse {
        status: 200,
        headers: Vec::new(),
        body: fixture("cover.jpg"),
        final_url: "https://evil.example/cover.jpg".to_string(),
        redirects: Vec::new(),
    });
    let provider = provider(&transport);

    let error = provider
        .cover_art(&StoreCoverId::new("ca:3000000001"))
        .expect_err("off-allowlist redirect");
    assert_eq!(error.code(), "policy");
}

#[test]
fn a_redirect_to_the_audio_cdn_is_allowed() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".into(), "image/jpeg".into())],
        body: fixture("cover.jpg"),
        final_url: "https://pds.bcbits.com/img/a3000000001_10.jpg".to_string(),
        redirects: vec![
            format!("{BASE}/rest/getCoverArt.view"),
            "https://pds.bcbits.com/img/a3000000001_10.jpg".to_string(),
        ],
    });
    let provider = provider(&transport);

    // The measured answer: `stream` redirects off bandcamp.com onto the CDN, so that
    // hop must be permitted while any other host stays refused.
    let image = provider
        .cover_art(&StoreCoverId::new("ca:3000000001"))
        .expect("CDN redirect");
    assert_eq!(image, fixture("cover.jpg"));
}

#[test]
fn the_client_name_is_sent_but_never_gated_on() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("ping_foreign_client.json"));
    transport.push_response(json("getArtists.json"));
    let provider = provider(&transport);

    // `c` is not validated by the server, so the provider must not validate the
    // server's self-description either.
    let info = provider.ping().expect("ping");
    assert_eq!(info.server_type.as_deref(), Some("SomeOtherServer"));
    assert!(!info.open_subsonic);
    assert_eq!(provider.verify_credentials(), Ok(()));
    assert_eq!(
        query_param(&transport.urls()[0], "c").as_deref(),
        Some("plippa")
    );
    assert!(transport.urls()[1].contains("getArtists.view"));
}

#[test]
fn the_provider_is_usable_as_a_shared_trait_object() {
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(json("getArtists.json"));
    let provider: Arc<dyn StoreProvider> = Arc::new(provider(&transport));

    let worker = {
        let provider = Arc::clone(&provider);
        std::thread::spawn(move || provider.verify_credentials())
    };
    assert_eq!(worker.join().expect("thread"), Ok(()));
    assert_eq!(provider.id().as_str(), "bandcamp");
}
