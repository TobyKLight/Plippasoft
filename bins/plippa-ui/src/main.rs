//! `plippa-ui` binary — continuous local UI process (MVP_11–16).
//!
//! ```text
//! plippa-ui [--backend=mock|poc2|auto] [board_id]
//! ```
//!
//! Default backend is `mock` (CI / desktop). `poc2` requires `--features poc2`.

use plippa_board::{workspace_root_from_crate, BoardProfile};
use plippa_ipc_proto::{request_poweroff, request_shutdown, resolve_paths, IpcConnection, IpcMessage};
use plippa_ui::hw::POC2_ENABLED;
use plippa_ui::{
    default_mock_wifi, run_ui_loop, spawn_display_worker, spawn_led_worker, DisplayBackend, LedCue,
    LibrarySource, LoopExit, LoopLimits, LoopState, MockDisplay, MockInput, MockLedCue, NfcBackend,
    SysProviders, SystemStatus, UiApp, UiConfig,
};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut want_poc2 = false;
    let mut board_id = "plippa-D-poc2".to_string();
    for arg in env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--backend=") {
            match rest {
                "mock" => want_poc2 = false,
                "poc2" => want_poc2 = true,
                "auto" => want_poc2 = POC2_ENABLED,
                other => eprintln!("plippa-ui: unknown backend `{other}`, using mock"),
            }
        } else if !arg.starts_with('-') {
            board_id = arg;
        }
    }

    #[cfg(not(feature = "poc2"))]
    {
        if want_poc2 {
            eprintln!("plippa-ui: --backend=poc2 needs --features poc2; using mock");
        }
        let _ = want_poc2;
    }

    let root = env::var("PLIPPA_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace_root_from_crate());

    let profile = match BoardProfile::load(&root, &board_id) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("plippa-ui: board `{board_id}` load error: {e}");
            None
        }
    };
    let (width, height, fan_fallback) = match &profile {
        Some(p) => {
            let (w, h) = p.display.logical_size();
            println!(
                "plippa-ui: loaded board `{}` ({}x{} logical, rot={}) roles menu={} volume={} back={}",
                p.capabilities.board_id,
                w,
                h,
                p.display.rotation,
                p.roles.menu_encoder,
                p.roles.volume_encoder,
                p.roles.back_button
            );
            println!("plippa-ui: board {}", p.optional_roles_summary());
            (w, h, p.fan.controller.clone())
        }
        None => {
            eprintln!("plippa-ui: board load failed; using 320x240 landscape defaults");
            (320, 240, "unknown".into())
        }
    };

    // ACC_03: the real radio worker only exists when the device build asked for it and the
    // poc2 backend is active. A mock/desktop run (or a build without the feature) has no
    // handle, so the Accessories screen says `No radio` rather than inventing data.
    #[cfg(feature = "accessory-2g4")]
    let accessories = if want_poc2 {
        plippa_ui::AccessoryHandle::spawn(plippa_hw::TwoG4Backend::new())
    } else {
        None
    };
    #[cfg(not(feature = "accessory-2g4"))]
    let accessories = None;

    let capabilities = match &profile {
        Some(p) => p.capabilities.clone(),
        None => plippa_core::Capabilities::desktop_mock(),
    };
    let mut app = UiApp::new(UiConfig {
        width,
        height,
        capabilities,
        // PWRSW_01: only the launcher arms host power-off; a dev run of the UI alone
        // must never halt the machine it is running on.
        poweroff_armed: env_flag("PLIPPA_POWEROFF"),
        accessories,
        // BCS_07: the tap screen asks the local store service over loopback. The launcher may
        // move it; the server's conventional port is the default.
        // SECFIX_02: that service is the server's loopback control listener (`127.0.0.1:8786`),
        // which answers only this UI: every call carries the per-boot token the server wrote to
        // the private runtime directory. The LAN portal on 8787 is not used by the device.
        store_base: Some(
            std::env::var("PLIPPA_STORE_BASE")
                .ok()
                .map(|base| base.trim().to_string())
                .filter(|base| !base.is_empty())
                .unwrap_or_else(|| "127.0.0.1:8786".to_string()),
        ),
        control_token_path: Some(resolve_paths().dir.join("portal.token")),
        ..UiConfig::default()
    })
    .with_system(SystemStatus {
        time_hhmm: "00:00".into(),
        temperature_c: None,
        fan_status: fan_fallback.clone(),
        portal_url: None,
        ip: None,
        ips: Vec::new(),
    });

    let mut library = Some(LibrarySource::from_env());
    let paths = resolve_paths();
    // Match server default: engine may still be opening Pulse before the sock exists.
    let ipc_wait = env::var("PLIPPA_IPC_CONNECT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(20);
    let mut conn =
        match IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(ipc_wait))
        {
            Ok(c) => {
                println!(
                    "plippa-ui: IPC connected to {}",
                    paths.engine_sock.display()
                );
                Some(c)
            }
            Err(e) => {
                eprintln!("plippa-ui: IPC connect failed: {e}");
                None
            }
        };

    if let Ok(key) = env::var("PLIPPA_UI_SMOKE_TAP_KEY") {
        if !key.is_empty() {
            if let Some(c) = conn.as_mut() {
                let _ = c.send(&IpcMessage::TapKey(plippa_ipc_proto::TapKeyPayload {
                    noubin_key: key,
                    tap_id: 1,
                    already_normalised: false,
                }));
                env::remove_var("PLIPPA_UI_SMOKE_TAP_KEY");
            }
        }
    }

    let deadline = env::var("PLIPPA_IPC_SMOKE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| Instant::now() + Duration::from_secs(secs));
    let stop_flag = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&stop_flag);
        if let Err(e) = ctrlc::set_handler(move || {
            eprintln!("plippa-ui: SIGINT — stopping so LEDs/display clear on exit");
            flag.store(true, Ordering::SeqCst);
        }) {
            eprintln!("plippa-ui: ctrlc handler not installed: {e}");
        }
    }
    // Launcher prefers SIGTERM; without this, the process dies before Drop clears WS2811.
    #[cfg(unix)]
    install_sigterm_flag(Arc::clone(&stop_flag));
    let limits = LoopLimits {
        max_iters: if env::var_os("PLIPPA_ONCE").is_some() {
            Some(1)
        } else {
            None
        },
        deadline,
        sleep: Duration::from_millis(10),
        stop_flag: Some(Arc::clone(&stop_flag)),
    };
    let mut state = LoopState::default();
    // One epoch for the loop clock and the I²C service's edge timestamps (WARP_06).
    state.epoch = Some(Instant::now());

    #[cfg(feature = "poc2")]
    if want_poc2 {
        if let Some(ref p) = profile {
            match try_run_poc2(
                &mut app,
                p,
                fan_fallback.clone(),
                &mut library,
                &mut conn,
                &mut state,
                limits.clone(),
                Arc::clone(&stop_flag),
            ) {
                Ok(exit) => {
                    finish(exit, &paths);
                    return;
                }
                Err(e) => eprintln!("plippa-ui: poc2 open failed ({e}); falling back to mock"),
            }
        }
    }

    // Mock / desktop path (default for CI) — no I²C / display / LED worker threads.
    println!("plippa-ui: backend=mock");
    let mut display = MockDisplay::new(width, height);
    let mut input = MockInput::new();
    let mut wifi = default_mock_wifi();
    app.init_wifi(&mut wifi);
    let mut nfc = Some(NfcBackend::mock_empty());
    let mut sys = SysProviders::mock("00:00", None, fan_fallback);
    let mut led = MockLedCue::default();
    let exit = run_ui_loop(
        &mut app,
        &mut display,
        &mut wifi,
        &mut input,
        &mut nfc,
        &mut sys,
        &mut library,
        &mut conn,
        &mut led,
        &mut state,
        limits,
    );
    finish(exit, &paths);
}

