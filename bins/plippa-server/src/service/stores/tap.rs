//! The composite tap answer (BCS_07).
//!
//! One round trip from the device after a Noubin tap: "is this release mine, and if not, how
//! do I buy it?". The answer composes BCS_06's matcher and the BCS_03 collection cache.
//!
//! Two kinds of tag link are understood, in the order the Noubin standard intends:
//!
//! 1. **A Noubin URL** (the normal case): the tag names a Noubin page, and that page's
//!    `releasePlatforms` list carries the Bandcamp link. Resolving it needs one web fetch, so
//!    the first call returns `state=lookup_consent` with the host to show and sends nothing;
//!    the caller repeats the call with `consent=true` and then gets the real answer.
//! 2. **A Bandcamp release URL** (the fallback for a tag that carries one directly): matched
//!    against the cache with no network at all.
//!
//! ```text
//!  POST /api/stores/tap { url, expectedDuration?, expectedTitle?, consent }
//!        │
//!        ├─ no store connected            → state=not_found, blocked=no_store
//!        ├─ credentials rejected          → state=not_found, blocked=credentials_rejected
//!        ├─ <artist>.bandcamp.com link    → match_release (cache only)
//!        │    ├─ no cache file at all     → state=not_found, blocked=no_store_cache
//!        │    ├─ auto usable / owned      → state=owned_downloadable
//!        │    ├─ candidate(s) to confirm  → state=ambiguous (at most 3)
//!        │    └─ nothing                  → state=not_owned + purchaseUrl (a want is recorded)
//!        └─ Noubin URL, no consent yet    → state=lookup_consent + lookup{fetchUrl, host, …}
//!             └─ with consent=true        → fetch the page, use its Bandcamp platform link,
//!                                           then the same match_release path
//! ```
//!
//! Three rules:
//!
//! * **A blocked state is a result, not an error.** "No store", a rejected credential pair, a
//!   tag with no usable link and a Noubin page without a Bandcamp link all come back as
//!   `not_found` so the device degrades to today's unmatched screen with one honest line,
//!   never an HTTP failure the UI must decode.
//! * **The purchase link is the canonical page.** [`TapAnswer::purchase_url`] is the parsed,
//!   query-free `https://<artist>.bandcamp.com/{album,track}/<slug>` — never a token-bearing
//!   or credential-bearing URL. Plippa never touches payment.
//! * **A `not_owned` answer records interest.** The tap is remembered as a want (bounded LRU in
//!   the associations file) so the next tap after a purchase re-offers the download.
//!
//! A tag link is never fetched without `consent=true` in the request: the store needed for the
//! match is unrelated to the permission to contact the tag's own page.

use serde::{Deserialize, Serialize};

use super::matching::{
    self, parse_bandcamp_url, BandcampUrl, Candidate, MatchRequest, MatchResult, Want,
};
use super::{log_line, StoreApiError, StoreHub};
use crate::web::fetch::WebError;
use plippa_noubin::{is_url_noubin_key, NoudataDocument};

/// Most candidates a tap answer carries: a person can pick from three, no more.
pub const MAX_TAP_CANDIDATES: usize = 3;

/// Largest JSON body the tap routes accept (a URL plus two optional hints).
pub const MAX_TAP_BODY: usize = 8 * 1024;

/// What [`StoreHub::release_link`] decided.
enum Resolved {
    /// A Bandcamp release page, ready for the cache-only matcher.
    Link(BandcampUrl),
    /// A complete answer that needs no matching (consent prompt, blocked, or no link).
    Answer(TapAnswer),
}

/// The unmatched fallback: an honest one-liner, never an HTTP error.
fn unmatched(message: &str) -> TapAnswer {
    TapAnswer {
        state: TapState::NotFound,
        match_result: None,
        purchase_url: None,
        candidates: Vec::new(),
        blocked: None,
        lookup: None,
        message: message.to_string(),
    }
}

/// A tap that could not run, with the reason the screen words differently.
fn blocked(reason: TapBlocked, message: &str) -> TapAnswer {
    TapAnswer {
        state: TapState::NotFound,
        match_result: None,
        purchase_url: None,
        candidates: Vec::new(),
        blocked: Some(reason),
        lookup: None,
        message: message.to_string(),
    }
}

