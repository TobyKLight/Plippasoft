//! Noubin Lazy Timestamp (`02 standard.md` §5).
//!
//! Internal representation is integer milliseconds. Fractional digits beyond the
//! third are rounded half-up to the nearest millisecond (`1.0005` → `1.001`).

use serde::{Serialize, Serializer};
use std::fmt;
use thiserror::Error;

/// A position on a media timeline, in milliseconds from the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NoubinTimestamp(u64);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TimestampError {
    #[error("empty timestamp")]
    Empty,
    #[error("negative timestamps are not permitted")]
    Negative,
    #[error("malformed timestamp")]
    Malformed,
    #[error("too many ':' separated components (max HH:MM:SS)")]
    TooManyComponents,
    #[error("timestamp out of range")]
    Overflow,
}

impl NoubinTimestamp {
    pub const ZERO: Self = Self(0);

    pub const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Parse `SS`, `SS.mmm`, `MM:SS`, `MM:SS.mmm`, `HH:MM:SS`, `HH:MM:SS.mmm`
    /// (components read right to left, may exceed 59, surrounding whitespace ignored).
    pub fn parse(input: &str) -> Result<Self, TimestampError> {
        let s = input.trim();
        if s.is_empty() {
            return Err(TimestampError::Empty);
        }
        if s.starts_with('-') {
            return Err(TimestampError::Negative);
        }
        let (main, frac) = match s.split_once('.') {
            Some((m, f)) => (m, Some(f)),
            None => (s, None),
        };
        if let Some(f) = frac {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return Err(TimestampError::Malformed);
            }
        }
        let parts: Vec<&str> = main.split(':').collect();
        if parts.len() > 3 {
            return Err(TimestampError::TooManyComponents);
        }
        let mut secs: u64 = 0;
        let mut unit: u64 = 1;
        for part in parts.iter().rev() {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(TimestampError::Malformed);
            }
            let v: u64 = part.parse().map_err(|_| TimestampError::Overflow)?;
            secs = v
                .checked_mul(unit)
                .and_then(|x| secs.checked_add(x))
                .ok_or(TimestampError::Overflow)?;
            unit *= 60;
        }
        let mut ms = secs.checked_mul(1000).ok_or(TimestampError::Overflow)?;
        if let Some(f) = frac {
            let b = f.as_bytes();
            let digit = |i: usize| b.get(i).map_or(0u64, |c| u64::from(c - b'0'));
            let mut frac_ms = digit(0) * 100 + digit(1) * 10 + digit(2);
            if digit(3) >= 5 {
                frac_ms += 1;
            }
            ms = ms.checked_add(frac_ms).ok_or(TimestampError::Overflow)?;
        }
        Ok(Self(ms))
    }
}

impl std::str::FromStr for NoubinTimestamp {
    type Err = TimestampError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Writer form: `HH:MM:SS` padded, `.mmm` only when non-zero and without trailing zeros.
impl fmt::Display for NoubinTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total_secs = self.0 / 1000;
        let ms = self.0 % 1000;
        write!(
            f,
            "{:02}:{:02}:{:02}",
            total_secs / 3600,
            (total_secs / 60) % 60,
            total_secs % 60
        )?;
        if ms != 0 {
            let frac = format!("{ms:03}");
            write!(f, ".{}", frac.trim_end_matches('0'))?;
        }
        Ok(())
    }
}

impl Serialize for NoubinTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
