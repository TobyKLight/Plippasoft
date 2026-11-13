//! Ownership matching for a Bandcamp link (BCS_06).
//!
//! Given a `releasePlatforms[].platformURL` from a Noubin, decide whether the release is
//! **already in the local library**, and if it is not, which store release to download. The
//! answer is never a silent guess: it is a ranked list of candidates, each with the
//! confidence the *server* computed and the reasons for it, and the person confirms.
//!
//! ```text
//!   tap ──GET /api/stores/bandcamp/match?url=…──▶ match_release
//!                                                   │ 1. recorded source / association (disk only)
//!                                                   │ 2. artist + normalised title (cache)
//!                                                   │ 3. duration fingerprint (cache)
//!                                                   │ 4. nothing → not_in_collection (a result)
//!                                                   ▼
//!                                   MatchResult { candidates, owned, confidence, reasons }
//! ```
//!
//! Four rules shape this module:
//!
//! * **GET never touches the network.** Stages 1-4 read the library and the BCS_03 collection
//!   cache only. The store's `search3` is unreliable (it returned zero results for a title it
//!   holds — research §7), so it is only an opt-in *lead* on the `POST` variant, capped below
//!   the confirmation band.
//! * **Never guess silently.** A miss is [`NOT_IN_COLLECTION`], not an error: the caller must
//!   answer "you do not own this, here is how to buy it". A candidate below
//!   [`CONFIRM_CONFIDENCE`] is not returned at all.
//! * **The server scores, not the client.** [`StoreHub::confirm_match`] re-runs the matcher
//!   for the candidate id and ignores any `confidence` in the request body.
//! * **The URL is opaque.** A Bandcamp URL has no store id in it (the Subsonic API exposes no
//!   URL or id at all), so a URL is identified by `artist/kind/slug` with the query stripped;
//!   percent-escapes are never decoded into a path segment.

use std::fs;
use std::path::{Path, PathBuf};

use plippa_noubin::{
    collect_noudata_paths, parse_noudata, read_noudata_capped, LibraryRoot, NoudataDocument,
    MIN_LIBRARY_SCAN_DEPTH,
};
use plippa_store::{StoreError, StoreRelease};
use serde::{Deserialize, Serialize};

use super::browse::fold_for_search;
use super::{cache, log_line, now_secs, StoreApiError, StoreHub, STORE_SLUG};

// ---------------------------------------------------------------------------
// Confidence bands
// ---------------------------------------------------------------------------

/// At or above this, the match is usable without asking (a recorded source, or an exact
/// normalised title against the cache).
pub const AUTO_USABLE_CONFIDENCE: f32 = 0.9;
/// At or above this the candidate may be shown and must be confirmed by the person before
/// anything is downloaded. Below it, nothing is offered.
pub const CONFIRM_CONFIDENCE: f32 = 0.6;
/// A recorded association is a decision the person already made, not a heuristic.
pub const ASSOCIATION_CONFIDENCE: f32 = 0.99;
/// `search3` cannot produce an actionable candidate: the store misses titles it holds.
pub const SEARCH_CONFIDENCE_CAP: f32 = 0.5;
/// The `reason` a result with no candidate carries. Never an error.
pub const NOT_IN_COLLECTION: &str = "not_in_collection";

/// A track duration within this many seconds is the same recording.
const DURATION_TOLERANCE_SECS: u64 = 2;
/// Promotion for a duration fingerprint.
const DURATION_BONUS: f32 = 0.2;
/// Extra promotion when the shape matches too (one track, and the total agrees).
const DURATION_SHAPE_BONUS: f32 = 0.1;
/// No combination of weak signals may claim more than this without being exact.
const CONFIDENCE_CAP: f32 = 0.95;

/// Longest accepted Bandcamp link.
const MAX_URL_BYTES: usize = 2048;
/// Longest accepted release slug.
const MAX_SLUG_BYTES: usize = 512;
/// Longest accepted store release id in a confirm request.
const MAX_RELEASE_ID_BYTES: usize = 256;
/// An `expectedDuration` outside `1..=24 h` is a caller bug, not a duration.
const MAX_EXPECTED_DURATION_SECS: u64 = 24 * 60 * 60;
/// Most `.noudata` files a single lookup reads before it stops looking for provenance.
const MAX_LIBRARY_DOCS_SCANNED: usize = 5_000;
/// Most associations kept (oldest dropped first).
pub const MAX_ASSOCIATIONS: usize = 512;
/// Hard cap on the associations file.
const MAX_ASSOCIATIONS_BYTES: usize = 256 * 1024;
/// Largest JSON body the match routes accept.
pub const MAX_MATCH_BODY: usize = 8 * 1024;

/// Key in a `.noudata` `extra` object holding the store provenance.
const PLIPPA_STORE_SOURCE: &str = "plippaStoreSource";

// ---------------------------------------------------------------------------
// URL
// ---------------------------------------------------------------------------

/// Which side of a Bandcamp page a link points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandcampKind {
    Album,
    Track,
}

impl BandcampKind {
    /// Stable word for JSON and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Track => "track",
        }
    }
}

/// A Bandcamp release link, understood without fetching it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BandcampUrl {
    /// Canonical form for recording as `plippaSourceURL`: `https`, lower-case host, the
    /// path as written, and **no query or fragment** (tracking parameters are never part of
    /// a release identity).
    pub url: String,
    /// The sub-domain: the artist's Bandcamp slug (`sable-music`).
    pub artist_slug: String,
    /// `album` or `track`.
    pub kind: BandcampKind,
    /// The page slug, exactly as written (percent-escapes are **not** decoded).
    pub slug: String,
}

impl BandcampUrl {
    /// Identity of the page: `artist/kind/slug`, lower-case ASCII, query-free.
    ///
    /// Two links that differ only by tracking parameters, host case or a fragment share
    /// this key; two links that differ in the slug never do.
    pub fn url_key(&self) -> String {
        format!(
            "{}/{}/{}",
            self.artist_slug.to_ascii_lowercase(),
            self.kind.as_str(),
            self.slug.to_ascii_lowercase()
        )
    }

