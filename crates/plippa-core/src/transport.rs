//! Transport and session state for playback control.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// High-level transport state for the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum TransportState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// Errors from invalid transport transitions.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    #[error("invalid transport transition: {from:?} → {to:?}")]
    InvalidTransition {
        from: TransportState,
        to: TransportState,
    },
}

impl TransportState {
    /// Whether `self → next` is a valid control-plane transition.
    ///
    /// Allowed:
    /// - `Stopped → Playing`
    /// - `Playing → Paused | Stopped`
    /// - `Paused → Playing | Stopped`
    /// - same-state (no-op) is allowed
    ///
    /// Rejected:
    /// - `Stopped → Paused` (nothing to pause)
    pub fn can_transition_to(self, next: Self) -> bool {
        use TransportState::*;
        match (self, next) {
            (a, b) if a == b => true,
            (Stopped, Playing) => true,
            (Stopped, Paused) => false,
            (Playing, Paused) | (Playing, Stopped) => true,
            (Paused, Playing) | (Paused, Stopped) => true,
            _ => false,
        }
    }

    /// Apply a transition or return [`TransportError::InvalidTransition`].
    pub fn transition(self, next: Self) -> Result<Self, TransportError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(TransportError::InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

/// Playback volume as an integer percent (0..=100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Volume {
    percent: u8,
}

impl Volume {
    pub const MIN: u8 = 0;
    pub const MAX: u8 = 100;
    pub const DEFAULT_PERCENT: u8 = 80;

    /// Create a volume, clamping to 0..=100.
    pub fn new(percent: u8) -> Self {
        Self {
            percent: percent.min(Self::MAX),
        }
    }

    pub const fn percent(self) -> u8 {
        self.percent
    }

    /// Linear gain in `0.0..=1.0` for audio scaling.
    pub fn as_linear(self) -> f32 {
        f32::from(self.percent) / f32::from(Self::MAX)
    }
}

impl Default for Volume {
    fn default() -> Self {
        Self::new(Self::DEFAULT_PERCENT)
    }
}

/// Snapshot of engine session state visible to UI / remote control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub transport: TransportState,
    pub volume: Volume,
    pub current_item: Option<crate::PlayableItemId>,
    /// Playback position within the active stream (ms from start).
    pub position_ms: u64,
    /// Known duration of the active stream (`None` if unknown / nothing loaded).
    pub duration_ms: Option<u64>,
    /// Display title for the loaded playable (may be empty).
    pub title: String,
    /// Display artist for the loaded playable (empty → UI may show blank).
    pub artist: String,
    /// Index of the current **track** in the `nowPlayingList` (0 when nothing loaded).
    /// Pairs with `NowPlayingListChanged`; see `plippa_core::NowPlayingList`.
    pub current_entry_index: u32,
    /// Index of the current cue among the current track's *enabled navigable* cues,
    /// derived from the media position (`None`: no cues, before the first, or in a gap).
    pub current_cue_index: Option<u32>,
    /// Previous control usable: `NowPlayingList::can_prev()` (something loaded). NB_14.
    pub can_prev: bool,
    /// Next control usable at the current media position: `NowPlayingList::can_next(pos)`.
    pub can_next: bool,
    // ---- WARP_01 (appended; Postcard is positional) -------------------------------------
    // Until WARP_05/09/14/17 land the engine stores and publishes the *targets* below but
    // applies nothing: `rate_permille` stays 1000, `warp_active` false, `speed_voice` Bypass.
    /// Instantaneous effective rate in permille, signed, **including seek** (1000 = 1.0x).
    pub rate_permille: i32,
    /// Session-wide set speed (permille, signed); the only thing `SetRate`/`ResetRate` edit.
    pub target_rate_permille: i32,
    /// Warp mode of the speed family (tempo, FF/RW taps).
    pub speed_warp_mode: crate::WarpMode,
    /// Warp mode of the seek family (hold scan, jog jump).
    pub seek_warp_mode: crate::WarpMode,
    /// Hold scan state (velocity-controlled, no destination).
    pub scan: crate::ScanState,
    /// Target of a **committed** `SeekTo` chase in progress (engine-owned); `None` while a
    /// scan runs and when idle. `position_ms` stays the actual playhead meanwhile.
    pub seek_target_ms: Option<u64>,
    /// Diagnostic only: which voice the speed family uses (never a UI indicator).
    pub speed_voice: crate::VoiceKind,
    /// Any warping/seeking DSP is active (false while the pure path runs).
    pub warp_active: bool,
    /// FF control usable: something is loaded (independent of transport/position/tempo).
    pub can_ff: bool,
    /// RW control usable: something is loaded (independent of transport/position/tempo).
    pub can_rw: bool,
    // ---- WARP_19 (appended) ------------------------------------------------------------
    /// Play-mode setting "Seek outside track" (engine-stored, the UI re-sends it).
    pub seek_outside_track: bool,
    /// Duration of the item `Previous` would reach (the UI's live jog target may run into it);
    /// `None` = no neighbour, setting off, or unknown.
    pub prev_duration_ms: Option<u64>,
    /// Duration of the item `Next` would reach; `None` as above.
    pub next_duration_ms: Option<u64>,
    // ---- WAVE_01 (appended) -------------------------------------------------------------
    /// Absolute path of this item's `.wfm` sidecar. `Some` whenever a media file is loaded,
    /// even if the file does not exist yet (the UI shows ANALYSING until it appears).
    pub waveform_path: Option<String>,
    // ---- UI_13 (appended) ---------------------------------------------------------------
    /// Collection the loaded item belongs to, as the owning record names it (album / playlist /
    /// mix / audiobook / podcast title). Empty for a single loose track, where repeating the
    /// track title as a collection would be noise, and when nothing is loaded.
    ///
    /// Untrusted text: the engine passes it through and the UI sanitises it before drawing.
    pub collection_title: String,
    // ---- PLAY_02 (appended) --------------------------------------------------------------
    /// Playback order the player setting asks for (`Normal` / `Shuffle`).
    pub order_mode: crate::OrderMode,
    /// What happens at the end of the item / queue. Cleared to `Off` by any new load.
    pub repeat_mode: crate::RepeatMode,
    /// Pause between items instead of auto-advancing: at the end of an item the transport goes
    /// `Stopped` with the *next* item already current at 0:00 (cues loaded), so Play starts it
    /// straight away (classroom / exercise use).
    pub pause_after_each: bool,
    /// Repeat-queue at the end of a shuffled list starts a fresh shuffle (default on).
    pub reshuffle_on_repeat: bool,
    /// Whether the loaded list can be put back into its source order (`Fixed` when nothing
    /// is loaded, so the UI can say "there is no fixed order" only for a dynamic queue).
    pub source_order: crate::SourceOrder,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            transport: TransportState::Stopped,
            volume: Volume::default(),
            current_item: None,
            position_ms: 0,
            duration_ms: None,
            title: String::new(),
            artist: String::new(),
            current_entry_index: 0,
            current_cue_index: None,
            can_prev: false,
            can_next: false,
            rate_permille: crate::RATE_UNITY_PERMILLE,
            target_rate_permille: crate::RATE_UNITY_PERMILLE,
            speed_warp_mode: crate::SPEED_WARP_DEFAULT,
            seek_warp_mode: crate::SEEK_WARP_DEFAULT,
            scan: crate::ScanState::None,
            seek_target_ms: None,
            speed_voice: crate::VoiceKind::Bypass,
            warp_active: false,
            can_ff: false,
            can_rw: false,
            seek_outside_track: false,
            prev_duration_ms: None,
            next_duration_ms: None,
            waveform_path: None,
            collection_title: String::new(),
            order_mode: crate::OrderMode::Normal,
            repeat_mode: crate::RepeatMode::Off,
            pause_after_each: false,
            reshuffle_on_repeat: true,
            source_order: crate::SourceOrder::Fixed,
        }
    }
}

