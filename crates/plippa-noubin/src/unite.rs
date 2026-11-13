//! `unite` (Noubin §4.7 / §4.7.1): match local media files to the `mediaList` of a fetched
//! `web.noudata` and build the local `_<release>.noudata` (plus a file rename/move *plan*).
//!
//! Pure logic: no network, no filesystem access, no audio decoding. Durations and embedded
//! tags arrive on [`CandidateFile`] (the server probes them with lofty); applying the plan to
//! disk is the caller's job (NB_09). Everything read from the web document, from tags and from
//! file names is untrusted (`NBSecurityRequirements.md`): it is only ever *compared*, and any
//! text that becomes a file/folder name goes through [`crate::sanitise`].
//!
//! # Algorithm ([`propose`])
//!
//! 1. **Classify** candidate files: supported audio, unsupported media (video etc., flagged,
//!    never matched), ignored (images, text, ...). A `_*.noudata` among them is Case C: only
//!    reported ([`UniteProposal::case_c`]), nothing is matched.
//! 2. **Group formats**: files with the same normalised stem but different extensions
//!    (`03 - Foo.mp3` + `03 - Foo.flac`) form one *unit*; a unit is matched as a whole and all
//!    its files land in the item's `localFilePaths`.
//! 3. **Score** every (item, unit) pair as a noisy-or of independent signals (weights below),
//!    multiplied by contradiction factors (wrong track number, length far off):
//!    expected filename (case/punctuation/extension-insensitive) 0.97; title similarity of the
//!    file name (vs `mediaTitle` or the expected filename) or of the tag title 0.9 x similarity;
//!    track/episode/chapter number 0.35-0.6 (0.5 for a tag number); length within tolerance
//!    (0.8 when only this file fits this item and vice versa, else 0.4; 0.2 when within three
//!    tolerances); order fallback 0.5 (only when unit count == item count and the natural-order
//!    position equals the item position).
//! 4. **Assign one-to-one, greedily**: pairs sorted by score (descending), ties broken by item
//!    position then natural file order; a pair is taken when both sides are still free and the
//!    score is at least [`MIN_MATCH`]. (Greedy is deterministic and, together with step 5,
//!    never picks a close call silently; a Hungarian solver would only change which of two
//!    near-equal pairings wins.)
//! 5. **Confidence separation**: a match is [`Confidence::Confident`] only when its score is at
//!    least [`CONFIDENT_MIN`] *and* it beats the best competing pair (same item, other unit; or
//!    same unit, other item) by [`AMBIGUITY_MARGIN`]. Otherwise it is [`Confidence::Low`] and
//!    the UI must ask. Pairs below [`MIN_MATCH`] are never proposed.
//! 6. **Alternate formats**: an unassigned unit that scores [`CONFIDENT_MIN`] for an item whose
//!    match uses other extensions is attached to that item (`AlternateFormat`).
//! 7. Leftovers are returned as `unmatched_items` / `unmatched_files`, each with a best guess
//!    (below threshold) for the manual-assignment UI.

use crate::normalise::normalise_noubin_key;
use crate::noudata::{MediaListItem, NoudataDocument};
use crate::noudata_types::{CoverImage, NoudataType, PlayableItemProfile};
use crate::sanitise::{
    is_safe_relative_path, safe_path_segment, sanitise_title, MAX_KEY_BYTES, MAX_SEGMENT_BYTES,
};
use crate::{import_placement, ExtractedFields, ImportKind};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Score at which a match is accepted at all (below: left for manual assignment).
pub const MIN_MATCH: f32 = 0.40;
/// Score at which a match may be [`Confidence::Confident`].
pub const CONFIDENT_MIN: f32 = 0.75;
/// A confident match must beat its best competitor by this much.
pub const AMBIGUITY_MARGIN: f32 = 0.10;
/// Length tolerance for items up to [`LONG_ITEM_MS`].
pub const LENGTH_TOLERANCE_MS: u64 = 2_000;
/// Items at or above this use a relative tolerance ([`LONG_TOLERANCE_PERCENT`]).
pub const LONG_ITEM_MS: u64 = 600_000;
/// Relative tolerance for long items (percent, never below [`LENGTH_TOLERANCE_MS`]).
pub const LONG_TOLERANCE_PERCENT: u64 = 1;

/// Extensions the Plippa index/engine plays (same list as the library scan).
pub const SUPPORTED_AUDIO_EXTS: &[&str] =
    &["mp3", "flac", "wav", "ogg", "opus", "m4a", "m4b", "aac"];
/// Media we recognise but cannot play (flagged, never matched).
const UNSUPPORTED_MEDIA_EXTS: &[&str] = &[
    "mp4", "m4v", "mkv", "avi", "mov", "wmv", "webm", "mpg", "mpeg", "flv", "wma", "aif", "aiff",
    "ape", "wv", "dsf", "dff", "mka", "ac3", "dts",
];
const KEYWORDS: &[&str] = &["episode", "ep", "chapter", "ch", "part", "pt", "track"];

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Embedded tags read from a candidate file (all optional, untrusted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddedTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
}

/// A local file offered for matching. `path` is opaque to this module (only its file name is
/// inspected); plan sources refer to it verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateFile {
    pub path: PathBuf,
    /// Lower-case extension without the dot (see [`CandidateFile::new`]).
    pub ext: String,
    pub duration_ms: Option<u64>,
    pub embedded_tags: EmbeddedTags,
    pub size: u64,
}