    /// The artist's name as the slug renders it (`sable-music` → `sable music`). A hint for
    /// matching and display, never written into metadata.
    pub fn artist_hint(&self) -> String {
        self.slug_words(&self.artist_slug)
    }

    /// The page title as the slug renders it, for matching and for a search query.
    pub fn title_hint(&self) -> String {
        self.slug_words(&self.slug)
    }

    /// The normalised title used to compare against cached release titles.
    fn title_key(&self) -> String {
        normalise_title(&self.title_hint())
    }

    fn slug_words(&self, slug: &str) -> String {
        decode_slug(slug).replace('-', " ")
    }
}

/// Parse `https://<artist>.bandcamp.com/{album,track}/<slug>`.
///
/// Rejects any other scheme (including `http`), any host that is not exactly three labels
/// ending in `.bandcamp.com`, userinfo, a port, a path that is not `/album/<slug>` or
/// `/track/<slug>`, and a control character in the slug. The query and fragment are dropped;
/// the slug is kept byte-for-byte so `%2F` can never become a path separator.
pub fn parse_bandcamp_url(input: &str) -> Result<BandcampUrl, StoreError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(reject("the link is empty"));
    }
    if raw.len() > MAX_URL_BYTES {
        return Err(reject("the link is longer than 2048 bytes"));
    }
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| reject("it is not an absolute URL"))?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(reject("only https:// links are accepted"));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    if authority.is_empty() {
        return Err(reject("it has no host"));
    }
    if authority.contains('@') {
        return Err(reject("it carries user information"));
    }
    if authority.contains(':') {
        return Err(reject("it names a port"));
    }
    if !authority.is_ascii() {
        return Err(reject("the host is not an ASCII domain name"));
    }
    let host = authority.to_ascii_lowercase();
    let mut labels = host.split('.');
    let artist = labels.next().unwrap_or_default();
    let (Some("bandcamp"), Some("com"), None) = (labels.next(), labels.next(), labels.next())
    else {
        return Err(reject("it is not a <artist>.bandcamp.com address"));
    };
    if !is_artist_label(artist) {
        return Err(reject("the host does not name a Bandcamp artist"));
    }
    // Query and fragment are not part of a release identity.
    let path = tail.split(['?', '#']).next().unwrap_or("");
    let mut segments = path.split('/');
    let (Some(""), Some(kind), Some(slug), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(reject("the path is not /album/<slug> or /track/<slug>"));
    };
    let kind = match kind {
        "album" => BandcampKind::Album,
        "track" => BandcampKind::Track,
        _ => return Err(reject("the path is not /album/<slug> or /track/<slug>")),
    };
    if !is_release_slug(slug) {
        return Err(reject("the release slug is empty or unsafe"));
    }
    Ok(BandcampUrl {
        url: format!("https://{host}{path}"),
        artist_slug: artist.to_string(),
        kind,
        slug: slug.to_string(),
    })
}

fn reject(reason: &str) -> StoreError {
    StoreError::policy(format!("not a Bandcamp release link: {reason}"))
}

/// A Bandcamp artist sub-domain: a DNS label, never `www`.
fn is_artist_label(label: &str) -> bool {
    !label.is_empty()
        && label != "www"
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_release_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_SLUG_BYTES
        && slug != "."
        && slug != ".."
        && !slug.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Decode `%XX` and `+` for a *title only*.
///
/// This result is never used as a path, a file name or an identity: the slug stays encoded
/// everywhere it matters, so a `%2F` in a link cannot be turned into a directory separator.
fn decode_slug(slug: &str) -> String {
    let bytes = slug.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1]);
                let low = hex_value(bytes[index + 2]);
                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push(high << 4 | low);
                        index += 3;
                    }
                    _ => {
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

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------

/// Fold a title (or artist) to the form the matcher compares.
///
/// Lower-cases, strips diacritics (the same table the collection search uses), drops
/// apostrophes so `we're` and the Bandcamp slug `were-…` agree, expands `&` to `and` so
/// `rock & roll` and `rock-and-roll` agree, collapses every other run of non-alphanumeric
/// characters to one space, and drops a leading `The`.
pub fn normalise_title(input: &str) -> String {
    let mut expanded = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '\'' | '\u{2018}' | '\u{2019}' | '\u{00b4}' | '`' => {}
            '&' => expanded.push_str(" and "),
            other => expanded.push(other),
        }
    }
    let folded = fold_for_search(&expanded);
    let mut tokens: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.len() > 1 && tokens[0] == "the" {
        tokens.remove(0);
    }
    tokens.join(" ")
}

/// How much an artist hint resembles a cached release's artist (`0.0` = not the same act).
fn artist_similarity(hint: &str, release_artist: &str) -> f32 {
    let left = normalise_title(hint);
    let right = normalise_title(release_artist);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    if left == right {
        return 1.0;
    }
    // A short name ("a") must never stand in for everyone.
    let shorter = left.len().min(right.len());
    if shorter < 4 {
        return 0.0;
    }
    let left_tokens: Vec<&str> = left.split(' ').collect();
    let right_tokens: Vec<&str> = right.split(' ').collect();
    let is_subset =
        |small: &[&str], large: &[&str]| small.iter().all(|token| large.contains(token));
    if is_subset(&left_tokens, &right_tokens) || is_subset(&right_tokens, &left_tokens) {
        return 0.85;
    }
    // Bandcamp sub-domains run the words together, and the slug is usually the artist's
    // name with the spaces removed: `orimoto` is `Ori Moto`, `sablemusic` is `Sable Music`.
    // Compare the space-free forms, otherwise the very common case scores zero and an owned
    // release is reported as not owned.
    let left_joined = left.replace(' ', "");
    let right_joined = right.replace(' ', "");
    if left_joined == right_joined {
        return 0.95;
    }
    // A slug that adds or drops a word (`sable` vs `sablemusic`) is weaker evidence.
    if left_joined.contains(&right_joined) || right_joined.contains(&left_joined) {
        return 0.8;
    }
    0.0
}

