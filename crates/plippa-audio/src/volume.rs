//! Linear playback volume (0.0–1.0), safe to set from a control thread.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Shared linear gain. Designed so the audio thread reads without locking.
#[derive(Debug, Clone)]
pub struct Volume {
    /// Gain stored as `f32` bits in an `AtomicU32`.
    bits: Arc<AtomicU32>,
}

impl Default for Volume {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl Volume {
    pub fn new(linear: f32) -> Self {
        let v = Self {
            bits: Arc::new(AtomicU32::new(0)),
        };
        v.set(linear);
        v
    }

    /// Clamp to [0.0, 1.0] and store.
    pub fn set(&self, linear: f32) {
        let g = linear.clamp(0.0, 1.0);
        self.bits.store(g.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }

    /// Apply current gain in-place to interleaved PCM.
    pub fn apply(&self, samples: &mut [f32]) {
        let g = self.get();
        if (g - 1.0).abs() < f32::EPSILON {
            return;
        }
        if g == 0.0 {
            samples.fill(0.0);
            return;
        }
        for s in samples.iter_mut() {
            *s *= g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_zero_silences() {
        let v = Volume::new(0.0);
        let mut samples = vec![0.5f32, -0.5, 0.25, -0.25];
        v.apply(&mut samples);
        assert!(samples.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn volume_clamps() {
        let v = Volume::new(2.0);
        assert!((v.get() - 1.0).abs() < f32::EPSILON);
        v.set(-1.0);
        assert_eq!(v.get(), 0.0);
    }
}