impl CandidateFile {
    /// Extension derived from `path`.
    pub fn new(path: impl Into<PathBuf>, size: u64) -> Self {
        let path = path.into();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        Self {
            path,
            ext,
            size,
            ..Default::default()
        }
    }

    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    pub fn with_tags(mut self, tags: EmbeddedTags) -> Self {
        self.embedded_tags = tags;
        self
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn stem(&self) -> String {
        let name = self.file_name();
        let ext = self.ext.to_ascii_lowercase();
        if !ext.is_empty() && name.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
            name[..name.len() - ext.len() - 1].to_string()
        } else {
            name
        }
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// Why a file was matched (shown to the user).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatchReason {
    ExpectedFilename,
    FilenameTitle {
        similarity: f32,
    },
    FilenameNumber,
    TagTitle {
        similarity: f32,
    },
    TagTrackNumber,
    /// `diff_ms` = |file length - expected length|.
    Length {
        diff_ms: u64,
        unique: bool,
    },
    OrderFallback,
    AlternateFormat,
    /// Downgraded to low confidence: another pair scores within [`AMBIGUITY_MARGIN`].
    Ambiguous {
        competitor: f32,
    },
    /// Contradicting evidence lowered the score.
    NumberMismatch,
    LengthMismatch {
        diff_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Safe to accept without asking.
    Confident,
    /// Proposed, but the user must confirm.
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MatchedFile {
    pub path: PathBuf,
    pub confidence: f32,
    pub reasons: Vec<MatchReason>,
}

/// One `mediaList` item with its proposed local file(s) (several = same track, other formats).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UniteMatch {
    pub item_index: usize,
    pub files: Vec<MatchedFile>,
    /// Score of the primary (first) file.
    pub confidence: f32,
    pub level: Confidence,
    pub reasons: Vec<MatchReason>,
}

/// Below-threshold suggestion for manual assignment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Guess {
    pub item_index: Option<usize>,
    pub path: Option<PathBuf>,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnmatchedItem {
    pub item_index: usize,
    pub title: Option<String>,
    pub best_guess: Option<Guess>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnmatchedFile {
    pub path: PathBuf,
    pub best_guess: Option<Guess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UniteWarningKind {
    /// Video etc.: not supported by the player, not matched.
    UnsupportedFile,
    /// A `_*.noudata` is present (Case C): associate only.
    CaseC,
    /// Length disagrees with `expectedLength` beyond tolerance.
    LengthMismatch,
    /// Two proposals were too close; left to the user.
    Ambiguous,
    /// Same name and extension seen twice; only one can be used.
    DuplicateFile,
    /// File count differs from item count.
    CountMismatch,
    /// Items with no `uniteData` at all (only order/tags can match them).
    NoExpectedData,
    /// Web document field looked unsafe and was replaced/dropped while building.
    Sanitised,
    /// Path-length / collision remarks from the rename plan.
    Plan,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UniteWarning {
    pub kind: UniteWarningKind,
    pub message: String,
    pub item_index: Option<usize>,
    pub path: Option<PathBuf>,
}

fn warning(kind: UniteWarningKind, message: impl Into<String>) -> UniteWarning {
    UniteWarning {
        kind,
        message: message.into(),
        item_index: None,
        path: None,
    }
}

/// Result of [`propose`].
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct UniteProposal {
    /// Ordered by `item_index`.
    pub matches: Vec<UniteMatch>,
    pub unmatched_items: Vec<UnmatchedItem>,
    /// Supported audio files that are not part of any match.
    pub unmatched_files: Vec<UnmatchedFile>,
    /// Video and other media the player cannot play (flagged, not matched).
    pub unsupported_files: Vec<PathBuf>,
    /// Images, text, per-track `.noudata`, ...: neither matched nor problems.
    pub ignored_files: Vec<PathBuf>,
    /// Case C: an existing `_<release>.noudata` was supplied; nothing was matched.
    pub case_c: Option<PathBuf>,
    pub warnings: Vec<UniteWarning>,
}

impl UniteProposal {
    /// Every item has a file and every playable file is used (§4.7 completion rule).
    /// Says nothing about confidence: see [`Self::needs_review`].
    pub fn is_complete(&self) -> bool {
        self.case_c.is_none() && self.unmatched_items.is_empty() && self.unmatched_files.is_empty()
    }

    /// Something needs the user (low confidence, leftovers, unsupported files).
    pub fn needs_review(&self) -> bool {
        !self.is_complete()
            || !self.unsupported_files.is_empty()
            || self.matches.iter().any(|m| m.level == Confidence::Low)
    }

    /// Mapping with every proposed match accepted (the UI edits/filters instead if the user
    /// changed anything).
    pub fn confirm_all(&self) -> ConfirmedMapping {
        self.confirm_where(|_| true)
    }

    /// Mapping with only the confident matches accepted.
    pub fn confirm_confident(&self) -> ConfirmedMapping {
        self.confirm_where(|m| m.level == Confidence::Confident)
    }

    fn confirm_where(&self, keep: impl Fn(&UniteMatch) -> bool) -> ConfirmedMapping {
        let mut all: Vec<PathBuf> = Vec::new();
        let mut assignments = Vec::new();
        for m in &self.matches {
            all.extend(m.files.iter().map(|f| f.path.clone()));
            if keep(m) {
                assignments.push(ConfirmedItem {
                    item_index: m.item_index,
                    files: m.files.iter().map(|f| f.path.clone()).collect(),
                });
            }
        }
        all.extend(self.unmatched_files.iter().map(|f| f.path.clone()));
        ConfirmedMapping {
            assignments,
            known_files: all,
            ignored_files: Vec::new(),
            allow_incomplete: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

fn tokenise(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    for c in s.chars() {
        if c == '&' {
            flush(&mut cur, &mut out);
            out.push("and".to_string());
        } else if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn is_num(t: &str) -> bool {
    !t.is_empty() && t.len() <= 4 && t.bytes().all(|b| b.is_ascii_digit())
}

/// `ep12` / `chapter01` -> number.
fn fused_keyword_number(t: &str) -> Option<u32> {
    KEYWORDS.iter().find_map(|kw| {
        t.strip_prefix(kw)
            .filter(|rest| is_num(rest))
            .and_then(|rest| rest.parse().ok())
    })
}

/// Number found in a token list and the remaining tokens.
struct Stripped {
    number: Option<u32>,
    keyworded: bool,
    tokens: Vec<String>,
}

fn strip_number(tokens: &[String]) -> Stripped {
    // keyword + number anywhere ("episode 12", "ch01")
    for i in 0..tokens.len() {
        if let Some(n) = fused_keyword_number(&tokens[i]) {
            let mut rest = tokens.to_vec();
            rest.remove(i);
            return Stripped {
                number: Some(n),
                keyworded: true,
                tokens: rest,
            };
        }
        if KEYWORDS.contains(&tokens[i].as_str()) && i + 1 < tokens.len() && is_num(&tokens[i + 1])
        {
            let n = tokens[i + 1].parse().ok();
            let mut rest = tokens.to_vec();
            rest.drain(i..=i + 1);
            return Stripped {
                number: n,
                keyworded: true,
                tokens: rest,
            };
        }
    }
    // leading number ("01 - foo", "1-05 foo": disc-track)
    if !tokens.is_empty() && is_num(&tokens[0]) {
        let take = if tokens.len() > 2
            && tokens[0].len() <= 2
            && is_num(&tokens[1])
            && tokens[1].len() <= 3
        {
            2
        } else {
            1
        };
        let n = tokens[take - 1].parse().ok();
        let rest: Vec<String> = tokens[take..].to_vec();
        // A bare number ("1999.mp3") also stays a title candidate.
        let rest = if rest.is_empty() {
            tokens.to_vec()
        } else {
            rest
        };
        return Stripped {
            number: n,
            keyworded: false,
            tokens: rest,
        };
    }
    Stripped {
        number: None,
        keyworded: false,
        tokens: tokens.to_vec(),
    }
}

/// A name broken into a number and title-token variants (whole, and each ` - ` segment).
#[derive(Debug, Clone, Default)]
struct ParsedName {
    number: Option<u32>,
    keyworded: bool,
    variants: Vec<Vec<String>>,
    /// All tokens joined (number included): the exact-name comparison key.
    joined: String,
    /// The title was only "keyword + number" (`Chapter 01`).
    pure_number: bool,
}

fn parse_name(text: &str) -> ParsedName {
    let text = text.replace('_', " ");
    let all = tokenise(&text);
    let joined = all.concat();
    let whole = strip_number(&all);
    let pure_number = whole.keyworded && whole.tokens.is_empty();
    let mut variants: Vec<Vec<String>> = Vec::new();
    let mut push = |v: Vec<String>| {
        if !v.is_empty() && !variants.contains(&v) {
            variants.push(v);
        }
    };
    push(whole.tokens.clone());
    let normalised = text
        .replace(" \u{2013} ", " - ")
        .replace(" \u{2014} ", " - ");
    for seg in normalised.split(" - ") {
        push(strip_number(&tokenise(seg)).tokens);
    }
    ParsedName {
        number: whole.number,
        keyworded: whole.keyworded,
        variants,
        joined,
        pure_number,
    }
}

/// 0.0..=1.0 similarity of two token lists.
fn similarity(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let (ja, jb) = (a.concat(), b.concat());
    if ja == jb {
        return 0.95;
    }
    let sa: HashSet<&String> = a.iter().collect();
    let sb: HashSet<&String> = b.iter().collect();
    if sa == sb {
        return 0.9;
    }
    let (small, ssmall, slarge) = if sa.len() <= sb.len() {
        (a, &sa, &sb)
    } else {
        (b, &sb, &sa)
    };
    if ssmall.is_subset(slarge) && small.concat().chars().count() >= 3 {
        return 0.55 + 0.35 * (ssmall.len() as f32 / slarge.len().max(1) as f32);
    }
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    let jac = inter / union.max(1.0);
    if jac >= 0.5 {
        0.8 * jac
    } else {
        0.0
    }
}

fn best_similarity(variants: &[Vec<String>], other: &[String]) -> f32 {
    variants
        .iter()
        .map(|v| similarity(v, other))
        .fold(0.0, f32::max)
}

/// Compare two natural-order strings (`track 2` < `track 10`).
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut ia, mut ib) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ia.peek().copied(), ib.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ia.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ia.next();
                }
                let mut nb = String::new();
                while let Some(c) = ib.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    ib.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(ca), Some(cb)) => {
                if ca != cb {
                    return ca.cmp(&cb);
                }
                ia.next();
                ib.next();
            }
        }
    }
}

fn tolerance_ms(expected_ms: u64) -> u64 {
    if expected_ms >= LONG_ITEM_MS {
        (expected_ms / 100 * LONG_TOLERANCE_PERCENT).max(LENGTH_TOLERANCE_MS)
    } else {
        LENGTH_TOLERANCE_MS
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Numbers are positions on the release (music, mix, other).
    Positional,
    /// Numbers are episode/chapter numbers (podcast, audiobook).
    Serial,
}

struct ItemInfo {
    position: usize,
    title: Vec<String>,
    title_number: Option<u32>,
    title_pure_number: bool,
    title_keyworded: bool,
    expected: Option<ParsedName>,
    expected_ms: Option<u64>,
}

struct FileInfo<'a> {
    file: &'a CandidateFile,
    name: ParsedName,
    tag_title: Vec<String>,
    tag_track: Option<u32>,
}

struct Unit {
    files: Vec<usize>,
}

struct PairScore {
    conf: f32,
    reasons: Vec<MatchReason>,
}

fn noisy_or(ws: &[f32]) -> f32 {
    1.0 - ws.iter().fold(1.0, |acc, w| acc * (1.0 - w))
}

fn strip_known_ext(name: &str) -> &str {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        let e = ext.to_ascii_lowercase();
        if SUPPORTED_AUDIO_EXTS.contains(&e.as_str())
            || UNSUPPORTED_MEDIA_EXTS.contains(&e.as_str())
        {
            return stem;
        }
    }
    name
}

fn item_info(idx: usize, item: &MediaListItem) -> ItemInfo {
    let title_text = item
        .media_title
        .as_deref()
        .map(sanitise_title)
        .unwrap_or_default();
    let parsed = parse_name(&title_text);
    let expected = item
        .unite_data
        .as_ref()
        .and_then(|u| u.expected_filename.as_deref())
        .map(sanitise_title)
        .map(|n| {
            // Only the file-name part of what the web document claims.
            let n = n.rsplit(['/', '\\']).next().unwrap_or("").to_string();
            parse_name(strip_known_ext(&n))
        })
        .filter(|p| !p.joined.is_empty());
    ItemInfo {
        position: idx,
        title: parsed.variants.first().cloned().unwrap_or_default(),
        title_number: parsed.number,
        title_pure_number: parsed.pure_number,
        title_keyworded: parsed.keyworded,
        expected,
        expected_ms: item
            .unite_data
            .as_ref()
            .and_then(|u| u.expected_length)
            .map(|t| t.as_millis()),
    }
}

enum NumOutcome {
    Match(f32),
    Mismatch(f32),
    None,
}

/// Compare a file/tag number with what the metadata says about the item.
fn number_outcome(n: Option<u32>, keyworded: bool, item: &ItemInfo, kind: Kind) -> NumOutcome {
    let Some(n) = n else { return NumOutcome::None };
    let explicit = item
        .expected
        .as_ref()
        .and_then(|e| e.number)
        .or(item.title_number);
    if let Some(x) = explicit {
        return if x == n {
            NumOutcome::Match(if keyworded || item.title_keyworded {
                0.6
            } else {
                0.55
            })
        } else {
            NumOutcome::Mismatch(0.7)
        };
    }
    let pos1 = item.position as u32 + 1;
    match (n == pos1, kind) {
        (true, Kind::Positional) => NumOutcome::Match(0.45),
        (true, Kind::Serial) => NumOutcome::Match(0.35),
        (false, Kind::Positional) => NumOutcome::Mismatch(0.8),
        (false, Kind::Serial) => NumOutcome::None,
    }
}

struct Ctx<'a> {
    files: &'a [FileInfo<'a>],
    kind: Kind,
}

fn score_pair(
    ctx: &Ctx,
    item: &ItemInfo,
    unit: &Unit,
    length_unique: bool,
    order_hit: bool,
) -> PairScore {
    let mut signals: Vec<(f32, MatchReason)> = Vec::new();
    let mut factor = 1.0f32;
    let mut reasons_extra: Vec<MatchReason> = Vec::new();

    // Name-derived signals: best over the unit's files.
    let mut exact = false;
    let mut name_sim = 0.0f32;
    let mut file_num = NumOutcome::None;
    let mut tag_sim = 0.0f32;
    let mut tag_num = NumOutcome::None;
    let mut best_diff: Option<u64> = None;

    let rank = |o: &NumOutcome| match o {
        NumOutcome::Match(_) => 2,
        NumOutcome::Mismatch(_) => 1,
        NumOutcome::None => 0,
    };
    for &fi in &unit.files {
        let f = &ctx.files[fi];
        if let Some(e) = &item.expected {
            if !f.name.joined.is_empty() && f.name.joined == e.joined {
                exact = true;
            }
            for ev in &e.variants {
                name_sim = name_sim.max(best_similarity(&f.name.variants, ev));
            }
        }
        name_sim = name_sim.max(best_similarity(&f.name.variants, &item.title));
        if f.name.pure_number
            && item.title_pure_number
            && f.name.number.is_some()
            && f.name.number == item.title_number
        {
            name_sim = name_sim.max(0.95);
        }
        let n = number_outcome(f.name.number, f.name.keyworded, item, ctx.kind);
        if rank(&n) > rank(&file_num) {
            file_num = n;
        } else if let (NumOutcome::Match(a), NumOutcome::Match(b)) = (&n, &file_num) {
            if a > b {
                file_num = n;
            }
        }
        tag_sim = tag_sim.max(similarity(&f.tag_title, &item.title));
        let t = match number_outcome(f.tag_track, true, item, ctx.kind) {
            NumOutcome::Match(_) => NumOutcome::Match(0.5),
            // Tag numbers are per disc / per compilation: a mismatch proves little.
            NumOutcome::Mismatch(_) => NumOutcome::None,
            NumOutcome::None => NumOutcome::None,
        };
        if rank(&t) > rank(&tag_num) {
            tag_num = t;
        }
        if let (Some(exp), Some(dur)) = (item.expected_ms, f.file.duration_ms) {
            let d = exp.abs_diff(dur);
            best_diff = Some(best_diff.map_or(d, |b| b.min(d)));
        }
    }

    if exact {
        signals.push((0.97, MatchReason::ExpectedFilename));
    }
    if name_sim >= 0.5 {
        signals.push((
            0.9 * name_sim,
            MatchReason::FilenameTitle {
                similarity: name_sim,
            },
        ));
    }
    match file_num {
        NumOutcome::Match(w) => signals.push((w, MatchReason::FilenameNumber)),
        NumOutcome::Mismatch(f) => {
            factor *= f;
            reasons_extra.push(MatchReason::NumberMismatch);
        }
        NumOutcome::None => {}
    }
    if tag_sim >= 0.5 {
        signals.push((
            0.9 * tag_sim,
            MatchReason::TagTitle {
                similarity: tag_sim,
            },
        ));
    }
    if let NumOutcome::Match(w) = tag_num {
        signals.push((w, MatchReason::TagTrackNumber));
    }
    if let (Some(exp), Some(d)) = (item.expected_ms, best_diff) {
        let tol = tolerance_ms(exp);
        if d <= tol {
            let w = if length_unique { 0.8 } else { 0.4 };
            signals.push((
                w,
                MatchReason::Length {
                    diff_ms: d,
                    unique: length_unique,
                },
            ));
        } else if d <= tol * 3 {
            signals.push((
                0.2,
                MatchReason::Length {
                    diff_ms: d,
                    unique: false,
                },
            ));
        } else {
            factor *= 0.6;
            reasons_extra.push(MatchReason::LengthMismatch { diff_ms: d });
        }
    }
    if order_hit {
        signals.push((0.5, MatchReason::OrderFallback));
    }

    let ws: Vec<f32> = signals.iter().map(|s| s.0).collect();
    let conf = (noisy_or(&ws) * factor).min(0.99);
    let mut reasons: Vec<MatchReason> = signals.into_iter().map(|s| s.1).collect();
    reasons.extend(reasons_extra);
    PairScore { conf, reasons }
}

// ---------------------------------------------------------------------------
// propose
// ---------------------------------------------------------------------------

enum Class {
    Audio,
    Unsupported,
    CaseC,
    Ignored,
}

fn classify_file(f: &CandidateFile) -> Class {
    let ext = f.ext.to_ascii_lowercase();
    let name = f.file_name();
    if ext == "noudata" && name.starts_with('_') {
        Class::CaseC
    } else if SUPPORTED_AUDIO_EXTS.contains(&ext.as_str()) {
        Class::Audio
    } else if UNSUPPORTED_MEDIA_EXTS.contains(&ext.as_str()) {
        Class::Unsupported
    } else {
        Class::Ignored
    }
}

fn kind_of(profile: PlayableItemProfile) -> Kind {
    match profile {
        PlayableItemProfile::Podcast | PlayableItemProfile::Audiobook => Kind::Serial,
        _ => Kind::Positional,
    }
}

/// Propose a one-to-one mapping of `web_doc.media_list` items to `candidates`.
/// Never guesses silently: see the module docs for the algorithm and thresholds.
pub fn propose(
    web_doc: &NoudataDocument,
    candidates: &[CandidateFile],
    profile: PlayableItemProfile,
) -> UniteProposal {
    let mut out = UniteProposal::default();
    let mut audio: Vec<&CandidateFile> = Vec::new();
    for c in candidates {
        match classify_file(c) {
            Class::Audio => audio.push(c),
            Class::Unsupported => {
                out.unsupported_files.push(c.path.clone());
                out.warnings.push(UniteWarning {
                    kind: UniteWarningKind::UnsupportedFile,
                    message: format!(
                        "{} is not a format Plippa can play; it was not matched",
                        display_name(&c.path)
                    ),
                    item_index: None,
                    path: Some(c.path.clone()),
                });
            }
            Class::CaseC => {
                if out.case_c.is_none() {
                    out.case_c = Some(c.path.clone());
                }
            }
            Class::Ignored => out.ignored_files.push(c.path.clone()),
        }
    }
    if out.case_c.is_some() {
        out.warnings.push(warning(
            UniteWarningKind::CaseC,
            "a _<release>.noudata came with the files: only associate it, no matching",
        ));
        return out;
    }

    let items = &web_doc.media_list;
    let kind = kind_of(profile);
    audio.sort_by(|a, b| {
        natural_cmp(&a.file_name(), &b.file_name()).then_with(|| a.path.cmp(&b.path))
    });

    let infos: Vec<FileInfo> = audio
        .iter()
        .map(|f| FileInfo {
            file: f,
            name: parse_name(&f.stem()),
            tag_title: f
                .embedded_tags
                .title
                .as_deref()
                .map(|t| strip_number(&tokenise(&sanitise_title(t))).tokens)
                .unwrap_or_default(),
            tag_track: f.embedded_tags.track_number,
        })
        .collect();

    // Units: same normalised stem, different extensions.
    let mut units: Vec<Unit> = Vec::new();
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (fi, f) in infos.iter().enumerate() {
        let key = f.name.joined.clone();
        let slot = if key.is_empty() {
            None
        } else {
            by_key.get(&key).and_then(|us| {
                us.iter().copied().find(|&u| {
                    units[u]
                        .files
                        .iter()
                        .all(|&x| !infos[x].file.ext.eq_ignore_ascii_case(&f.file.ext))
                })
            })
        };
        match slot {
            Some(u) => units[u].files.push(fi),
            None => {
                units.push(Unit { files: vec![fi] });
                if !key.is_empty() {
                    by_key.entry(key).or_default().push(units.len() - 1);
                }
            }
        }
    }
    // Same name + same extension twice (e.g. in two folders).
    for (i, f) in infos.iter().enumerate() {
        if infos[..i].iter().any(|g| {
            !f.name.joined.is_empty()
                && g.name.joined == f.name.joined
                && g.file.ext.eq_ignore_ascii_case(&f.file.ext)
        }) {
            out.warnings.push(UniteWarning {
                kind: UniteWarningKind::DuplicateFile,
                message: format!("{} appears more than once", display_name(&f.file.path)),
                item_index: None,
                path: Some(f.file.path.clone()),
            });
        }
    }

    if items.iter().all(|i| i.unite_data.is_none()) && !items.is_empty() {
        out.warnings.push(warning(
            UniteWarningKind::NoExpectedData,
            "the release metadata has no uniteData: matching relies on titles, tags and order",
        ));
    }
    if units.len() != items.len() {
        out.warnings.push(warning(
            UniteWarningKind::CountMismatch,
            format!(
                "{} item(s) in the release but {} track file(s)",
                items.len(),
                units.len()
            ),
        ));
    }

    let item_infos: Vec<ItemInfo> = items
        .iter()
        .enumerate()
        .map(|(i, m)| item_info(i, m))
        .collect();
    let ctx = Ctx {
        files: &infos,
        kind,
    };

    // Length fit matrix (for uniqueness).
    let fits = |ii: &ItemInfo, u: &Unit| -> bool {
        match ii.expected_ms {
            Some(exp) => u.files.iter().any(|&fi| {
                ctx.files[fi]
                    .file
                    .duration_ms
                    .is_some_and(|d| exp.abs_diff(d) <= tolerance_ms(exp))
            }),
            None => false,
        }
    };
    let fit: Vec<Vec<bool>> = item_infos
        .iter()
        .map(|ii| units.iter().map(|u| fits(ii, u)).collect())
        .collect();
    let row_count: Vec<usize> = fit
        .iter()
        .map(|r| r.iter().filter(|b| **b).count())
        .collect();
    let col_count: Vec<usize> = (0..units.len())
        .map(|u| fit.iter().filter(|r| r[u]).count())
        .collect();
    let same_count = units.len() == items.len();

    let mut scores: Vec<Vec<PairScore>> = Vec::with_capacity(items.len());
    for (i, ii) in item_infos.iter().enumerate() {
        let mut row = Vec::with_capacity(units.len());
        for (u, unit) in units.iter().enumerate() {
            let unique = fit[i][u] && row_count[i] == 1 && col_count[u] == 1;
            row.push(score_pair(&ctx, ii, unit, unique, same_count && i == u));
        }
        scores.push(row);
    }

    // Greedy one-to-one assignment.
    let mut pairs: Vec<(usize, usize, f32)> = Vec::new();
    for (i, row) in scores.iter().enumerate() {
        for (u, s) in row.iter().enumerate() {
            if s.conf >= MIN_MATCH {
                pairs.push((i, u, s.conf));
            }
        }
    }
    pairs.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(Ordering::Equal)
            .then(a.0.cmp(&b.0))
            .then(a.1.cmp(&b.1))
    });
    let mut item_unit: Vec<Option<usize>> = vec![None; items.len()];
    let mut unit_item: Vec<Option<usize>> = vec![None; units.len()];
    for (i, u, _) in pairs {
        if item_unit[i].is_none() && unit_item[u].is_none() {
            item_unit[i] = Some(u);
            unit_item[u] = Some(i);
        }
    }

    // Alternate formats: leftover units that clearly belong to an already matched item
    // (other extension, confident score). They are not competitors of that item's match.
    let mut alt_of: Vec<Option<usize>> = vec![None; units.len()];
    for u in 0..units.len() {
        if unit_item[u].is_some() {
            continue;
        }
        let best = (0..items.len())
            .map(|i| (i, scores[i][u].conf))
            .max_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(Ordering::Equal)
                    .then(b.0.cmp(&a.0))
            });
        let Some((i, conf)) = best else { continue };
        let Some(main) = item_unit[i] else { continue };
        if conf < CONFIDENT_MIN
            || scores[i][main]
                .reasons
                .iter()
                .any(|r| matches!(r, MatchReason::LengthMismatch { .. }))
        {
            continue;
        }
        let ext_of = |fi: usize| infos[fi].file.ext.to_ascii_lowercase();
        let taken_exts: HashSet<String> = units[main]
            .files
            .iter()
            .chain(
                alt_of
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| **a == Some(i))
                    .flat_map(|(u2, _)| units[u2].files.iter()),
            )
            .map(|&fi| ext_of(fi))
            .collect();
        if units[u]
            .files
            .iter()
            .all(|&fi| !taken_exts.contains(&ext_of(fi)))
        {
            alt_of[u] = Some(i);
        }
    }

    // Build matches with separation check.
    for i in 0..items.len() {
        let Some(u) = item_unit[i] else { continue };
        let s = &scores[i][u];
        let mut competitor = 0.0f32;
        for (u2, s2) in scores[i].iter().enumerate() {
            if u2 != u && alt_of[u2] != Some(i) {
                competitor = competitor.max(s2.conf);
            }
        }
        for (i2, row) in scores.iter().enumerate() {
            if i2 != i {
                competitor = competitor.max(row[u].conf);
            }
        }
        let mut reasons = s.reasons.clone();
        let mut level = if s.conf >= CONFIDENT_MIN {
            Confidence::Confident
        } else {
            Confidence::Low
        };
        if level == Confidence::Confident && s.conf - competitor < AMBIGUITY_MARGIN {
            level = Confidence::Low;
            reasons.push(MatchReason::Ambiguous { competitor });
            out.warnings.push(UniteWarning {
                kind: UniteWarningKind::Ambiguous,
                message: format!("item {} has more than one plausible file", i + 1),
                item_index: Some(i),
                path: None,
            });
        }
        if let Some(MatchReason::LengthMismatch { diff_ms }) = reasons
            .iter()
            .find(|r| matches!(r, MatchReason::LengthMismatch { .. }))
        {
            out.warnings.push(UniteWarning {
                kind: UniteWarningKind::LengthMismatch,
                message: format!(
                    "item {}: file length differs from the expected length by {} s",
                    i + 1,
                    diff_ms / 1000
                ),
                item_index: Some(i),
                path: None,
            });
        }
        let mut files: Vec<MatchedFile> = units[u]
            .files
            .iter()
            .map(|&fi| MatchedFile {
                path: infos[fi].file.path.clone(),
                confidence: s.conf,
                reasons: reasons.clone(),
            })
            .collect();
        for (u2, a) in alt_of.iter().enumerate() {
            if *a != Some(i) {
                continue;
            }
            for &fi in &units[u2].files {
                let mut r = scores[i][u2].reasons.clone();
                r.push(MatchReason::AlternateFormat);
                files.push(MatchedFile {
                    path: infos[fi].file.path.clone(),
                    confidence: scores[i][u2].conf,
                    reasons: r,
                });
            }
        }
        out.matches.push(UniteMatch {
            item_index: i,
            files,
            confidence: s.conf,
            level,
            reasons,
        });
    }
    for (u, a) in alt_of.iter().enumerate() {
        if a.is_some() {
            unit_item[u] = *a;
        }
    }

    // Leftovers with best guesses.
    for i in 0..items.len() {
        if item_unit[i].is_some() {
            continue;
        }
        let guess = (0..units.len())
            .filter(|&u| unit_item[u].is_none())
            .map(|u| (u, scores[i][u].conf))
            .filter(|(_, c)| *c > 0.0)
            .max_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(Ordering::Equal)
                    .then(b.0.cmp(&a.0))
            })
            .map(|(u, c)| Guess {
                item_index: None,
                path: Some(infos[units[u].files[0]].file.path.clone()),
                confidence: c,
            });
        out.unmatched_items.push(UnmatchedItem {
            item_index: i,
            title: items[i].media_title.as_deref().map(sanitise_title),
            best_guess: guess,
        });
    }
    for (u, unit) in units.iter().enumerate() {
        if unit_item[u].is_some() {
            continue;
        }
        let guess = (0..items.len())
            .filter(|&i| item_unit[i].is_none())
            .map(|i| (i, scores[i][u].conf))
            .filter(|(_, c)| *c > 0.0)
            .max_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(Ordering::Equal)
                    .then(b.0.cmp(&a.0))
            })
            .map(|(i, c)| Guess {
                item_index: Some(i),
                path: None,
                confidence: c,
            });
        for &fi in &unit.files {
            out.unmatched_files.push(UnmatchedFile {
                path: infos[fi].file.path.clone(),
                best_guess: guess.clone(),
            });
        }
    }
    out
}

