//! Plippa audio: decode, quality selection, mix, sinks.
//!
//! # Playback path (MVP_13)
//!
//! Prefer [`stream::StreamDecoder`] — chunked pull at the **file native rate**.
//! Do not call [`prepare_for_playback`] on the Play/tap path (whole-file PCM +
//! resample thrash on 1 GB). PipeWire mixes / converts rates.
//!
//! # Threading
//!
//! Own sinks on a dedicated audio thread. [`Volume`] may be cloned and adjusted
//! from a control thread. See the crate README.

pub mod decode;
pub mod error;
pub mod formats;
pub mod hostile;
pub mod index;
pub mod isolate;
pub mod mixer;
pub mod position;
pub mod prepare;
pub mod route;
pub mod scan;
pub mod seek;
pub mod select;
pub mod ratestage;
pub mod sink;
pub mod sinc;
pub mod stream;
pub mod stretch;
pub mod volume;
pub mod window;
pub mod worker;
pub mod wsola;
pub mod xfade;

#[cfg(any(test, feature = "testsupport"))]
pub mod testsupport;

pub use decode::{decode, probe, AudioProbe, PcmBuffer};
pub use error::AudioError;
pub use mixer::Mixer;
pub use prepare::{prepare_for_playback, OverspecPolicy};
pub use select::{quality_score, select_best_local_path, PathCandidate};
pub use sink::{AudioSink, AudioSinkFactory, MockSink, PulseConfig};
#[cfg(feature = "pulse")]
pub use sink::PulseSink;
pub use stream::{
    validate_for_stream, StreamDecoder, DEFAULT_CHUNK_FRAMES, MAX_STREAM_BUFFER_BYTES,
    PREFETCH_TARGET_BYTES, REFILL_CHUNKS_PER_TURN, START_CHUNK_FRAMES, START_PREFETCH_BYTES,
};
pub use volume::Volume;
pub use ratestage::{
    Quantum, RampSpec, RateConfig, RateRamp, RateSource, RateStage, VoiceSelector,
};
pub use scan::{scan_rate_cap, ScanProfile};
pub use seek::{chase_step, final_quantum, ChaseConfig};
pub use sinc::SincTables;
pub use window::{SourceWindow, WindowConfig};
pub use xfade::{apply_output_stage, Crossfader, XfadeConfig};
pub use stretch::{
    make_stretcher, signalsmith_built, StretchConfig, StretchPreset, StretchVoice, Stretcher,
    STRETCH_BLOCK_FRAMES, STRETCH_MAX_RATE, STRETCH_MIN_RATE, STRETCH_PREFETCH_BYTES,
};
#[cfg(feature = "stretch")]
pub use stretch::SignalsmithStretcher;
pub use wsola::WsolaStretcher;
pub use worker::{
    BlockSource, FeedParams, Hint, ThroughputEstimator, WaypointWindow, WindowFeed, WorkerStats,
};
pub use index::{IndexSlot, Mp3Index};
