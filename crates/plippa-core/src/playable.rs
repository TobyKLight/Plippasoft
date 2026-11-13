//! Playable-item identity (Noubin §1.21 — types only; no `.noudata` I/O).

use serde::{Deserialize, Serialize};

/// Opaque id assigned by the library index (`plippa-server`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayableItemId(pub u64);

impl PlayableItemId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Identity fields used for display and Noubin association.
///
/// Aligns with Noubin metadata concepts (`playableItemTitle`, `noubinKeyNormalised`)
/// without pulling in full `.noudata` schema (that lives in `plippa-noubin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayableItemIdentity {
    pub id: PlayableItemId,
    /// Display title (`playableItemTitle`).
    pub title: String,
    /// Normalised key for tap matching, if associated (`noubinKeyNormalised`).
    pub noubin_key_normalised: Option<String>,
}

impl PlayableItemIdentity {
    pub fn new(id: PlayableItemId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            noubin_key_normalised: None,
        }
    }

    pub fn with_noubin_key_normalised(mut self, key: impl Into<String>) -> Self {
        self.noubin_key_normalised = Some(key.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_defaults_without_key() {
        let item = PlayableItemIdentity::new(PlayableItemId::new(1), "Smooth Jazz");
        assert_eq!(item.title, "Smooth Jazz");
        assert!(item.noubin_key_normalised.is_none());
    }

    #[test]
    fn identity_can_carry_normalised_key() {
        let item = PlayableItemIdentity::new(PlayableItemId::new(2), "Album")
            .with_noubin_key_normalised("album.noubin.xyz123");
        assert_eq!(
            item.noubin_key_normalised.as_deref(),
            Some("album.noubin.xyz123")
        );
    }
}