fn display_name(p: &Path) -> String {
    sanitise_title(
        &p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// Case C
// ---------------------------------------------------------------------------

/// Case C helper: the `_<release>.noudata` among `candidates`, if any (by name only).
pub fn detect_case_c(candidates: &[CandidateFile]) -> Option<&CandidateFile> {
    candidates
        .iter()
        .find(|c| matches!(classify_file(c), Class::CaseC))
}

/// Case C: only associate. Sets `noubinKey`/`noubinKeyNormalised` on an existing local release
/// document (or leaves it untouched when `key` is `None`). Nothing is re-matched.
pub fn associate_existing(
    mut doc: NoudataDocument,
    key: Option<&str>,
) -> Result<NoudataDocument, UniteError> {
    if let Some(k) = key {
        let (key, norm) = checked_key(k)?;
        doc.set_noubin_keys(key, norm);
    }
    Ok(doc)
}

fn checked_key(k: &str) -> Result<(String, String), UniteError> {
    let t = k.trim();
    if t.is_empty() || t.len() > MAX_KEY_BYTES || t.chars().any(char::is_control) {
        return Err(UniteError::InvalidKey);
    }
    Ok((t.to_string(), normalise_noubin_key(t)))
}

// ---------------------------------------------------------------------------
// build_local_doc
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UniteError {
    #[error("{} item(s) have no file and {} file(s) are unaccounted for", .missing_items.len(), .leftover_files.len())]
    Incomplete {
        missing_items: Vec<usize>,
        leftover_files: Vec<PathBuf>,
    },
    #[error("item {0} does not exist in the release")]
    UnknownItem(usize),
    #[error("item {0} is assigned twice")]
    DuplicateItem(usize),
    #[error("file {0:?} is assigned to more than one item")]
    DuplicateFile(PathBuf),
    #[error("the Noubin key is empty, too long or contains control characters")]
    InvalidKey,
}

/// One confirmed assignment: item position -> local file(s) (several = other formats).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedItem {
    pub item_index: usize,
    pub files: Vec<PathBuf>,
}

/// The user-confirmed mapping (from [`UniteProposal::confirm_all`] / `confirm_confident`, or
/// built by the UI after manual reassignment).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfirmedMapping {
    pub assignments: Vec<ConfirmedItem>,
    /// Every playable file offered to the unite (to check that all are accounted for).
    pub known_files: Vec<PathBuf>,
    /// Files the user chose to leave out (counted as accounted for; not moved).
    pub ignored_files: Vec<PathBuf>,
    /// Build even when items or files are left over (user chose "finish anyway").
    pub allow_incomplete: bool,
}