/// The title agreement bands (research §9's ranked signal).
fn title_score(url_title: &str, release_title: &str) -> f32 {
    if url_title.is_empty() || release_title.is_empty() {
        return 0.0;
    }
    if url_title == release_title {
        return 0.9;
    }
    let (shorter, longer) = if url_title.len() <= release_title.len() {
        (url_title, release_title)
    } else {
        (release_title, url_title)
    };
    // A one- or two-character fragment is inside everything.
    if shorter.len() >= 4 && longer.contains(shorter) {
        return 0.7;
    }
    if shorter.len() >= 4 && same_token_set(url_title, release_title) {
        return 0.65;
    }
    0.0
}

fn same_token_set(left: &str, right: &str) -> bool {
    let mut left_tokens: Vec<&str> = left.split(' ').collect();
    let mut right_tokens: Vec<&str> = right.split(' ').collect();
    left_tokens.sort_unstable();
    right_tokens.sort_unstable();
    left_tokens == right_tokens
}

/// Round a score to the three decimals the JSON carries (no `0.850000024` in a payload).
fn round_confidence(value: f32) -> f32 {
    (value * 1000.0).round() / 1000.0
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// One ranked release the person may download.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    /// Opaque store release id, for `download/prepare`.
    pub store_release_id: String,
    pub title: String,
    pub artist: String,
    pub track_count: Option<u32>,
    /// Total length in whole seconds when the store declares one.
    pub duration_s: Option<u64>,
    /// Recomputed by the server; never taken from a request.
    pub confidence: f32,
    /// Why this candidate scored what it scored, in words.
    pub reasons: Vec<String>,
    /// `association`, `library`, `cache` or `search`.
    pub source: String,
}

/// A library item the store provenance names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItemRef {
    /// `.noudata` path relative to the library root.
    pub noudata_path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// The store release the item records, when it records one.
    pub store_release_id: Option<String>,
}

/// The answer to an ownership question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchResult {
    /// Canonical link this answers (query stripped).
    pub url: String,
    pub artist_slug: String,
    pub kind: BandcampKind,
    /// Ranked candidates at or above the confirmation band (may be empty).
    pub candidates: Vec<Candidate>,
    /// Opt-in `search3` leads, always below the confirmation band. Never actionable.
    pub leads: Vec<Candidate>,
    /// The library item proving this is already owned, when one exists.
    pub owned: Option<LibraryItemRef>,
    /// The top candidate's confidence (`0.0` when there is none).
    pub confidence: f32,
    /// Why the result is what it is, in words.
    pub reasons: Vec<String>,
    /// `not_in_collection` when nothing was found; `None` otherwise.
    pub reason: Option<String>,
    /// `confidence >= 0.9`: usable without asking.
    pub auto_usable: bool,
    /// `0.6 <= confidence < 0.9`: the person must confirm the candidate shown.
    pub requires_confirmation: bool,
    /// The collection cache is absent, a day old, or incomplete: a miss may be false.
    pub cache_stale: bool,
}

/// What the portal asks for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchRequest {
    /// A `releasePlatforms[].platformURL` value.
    pub url: String,
    /// `mediaList[].uniteData.expectedLength` in seconds, when the caller has it.
    #[serde(default)]
    pub expected_duration: Option<u64>,
    /// Consult `search3` when the cache has nothing for the artist. Only the POST route
    /// may set this: the GET route is cache-only by contract.
    #[serde(default)]
    pub allow_search: bool,
}

/// The candidate the person confirmed. Any `confidence` sent is ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmCandidate {
    pub store_release_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    /// Accepted for compatibility with the `MatchResult` shape; never trusted.
    #[serde(default)]
    pub confidence: Option<f32>,
}

/// Body of `POST /api/stores/bandcamp/match/confirm`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmMatchRequest {
    pub url: String,
    pub candidate: ConfirmCandidate,
    #[serde(default)]
    pub expected_duration: Option<u64>,
}

/// A recorded association, as the portal shows it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociationSummary {
    pub url_key: String,
    pub store_release_id: String,
    pub title: String,
    pub artist: String,
    pub confidence: f32,
    pub confirmed_at: u64,
}

/// `POST /api/stores/bandcamp/match/confirm` answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchConfirmation {
    pub ok: bool,
    pub url: String,
    pub store_release_id: String,
    /// The score the server computed (not the one the request carried).
    pub confidence: f32,
    /// Library items whose `.noudata` provenance was updated (0 when none exists yet).
    pub provenance_updated: u32,
    pub association: AssociationSummary,
}

/// One association as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Association {
    pub url_key: String,
    pub store_release_id: String,
    pub title: String,
    pub artist: String,
    pub confidence: f32,
    pub confirmed_at: u64,
    /// Canonical link the association was made from, for display.
    pub url: String,
}

/// `<library>/.plippa/stores/bandcamp.associations.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Associations {
    #[serde(default)]
    pub store_id: String,
    /// Newest first, at most [`MAX_ASSOCIATIONS`].
    #[serde(default)]
    pub entries: Vec<Association>,
    /// BCS_07: "I want this later" interests from device taps, newest first, at most
    /// [`MAX_WANTS`]. Kept in the same file so one write covers both and a disconnect
    /// ([`remove_associations`]) clears them together.
    #[serde(default)]
    pub wants: Vec<Want>,
}

/// One recorded interest: a release the person tapped and was offered for purchase.
///
/// The identity is [`BandcampUrl::url_key`] (`artist/kind/slug`), which is what a later tap of
/// the same link produces, so a purchase-and-tap-again clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Want {
    pub url_key: String,
    /// Canonical link, query-free. Shown back to the person, never fetched by the device.
    pub url: String,
    /// Unix seconds; the LRU order (oldest dropped first).
    pub wanted_at: u64,
    /// The release title the link renders to, for a portal list.
    #[serde(default)]
    pub title: Option<String>,
    /// The artist name the link renders to.
    #[serde(default)]
    pub artist: Option<String>,
}