/// Map a web-hub failure onto the tap screen's blocked reasons.
fn web_blocked(error: &WebError) -> TapAnswer {
    match error {
        WebError::ConsentRequired => blocked(
            TapBlocked::ConsentNeeded,
            "Reading this release page needs web lookups, which are off.",
        ),
        WebError::Disabled(reason) => blocked(TapBlocked::WebUnavailable, reason),
        WebError::Offline(_) => blocked(
            TapBlocked::Offline,
            "There is no internet connection to read this release page.",
        ),
        WebError::NotAUrl => unmatched("This tag does not point at a release page."),
        // Everything else (a refused address, a 404, a document that is not a release, a
        // timeout, a busy fetcher) is reported the same way: the page could not be read now.
        _ => blocked(
            TapBlocked::Offline,
            "This release page could not be read. Try again.",
        ),
    }
}

/// The Bandcamp release a Noubin page declares.
///
/// The standard keeps platform links on the release document rather than on the tag, so this
/// is the primary way a tap finds its store page. A `releasePlatforms` entry named Bandcamp
/// wins; any other entry whose URL parses as a Bandcamp release page is accepted as a fallback.
fn bandcamp_release_link(doc: &NoudataDocument) -> Option<String> {
    let platforms = doc.release_data.as_ref()?.release_platforms.as_ref()?;
    let mut fallback = None;
    for platform in platforms {
        if parse_bandcamp_url(&platform.platform_url).is_err() {
            continue;
        }
        if platform.platform_name.trim().eq_ignore_ascii_case("bandcamp") {
            return Some(platform.platform_url.clone());
        }
        if fallback.is_none() {
            fallback = Some(platform.platform_url.clone());
        }
    }
    fallback
}

/// What the device should show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapState {
    /// The release is in the connected store's collection: offer the download.
    OwnedDownloadable,
    /// The release is not owned: offer the purchase link.
    NotOwned,
    /// One to [`MAX_TAP_CANDIDATES`] candidates need a person's decision.
    Ambiguous,
    /// The tag is a Noubin page that must be read before the release can be named: show
    /// [`TapAnswer::lookup`] and repeat the call with `consent=true` when the person agrees.
    LookupConsent,
    /// Nothing could be answered: fall back to the unmatched screen.
    NotFound,
}

/// Why a tap could not be answered. Distinct because the device says something different for
/// each; `None` means the lookup itself ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapBlocked {
    /// No store credentials are stored (or the secrets directory is unusable).
    NoStore,
    /// The store refused the stored credentials: they must be fixed in the portal.
    CredentialsRejected,
    /// Reading the tag's Noubin page needs consent, and it was not given.
    #[serde(rename = "consent_required")]
    ConsentNeeded,
    /// Web lookups are switched off on this Plippa (`--no-web` / `PLIPPA_WEB=off`).
    WebUnavailable,
    /// No network, or the lookup itself failed.
    Offline,
    /// The store is connected but this library has never synced its collection (a new stick,
    /// a first boot, or the cache file was removed). Distinct from a *stale* cache, which is
    /// still matched and reported through the match's `cache_stale`.
    NoStoreCache,
}

/// What reading the tag's Noubin page would contact, shown **before** anything is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformLookup {
    /// The tag's key, as sanitised for display.
    pub noubin_key: String,
    /// The page a lookup would fetch (`…/web.noudata`).
    pub fetch_url: String,
    /// The host named in the consent sentence.
    pub host: String,
    /// The sentence the screen may show verbatim.
    pub consent_message: String,
}

/// `POST /api/stores/tap` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapRequest {
    /// A `releasePlatforms[].platformURL` value (the Noubin key), or a Bandcamp release URL.
    pub url: String,
    /// `mediaList[].uniteData.expectedLength` in seconds, when the caller has it.
    #[serde(default)]
    pub expected_duration: Option<u64>,
    /// The release title the tag declared, for the device's title line while it looks up.
    #[serde(default)]
    pub expected_title: Option<String>,
    /// `true` only after the person agreed to contact the host named in
    /// [`TapAnswer::lookup`]. Required for a Noubin URL; ignored by the Bandcamp fast path.
    #[serde(default)]
    pub consent: Option<bool>,
}

/// The tap answer. `state` is the decision; the rest is what the screen draws.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TapAnswer {
    pub state: TapState,
    /// The full BCS_06 result (the reasons, the confidence, the cache staleness), as `match`
    /// on the wire. `None` only when the answer is blocked or the link is not a store page.
    #[serde(rename = "match")]
    pub match_result: Option<MatchResult>,
    /// The page to open in the buyer's own browser, canonical and query-free. `None` unless
    /// the state is [`TapState::NotOwned`].
    pub purchase_url: Option<String>,
    /// At most [`MAX_TAP_CANDIDATES`] candidates, best first.
    pub candidates: Vec<Candidate>,
    /// Set when the answer is "fall back", so the device can say why.
    pub blocked: Option<TapBlocked>,
    /// Set when the state is [`TapState::LookupConsent`]: what a lookup would contact.
    pub lookup: Option<PlatformLookup>,
    /// A short sentence the device may show verbatim. No codes, no credentials.
    pub message: String,
}

