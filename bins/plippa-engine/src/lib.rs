//! `plippa-engine` — control plane and media playback.
//!
//! Audio decode/mix/output runs on a dedicated thread ([`audio_thread`]); IPC,
//! SQLite lookups, and session state stay on the control plane so blocking I/O
//! cannot glitch the buffer (architecture priority 0).

mod audio_thread;
mod collection;
mod config;
mod engine;
mod ipc_hub;
mod resume;
mod route;
mod route_factory;

pub use audio_thread::{
    AudioCommand, AudioThreadConfig, AudioThreadHandle, AudioThreadStats, CrossEvent, NeighbourSrc,
    DecoderHook, PlayArm, RateChange,
};
pub use config::{EngineConfig, SinkMode};
pub use engine::{
    default_sfx_candidates, resolve_sfx_path, Engine, EngineError, EngineEvent, MockStreamOpen,
    MAX_PUBLISHED_ENTRIES, RESUME_MIN_DURATION_MS,
};
pub use ipc_hub::{
    run_hub, run_hub_with_sfx, HubShutdown, StatusPacer, STATUS_INTERVAL_FAST, STATUS_INTERVAL_SLOW,
};
pub use resume::{ResumeEntry, ResumeStore, MAX_RESUME_ENTRIES, RESUME_RESTART_TAIL_MS};
pub use route::RouteManager;
pub use route_factory::{RouteTarget, RouteTargetCell};
#[cfg(feature = "pulse")]
pub use route_factory::RoutedSinkFactory;
