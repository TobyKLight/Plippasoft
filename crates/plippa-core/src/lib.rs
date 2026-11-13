//! Shared Plippa domain types: playable items, transport, and board capabilities.
//!
//! Hardware-free and suitable for engine, UI, and server processes.

pub mod audio_route;
mod capabilities;
mod collection_title;
mod nowplaying;
mod play_mode;
mod playable;
pub mod qr;
pub mod safe_fs;
mod transport;
mod warp;
pub mod waveform;

pub use audio_route::{
    sanitise_wanted, AudioOutput, AudioRoute, AudioRouteProblem, AudioRouteWanted,
    DELAY_MAX_MS, DELAY_STEP_MS, INTERNAL_SINK_PREFIX, MAX_ENABLED_OUTPUTS,
};
pub use capabilities::{audio_format_label, AudioFormatSupport, Capabilities};
pub use collection_title::{compose_collection_title, LoadedKind, COLLECTION_TITLE_MAX_CHARS};
pub use nowplaying::{
    media_position_ms, Clock, CueSpan, ListTrack, NavAction, NowPlayingEntry, NowPlayingEntryKind,
    NowPlayingList, NowPlayingListKind, OrderOutcome, PreviousPressTracker, ResumePolicy,
    SystemClock, CUE_BOUNDARY_LEAD_MS, PREVIOUS_DOUBLE_PRESS_WINDOW,
    PREVIOUS_RESTART_THRESHOLD_MS,
};
pub use playable::{PlayableItemId, PlayableItemIdentity};
pub use play_mode::{
    cycle_next, play_mode_text, OrderMode, PlayModeState, RepeatMode, SourceOrder,
    PLAY_MODE_CYCLE,
};
pub use qr::{QrError, QrErrorLevel, QrMatrix};
pub use transport::{SessionState, TransportError, TransportState, Volume};
pub use warp::{
    clamp_seek_target_ms, max_rate, rate_permille_valid, scan_speed_scale, scan_speed_step,
    scan_speed_valid, seek_voice, tempo_range, ScanDir, ScanState, SeekDriver, VoiceKind,
    WarpMode, WarpModeSettings, MAX_SEEK_STRETCH_CHASE, MAX_SEEK_STRETCH_SCAN,
    MAX_SEEK_VARI_CHASE, MAX_SEEK_VARI_SCAN, SCAN_SPEED_DEFAULT_PERMILLE,
    SCAN_SPEED_LADDER_PERMILLE, SCAN_SPEED_MAX_PERMILLE, SCAN_SPEED_MIN_PERMILLE,
    TEMPO_MAX_PERMILLE, TEMPO_MIN_PERMILLE, RATE_MAX_ABS_PERMILLE, RATE_MIN_ABS_PERMILLE,
    RATE_UNITY_PERMILLE, SEEK_END_GUARD_MS, SEEK_WARP_DEFAULT, SPEED_WARP_DEFAULT,
};
pub use waveform::{
    decode as decode_waveform, decode_header as decode_waveform_header,
    encode_to_vec as encode_waveform, Accumulator as WaveformAccumulator, Waveform,
    WaveformError, WaveformHeader,
};