/// Where the release goes. `dir` is relative to the library root; `existing_names` are the
/// file names already in `dir` (lower-cased comparison; FAT is case-insensitive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub dir: PathBuf,
    pub existing_names: HashSet<String>,
}

impl Placement {
    /// Target folder from the release profile via the §4.4 placement helper
    /// (`Music/<artist>/<release>/`, `Music/Mixes/<dj>/`, `Audiobooks/<author>/<book>/`,
    /// `Podcasts/<show>/`, `Other/<group>/`). Segments are made safe (no `..`, hidden or
    /// reserved names, <= 200 bytes).
    pub fn for_release(web_doc: &NoudataDocument, profile: PlayableItemProfile) -> Self {
        // The artist's own folder-safe choices (standard 03 `playableItemFolderSafeTitle`,
        // `primaryArtistFolderSafeName`) name the local folders; they are still cleaned below.
        let title = shipped_folder_safe_title(web_doc).unwrap_or_else(|| doc_title(web_doc));
        let artist = web_doc
            .release_data
            .as_ref()
            .and_then(|r| r.credits.as_ref())
            .and_then(|c| c.primary_artist_folder_safe_name.as_deref())
            .map(sanitise_title)
            .filter(|a| !a.is_empty())
            .or_else(|| {
                web_doc
                    .primary_artist_display()
                    .map(|a| sanitise_title(&a))
                    .filter(|a| !a.is_empty())
            });
        let kind = match profile {
            PlayableItemProfile::Music => ImportKind::Music,
            PlayableItemProfile::Mix => ImportKind::Mix,
            PlayableItemProfile::Audiobook => ImportKind::Audiobook,
            PlayableItemProfile::Podcast => ImportKind::Podcast,
            PlayableItemProfile::Other => ImportKind::Other,
        };
        let p = import_placement(
            kind,
            &ExtractedFields {
                title: Some(title.clone()),
                album: Some(title),
                artist,
                ..Default::default()
            },
            "x.mp3",
        );
        let dir: PathBuf = p
            .relative_dir
            .components()
            .map(|c| {
                safe_path_segment(
                    &c.as_os_str().to_string_lossy(),
                    "Unknown",
                    MAX_SEGMENT_BYTES,
                )
            })
            .collect();
        Self {
            dir,
            existing_names: HashSet::new(),
        }
    }

