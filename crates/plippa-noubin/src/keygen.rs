//! §1.16 Local Noubin Key generation and collision avoidance.

use crate::normalise::normalise_noubin_key;
use rand::Rng;
use std::collections::HashSet;
use thiserror::Error;

/// Errors from local key generation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyGenError {
    #[error("tag key capacity is zero")]
    CapacityTooSmall,
    #[error("could not find a non-colliding key after many attempts")]
    CollisionExhausted,
}

/// Parameters for generating a local-only Noubin Key (NDEF Text).
#[derive(Debug, Clone)]
pub struct LocalKeyParams<'a> {
    /// Display title used in `<title>.noubin.<random>` form.
    pub playable_item_title: &'a str,
    /// Maximum UTF-8 byte length of the key string that fits the tag.
    ///
    /// For very small tags (~48 bytes of usable text payload), pass that budget;
    /// when the full form does not fit, only a random string is used (§1.16).
    pub max_key_utf8_bytes: usize,
    /// Existing `noubinKeyNormalised` values that must not collide.
    pub existing_normalised: &'a HashSet<String>,
    /// Length of the random suffix (and of random-only keys).
    pub random_len: usize,
}

/// Generate a local Noubin Key that normalises uniquely against `existing_normalised`.
///
/// Returns `(noubin_key, noubin_key_normalised)`.
pub fn generate_local_noubin_key(
    params: &LocalKeyParams<'_>,
) -> Result<(String, String), KeyGenError> {
    if params.max_key_utf8_bytes == 0 {
        return Err(KeyGenError::CapacityTooSmall);
    }
    let random_len = params.random_len.max(1);

    for _ in 0..64 {
        let random = random_alnum(random_len);
        let candidate =
            build_candidate(params.playable_item_title, &random, params.max_key_utf8_bytes);
        let normalised = normalise_noubin_key(&candidate);
        if !params.existing_normalised.contains(&normalised) {
            return Ok((candidate, normalised));
        }
    }
    Err(KeyGenError::CollisionExhausted)
}

fn build_candidate(title: &str, random: &str, max_bytes: usize) -> String {
    let full = format!("{title}.noubin.{random}");
    if full.len() <= max_bytes {
        return full;
    }
    // §1.16: very small tags — recommend random string only.
    if random.len() <= max_bytes {
        return random.to_string();
    }
    // Truncate random to fit (last resort).
    let mut out = String::new();
    for ch in random.chars() {
        let mut buf = [0u8; 4];
        let enc = ch.encode_utf8(&mut buf);
        if out.len() + enc.len() > max_bytes {
            break;
        }
        out.push_str(enc);
    }
    out
}

fn random_alnum(len: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_capacity_uses_random_only() {
        let existing = HashSet::new();
        let title = "Smooth Jazz Playlist With A Very Long Name";
        let params = LocalKeyParams {
            playable_item_title: title,
            max_key_utf8_bytes: 48,
            existing_normalised: &existing,
            random_len: 12,
        };
        let (key, norm) = generate_local_noubin_key(&params).unwrap();
        assert!(key.len() <= 48);
        assert!(!key.contains(".noubin."), "48-byte budget should drop title form");
        assert_eq!(norm, normalise_noubin_key(&key));
        assert_eq!(norm, key.to_ascii_lowercase()); // plain text random
    }

    #[test]
    fn collision_avoidance_retries() {
        let mut existing = HashSet::new();
        // Generate once, seed that normalised key, then generate again.
        let (key1, norm1) = generate_local_noubin_key(&LocalKeyParams {
            playable_item_title: "Mix",
            max_key_utf8_bytes: 256,
            existing_normalised: &existing,
            random_len: 8,
        })
        .unwrap();
        existing.insert(norm1.clone());
        let (key2, norm2) = generate_local_noubin_key(&LocalKeyParams {
            playable_item_title: "Mix",
            max_key_utf8_bytes: 256,
            existing_normalised: &existing,
            random_len: 8,
        })
        .unwrap();
        assert_ne!(norm1, norm2);
        assert!(key1.contains(".noubin."));
        assert!(key2.contains(".noubin."));
    }
}