/// `POST /api/stores/bandcamp/want` body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WantRequest {
    pub url: String,
}

/// `POST /api/stores/bandcamp/want` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WantAnswer {
    pub ok: bool,
    /// Wants recorded after this call.
    pub wants: usize,
    /// Canonical identity of the link that was recorded.
    pub url_key: String,
    /// How many entries the bounded list can hold.
    pub max_wants: usize,
}

impl StoreHub {
    /// Answer one device tap. Cache only, never the network.
    pub fn tap(&self, request: &TapRequest) -> Result<TapAnswer, StoreApiError> {
        let url = request.url.trim();
        if url.is_empty() || url.len() > 2048 {
            return Err(StoreApiError::InvalidRequest(
                "url must be a store link of at most 2048 bytes".to_string(),
            ));
        }
        let status = self.status();
        let store = &status.stores[0];
        if !status.secrets_available || !store.connected {
            let message = if !status.secrets_available {
                status
                    .secrets_reason
                    .clone()
                    .unwrap_or_else(|| "This Plippa has no store set up yet".to_string())
            } else {
                "No store is connected yet. Connect Bandcamp in the web portal.".to_string()
            };
            return Ok(TapAnswer {
                state: TapState::NotFound,
                match_result: None,
                purchase_url: None,
                candidates: Vec::new(),
                blocked: Some(TapBlocked::NoStore),
                lookup: None,
                message,
            });
        }
        if store.rejected {
            return Ok(TapAnswer {
                state: TapState::NotFound,
                match_result: None,
                purchase_url: None,
                candidates: Vec::new(),
                blocked: Some(TapBlocked::CredentialsRejected),
                lookup: None,
                message: "Bandcamp refused the stored sign-in. Fix it in the web portal."
                    .to_string(),
            });
        }

        // The tag names either the store page itself (the fallback the standard allows) or,
        // normally, a Noubin page whose `releasePlatforms` carry the store link.
        let parsed = match self.release_link(request, url)? {
            Resolved::Link(parsed) => parsed,
            // LookupConsent or a "not a release" fallback: already a complete answer.
            Resolved::Answer(answer) => return Ok(answer),
        };
        let url = parsed.url.clone();

        let result = self.match_release(&MatchRequest {
            url,
            expected_duration: request.expected_duration,
            // The tap endpoint is the pre-consent path: no `search3`, ever.
            allow_search: false,
        })?;
        let candidates: Vec<Candidate> = result
            .candidates
            .iter()
            .take(MAX_TAP_CANDIDATES)
            .cloned()
            .collect();

        let state = if result.owned.is_some() || result.auto_usable {
            TapState::OwnedDownloadable
        } else if !candidates.is_empty() {
            TapState::Ambiguous
        } else {
            TapState::NotOwned
        };

        // With nothing to match against, "you do not own this" would be a lie and a dead end:
        // say the library has not been synced, and record no want (the person may own it).
        if state == TapState::NotOwned && self.cache_snapshot().is_none() {
            let mut answer = blocked(
                TapBlocked::NoStoreCache,
                "This device has not synced your Bandcamp library yet.",
            );
            answer.match_result = Some(result);
            return Ok(answer);
        }

        let message = match state {
            TapState::OwnedDownloadable => {
                let count = candidates
                    .first()
                    .and_then(|c| c.track_count)
                    .map(|n| format!(" ({n} tracks)"))
                    .unwrap_or_default();
                format!("You own this on Bandcamp{count}. Download it to this Plippa?")
            }
            TapState::Ambiguous if candidates.len() == 1 => {
                "One possible release was found. Is this the one?".to_string()
            }
            TapState::Ambiguous => "Which release is this?".to_string(),
            TapState::NotOwned => "You do not own this on Bandcamp.".to_string(),
            // Only `release_link` produces this state, and it returns before matching.
            TapState::LookupConsent => {
                "This tag needs its release page read to find the store link.".to_string()
            }
            TapState::NotFound => "This release is not in your collection.".to_string(),
        };

        if state == TapState::NotOwned {
            // Remember the interest so the next tap after the purchase re-offers the
            // download. A cache miss is not a failure of the answer.
            if let Some(root) = self.library_root() {
                let want = Want {
                    url_key: parsed.url_key(),
                    url: parsed.url.clone(),
                    wanted_at: super::now_secs(),
                    title: Some(parsed.title_hint()),
                    artist: Some(parsed.artist_hint()),
                };
                if let Err(error) = matching::record_want(&root, want) {
                    log_line(&format!("the tap interest could not be recorded: {error}"));
                }
            }
        }

        Ok(TapAnswer {
            state,
            match_result: Some(result),
            purchase_url: (state == TapState::NotOwned).then(|| parsed.url.clone()),
            candidates,
            blocked: None,
            lookup: None,
            message,
        })
    }