/// Most wants kept in the associations file.
pub const MAX_WANTS: usize = 200;

/// Path of the associations file for `root`.
pub fn associations_path(root: &Path) -> PathBuf {
    cache::stores_dir(root).join(format!("{STORE_SLUG}.associations.json"))
}

/// Remove the associations file (called when the store is disconnected).
pub(crate) fn remove_associations(root: &Path) {
    if cache::check_state_dirs(root).is_err() {
        return;
    }
    let path = associations_path(root);
    // No `exists()` pre-check: unlinking a link never follows it, and a missing file is
    // already the desired state (SRV-05).
    if let Err(error) = fs::remove_file(&path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log_line(&format!("could not remove the store associations: {error}"));
        }
    }
}

fn read_associations(root: &Path) -> Associations {
    let empty = Associations::default();
    if let Err(reason) = cache::check_state_dirs(root) {
        log_line(&format!("store associations refused: {reason}"));
        return empty;
    }
    let bytes = match cache::read_state_file(&associations_path(root), MAX_ASSOCIATIONS_BYTES as u64)
    {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return empty,
        Err(error) => {
            log_line(&format!("the store associations could not be read ({error})"));
            return empty;
        }
    };
    match serde_json::from_slice::<Associations>(&bytes) {
        Ok(mut file) => {
            file.store_id = STORE_SLUG.to_string();
            file.entries.truncate(MAX_ASSOCIATIONS);
            file.wants.truncate(MAX_WANTS);
            file
        }
        Err(error) => {
            log_line(&format!(
                "the store associations file could not be read ({error}); starting empty"
            ));
            empty
        }
    }
}

/// Record (or refresh) interest in one link and keep the list bounded, newest first.
///
/// Returns how many wants are stored. A want is identified by [`BandcampUrl::url_key`], so
/// asking twice replaces the entry instead of growing the file, and the refreshed entry moves
/// to the front (that is the "recently wanted" order the device shows).
pub fn record_want(root: &Path, want: Want) -> Result<usize, StoreApiError> {
    let mut file = read_associations(root);
    file.store_id = STORE_SLUG.to_string();
    file.wants.retain(|existing| existing.url_key != want.url_key);
    file.wants.insert(0, want);
    file.wants.truncate(MAX_WANTS);
    write_associations(root, &file)?;
    Ok(file.wants.len())
}

/// Remove wants that a completed download answers, by canonical link identity.
///
/// A want recorded from a `not_owned` tap has no candidate, so there is no release id to match
/// on; the tapped link's own `url_key` is what a later Path A download carries. Returns how
/// many entries were removed.
pub fn clear_wants(root: &Path, url_keys: &[String]) -> usize {
    let mut file = read_associations(root);
    let before = file.wants.len();
    file.wants
        .retain(|want| !url_keys.iter().any(|key| key == &want.url_key));
    let removed = before - file.wants.len();
    if removed > 0 {
        if let Err(error) = write_associations(root, &file) {
            log_line(&format!("the finished download could not clear its wants: {error}"));
            return 0;
        }
    }
    removed
}

/// The recorded wants, newest first.
///
/// Only the tests read the whole list today; the routes answer with a count.
#[cfg(test)]
pub fn read_wants(root: &Path) -> Vec<Want> {
    read_associations(root).wants
}

fn write_associations(root: &Path, file: &Associations) -> Result<(), StoreApiError> {
    let bytes = serde_json::to_vec(file)
        .map_err(|error| StoreApiError::Cache(format!("cannot encode associations: {error}")))?;
    if bytes.len() > MAX_ASSOCIATIONS_BYTES {
        return Err(StoreApiError::Cache(format!(
            "the associations file would be {} bytes, above the {MAX_ASSOCIATIONS_BYTES}-byte cap",
            bytes.len()
        )));
    }
    cache::write_private_atomic(&associations_path(root), &bytes).map_err(StoreApiError::Cache)
}

// ---------------------------------------------------------------------------
// Library provenance
// ---------------------------------------------------------------------------

/// What one `.noudata` says about where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LibrarySource {
    noudata_path: String,
    title: Option<String>,
    artist: Option<String>,
    store_release_id: Option<String>,
    url: Option<String>,
    url_key: Option<String>,
}

impl LibrarySource {
    fn item_ref(&self) -> LibraryItemRef {
        LibraryItemRef {
            noudata_path: self.noudata_path.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            store_release_id: self.store_release_id.clone(),
        }
    }

