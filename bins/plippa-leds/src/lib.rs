//! `plippa-leds` — the LED strip helper (SYS-02).
//!
//! The WS2811 strips are driven by the SoC's PWM/DMA engine, which `rs_ws281x` reaches by
//! mapping `/dev/mem`; the udev rule lets the helper *open* `/dev/mem`, and the kernel gates
//! `read`/`write`/`mmap` on it with `CAP_SYS_RAWIO`.
//! Code execution in that process is therefore root-equivalent, so the capability lives here
//! and nowhere else: the UI composes colours and sends **fixed-size frames** over a unix
//! socket ([`protocol`]), and this process only ever applies a colour to an LED.
//!
//! What the helper does not do: no parsing of anything but the frame format, no board file
//! format beyond `plippa-board`'s typed profile (the same one the UI composes from), no
//! network, no files it did not create.
//!
//! Process rules (see the module docs of [`server`]): socket `0600` in the launcher's `0700`
//! IPC directory, `SO_PEERCRED` must be our own uid, one client at a time (a new client
//! replaces the old one), strips blank on start-up, on a clean exit and after [`server::DARK_AFTER`]
//! without a frame.

pub mod capability;
pub mod protocol;
pub mod server;
#[cfg(feature = "ws281x")]
pub mod ws281x;

pub use protocol::{Layout, Rgb, StreamSpec};
pub use server::{socket_path, FrameSink, LedsServer, NullSink, RecordingSink, SOCKET_NAME};
#[cfg(feature = "ws281x")]
pub use ws281x::Ws2811Sink;
