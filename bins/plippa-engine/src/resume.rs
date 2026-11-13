//! Session-only resume bookmarks for noubin taps (not persisted to `.noudata`).
//!
//! Remembers the last [`MAX_RESUME_ENTRIES`] tracks played this process. On a
//! later tap of the same Noubin, playback can resume from the stored position
//! unless the bookmark is in the final [`RESUME_RESTART_TAIL_MS`] of the track
//! (or the track already finished) — then the next tap restarts from 0.

/// How many recent tracks to keep in RAM for this session.
pub const MAX_RESUME_ENTRIES: usize = 10;

/// If the bookmark is within this many ms of the end (or past EOF), next tap restarts.
pub const RESUME_RESTART_TAIL_MS: u64 = 15_000;

/// One session bookmark for a playable's media-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeEntry {
    /// Normalised Noubin key that loaded this playable.
    pub normalised_key: String,
    /// `mediaList` position (source order, stable under shuffle) that was playing.
    pub track_index: usize,
    /// Playback position when last left (Stop / Pause / switch / snapshot).
    pub position_ms: u64,
    /// Known duration when available (`None` for many MP3s until EOF).
    pub duration_ms: Option<u64>,
    /// True after the stream hit EOF — next tap should restart.
    pub finished: bool,
}

impl ResumeEntry {
    /// Whether the next **tap** should start from 0 instead of seeking here.
    pub fn should_restart(&self) -> bool {
        if self.finished || self.position_ms == 0 {
            return true;
        }
        match self.duration_ms {
            Some(dur) if dur > 0 => self.position_ms.saturating_add(RESUME_RESTART_TAIL_MS) >= dur,
            _ => false,
        }
    }
}

/// LRU-ish session store: newest entries at the back; capped at [`MAX_RESUME_ENTRIES`].
#[derive(Debug, Default, Clone)]
pub struct ResumeStore {
    entries: Vec<ResumeEntry>,
}

impl ResumeStore {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Lookup by normalised key (most recent match if duplicates somehow exist).
    pub fn get(&self, normalised_key: &str) -> Option<&ResumeEntry> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.normalised_key == normalised_key)
    }

    /// Insert or replace the bookmark for this key; move it to most-recent.
    pub fn upsert(&mut self, entry: ResumeEntry) {
        self.entries
            .retain(|e| e.normalised_key != entry.normalised_key);
        self.entries.push(entry);
        while self.entries.len() > MAX_RESUME_ENTRIES {
            self.entries.remove(0);
        }
    }

    /// Forget a key (e.g. after a restart-from-zero tap consumes the bookmark).
    pub fn remove(&mut self, normalised_key: &str) {
        self.entries
            .retain(|e| e.normalised_key != normalised_key);
    }

    /// Drop every bookmark (portal Unload / forget session resume).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub fn keys_oldest_first(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|e| e.normalised_key.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_when_finished_or_in_tail() {
        let finished = ResumeEntry {
            normalised_key: "a".into(),
            track_index: 0,
            position_ms: 10_000,
            duration_ms: Some(60_000),
            finished: true,
        };
        assert!(finished.should_restart());

        let in_tail = ResumeEntry {
            normalised_key: "a".into(),
            track_index: 0,
            position_ms: 50_000,
            duration_ms: Some(60_000),
            finished: false,
        };
        assert!(in_tail.should_restart()); // 50s + 15s >= 60s

        let mid = ResumeEntry {
            normalised_key: "a".into(),
            track_index: 0,
            position_ms: 20_000,
            duration_ms: Some(60_000),
            finished: false,
        };
        assert!(!mid.should_restart());

        let at_start = ResumeEntry {
            normalised_key: "a".into(),
            track_index: 0,
            position_ms: 0,
            duration_ms: Some(60_000),
            finished: false,
        };
        assert!(at_start.should_restart());

        let unknown_dur = ResumeEntry {
            normalised_key: "a".into(),
            track_index: 0,
            position_ms: 90_000,
            duration_ms: None,
            finished: false,
        };
        assert!(!unknown_dur.should_restart());
    }

    #[test]
    fn store_caps_at_ten_and_updates_in_place() {
        let mut store = ResumeStore::new();
        for i in 0..12 {
            store.upsert(ResumeEntry {
                normalised_key: format!("k{i}"),
                track_index: 0,
                position_ms: i * 1000,
                duration_ms: Some(60_000),
                finished: false,
            });
        }
        assert_eq!(store.len(), MAX_RESUME_ENTRIES);
        assert!(store.get("k0").is_none());
        assert!(store.get("k1").is_none());
        assert_eq!(store.get("k2").map(|e| e.position_ms), Some(2_000));

        store.upsert(ResumeEntry {
            normalised_key: "k5".into(),
            track_index: 1,
            position_ms: 9_999,
            duration_ms: Some(60_000),
            finished: false,
        });
        assert_eq!(store.len(), MAX_RESUME_ENTRIES);
        let e = store.get("k5").unwrap();
        assert_eq!(e.track_index, 1);
        assert_eq!(e.position_ms, 9_999);
        assert_eq!(store.keys_oldest_first().last().copied(), Some("k5"));

        store.clear();
        assert!(store.is_empty());
        assert!(store.get("k5").is_none());
    }
}