    /// The recorded link names `url` (by canonical form or by key).
    fn names_url(&self, url: &BandcampUrl) -> bool {
        let key = url.url_key();
        if self.url_key.as_deref() == Some(key.as_str()) {
            return true;
        }
        match self.url.as_deref() {
            Some(recorded) => parse_bandcamp_url(recorded)
                .map(|parsed| parsed.url_key() == key)
                .unwrap_or(false),
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

impl StoreHub {
    /// Answer "do I own this link, and if not what should I download?".
    ///
    /// **Never touches the network** unless `request.allow_search` is set, which only the
    /// POST route does. Stages run cheapest first and stop when one is decisive:
    ///
    /// 1. a recorded store source (`plippaStoreSource` on a library item) or an association
    ///    made by [`Self::confirm_match`] → owned, confidence 0.99, zero I/O beyond disk;
    /// 2. artist + normalised title against the collection cache (0.9 exact, 0.7 contains,
    ///    0.65 token set);
    /// 3. a duration fingerprint on a stage-2 candidate (+0.2, +0.1 when the release is a
    ///    single track and its total also agrees, capped at 0.95);
    /// 4. nothing → [`NOT_IN_COLLECTION`] with no candidates, as a successful result.
    pub fn match_release(&self, request: &MatchRequest) -> Result<MatchResult, StoreApiError> {
        let url = parse_bandcamp_url(&request.url).map_err(|error| match error {
            StoreError::Policy { reason } => StoreApiError::InvalidUrl(reason),
            other => StoreApiError::Store(other),
        })?;
        if let Some(seconds) = request.expected_duration {
            if seconds == 0 || seconds > MAX_EXPECTED_DURATION_SECS {
                return Err(StoreApiError::InvalidRequest(format!(
                    "expectedDuration must be between 1 and {MAX_EXPECTED_DURATION_SECS} seconds"
                )));
            }
        }
        let now = now_secs();
        let cache = self.cache_snapshot();
        let cache_stale = cache
            .as_ref()
            .map(|cache| cache.is_stale(now))
            .unwrap_or(true);
        let releases = cache
            .as_ref()
            .map(|cache| cache.releases.clone())
            .unwrap_or_default();

        // Stage 1: what the library already records, and what the person already confirmed.
        let sources = self.library_store_sources();
        let associations = match self.library_root() {
            Some(root) => read_associations(&root),
            None => Associations::default(),
        };
        let stage_one = stage_one(&url, &associations, &sources, &releases);
        if !stage_one.candidates.is_empty() || stage_one.owned.is_some() {
            return Ok(finish(
                &url,
                stage_one.candidates,
                Vec::new(),
                stage_one.owned,
                stage_one.reasons,
                cache_stale,
            ));
        }

        // Stage 2 and 3: the cache, artist-gated, then promoted by a duration fingerprint.
        let artist_hint = url.artist_hint();
        let title_hint = url.title_hint();
        let mut candidates: Vec<Candidate> = Vec::new();
        for release in &releases {
            if let Some((confidence, reasons)) = score_release(
                &artist_hint,
                &title_hint,
                url.kind,
                release,
                request.expected_duration,
            ) {
                candidates.push(candidate_from_release(
                    release, confidence, reasons, "cache",
                ));
            }
        }
        candidates.retain(|candidate| candidate.confidence >= CONFIRM_CONFIDENCE);
        candidates.sort_by(|left, right| {
            right
                .confidence
                .partial_cmp(&left.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.store_release_id.cmp(&right.store_release_id))
        });
        let mut reasons = Vec::new();
        if !candidates.is_empty() {
            reasons.push(format!(
                "{} cached release(s) share this artist and title",
                candidates.len()
            ));
        }

        // Stage 4 (or the opt-in tie-breaker): search3 is a lead, never proof. It is only
        // consulted when the cache knows nothing at all about this artist.
        let artist_cached = releases
            .iter()
            .any(|release| artist_similarity(&artist_hint, &release.artist) > 0.0);
        let mut leads = Vec::new();
        if candidates.is_empty() && request.allow_search && !artist_cached {
            match self.search_leads(&url, request.expected_duration) {
                Ok(found) => {
                    if found.is_empty() {
                        reasons.push("the store's search returned no lead".to_string());
                    } else {
                        reasons.push(
                            "search3 found leads only; the store misses titles it holds, so they \
                             are not actionable"
                                .to_string(),
                        );
                    }
                    leads = found;
                }
                Err(StoreApiError::Store(StoreError::Unimplemented { .. }))
                | Err(StoreApiError::NotConnected) => {
                    reasons.push("search was requested but the store could not answer".to_string());
                }
                Err(error) => {
                    log_line(&format!(
                        "the optional search tie-breaker failed: {}",
                        error.code()
                    ));
                    reasons.push("search was requested but the store could not answer".to_string());
                }
            }
        }
        Ok(finish(&url, candidates, leads, None, reasons, cache_stale))
    }

    /// Record the candidate the person confirmed, so the next lookup is stage 1.
    ///
    /// The confidence is **recomputed**: a request-supplied score is ignored, a candidate
    /// that does not score at least [`CONFIRM_CONFIDENCE`] is refused, and a candidate that
    /// is not in the cached collection is refused outright. When a library item already
    /// carries this store's provenance, its `plippaStoreSource` is completed with the link
    /// and release id so a later lookup is exact.
    pub fn confirm_match(
        &self,
        request: &ConfirmMatchRequest,
    ) -> Result<MatchConfirmation, StoreApiError> {
        let url = parse_bandcamp_url(&request.url).map_err(|error| match error {
            StoreError::Policy { reason } => StoreApiError::InvalidUrl(reason),
            other => StoreApiError::Store(other),
        })?;
        let release_id = request.candidate.store_release_id.trim();
        if release_id.is_empty() || release_id.len() > MAX_RELEASE_ID_BYTES {
            return Err(StoreApiError::InvalidRequest(
                "the candidate has no store release id".to_string(),
            ));
        }
        if let Some(seconds) = request.expected_duration {
            if seconds == 0 || seconds > MAX_EXPECTED_DURATION_SECS {
                return Err(StoreApiError::InvalidRequest(format!(
                    "expectedDuration must be between 1 and {MAX_EXPECTED_DURATION_SECS} seconds"
                )));
            }
        }
        let Some(root) = self.library_root() else {
            return Err(StoreApiError::Unavailable(
                "the media library root is not available yet".to_string(),
            ));
        };
        let releases = self
            .cache_snapshot()
            .map(|cache| cache.releases)
            .unwrap_or_default();
        let cached = releases
            .iter()
            .find(|release| release.id.as_str() == release_id);
        let associations = read_associations(&root);
        let already_recorded = associations
            .entries
            .iter()
            .any(|entry| entry.url_key == url.url_key() && entry.store_release_id == release_id);

        let (confidence, title, artist) = if already_recorded {
            // Confirming again is idempotent: this was a decision, not a score.
            match cached {
                Some(release) => (
                    ASSOCIATION_CONFIDENCE,
                    release.title.clone(),
                    release.artist.clone(),
                ),
                None => (
                    ASSOCIATION_CONFIDENCE,
                    request.candidate.title.clone().unwrap_or_default(),
                    request.candidate.artist.clone().unwrap_or_default(),
                ),
            }
        } else {
            let Some(release) = cached else {
                // The server cannot verify a release it does not hold; a client cannot
                // invent one.
                return Err(StoreApiError::UnknownRelease);
            };
            match score_release(
                &url.artist_hint(),
                &url.title_hint(),
                url.kind,
                release,
                request.expected_duration,
            ) {
                Some((score, _)) if score >= CONFIRM_CONFIDENCE => {
                    (score, release.title.clone(), release.artist.clone())
                }
                Some((score, _)) => {
                    return Err(StoreApiError::LowConfidence {
                        score: format!("{score:.2}"),
                    })
                }
                None => {
                    return Err(StoreApiError::LowConfidence {
                        score: "0.00".to_string(),
                    })
                }
            }
        };

        let entry = Association {
            url_key: url.url_key(),
            store_release_id: release_id.to_string(),
            title,
            artist,
            confidence,
            confirmed_at: now_secs(),
            url: url.url.clone(),
        };
        let mut file = associations;
        file.store_id = STORE_SLUG.to_string();
        file.entries
            .retain(|existing| existing.url_key != entry.url_key);
        file.entries.insert(0, entry.clone());
        file.entries.truncate(MAX_ASSOCIATIONS);
        write_associations(&root, &file)?;
        let provenance_updated = self.update_provenance(&root, &entry);
        log_line(&format!(
            "store association recorded: {} -> {} at {:.2}{}",
            entry.url_key,
            entry.store_release_id,
            entry.confidence,
            if provenance_updated > 0 {
                format!(" ({provenance_updated} library item(s) updated)")
            } else {
                String::new()
            }
        ));
        Ok(MatchConfirmation {
            ok: true,
            url: url.url.clone(),
            store_release_id: entry.store_release_id.clone(),
            confidence,
            provenance_updated,
            association: AssociationSummary {
                url_key: entry.url_key,
                store_release_id: entry.store_release_id,
                title: entry.title,
                artist: entry.artist,
                confidence: entry.confidence,
                confirmed_at: entry.confirmed_at,
            },
        })
    }

    /// Every `.noudata` under the library that records this store's provenance.
    ///
    /// Bounded ([`MAX_LIBRARY_DOCS_SCANNED`]): an enormous library stops looking rather than
    /// holding a request open. Unreadable files are skipped; the scanner owns reporting
    /// those, and a matching request is not the place to complain about them.
    fn library_store_sources(&self) -> Vec<LibrarySource> {
        let Some(root) = self.library_root() else {
            return Vec::new();
        };
        let paths =
            match collect_noudata_paths(&LibraryRoot::new(root.clone()), MIN_LIBRARY_SCAN_DEPTH) {
                Ok(paths) => paths,
                Err(error) => {
                    log_line(&format!(
                        "the library could not be read while matching: {error}"
                    ));
                    return Vec::new();
                }
            };
        let mut sources = Vec::new();
        for path in paths.into_iter().take(MAX_LIBRARY_DOCS_SCANNED) {
            let Ok(bytes) = read_noudata_capped(&path) else {
                continue;
            };
            let Ok(doc) = parse_noudata(&bytes) else {
                continue;
            };
            let Some(record) = store_source_record(&doc) else {
                continue;
            };
            sources.push(LibrarySource {
                noudata_path: path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/"),
                title: doc.playable_item_title.clone(),
                artist: doc.primary_artist_display(),
                store_release_id: record.release_id,
                url: record.url,
                url_key: record.url_key,
            });
        }
        sources
    }

    /// Complete the provenance of library items that already carry this store's source.
    ///
    /// Only items that already have a `plippaStoreSource` are touched: an item without one
    /// may be the person's own rip of the same title, and claiming it for a store release
    /// would be exactly the silent guess this package forbids.
    fn update_provenance(&self, root: &Path, entry: &Association) -> u32 {
        let paths = match collect_noudata_paths(
            &LibraryRoot::new(root.to_path_buf()),
            MIN_LIBRARY_SCAN_DEPTH,
        ) {
            Ok(paths) => paths,
            Err(error) => {
                log_line(&format!(
                    "the library could not be read while recording provenance: {error}"
                ));
                return 0;
            }
        };
        let mut updated = 0;
        for path in paths.into_iter().take(MAX_LIBRARY_DOCS_SCANNED) {
            let Ok(bytes) = read_noudata_capped(&path) else {
                continue;
            };
            let Ok(mut doc) = parse_noudata(&bytes) else {
                continue;
            };
            let Some(record) = store_source_record(&doc) else {
                continue;
            };
            let names_release =
                record.release_id.as_deref() == Some(entry.store_release_id.as_str());
            let names_title = record.release_id.is_none()
                && doc
                    .playable_item_title
                    .as_deref()
                    .map(|title| normalise_title(title) == normalise_title(&entry.title))
                    .unwrap_or(false)
                && doc
                    .primary_artist_display()
                    .map(|artist| artist_similarity(&entry.artist, &artist) > 0.0)
                    .unwrap_or(false);
            if !names_release && !names_title {
                continue;
            }
            let mut record_json = match doc.extra.get(PLIPPA_STORE_SOURCE) {
                Some(serde_json::Value::Object(map)) => map.clone(),
                _ => serde_json::Map::new(),
            };
            record_json.insert("store".to_string(), STORE_SLUG.into());
            record_json.insert(
                "releaseId".to_string(),
                serde_json::Value::String(entry.store_release_id.clone()),
            );
            record_json.insert(
                "url".to_string(),
                serde_json::Value::String(entry.url.clone()),
            );
            record_json.insert(
                "urlKey".to_string(),
                serde_json::Value::String(entry.url_key.clone()),
            );
            record_json.insert(
                "confirmedAt".to_string(),
                serde_json::Value::from(entry.confirmed_at),
            );
            doc.extra.insert(
                PLIPPA_STORE_SOURCE.to_string(),
                serde_json::Value::Object(record_json),
            );
            match doc.write_to_path(&path) {
                Ok(()) => updated += 1,
                Err(error) => log_line(&format!(
                    "could not record store provenance on {}: {error}",
                    path.display()
                )),
            }
        }
        updated
    }

    /// `search3` leads for a link whose artist is not in the cache at all.
    fn search_leads(
        &self,
        url: &BandcampUrl,
        expected_duration: Option<u64>,
    ) -> Result<Vec<Candidate>, StoreApiError> {
        let creds = self.load_creds().ok_or(StoreApiError::NotConnected)?;
        let provider = self.provider(&creds)?;
        let hits = provider.search_tracks(&url.title_hint())?;
        let title_key = url.title_key();
        let mut leads: Vec<Candidate> = Vec::new();
        for hit in hits {
            let Some(release_id) = hit.release_id else {
                continue;
            };
            let score = title_score(&title_key, &normalise_title(&hit.track.title));
            if score == 0.0 {
                continue;
            }
            let mut confidence = score * SEARCH_SCALE;
            let mut reasons = vec!["search3 hit; not evidence of ownership".to_string()];
            if let (Some(expected), Some(ms)) = (expected_duration, hit.track.duration_ms) {
                let seconds = (ms + 500) / 1000;
                if seconds.abs_diff(expected) <= DURATION_TOLERANCE_SECS {
                    confidence += DURATION_BONUS * 0.5;
                    reasons.push("search hit duration matches".to_string());
                }
            }
            confidence = round_confidence(confidence.min(SEARCH_CONFIDENCE_CAP));
            let duplicate = leads
                .iter()
                .any(|lead| lead.store_release_id == release_id.as_str());
            if duplicate {
                continue;
            }
            leads.push(Candidate {
                store_release_id: release_id.as_str().to_string(),
                title: hit.track.title.clone(),
                artist: hit.artist.clone().unwrap_or_default(),
                track_count: None,
                duration_s: hit.track.duration_ms.map(|ms| (ms + 500) / 1000),
                confidence,
                reasons,
                source: "search".to_string(),
            });
        }
        leads.sort_by(|left, right| {
            right
                .confidence
                .partial_cmp(&left.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.store_release_id.cmp(&right.store_release_id))
        });
        leads.truncate(MAX_SEARCH_LEADS);
        Ok(leads)
    }
}

/// Most `search3` leads returned.
const MAX_SEARCH_LEADS: usize = 10;
/// A `search3` title score is halved before it becomes a lead: the store misses titles it
/// holds, so its own hit is a hint, not a measurement.
const SEARCH_SCALE: f32 = 0.5;

/// The `plippaStoreSource` record of one document, if it is this store's.
struct StoreSourceRecord {
    release_id: Option<String>,
    url: Option<String>,
    url_key: Option<String>,
}

fn store_source_record(doc: &NoudataDocument) -> Option<StoreSourceRecord> {
    let serde_json::Value::Object(map) = doc.extra.get(PLIPPA_STORE_SOURCE)? else {
        return None;
    };
    let store = map
        .get("store")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(STORE_SLUG);
    if store != STORE_SLUG {
        return None;
    }
    let get = |key: &str| {
        map.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    Some(StoreSourceRecord {
        release_id: get("releaseId"),
        url: get("url"),
        url_key: get("urlKey"),
    })
}

/// Stage 1's answer.
#[derive(Default)]
struct StageOne {
    candidates: Vec<Candidate>,
    owned: Option<LibraryItemRef>,
    reasons: Vec<String>,
}

fn stage_one(
    url: &BandcampUrl,
    associations: &Associations,
    sources: &[LibrarySource],
    releases: &[StoreRelease],
) -> StageOne {
    let key = url.url_key();
    let association = associations
        .entries
        .iter()
        .find(|entry| entry.url_key == key);
    let mut stage = StageOne::default();

    let owned_source = sources.iter().find(|source| {
        let names_release = association
            .map(|entry| {
                source.store_release_id.as_deref() == Some(entry.store_release_id.as_str())
            })
            .unwrap_or(false);
        source.names_url(url) || names_release
    });
    if let Some(source) = owned_source {
        stage.owned = Some(source.item_ref());
        stage
            .reasons
            .push("a library item records this store release".to_string());
    }

    if let Some(entry) = association {
        let cached = releases
            .iter()
            .find(|release| release.id.as_str() == entry.store_release_id);
        let (title, artist, track_count, duration_s) = match cached {
            Some(release) => (
                release.title.clone(),
                release.artist.clone(),
                release.track_count,
                release_duration_s(release),
            ),
            None => (entry.title.clone(), entry.artist.clone(), None, None),
        };
        stage.candidates.push(Candidate {
            store_release_id: entry.store_release_id.clone(),
            title,
            artist,
            track_count,
            duration_s,
            confidence: ASSOCIATION_CONFIDENCE,
            reasons: vec![
                "this link was confirmed before".to_string(),
                if cached.is_some() {
                    "the release is in the collection cache".to_string()
                } else {
                    "the release is no longer in the collection cache".to_string()
                },
            ],
            source: "association".to_string(),
        });
        stage
            .reasons
            .push("a recorded association answers without a search".to_string());
        return stage;
    }

    // No association: a library item that names this link is still proof (BCS_05 writes the
    // source URL with the item), and its recorded release id is the candidate.
    if let Some(source) = &stage.owned {
        if let Some(release_id) = &source.store_release_id {
            stage.candidates.push(Candidate {
                store_release_id: release_id.clone(),
                title: source.title.clone().unwrap_or_default(),
                artist: source.artist.clone().unwrap_or_default(),
                track_count: None,
                duration_s: None,
                confidence: ASSOCIATION_CONFIDENCE,
                reasons: vec!["the library item records this store release".to_string()],
                source: "library".to_string(),
            });
        }
    }
    stage
}

/// Score one cached release against a link. `None` when the artist or title does not agree.
fn score_release(
    artist_hint: &str,
    title_hint: &str,
    kind: BandcampKind,
    release: &StoreRelease,
    expected_duration: Option<u64>,
) -> Option<(f32, Vec<String>)> {
    let artist = artist_similarity(artist_hint, &release.artist);
    if artist == 0.0 {
        return None;
    }
    let release_title = normalise_title(&release.title);
    let title = title_score(title_hint, &release_title);
    if title == 0.0 {
        return None;
    }
    let mut reasons = Vec::new();
    reasons.push(if artist >= 1.0 {
        "the artist matches".to_string()
    } else {
        "the artist name agrees".to_string()
    });
    reasons.push(match title {
        score if score >= 0.9 => "the title matches exactly after normalisation".to_string(),
        score if score >= 0.7 => "one title contains the other".to_string(),
        _ => "the same title words in a different order".to_string(),
    });
    let mut confidence = title;
    if let Some(expected) = expected_duration {
        if let Some(total_ms) = release.duration_ms {
            let total = (total_ms + 500) / 1000;
            if total.abs_diff(expected) <= DURATION_TOLERANCE_SECS {
                confidence += DURATION_BONUS;
                reasons.push("the release length is within 2 s of the expected length".to_string());
                if kind == BandcampKind::Track && release.track_count == Some(1) {
                    confidence += DURATION_SHAPE_BONUS;
                    reasons.push("it is a one-track release and the length agrees".to_string());
                }
                confidence = confidence.min(CONFIDENCE_CAP);
            }
        }
    }
    Some((round_confidence(confidence), reasons))
}

fn candidate_from_release(
    release: &StoreRelease,
    confidence: f32,
    reasons: Vec<String>,
    source: &str,
) -> Candidate {
    Candidate {
        store_release_id: release.id.as_str().to_string(),
        title: release.title.clone(),
        artist: release.artist.clone(),
        track_count: release.track_count,
        duration_s: release_duration_s(release),
        confidence: round_confidence(confidence),
        reasons,
        source: source.to_string(),
    }
}

/// Build the payload, deriving the confidence band flags from the top candidate.
fn finish(
    url: &BandcampUrl,
    candidates: Vec<Candidate>,
    leads: Vec<Candidate>,
    owned: Option<LibraryItemRef>,
    reasons: Vec<String>,
    cache_stale: bool,
) -> MatchResult {
    let confidence = candidates
        .first()
        .map(|candidate| candidate.confidence)
        .unwrap_or(0.0);
    // A miss is a result ("you do not own this"), never an error. An owned item without a
    // candidate is not a miss: the library already answers the question.
    let reason = (candidates.is_empty() && owned.is_none()).then(|| NOT_IN_COLLECTION.to_string());
    MatchResult {
        url: url.url.clone(),
        artist_slug: url.artist_slug.clone(),
        kind: url.kind,
        candidates,
        leads,
        owned,
        confidence,
        reasons,
        reason,
        auto_usable: confidence >= AUTO_USABLE_CONFIDENCE,
        requires_confirmation: (CONFIRM_CONFIDENCE..AUTO_USABLE_CONFIDENCE).contains(&confidence),
        cache_stale,
    }
}

/// A cached release's total length in whole seconds.
fn release_duration_s(release: &StoreRelease) -> Option<u64> {
    release.duration_ms.map(|ms| (ms + 500) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bandcamp_url_keeps_the_slug_opaque() {
        let url =
            parse_bandcamp_url("https://sable-music.bandcamp.com/track/were-getting-older-baby")
                .expect("a real link");
        assert_eq!(url.artist_slug, "sable-music");
        assert_eq!(url.kind, BandcampKind::Track);
        assert_eq!(url.slug, "were-getting-older-baby");
        assert_eq!(
            url.url,
            "https://sable-music.bandcamp.com/track/were-getting-older-baby"
        );
        assert_eq!(url.title_key(), "were getting older baby");

        // Percent escapes stay escaped in the identity, and are only decoded for matching.
        let escaped =
            parse_bandcamp_url("https://x.bandcamp.com/track/a%2F..%2Fb").expect("a link");
        assert_eq!(escaped.slug, "a%2F..%2Fb");
        assert_eq!(escaped.url, "https://x.bandcamp.com/track/a%2F..%2Fb");
        assert_eq!(escaped.title_key(), "a b");
    }

    #[test]
    fn an_artist_slug_with_the_spaces_removed_still_names_the_artist() {
        // Bandcamp sub-domains cannot carry spaces, so the slug is the artist's name run
        // together: `orimoto` is `Ori Moto`. Scoring that as "not the same act" reports an
        // owned release as not owned.
        assert_eq!(artist_similarity("orimoto", "Ori Moto"), 0.95);
        assert_eq!(artist_similarity("ori-moto", "Ori Moto"), 1.0);
        assert_eq!(artist_similarity("sablemusic", "Sable Music"), 0.95);
        // A slug that only contains the other name is weaker evidence, and a different act
        // still scores zero so an unrelated release is never offered as "owned".
        assert!((artist_similarity("sablemusic", "Sable") - 0.8).abs() < f32::EPSILON);
        assert_eq!(artist_similarity("orimoto", "Isorinne"), 0.0);
    }

    #[test]
    fn score_bands_follow_the_documented_thresholds() {
        assert_eq!(
            title_score("were getting older baby", "were getting older baby"),
            0.9
        );
        assert_eq!(
            title_score("were getting older baby", "were getting older baby live"),
            0.7
        );
        assert_eq!(
            title_score("getting older baby were", "were getting older baby"),
            0.65
        );
        assert_eq!(title_score("one", "another"), 0.0);
        // A very short fragment is not "contained" in everything.
        assert_eq!(title_score("go", "going home"), 0.0);
    }
}
