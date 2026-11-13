//! `plippa-server` binary — LAN media management portal.

use plippa_ipc_proto::{ensure_ipc_dir, resolve_paths};
use plippa_server::{
    connect_or_null, ipc_connect_timeout, serve_until, spawn_background_scanner,
    spawn_cover_worker, spawn_mount_watcher, spawn_resume_writer, spawn_store_sync_worker,
    spawn_waveform_worker, waveform::WAVE_LOOP_INTERVAL_MS,
    web::{WebConfig, WebHub},
    ServerConfig, ServerService,
    MOUNT_POLL_INTERVAL,
};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Pointer to the `Arc<AtomicBool>` shared with the threads (signals can only touch a
/// process-global). Set once at start-up; the `Arc` lives for the whole process.
static STOP_PTR: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

/// SIGINT / SIGTERM: stop accepting, let the workers return, then quiesce.
extern "C" fn on_stop_signal(_sig: libc::c_int) {
    let ptr = STOP_PTR.load(Ordering::SeqCst);
    if !ptr.is_null() {
        // SAFETY: `STOP_PTR` points at an `Arc<AtomicBool>` kept alive for the process
        // lifetime; storing an atomic is async-signal-safe.
        unsafe { (*ptr).store(true, Ordering::SeqCst) };
    }
}

fn install_signal_handlers(stop: &Arc<AtomicBool>) {
    STOP_PTR.store(Arc::as_ptr(stop) as *mut AtomicBool, Ordering::SeqCst);
    // SAFETY: plain libc calls with a valid handler function.
    unsafe {
        let handler = on_stop_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

fn main() {
    let cfg = ServerConfig::from_args(std::env::args());
    let paths = resolve_paths();
    // The private runtime directory also holds the control token the device UI reads.
    if let Err(e) = ensure_ipc_dir(&paths) {
        eprintln!("plippa-server: {e}");
    }
    let ipc = connect_or_null(&paths.engine_sock, ipc_connect_timeout());
    let associate = ServerService::default_mock_associate();
    // BCS_03: `--secrets-dir` / `PLIPPA_SECRETS_DIR`; a missing directory degrades the
    // store feature only, and the library portal keeps working.
    let mut service = ServerService::start_with_stores(
        cfg.library_root.clone(),
        ipc,
        associate,
        cfg.secrets_dir.clone(),
    );
    // MEDIA_01: the board decides which formats the portal may import and analyse.
    service.set_capabilities(plippa_server::load_capabilities(std::env::args()));
    // NB_08: `--no-web` / `PLIPPA_WEB=off` turns the whole online lookup off.
    service.set_web_hub(Arc::new(WebHub::new(WebConfig::from_args_env(
        std::env::args(),
    ))));
    let service = Arc::new(Mutex::new(service));
    // Logs the directory only (never a credentials file name).
    let stores = match service.lock() {
        Ok(service) => service.stores(),
        Err(poisoned) => poisoned.into_inner().stores(),
    };
    // BCS_04/05: one web fetcher, one index, one deleter for the store hub. `main` and the
    // end-to-end test call the same function, so they assemble the same system.
    ServerService::wire_store_hub(&service);
    stores.log_startup();

    // PWRSW_01: SIGINT / SIGTERM start an orderly stop instead of killing writers mid-loop.
    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handlers(&stop);

    // Lazy background rescan + duration probe (hot-plug / dropped files); low priority, own
    // index connection, never aborts the process.
    let scanner = spawn_background_scanner(
        Arc::clone(&service),
        Duration::from_secs(30),
        Arc::clone(&stop),
    );
    // Hot-plug: a 1 s discovery poll reopens index connections after a stick is unplugged,
    // remounted at the same path, swapped, or plugged in for the first time.
    let mounts = spawn_mount_watcher(
        Arc::clone(&service),
        MOUNT_POLL_INTERVAL,
        Arc::clone(&stop),
    );
    // Engine → server long-form resume points (audiobook/podcast/mix) → `.noudata`.
    let resume = spawn_resume_writer(Arc::clone(&service), Duration::from_secs(1), Arc::clone(&stop));
    // WAVE_01: waveform sidecars for every playable media file; always on, `nice +10`.
    let waveform = spawn_waveform_worker(
        Arc::clone(&service),
        Duration::from_millis(WAVE_LOOP_INTERVAL_MS),
        Arc::clone(&stop),
    );

    // ART_03: thumbnails for art dropped onto the stick; one image at a time, `nice +10`.
    let covers = spawn_cover_worker(Arc::clone(&service), Arc::clone(&stop));

    // BCS_03: automatic collection refresh (start-up when due, never mid-playback, at
    // most once per six hours). A failed spawn only loses the automatic trigger.
    let store_sync = match spawn_store_sync_worker(
        Arc::clone(&stores),
        Arc::clone(&service),
        Arc::clone(&stop),
    ) {
        Ok(handle) => Some(handle),
        Err(err) => {
            eprintln!("plippa-server: automatic collection sync is off: {err}");
            None
        }
    };

    let serve_result = serve_until(cfg, Arc::clone(&service), Some(Arc::clone(&stop)));

    // Every writer thread must be finished before the checkpoint / volume removal: the
    // scanner owns its own SQLite connection, the waveform worker may be writing a sidecar.
    for handle in [scanner, mounts, resume, waveform, covers] {
        let _ = handle.join();
    }
    if let Some(handle) = store_sync {
        let _ = handle.join();
    }
    match service.lock() {
        Ok(mut svc) => svc.quiesce(),
        Err(poisoned) => poisoned.into_inner().quiesce(),
    }

    if let Err(e) = serve_result {
        eprintln!("plippa-server error: {e}");
        std::process::exit(1);
    }
}
