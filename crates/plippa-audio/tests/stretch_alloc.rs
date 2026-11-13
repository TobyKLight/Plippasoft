//! WARP_09: rendering through the stretch voice does not allocate in steady state (Rust side:
//! the C++ stretcher allocates with `operator new`, which this counter does not see), and
//! stays finite and in range over a long render with a ramping rate.
#![cfg(feature = "testsupport")]

use plippa_audio::testsupport::{scan_numeric, sine};
use plippa_audio::{Quantum, SourceWindow, StretchConfig, StretchVoice, WindowConfig};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: Counting = Counting;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn long_render(force_wsola: bool) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    const HZ: u32 = 44_100;
    let frames = HZ as usize * 60;
    let src = sine(440.0, HZ, frames, 0.5);
    let cfg = WindowConfig {
        window_cap_bytes: (frames + 64) * 4,
        history_s: 1.0,
        lookahead_s: 1.0,
        min_history_s: 0.0,
        ..WindowConfig::default()
    };
    let mut w = SourceWindow::new(1, HZ, &cfg);
    w.reset(0);
    assert!(w.push(&src, 0));
    let mut v = StretchVoice::new(
        &StretchConfig {
            force_wsola,
            ..StretchConfig::default()
        },
        1,
        HZ,
    );
    v.request_prime(0.0);
    let mut blk = vec![0.0f32; 256];
    let mut all = Vec::with_capacity(frames);
    let mut warm = 0usize;
    let mut before = 0usize;
    let mut i = 0usize;
    loop {
        // Rate wobbles between 0.5 and 1.8 (a ramp per block, never constant).
        let rate = 1.15 + 0.65 * (i as f64 * 0.01).sin();
        match v.render(&w, true, rate, &mut blk) {
            Quantum::Rendered { frames: f, .. } => all.extend_from_slice(&blk[..f]),
            Quantum::End => break,
            Quantum::NeedData => panic!("NeedData"),
        }
        i += 1;
        warm += 1;
        if warm == 200 {
            before = ALLOCS.load(Ordering::SeqCst);
            all.reserve(frames * 2);
            before = before.max(ALLOCS.load(Ordering::SeqCst));
        }
    }
    let grown = ALLOCS.load(Ordering::SeqCst) - before;
    assert_eq!(grown, 0, "allocations during steady-state rendering");
    let scan = scan_numeric(&all);
    assert!(scan.is_clean(), "{scan:?}");
    assert!(all.iter().all(|s| s.abs() <= 1.5), "out of range sample");
}

#[test]
fn wsola_voice_does_not_allocate_in_steady_state() {
    long_render(true);
}

#[cfg(feature = "stretch")]
#[test]
fn signalsmith_voice_does_not_allocate_in_steady_state_on_the_rust_side() {
    long_render(false);
}