fn finish(exit: LoopExit, paths: &plippa_ipc_proto::IpcPaths) {
    println!("plippa-ui: loop exit {exit:?}");
    // Only hold-to-power asks the launcher to stop siblings. SIGINT must not
    // write a sentinel after the shell has already returned.
    match exit {
        LoopExit::Shutdown | LoopExit::PowerOff => {
            let power_off = exit == LoopExit::PowerOff;
            let result = if power_off {
                request_poweroff(paths)
            } else {
                request_shutdown(paths)
            };
            let path = if power_off {
                paths.poweroff_path()
            } else {
                paths.shutdown_path()
            };
            if let Err(e) = result {
                eprintln!(
                    "plippa-ui: failed to write {} sentinel: {e}",
                    path.display()
                );
            } else {
                eprintln!("plippa-ui: wrote sentinel {}", path.display());
            }
        }
        LoopExit::MaxIters | LoopExit::Deadline | LoopExit::Interrupted | LoopExit::Once => {}
    }
}

#[cfg(feature = "poc2")]
fn try_run_poc2(
    app: &mut UiApp,
    profile: &BoardProfile,
    fan_fallback: String,
    library: &mut Option<LibrarySource>,
    conn: &mut Option<IpcConnection>,
    state: &mut LoopState,
    limits: LoopLimits,
    stop_flag: Arc<AtomicBool>,
) -> Result<LoopExit, String> {
    // Screen-fault bisect switches: each disables exactly one subsystem that this
    // process runs alongside the SPI present and that the lab harness cannot
    // reproduce. Unset = normal behaviour (no behavioural change in production).
    let no_i2c = env_flag("PLIPPA_UI_NO_I2C");
    let no_leds = env_flag("PLIPPA_UI_NO_LEDS");
    let no_sys = env_flag("PLIPPA_UI_NO_SYS");

    let display_hw =
        plippa_ui::hw::Ili9341Display::open(&profile.display).map_err(|e| e.to_string())?;
    let (dw, dh) = display_hw.size();
    let display_worker = spawn_display_worker(display_hw);
    let mut display = display_worker.proxy(dw, dh);

    let mut i2c = if no_i2c {
        eprintln!("plippa-ui: PLIPPA_UI_NO_I2C=1 — I²C service disabled, inputs idle");
        None
    } else {
        Some(
            plippa_ui::hw::spawn_poc2_i2c_service(
                profile,
                Arc::clone(&stop_flag),
                state.epoch.unwrap_or_else(Instant::now),
            )
            .map_err(|e| e.to_string())?,
        )
    };
    let mut nfc: Option<NfcBackend>;
    let mut input: Box<dyn plippa_ui::InputSource> = match i2c.as_mut() {
        Some(svc) => {
            let (chan, nfc_client) = svc.take_endpoints();
            state.nfc_poll_interval_ms = profile.nfc.poll_interval_ms();
            nfc = Some(NfcBackend::from_channel(nfc_client));
            Box::new(chan)
        }
        None => {
            nfc = Some(NfcBackend::mock_empty());
            Box::new(MockInput::new())
        }
    };
    let mut wifi = plippa_ui::NmcliWifi::new();
    app.init_wifi(&mut wifi);
    let mut sys = if no_sys {
        eprintln!("plippa-ui: PLIPPA_UI_NO_SYS=1 — sys providers mocked");
        SysProviders::mock("00:00", None, fan_fallback)
    } else {
        SysProviders::system(fan_fallback)
    };

    let (mut led, led_worker): (Box<dyn LedCue>, Option<_>) = if no_leds {
        eprintln!("plippa-ui: PLIPPA_UI_NO_LEDS=1 — LED worker disabled");
        (Box::new(MockLedCue::default()), None)
    } else {
        let led_hw = plippa_ui::hw::Poc2LedCue::open(profile);
        let worker = spawn_led_worker(led_hw);
        (Box::new(worker.proxy()), Some(worker))
    };

    println!("plippa-ui: backend=poc2 (I²C + display + LED workers)");
    let exit = run_ui_loop(
        app,
        &mut display,
        &mut wifi,
        &mut *input,
        &mut nfc,
        &mut sys,
        library,
        conn,
        &mut *led,
        state,
        limits,
    );
    // Join order: display + LED (clear) first, then I²C — see ui-threading.md.
    display.blank();
    led.clear_all();
    display_worker.stop_and_join(Duration::from_millis(500));
    if let Some(w) = led_worker {
        w.stop_and_join(Duration::from_millis(500));
    }
    if let Some(svc) = i2c {
        svc.stop_and_join(Duration::from_millis(500));
    }
    Ok(exit)
}

/// True for `NAME=1` / `NAME=true` / `NAME=yes` (unset, empty and `0` are false).
fn env_flag(name: &str) -> bool {
    plippa_ui::env_flag(name)
}

#[cfg(unix)]
fn install_sigterm_flag(flag: Arc<AtomicBool>) {
    use std::sync::atomic::AtomicPtr;
    static FLAG_PTR: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());
    // Leak one Arc clone for process lifetime (handler needs a stable address).
    let raw = Arc::into_raw(flag) as *mut AtomicBool;
    FLAG_PTR.store(raw, Ordering::SeqCst);
    unsafe extern "C" fn on_sigterm(_: libc::c_int) {
        let p = FLAG_PTR.load(Ordering::SeqCst);
        if !p.is_null() {
            unsafe {
                (*p).store(true, Ordering::SeqCst);
            }
        }
    }
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as usize);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn env_flag_is_shared_with_the_library() {
        // The bisect switches must behave identically in the binary and in `hw`.
        assert_eq!(super::env_flag("PLIPPA_TEST_SHARED_UNSET"), plippa_ui::env_flag("PLIPPA_TEST_SHARED_UNSET"));
    }
}
