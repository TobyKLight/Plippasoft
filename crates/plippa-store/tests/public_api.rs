//! Exercises the crate the way a downstream consumer does: through the public
//! surface only.

use std::sync::Arc;

use plippa_store::{
    AudioPreference, CountingTransport, CredentialStore, Credentials, HeaderMap, MockProvider,
    PageRequest, StoreError, StoreId, StoreProvider, StoreReleaseId, StoreTrackRef, Transport,
    TransportResponse,
};

#[test]
fn a_provider_can_be_shared_across_threads_as_a_trait_object() {
    let provider: Arc<dyn StoreProvider> = Arc::new(MockProvider::new());
    let worker = {
        let provider = Arc::clone(&provider);
        std::thread::spawn(move || {
            let page = provider.list_releases(PageRequest::first(10))?;
            Ok::<_, StoreError>(page.items.len())
        })
    };
    assert_eq!(worker.join().expect("thread").expect("listing"), 2);
    assert_eq!(provider.id().as_str(), "mock");
}

#[test]
fn the_transport_seam_is_implementable_outside_the_crate() {
    struct Uppercase;

    impl Transport for Uppercase {
        fn get(
            &self,
            url: &str,
            headers: &HeaderMap,
            max_bytes: u64,
        ) -> Result<TransportResponse, StoreError> {
            assert!(headers.iter().any(|(name, _)| name == "User-Agent"));
            assert!(max_bytes > 0);
            Ok(TransportResponse::new(
                200,
                url,
                b"{\"ok\":true}".to_vec(),
            ))
        }
    }

    let headers: HeaderMap = vec![("User-Agent".into(), "Plippa/1".into())];
    let response = Uppercase
        .get("https://bandcamp.com/rest/ping.view?u=U&s=S", &headers, 512)
        .expect("response");
    assert_eq!(response.body_text(), Some("{\"ok\":true}"));

    let double = CountingTransport::new();
    double.push_body("https://bandcamp.com/x", b"queued");
    let queued = double
        .get("https://bandcamp.com/x", &HeaderMap::new(), 64)
        .expect("queued");
    assert_eq!(queued.body_text(), Some("queued"));
}

#[test]
fn a_mock_provider_drives_a_whole_flow_without_a_network() {
    let provider = MockProvider::new();
    let ids = provider.release_ids();
    assert_eq!(ids.len(), 2);

    let release = provider.get_release(&ids[0]).expect("release");
    let source = provider
        .fetch_track_audio(&release.tracks[0].id, AudioPreference::OriginalOrStream)
        .expect("audio source");
    assert!(source.url.expose().starts_with(plippa_store::MOCK_AUDIO_HOST));

    assert_eq!(
        provider.verify_credentials(),
        Ok(()),
        "the mock accepts its own credentials"
    );
    assert_eq!(
        provider.called_methods(),
        vec!["get_release", "fetch_track_audio", "verify_credentials"]
    );

    assert_eq!(
        provider.get_release(&StoreReleaseId::new("missing")),
        Err(StoreError::NotFound)
    );
    assert_eq!(
        provider.fetch_track_audio(&StoreTrackRef::new("missing"), AudioPreference::default()),
        Err(StoreError::NotFound)
    );
}

#[test]
fn only_the_token_helper_is_reachable_from_outside_the_crate() {
    // PAR-07: the `md5` module is `pub(crate)`, so the raw primitives cannot be reached
    // from here at all (that would be a compile error in this file). Only the protocol's
    // token construction is exported; its output is checked against RFC 1321's known
    // Subsonic vector.
    assert_eq!(
        plippa_store::subsonic_token("s3cr3tPassw0rd", "1a2b3c4d5e6f7788"),
        "8ad37da65da26c816c451fc4a45df585"
    );
}

// PAR-07's "an audio source cannot be serialised" is pinned by the `compile_fail`
// doctests on `SourceUrl` and `AudioSource` (`cargo test --doc`). There is deliberately
// no test here that greps the declarations for a `Serialize` derive: it would pass with
// the derive added above the struct it looks for, and reading source text as a proxy
// for behaviour is not a test.

#[test]
fn credentials_survive_a_store_restart() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "plippa-store-public-{}-{nanos}",
        std::process::id()
    ));
    let store = CredentialStore::new(dir.join("secrets"));
    let id = StoreId::new("bandcamp").expect("slug");
    let creds = Credentials::new("fan", "secret").normalised().expect("valid");

    store.save(&id, &creds).expect("save");
    drop(store);

    let reopened = CredentialStore::new(dir.join("secrets"));
    assert_eq!(reopened.load(&id), Some(creds));
    assert!(reopened.remove(&id));
    assert_eq!(reopened.load(&id), None);

    let _ = std::fs::remove_dir_all(&dir);
}