impl SessionState {
    /// Attempt a transport transition in place.
    pub fn set_transport(&mut self, next: TransportState) -> Result<(), TransportError> {
        self.transport = self.transport.transition(next)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_to_playing_allowed() {
        assert!(TransportState::Stopped
            .transition(TransportState::Playing)
            .is_ok());
    }

    #[test]
    fn stopped_to_paused_rejected() {
        let err = TransportState::Stopped
            .transition(TransportState::Paused)
            .expect_err("Stopped → Paused must fail");
        assert!(matches!(
            err,
            TransportError::InvalidTransition {
                from: TransportState::Stopped,
                to: TransportState::Paused,
            }
        ));
    }

    #[test]
    fn playing_pause_stop_cycle() {
        let mut state = TransportState::Stopped;
        state = state.transition(TransportState::Playing).unwrap();
        state = state.transition(TransportState::Paused).unwrap();
        state = state.transition(TransportState::Playing).unwrap();
        state = state.transition(TransportState::Stopped).unwrap();
        assert_eq!(state, TransportState::Stopped);
    }

    #[test]
    fn same_state_is_noop_ok() {
        assert_eq!(
            TransportState::Playing
                .transition(TransportState::Playing)
                .unwrap(),
            TransportState::Playing
        );
    }

    #[test]
    fn volume_clamps_and_linear() {
        assert_eq!(Volume::new(150).percent(), 100);
        assert!((Volume::new(50).as_linear() - 0.5).abs() < f32::EPSILON);
        assert_eq!(Volume::DEFAULT_PERCENT, 80);
        assert_eq!(Volume::default().percent(), Volume::DEFAULT_PERCENT);
    }

    #[test]
    fn session_set_transport_rejects_invalid() {
        let mut session = SessionState::default();
        assert!(session.set_transport(TransportState::Paused).is_err());
        assert_eq!(session.transport, TransportState::Stopped);
        session.set_transport(TransportState::Playing).unwrap();
        assert_eq!(session.transport, TransportState::Playing);
    }
}
