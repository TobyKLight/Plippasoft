//! `plippa-engine` — control plane and media playback (long-running).

use plippa_board::{workspace_root_from_crate, BoardProfile};
use plippa_core::Capabilities;
use plippa_engine::{
    resolve_sfx_path, run_hub_with_sfx, Engine, EngineConfig, HubShutdown, RouteManager,
    RouteTarget, SinkMode,
};
#[cfg(feature = "pulse")]
use plippa_engine::RouteTargetCell;
use plippa_ipc_proto::{ensure_ipc_dir, resolve_paths};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut cfg = EngineConfig::from_args(env::args());
    if cfg.repo_root == PathBuf::from(".") {
        if let Ok(v) = env::var("PLIPPA_ROOT") {
            if !v.is_empty() {
                cfg.repo_root = PathBuf::from(v);
            }
        } else {
            cfg.repo_root = workspace_root_from_crate();
        }
    }

    let board = load_board_settings(&cfg);

    let paths = resolve_paths();
    if let Err(e) = ensure_ipc_dir(&paths) {
        eprintln!(
            "plippa-engine: cannot create IPC dir {}: {e}",
            paths.dir.display()
        );
        std::process::exit(1);
    }

    let mut engine = match open_engine(
        &cfg,
        board.capabilities,
        board.local_sink,
        board.local_sink_match,
    ) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("plippa-engine failed to start: {e}");
            std::process::exit(1);
        }
    };

    let sfx_path = resolve_sfx_path(cfg.sfx_path.as_deref(), &cfg.repo_root);
    if sfx_path.is_none() {
        eprintln!(
            "plippa-engine: no tap SFX found (looked under {}/assets/plippaSFX)",
            cfg.repo_root.display()
        );
    }

    for n in engine.notices() {
        eprintln!("plippa-engine: notice [{}] {}", n.code, n.message);
    }

    println!(
        "plippa-engine ready (board={}, library={}, roots={}, sink={}, index={}, audio thread spawned)",
        engine.capabilities().board_id,
        engine.library_root().display(),
        engine.library_roots().len(),
        cfg.sink.as_str(),
        if engine.has_library_index() {
            "ok"
        } else {
            "missing"
        }
    );

    let shutdown: HubShutdown = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown);
        let _ = ctrlc::set_handler(move || {
            flag.store(true, Ordering::SeqCst);
        });
    }

    let deadline = env::var("PLIPPA_IPC_SMOKE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| Instant::now() + Duration::from_secs(secs));

    // Bind IPC before SFX decode so UI/server don't time out into NullIpc.
    if let Err(e) = run_hub_with_sfx(
        &mut engine,
        &paths,
        deadline,
        Some(Arc::clone(&shutdown)),
        sfx_path.as_deref(),
    ) {
        eprintln!("plippa-engine IPC hub error: {e}");
        engine.shutdown_audio();
        std::process::exit(1);
    }

    engine.shutdown_audio();
    eprintln!("plippa-engine: exited cleanly");
}

/// Capabilities plus the optional local-output hints from the same board profile.
struct BoardSettings {
    capabilities: Capabilities,
    local_sink: Option<String>,
    local_sink_match: Option<String>,
}

fn load_board_settings(cfg: &EngineConfig) -> BoardSettings {
    let desktop = || BoardSettings {
        capabilities: Capabilities::desktop_mock(),
        local_sink: None,
        local_sink_match: None,
    };
    let Some(board_id) = cfg.board_id.as_deref() else {
        return desktop();
    };
    match BoardProfile::load(&cfg.repo_root, board_id) {
        Ok(profile) => {
            eprintln!(
                "plippa-engine: loaded board `{}`",
                profile.capabilities.board_id
            );
            BoardSettings {
                capabilities: profile.capabilities,
                local_sink: profile.audio.local_sink,
                local_sink_match: profile.audio.local_sink_match,
            }
        }
        Err(e) => {
            eprintln!(
                "plippa-engine: board `{board_id}` load failed ({e}); using desktop_mock caps"
            );
            desktop()
        }
    }
}

