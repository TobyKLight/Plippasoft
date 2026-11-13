//! The name shown on the "collection" line: the loaded collection, plus the title of the mix
//! that is playing inside it ("Band Name Bangers (Live In New York)").
//!
//! One pure rule shared by the engine (which publishes it on `SessionState`) so every
//! surface shows the same string.

/// What the loaded playable is, as far as the collection line cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadedKind {
    /// A single loose track: no collection line.
    Track,
    /// A mix loaded on its own: its title is the collection name, never suffixed.
    Mix,
    /// A playlist, album, audiobook or podcast.
    Collection,
}

/// Longest composed name in characters (the screens cap single-line text at the same
/// value, so a longer string would be clipped from the end and lose the mix title).
pub const COLLECTION_TITLE_MAX_CHARS: usize = 120;

const ELLIPSIS: char = '…';

/// Compose the collection line.
///
/// `collection` and `playing_mix_title` must already be sanitised (control, bidi and
/// zero-width characters stripped; `plippa_noubin::sanitise_line`). `playing_mix_title` is
/// `Some` only when the item that is playing is a mix with navigable cues.
///
/// The suffix `(M)` is added only for [`LoadedKind::Collection`], when `M` is non-empty and
/// differs from the collection title (case-insensitive, trimmed). When the result would
/// exceed [`COLLECTION_TITLE_MAX_CHARS`] the collection part is clipped with `…` and the
/// suffix stays whole; `M` itself is clipped only when it alone does not fit.
pub fn compose_collection_title(
    collection: &str,
    kind: LoadedKind,
    playing_mix_title: Option<&str>,
) -> String {
    let collection = collection.trim();
    match kind {
        LoadedKind::Track => return String::new(),
        LoadedKind::Mix => return clip(collection, COLLECTION_TITLE_MAX_CHARS),
        LoadedKind::Collection => {}
    }
    let mix = playing_mix_title
        .map(str::trim)
        .filter(|m| !m.is_empty() && m.to_lowercase() != collection.to_lowercase());
    let Some(mix) = mix.filter(|_| !collection.is_empty()) else {
        return clip(collection, COLLECTION_TITLE_MAX_CHARS);
    };
    // " (" + M + ")"
    let suffix_chars = mix.chars().count() + 3;
    let collection_chars = collection.chars().count();
    if collection_chars + suffix_chars <= COLLECTION_TITLE_MAX_CHARS {
        return format!("{collection} ({mix})");
    }
    // Room for at least one collection character plus the ellipsis.
    if suffix_chars + 2 <= COLLECTION_TITLE_MAX_CHARS && !collection.is_empty() {
        let keep = COLLECTION_TITLE_MAX_CHARS - suffix_chars - 1;
        let head: String = collection.chars().take(keep).collect();
        return format!("{}{ELLIPSIS} ({mix})", head.trim_end());
    }
    // The mix title alone fills the line: show it on its own, bracketed, never "(" dangling.
    format!("({})", clip(mix, COLLECTION_TITLE_MAX_CHARS - 2))
}

fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}{ELLIPSIS}", head.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use LoadedKind::{Collection, Mix, Track};

    const LIVE: &str = "Live In New York";

    #[test]
    fn playlist_with_playing_mix_gets_the_suffix() {
        assert_eq!(
            compose_collection_title("Band Name Bangers", Collection, Some(LIVE)),
            "Band Name Bangers (Live In New York)"
        );
        // An album that holds a mix behaves the same.
        assert_eq!(
            compose_collection_title("Box Set", Collection, Some(LIVE)),
            "Box Set (Live In New York)"
        );
    }

    #[test]
    fn plain_track_in_a_collection_shows_only_the_collection() {
        assert_eq!(
            compose_collection_title("Band Name Bangers", Collection, None),
            "Band Name Bangers"
        );
    }

    #[test]
    fn mix_on_its_own_keeps_its_title_without_a_suffix() {
        assert_eq!(compose_collection_title(LIVE, Mix, Some(LIVE)), LIVE);
        assert_eq!(compose_collection_title(LIVE, Mix, Some("Other")), LIVE);
    }

    #[test]
    fn loose_track_has_no_collection_line() {
        assert_eq!(compose_collection_title("Song", Track, Some(LIVE)), "");
    }

    #[test]
    fn same_or_empty_mix_title_adds_nothing() {
        assert_eq!(
            compose_collection_title("Live in new york", Collection, Some("  LIVE IN NEW YORK ")),
            "Live in new york"
        );
        assert_eq!(compose_collection_title("Bangers", Collection, Some("")), "Bangers");
        assert_eq!(compose_collection_title("Bangers", Collection, Some("  ")), "Bangers");
    }

    #[test]
    fn long_collection_is_clipped_and_the_suffix_stays_whole() {
        let long = "x".repeat(300);
        let s = compose_collection_title(&long, Collection, Some(LIVE));
        assert_eq!(s.chars().count(), COLLECTION_TITLE_MAX_CHARS);
        assert!(s.ends_with("… (Live In New York)"), "{s}");
        assert!(s.starts_with("xxx"));
    }

    #[test]
    fn exact_fit_is_not_clipped() {
        let c = "c".repeat(COLLECTION_TITLE_MAX_CHARS - LIVE.chars().count() - 3);
        let s = compose_collection_title(&c, Collection, Some(LIVE));
        assert_eq!(s, format!("{c} ({LIVE})"));
        let c = format!("{c}c");
        let s = compose_collection_title(&c, Collection, Some(LIVE));
        assert_eq!(s.chars().count(), COLLECTION_TITLE_MAX_CHARS);
        assert!(s.contains('…'));
    }

    #[test]
    fn huge_mix_title_is_clipped_alone_and_never_dangles() {
        let m = "m".repeat(500);
        let s = compose_collection_title("Bangers", Collection, Some(&m));
        assert!(s.starts_with('(') && s.ends_with("…)"), "{s}");
        assert_eq!(s.chars().count(), COLLECTION_TITLE_MAX_CHARS);
        // Mix that leaves exactly one collection character is still composed.
        let m = "m".repeat(COLLECTION_TITLE_MAX_CHARS - 5);
        let s = compose_collection_title("Bangers", Collection, Some(&m));
        assert_eq!(s, format!("B… ({m})"));
        assert_eq!(s.chars().count(), COLLECTION_TITLE_MAX_CHARS);
    }

    #[test]
    fn never_produces_empty_brackets() {
        for c in ["", "Bangers"] {
            for m in [None, Some(""), Some(" "), Some(LIVE)] {
                let s = compose_collection_title(c, Collection, m);
                assert!(!s.contains("()") && !s.starts_with(" ("), "{s:?}");
            }
        }
    }
}