    pub fn with_existing_names(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.existing_names = names.into_iter().map(|n| n.to_lowercase()).collect();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOpKind {
    /// Already at the target path.
    Keep,
    /// Move into the release folder, name unchanged.
    Move,
    /// Move into the release folder and rename (illegal characters, length, collision).
    MoveAndRename,
}

/// One planned file operation (applied by NB_09; this module never touches the disk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedFileOp {
    pub item_index: usize,
    /// As given in the candidate list.
    pub source: PathBuf,
    /// Library-root-relative target (`Placement::dir` + safe name).
    pub target: PathBuf,
    pub op: FileOpKind,
}

/// Result of [`build_local_doc`].
#[derive(Debug, Clone, PartialEq)]
pub struct LocalDocPlan {
    pub doc: NoudataDocument,
    /// Library-root-relative path of the `.noudata` to write (`_<release>.noudata`, or
    /// `<track>.noudata` for a single-item release).
    pub noudata_path: PathBuf,
    pub file_ops: Vec<PlannedFileOp>,
    /// Item positions left without a file (only with `allow_incomplete`).
    pub missing_items: Vec<usize>,
    pub warnings: Vec<UniteWarning>,
}

/// The artist's `playableItemFolderSafeTitle`, sanitised; `None` when absent or empty.
fn shipped_folder_safe_title(doc: &NoudataDocument) -> Option<String> {
    doc.playable_item_folder_safe_title
        .as_deref()
        .map(sanitise_title)
        .filter(|t| !t.is_empty())
}

fn doc_title(doc: &NoudataDocument) -> String {
    let t = doc
        .playable_item_title
        .as_deref()
        .map(sanitise_title)
        .unwrap_or_default();
    if t.is_empty() {
        "Untitled".to_string()
    } else {
        t
    }
}

fn clear_local_image_names(c: &mut Option<CoverImage>) {
    if let Some(ci) = c {
        for img in &mut ci.images {
            img.image_local_filename = None;
        }
    }
}

const MAX_REL_PATH: usize = 250;

/// Build the local document and the rename/move plan from `web_doc` + the confirmed mapping.
///
/// * `localFilePaths` = file names relative to the folder the `.noudata` lives in (the
///   placement folder), in mapping order; `uniteData` is **kept** (it holds
///   `expectedFilename`/`freeDownloadURL`; `expectedLength` is a hint the probe may refresh,
///   not authoritative any more).
/// * `noubinKey`/`noubinKeyNormalised` are set only from `key`; any present in the (untrusted)
///   web document, plus `referToReleaseNoudata`, `localFilePaths` and `imageLocalFilename`
///   values, are dropped.
/// * A title with characters illegal in paths gets `playableItemFolderSafeTitle` (§1.28);
///   media files with illegal/over-long names are renamed in the plan.
/// * Fails with [`UniteError::Incomplete`] unless every item has a file and every file is
///   accounted for, or `allow_incomplete` is set.
pub fn build_local_doc(
    web_doc: &NoudataDocument,
    mapping: &ConfirmedMapping,
    key: Option<&str>,
    placement: &Placement,
) -> Result<LocalDocPlan, UniteError> {
    let n = web_doc.media_list.len();
    let key = key.map(checked_key).transpose()?;

    // Validate the mapping.
    let mut seen_items = HashSet::new();
    let mut seen_files: HashSet<&Path> = HashSet::new();
    for a in &mapping.assignments {
        if a.item_index >= n {
            return Err(UniteError::UnknownItem(a.item_index));
        }
        if !seen_items.insert(a.item_index) {
            return Err(UniteError::DuplicateItem(a.item_index));
        }
        for f in &a.files {
            if !seen_files.insert(f.as_path()) {
                return Err(UniteError::DuplicateFile(f.clone()));
            }
        }
    }
    let missing_items: Vec<usize> = (0..n)
        .filter(|i| {
            mapping
                .assignments
                .iter()
                .find(|a| a.item_index == *i)
                .is_none_or(|a| a.files.is_empty())
        })
        .collect();
    let ignored: HashSet<&Path> = mapping.ignored_files.iter().map(PathBuf::as_path).collect();
    let leftover_files: Vec<PathBuf> = mapping
        .known_files
        .iter()
        .filter(|f| !seen_files.contains(f.as_path()) && !ignored.contains(f.as_path()))
        .cloned()
        .collect();
    if (!missing_items.is_empty() || !leftover_files.is_empty()) && !mapping.allow_incomplete {
        return Err(UniteError::Incomplete {
            missing_items,
            leftover_files,
        });
    }

    let mut warnings: Vec<UniteWarning> = Vec::new();
    let mut doc = web_doc.clone();
    // The standard fixes the text of `_whatAmILookingAt`; a linkhost may word its own
    // (tolerated when fetching), but the local file this writer creates carries the fixed one.
    doc.what_am_i_looking_at = Some(crate::WHAT_AM_I_LOOKING_AT.to_string());

    // Titles.
    let raw_title = doc.playable_item_title.clone().unwrap_or_default();
    let title = doc_title(web_doc);
    if raw_title != title {
        warnings.push(warning(
            UniteWarningKind::Sanitised,
            "release title was cleaned",
        ));
    }
    doc.playable_item_title = Some(title.clone());
    // A folder-safe title shipped by the artist wins (standard 03: useful when automatic
    // conversion would drop characters); being untrusted it is cleaned and made path-safe again.
    let folder_safe = shipped_folder_safe_title(&doc)
        .map(|t| safe_path_segment(&t, "Untitled", MAX_SEGMENT_BYTES))
        .unwrap_or_else(|| safe_path_segment(&title, "Untitled", MAX_SEGMENT_BYTES));
    if folder_safe != title || doc.playable_item_folder_safe_title.is_some() {
        doc.playable_item_folder_safe_title = Some(folder_safe.clone());
    }
    doc.playable_item_url_safe_title = doc
        .playable_item_url_safe_title
        .take()
        .map(|s| sanitise_title(&s))
        .filter(|s| !s.is_empty() && !s.contains(['/', '\\']));

    // Keys: only from the tapped tag.
    doc.noubin_key = None;
    doc.noubin_key_normalised = None;
    if let Some((k, norm)) = key {
        doc.set_noubin_keys(k, norm);
    }
    doc.refer_to_release_noudata = None;
    // The united file is the user's local `release` (the web document is `release`, or omits
    // the type; `playableItemProfile` is carried over from it).
    doc.extra.remove("noudataType");
    doc.noudata_type = Some(NoudataType::Release);

    // Local image names in a fetched document point at nothing yet (NB_09 downloads them).
    if let Some(rd) = doc.release_data.as_mut() {
        clear_local_image_names(&mut rd.cover_image);
    }
    if let Some(lp) = doc.local_playback_data.as_mut() {
        clear_local_image_names(&mut lp.local_cover_image);
    }

    // Noudata name + folder.
    let single = n == 1;
    let stem = if single {
        folder_safe.clone()
    } else {
        format!("_{folder_safe}")
    };
    let max_name = MAX_SEGMENT_BYTES
        .min(MAX_REL_PATH.saturating_sub(placement.dir.to_string_lossy().len() + 1))
        .max(40);
    let mut taken: HashSet<String> = placement
        .existing_names
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
    let noudata_base = safe_path_segment(&format!("{stem}.noudata"), "release.noudata", max_name);
    let noudata_name = unique_name(&noudata_base, &mut taken);
    if noudata_name != noudata_base {
        warnings.push(warning(
            UniteWarningKind::Plan,
            "a .noudata with that name already exists in the target folder; the new one gets a numbered name",
        ));
    }
    let noudata_path = placement.dir.join(&noudata_name);

    // Files.
    let mut file_ops = Vec::new();
    for (i, item) in doc.media_list.iter_mut().enumerate() {
        item.local_file_paths = None;
        if let Some(t) = item.media_title.take() {
            let t = sanitise_title(&t);
            item.media_title = Some(t).filter(|t| !t.is_empty());
        }
        clear_local_image_names(&mut item.item_cover_image);
        if let Some(u) = item.unite_data.as_mut() {
            if let Some(name) = u.expected_filename.take() {
                let clean = sanitise_title(&name);
                let safe = if is_safe_relative_path(&clean) && !clean.contains('/') {
                    clean
                } else {
                    warnings.push(UniteWarning {
                        kind: UniteWarningKind::Sanitised,
                        message: format!(
                            "item {}: expectedFilename was not a plain file name",
                            i + 1
                        ),
                        item_index: Some(i),
                        path: None,
                    });
                    safe_path_segment(&clean, "file", MAX_SEGMENT_BYTES)
                };
                u.expected_filename = Some(safe);
            }
        }
        let Some(assign) = mapping.assignments.iter().find(|a| a.item_index == i) else {
            continue;
        };
        let mut names = Vec::new();
        for src in &assign.files {
            let raw = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let safe = safe_path_segment(&raw, "track", max_name);
            let target = placement.dir.join(&safe);
            let (name, op) = if *src == target {
                taken.insert(safe.to_lowercase());
                (safe, FileOpKind::Keep)
            } else {
                let name = unique_name(&safe, &mut taken);
                let op = if name == raw {
                    FileOpKind::Move
                } else {
                    FileOpKind::MoveAndRename
                };
                (name, op)
            };
            let target = placement.dir.join(&name);
            if target.to_string_lossy().len() > 260 {
                warnings.push(UniteWarning {
                    kind: UniteWarningKind::Plan,
                    message: format!("{} exceeds the 260 character path limit", target.display()),
                    item_index: Some(i),
                    path: None,
                });
            }
            file_ops.push(PlannedFileOp {
                item_index: i,
                source: src.clone(),
                target,
                op,
            });
            names.push(name);
        }
        if !names.is_empty() {
            item.local_file_paths = Some(names);
        }
    }

    // Unaccounted paths (only reachable with allow_incomplete) are simply not touched.
    doc.prepare_for_write();
    Ok(LocalDocPlan {
        doc,
        noudata_path,
        file_ops,
        missing_items,
        warnings,
    })
}

/// `name`, or `stem (2).ext`, `stem (3).ext`... until it is free (case-insensitive).
fn unique_name(name: &str, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.len() <= 8 => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    for n in 2..10_000 {
        let candidate = format!("{stem} ({n}){ext}");
        if taken.insert(candidate.to_lowercase()) {
            return candidate;
        }
    }
    // Practically unreachable; keep going with a distinct name rather than overwrite.
    let candidate = format!("{stem} (dup){ext}");
    taken.insert(candidate.to_lowercase());
    candidate
}

/// Re-exported so callers can show/derive the folder-safe form of any title (§1.28).
pub fn folder_safe_title(title: &str) -> String {
    safe_path_segment(&sanitise_title(title), "Untitled", MAX_SEGMENT_BYTES)
}