fn open_engine(
    cfg: &EngineConfig,
    caps: Capabilities,
    local_sink: Option<String>,
    local_sink_match: Option<String>,
) -> Result<Engine, String> {
    match cfg.sink {
        SinkMode::Mock => {
            open_mock_engine(cfg.library_root.clone(), caps, local_sink, local_sink_match)
        }
        SinkMode::Pulse => open_pulse_engine(
            cfg.library_root.clone(),
            caps,
            local_sink,
            local_sink_match,
        ),
    }
}

/// Mock sinks never open a device; the route manager still runs so the UI sees the
/// same wire and the engine follows the same target/restream rules (AUDIO_03 §5.2).
fn open_mock_engine(
    library_root: Option<std::path::PathBuf>,
    caps: Capabilities,
    local_sink: Option<String>,
    local_sink_match: Option<String>,
) -> Result<Engine, String> {
    let sinks = local_sink
        .as_deref()
        .map(|id| {
            vec![plippa_audio::route::SinkInfo {
                id: id.to_string(),
                description: id.to_string(),
                available: true,
                latency_ms: 0,
                props: Default::default(),
                is_default: true,
            }]
        })
        .unwrap_or_default();
    let control = plippa_audio::route::MockOutputControl::new(sinks);
    let target = RouteTarget::default_cell();
    let mut engine = Engine::open_mock_discovered(library_root, caps)
        .map_err(|e| e.to_string())?
        .with_local_sink(local_sink, local_sink_match);
    engine.attach_route(RouteManager::new(Box::new(control), None, None), target);
    Ok(engine)
}

#[cfg(feature = "pulse")]
fn open_pulse_engine(
    library_root: Option<std::path::PathBuf>,
    caps: Capabilities,
    local_sink: Option<String>,
    local_sink_match: Option<String>,
) -> Result<Engine, String> {
    ensure_pulse_server_env();
    // MVP_14: do **not** open Pulse before `engine.sock` listens. The factory
    // connects lazily on the audio thread at each stream’s native rate and reads
    // the route target (device + latency profile) the manager last pushed.
    eprintln!("plippa-engine: Pulse factory ready (lazy connect on first stream)");
    let target: RouteTargetCell = RouteTarget::default_cell();
    let factory = plippa_engine::RoutedSinkFactory::new(std::sync::Arc::clone(&target));
    let mut engine = Engine::open_with_sink_factory_discovered(library_root, caps, factory)
        .map_err(|e| e.to_string())?
        .with_local_sink(local_sink, local_sink_match);
    // A missing audio server does not abort: the manager reports a problem and the
    // target falls back to the server default (AUDIO_03 §5.3).
    let control = plippa_audio::route::PipeWireControl::new();
    engine.attach_route(RouteManager::new(Box::new(control), None, None), target);
    Ok(engine)
}

#[cfg(feature = "pulse")]
fn session_pulse_server() -> Option<String> {
    if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
        let path = std::path::PathBuf::from(xdg).join("pulse/native");
        if path.exists() {
            return Some(format!("unix:{}", path.display()));
        }
    }
    // Last-resort scan (lab devices where XDG isn't set on the engine process).
    let Ok(entries) = std::fs::read_dir("/run/user") else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("pulse/native");
        if path.exists() {
            return Some(format!("unix:{}", path.display()));
        }
    }
    None
}

#[cfg(not(feature = "pulse"))]
fn open_pulse_engine(
    library_root: Option<std::path::PathBuf>,
    caps: Capabilities,
    local_sink: Option<String>,
    local_sink_match: Option<String>,
) -> Result<Engine, String> {
    eprintln!(
        "plippa-engine: --sink=pulse requested but binary built without `pulse` feature; using mock"
    );
    open_mock_engine(library_root, caps, local_sink, local_sink_match)
}

#[cfg(feature = "pulse")]
fn ensure_pulse_server_env() {
    if env::var_os("PULSE_SERVER").is_some() {
        return;
    }
    // Use the live user-session PipeWire socket when present. Otherwise leave
    // PULSE_SERVER unset so libpulse falls back to its own default.
    if let Some(session) = session_pulse_server() {
        env::set_var("PULSE_SERVER", &session);
        eprintln!("plippa-engine: PULSE_SERVER={session}");
    }
}
