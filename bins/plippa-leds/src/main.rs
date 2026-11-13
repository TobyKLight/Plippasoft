//! `plippa-leds` binary — the LED strip helper (SYS-02).
//!
//! ```text
//! plippa-leds [--board=<id>]
//! ```
//!
//! Loads the same `plippa-board` profile the UI composes from, checks that it holds
//! `CAP_SYS_RAWIO`, then serves fixed-size colour frames on `$PLIPPA_IPC_DIR/leds.sock`
//! until SIGTERM/SIGINT, blanking the strips on the way in and out.

use plippa_board::{workspace_root_from_crate, BoardProfile};
use plippa_ipc_proto::resolve_paths;
use plippa_leds::capability;
use plippa_leds::protocol::Layout;
use plippa_leds::server::{LedsServer, NullSink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// `EX_CONFIG`: a profile problem, not a crash.
const EXIT_CONFIG: i32 = capability::EXIT_CONFIG;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let (root, board_id) = args();
    let profile = match BoardProfile::load(&root, &board_id) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("plippa-leds: board `{board_id}` load error: {error}");
            return EXIT_CONFIG;
        }
    };
    let layout = Layout::from_profile(&profile);
    if let Err(message) = capability::gate(cfg!(feature = "ws281x"), capability::read_cap_sys_rawio())
    {
        eprintln!("plippa-leds: {message}");
        return capability::EXIT_NO_CAP;
    }
    if layout.streams().is_empty() {
        eprintln!("plippa-leds: board `{board_id}` has no [led_streams]; nothing to drive");
        return EXIT_CONFIG;
    }
    // The board file is writable by the same user that runs the helper, and this process holds
    // CAP_SYS_RAWIO: refuse a layout the protocol would not accept before a frame reaches a strip.
    if let Err(error) = layout.validate() {
        eprintln!("plippa-leds: board `{board_id}` has an unusable [led_streams]: {error}");
        return EXIT_CONFIG;
    }

    let paths = resolve_paths();
    let server = match LedsServer::bind(&paths, layout.clone()) {
        Ok(server) => server,
        Err(error) => {
            eprintln!(
                "plippa-leds: cannot listen on {}: {error}",
                plippa_leds::socket_path(&paths.dir).display()
            );
            return EXIT_CONFIG;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handlers(Arc::clone(&stop));

    match sink(&profile, &layout) {
        Ok(sink) => match server.serve(sink, &stop) {
            Ok(_) => 0,
            Err(error) => {
                eprintln!("plippa-leds: server error: {error}");
                1
            }
        },
        Err(error) => {
            // The strips cannot be opened, but serving still keeps the UI from reconnecting in
            // a loop; the helper is dark, which is the same as an absent helper to the owner.
            eprintln!("plippa-leds: WS2811 open failed ({error}); serving with no output");
            match server.serve(NullSink, &stop) {
                Ok(_) => 0,
                Err(error) => {
                    eprintln!("plippa-leds: server error: {error}");
                    1
                }
            }
        }
    }
}

fn args() -> (PathBuf, String) {
    let mut board_id = std::env::var("PLIPPA_BOARD")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "plippa-D-poc2".to_string());
    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--board=") {
            if !rest.is_empty() {
                board_id = rest.to_string();
            }
        }
    }
    let root = std::env::var("PLIPPA_ROOT")
        .ok()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(workspace_root_from_crate);
    (root, board_id)
}

#[cfg(feature = "ws281x")]
fn sink(profile: &BoardProfile, layout: &Layout) -> Result<plippa_leds::Ws2811Sink, String> {
    plippa_leds::Ws2811Sink::open(profile, layout)
}

#[cfg(not(feature = "ws281x"))]
fn sink(_profile: &BoardProfile, _layout: &Layout) -> Result<NullSink, String> {
    eprintln!("plippa-leds: built without the `ws281x` feature: no strip output");
    Ok(NullSink)
}

fn install_signal_handlers(flag: Arc<AtomicBool>) {
    use std::sync::atomic::AtomicPtr;
    static FLAG_PTR: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());
    // Leak one Arc for the process lifetime: the handler needs a stable address.
    let raw = Arc::into_raw(flag) as *mut AtomicBool;
    FLAG_PTR.store(raw, Ordering::SeqCst);
    unsafe extern "C" fn on_stop(_: libc::c_int) {
        let pointer = FLAG_PTR.load(Ordering::SeqCst);
        if !pointer.is_null() {
            // SAFETY: `pointer` is the leaked `Arc<AtomicBool>` above, alive for the process.
            unsafe {
                (*pointer).store(true, Ordering::SeqCst);
            }
        }
    }
    // SAFETY: both handlers are async-signal-safe (an atomic store on a live allocation).
    unsafe {
        libc::signal(libc::SIGTERM, on_stop as *const () as usize);
        libc::signal(libc::SIGINT, on_stop as *const () as usize);
    }
}
