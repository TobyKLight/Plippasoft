//! Play modes (PLAY_02): playback order, repeat behaviour and the SW2 cycle.
//!
//! The mode is a **player setting**, not file data: Noubin v0.4.1 retired
//! `localPlaybackData.{shuffle,repeatList,stopAfterEach}` and the engine keeps the
//! mode in its session state instead (reset by an engine restart, persisted nowhere).
//!
//! Three orthogonal pieces:
//!
//! * [`OrderMode`] — normal (`mediaList` order) or shuffle.
//! * [`RepeatMode`] — off, repeat the queue, or repeat the current item.
//! * [`SourceOrder`] — whether the loaded list can be put back into its source order
//!   ([`SourceOrder::Fixed`]: an album / playlist / mix from a `.noudata`), or is a
//!   dynamic queue built by add-to-queue / play-next ([`SourceOrder::Dynamic`], no order
//!   to go back to).
//!
//! [`PLAY_MODE_CYCLE`] is the canonical single-button cycle the UI walks; the engine only
//! exposes the orthogonal setters, and a UI may drive them in any other combination.

use serde::{Deserialize, Serialize};

/// What the user asked playback order to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum OrderMode {
    /// Source order of the loaded list (`mediaList` order for a fixed list).
    #[default]
    Normal,
    /// A shuffled order; the current track stays where the playhead is.
    Shuffle,
}

/// What happens at the end of the current item / queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum RepeatMode {
    /// Stop at the end of the list.
    #[default]
    Off,
    /// Wrap to the start of the list (reshuffling a shuffled list when configured to).
    Queue,
    /// Restart the current item at its end. Cleared by any new load.
    Item,
}

/// Whether the loaded list can be put back into its source order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SourceOrder {
    /// Album / playlist / mix loaded from a `.noudata`: `mediaList` order can be restored.
    #[default]
    Fixed,
    /// A queue grown by add-to-queue / play-next: there is no order to go back to.
    Dynamic,
}

/// One canonical rung of the PlayMode (SW2) cycle: order × repeat, plus the item rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlayModeState {
    pub order: OrderMode,
    pub repeat: RepeatMode,
}

/// The five rungs, in cycle order (owner, 2026-10-24). `RepeatMode::Item` is its own rung:
/// order is irrelevant while one item repeats, so it is normalised to `OrderMode::Normal`.
pub const PLAY_MODE_CYCLE: [PlayModeState; 5] = [
    PlayModeState {
        order: OrderMode::Normal,
        repeat: RepeatMode::Off,
    },
    PlayModeState {
        order: OrderMode::Shuffle,
        repeat: RepeatMode::Off,
    },
    PlayModeState {
        order: OrderMode::Normal,
        repeat: RepeatMode::Queue,
    },
    PlayModeState {
        order: OrderMode::Shuffle,
        repeat: RepeatMode::Queue,
    },
    PlayModeState {
        order: OrderMode::Normal,
        repeat: RepeatMode::Item,
    },
];

/// Canonical form of a mode: repeat-item has no meaningful order.
fn canonical(s: PlayModeState) -> PlayModeState {
    if s.repeat == RepeatMode::Item {
        PlayModeState {
            order: OrderMode::Normal,
            repeat: RepeatMode::Item,
        }
    } else {
        s
    }
}

/// The rung after `cur`. Non-canonical input (e.g. Shuffle + Item) is normalised first, so the
/// cycle can never get stuck.
pub fn cycle_next(cur: PlayModeState) -> PlayModeState {
    let cur = canonical(cur);
    let index = PLAY_MODE_CYCLE
        .iter()
        .position(|rung| *rung == cur)
        .unwrap_or(0);
    PLAY_MODE_CYCLE[(index + 1) % PLAY_MODE_CYCLE.len()]
}

/// `Normal` / `Shuffle` / `Repeat all` / `Shuffle + repeat` / `Repeat one` (popup wording).
///
/// The longest wording, `Shuffle + repeat`, is 16 glyphs: row-safe for the status line.
pub fn play_mode_text(s: PlayModeState) -> &'static str {
    match canonical(s) {
        PlayModeState {
            order: OrderMode::Normal,
            repeat: RepeatMode::Off,
        } => "Normal",
        PlayModeState {
            order: OrderMode::Shuffle,
            repeat: RepeatMode::Off,
        } => "Shuffle",
        PlayModeState {
            order: OrderMode::Normal,
            repeat: RepeatMode::Queue,
        } => "Repeat all",
        PlayModeState {
            order: OrderMode::Shuffle,
            repeat: RepeatMode::Queue,
        } => "Shuffle + repeat",
        PlayModeState {
            repeat: RepeatMode::Item,
            ..
        } => "Repeat one",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_mode_cycle_is_five_rungs_and_wraps() {
        assert_eq!(PLAY_MODE_CYCLE.len(), 5);
        let mut seen = Vec::new();
        let mut cur = PLAY_MODE_CYCLE[0];
        for _ in 0..5 {
            seen.push(cur);
            cur = cycle_next(cur);
        }
        assert_eq!(seen, PLAY_MODE_CYCLE.to_vec(), "the cycle order is the table");
        assert_eq!(cur, PLAY_MODE_CYCLE[0], "the fifth rung wraps to the first");
        assert_eq!(
            play_mode_text(PLAY_MODE_CYCLE[0]),
            "Normal",
            "a fresh engine is normal / repeat off"
        );
        let texts: Vec<&str> = PLAY_MODE_CYCLE.iter().copied().map(play_mode_text).collect();
        assert_eq!(
            texts,
            ["Normal", "Shuffle", "Repeat all", "Shuffle + repeat", "Repeat one"]
        );
        for t in texts {
            assert!(t.chars().count() <= 20, "{t} is row-safe");
        }
    }

    #[test]
    fn play_mode_cycle_normalises_shuffle_plus_item() {
        let shuffle_item = PlayModeState {
            order: OrderMode::Shuffle,
            repeat: RepeatMode::Item,
        };
        // Order is irrelevant on the item rung: it is the repeat-one rung, and the next rung
        // is the first one (never stuck).
        assert_eq!(cycle_next(shuffle_item), PLAY_MODE_CYCLE[0]);
        assert_eq!(play_mode_text(shuffle_item), "Repeat one");
        assert_eq!(
            cycle_next(PLAY_MODE_CYCLE[4]),
            PLAY_MODE_CYCLE[0],
            "repeat one is the last rung"
        );
    }
}