    /// Turn the tag's link into a Bandcamp release page.
    ///
    /// A tag that already names `<artist>.bandcamp.com/...` needs nothing else (the standard's
    /// fallback). A Noubin URL is the normal case: the release page declares its store links,
    /// so it must be read — consent-gated, and only once: the first call answers
    /// [`TapState::LookupConsent`] with the host, and the caller repeats with `consent=true`.
    fn release_link(
        &self,
        request: &TapRequest,
        url: &str,
    ) -> Result<Resolved, StoreApiError> {
        if let Ok(parsed) = parse_bandcamp_url(url) {
            return Ok(Resolved::Link(parsed));
        }
        if !is_url_noubin_key(url) {
            // Not a URL at all (a local tag key) or not one we can read: the unmatched screen,
            // with a line that says what was wrong rather than blaming Bandcamp.
            return Ok(Resolved::Answer(unmatched(
                "This tag does not point at a release page.",
            )));
        }
        let Some(web) = self
            .web
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return Ok(Resolved::Answer(blocked(
                TapBlocked::WebUnavailable,
                "This Plippa cannot look up the release page right now.",
            )));
        };
        // `prepare` computes what would be contacted and sends nothing.
        let prepared = match web.prepare(url) {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Resolved::Answer(web_blocked(&error))),
        };
        if request.consent != Some(true) {
            return Ok(Resolved::Answer(TapAnswer {
                state: TapState::LookupConsent,
                match_result: None,
                purchase_url: None,
                candidates: Vec::new(),
                blocked: Some(TapBlocked::ConsentNeeded),
                lookup: Some(PlatformLookup {
                    noubin_key: prepared.noubin_key,
                    fetch_url: prepared.fetch_url,
                    host: prepared.host,
                    consent_message: prepared.consent_message,
                }),
                message: "This tag needs its release page read to find the store link.".to_string(),
            }));
        }
        let held = match web.fetch_held(url, request.consent) {
            Ok(held) => held,
            Err(error) => return Ok(Resolved::Answer(web_blocked(&error))),
        };
        let Some(link) = bandcamp_release_link(&held.doc) else {
            return Ok(Resolved::Answer(unmatched(
                "This release page does not list a Bandcamp release.",
            )));
        };
        match parse_bandcamp_url(&link) {
            Ok(parsed) => Ok(Resolved::Link(parsed)),
            // The page itself declared something that is not a Bandcamp release page.
            Err(_) => Ok(Resolved::Answer(unmatched(
                "This release page does not list a Bandcamp release.",
            ))),
        }
    }

    /// Record (or refresh) interest in a link, so the next tap re-offers the download.
    pub fn want(&self, request: &WantRequest) -> Result<WantAnswer, StoreApiError> {
        let parsed = parse_bandcamp_url(&request.url).map_err(|error| match error {
            plippa_store::StoreError::Policy { reason } => StoreApiError::InvalidUrl(reason),
            other => StoreApiError::Store(other),
        })?;
        let Some(root) = self.library_root() else {
            return Err(StoreApiError::Unavailable(
                "the media library root is not available yet".to_string(),
            ));
        };
        let want = Want {
            url_key: parsed.url_key(),
            url: parsed.url.clone(),
            wanted_at: super::now_secs(),
            title: Some(parsed.title_hint()),
            artist: Some(parsed.artist_hint()),
        };
        let wants = matching::record_want(&root, want)?;
        log_line(&format!(
            "store want recorded: {} ({wants} of {} kept)",
            parsed.url_key(),
            matching::MAX_WANTS
        ));
        Ok(WantAnswer {
            ok: true,
            wants,
            url_key: parsed.url_key(),
            max_wants: matching::MAX_WANTS,
        })
    }
}
