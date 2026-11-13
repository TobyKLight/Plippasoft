//! UDS hub: accept UI/server clients, dispatch into the engine, broadcast events.

use crate::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcConnection, IpcListener, IpcMessage, IpcPaths, IpcTransportError};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Shared flag set by SIGINT/SIGTERM handlers to leave [`run_hub`].
pub type HubShutdown = Arc<AtomicBool>;

/// Periodic hub status refresh interval (ms) in the normal state. Documented UI_02.
pub const STATUS_INTERVAL_SLOW: Duration = Duration::from_millis(500);
/// WARP_01: refresh interval while the set speed is not 1.0x, a scan runs or a committed
/// seek is in progress (live position/target for the UI).
pub const STATUS_INTERVAL_FAST: Duration = Duration::from_millis(100);

/// Decides when the periodic status refresh is due. Pure (the caller supplies `now`), so the
/// cadence rule is unit-testable with an injected clock.
#[derive(Debug, Clone, Copy)]
pub struct StatusPacer {
    last_push: Instant,
}

impl StatusPacer {
    pub fn new(now: Instant) -> Self {
        Self { last_push: now }
    }

    /// Interval for the current engine state.
    pub fn interval(fast: bool) -> Duration {
        if fast {
            STATUS_INTERVAL_FAST
        } else {
            STATUS_INTERVAL_SLOW
        }
    }

    /// Whether a periodic refresh is due at `now`.
    pub fn due(&self, now: Instant, fast: bool) -> bool {
        now.saturating_duration_since(self.last_push) >= Self::interval(fast)
    }

    /// Record that a status went out at `now` (periodic or event-driven).
    pub fn mark(&mut self, now: Instant) {
        self.last_push = now;
    }
}

/// Max inbound frames drained per client per tick (matches UI drain discipline).
const INBOUND_DRAIN_CAP: usize = 32;

fn is_engine_inbound(msg: &IpcMessage) -> bool {
    matches!(
        msg,
        IpcMessage::TapNdef(_)
            | IpcMessage::TapKey(_)
            | IpcMessage::PlaybackCommand(_)
            | IpcMessage::LibraryIndexUpdated(_)
    )
}

fn is_relay(msg: &IpcMessage) -> bool {
    matches!(
        msg,
        IpcMessage::AssociateRequest(_) | IpcMessage::AssociateResponse(_)
    )
}

fn send_all(
    clients: &mut [IpcConnection],
    msg: &IpcMessage,
    skip: Option<usize>,
    dead: &mut BTreeSet<usize>,
) {
    for (i, c) in clients.iter_mut().enumerate() {
        if skip == Some(i) || dead.contains(&i) {
            continue;
        }
        if let Err(e) = c.send(msg) {
            eprintln!(
                "plippa-engine: drop IPC client {i} on send ({e}) — \
                 peer not reading (would wedge hub)"
            );
            dead.insert(i);
        }
    }
}

fn drain_engine_events(
    engine: &mut Engine,
    clients: &mut [IpcConnection],
    dead: &mut BTreeSet<usize>,
) {
    for ev in engine.take_events() {
        let msg = match ev {
            EngineEvent::Status(s) => IpcMessage::PlaybackStatus(s),
            EngineEvent::Notice(n) => IpcMessage::UserNotice(n),
            EngineEvent::NowPlaying(l) => IpcMessage::NowPlayingListChanged(l),
            EngineEvent::ResumeWriteback(w) => IpcMessage::ResumeWriteback(w),
            EngineEvent::UnmatchedNoubin(u) => IpcMessage::UnmatchedNoubin(u),
            EngineEvent::AudioRoute(r) => IpcMessage::AudioRouteChanged(r),
        };
        send_all(clients, &msg, None, dead);
    }
}

fn sweep_dead(clients: &mut Vec<IpcConnection>, dead: &BTreeSet<usize>) {
    for &i in dead.iter().rev() {
        if i < clients.len() {
            clients.swap_remove(i);
        }
    }
}

/// Run the listen / accept / dispatch loop until deadline, shutdown flag, or forever.
///
/// `sfx_path`: if set, decode after the socket is bound so UI/server can
/// connect while SFX loads (avoids NullIpc when decode is slow on a Pi).
pub fn run_hub(
    engine: &mut Engine,
    paths: &IpcPaths,
    deadline: Option<Instant>,
    shutdown: Option<HubShutdown>,
) -> Result<(), IpcTransportError> {
    run_hub_with_sfx(engine, paths, deadline, shutdown, None)
}

/// Like [`run_hub`], optionally loading tap SFX after bind.
///
/// SFX decode runs **after** the first accept drain so UI/server can connect
/// while a short cue file loads (MVP_14).
pub fn run_hub_with_sfx(
    engine: &mut Engine,
    paths: &IpcPaths,
    deadline: Option<Instant>,
    shutdown: Option<HubShutdown>,
    sfx_path: Option<&std::path::Path>,
) -> Result<(), IpcTransportError> {
    let listener = IpcListener::bind(paths)?;
    eprintln!(
        "plippa-engine: IPC listening on {}",
        listener.paths.engine_sock.display()
    );

    let mut sfx_pending = sfx_path.map(|p| p.to_path_buf());
    let mut clients: Vec<IpcConnection> = Vec::new();
    let mut pacer = StatusPacer::new(Instant::now());
    let mut last_published_revision = engine.status_revision();

    // Flush any open-time notices (missing index, etc.) once clients exist —
    // also keep them queued for the first drain after accept.
    loop {
        if shutdown.as_ref().is_some_and(|s| s.load(Ordering::SeqCst)) {
            eprintln!("plippa-engine: shutdown requested");
            return Ok(());
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Ok(());
            }
        }

        while let Some(c) = listener.try_accept()? {
            eprintln!(
                "plippa-engine: IPC client connected (n={})",
                clients.len() + 1
            );
            clients.push(c);
            // Push the route first: the Audio screen needs it before any status.
            engine.publish_audio_route();
            // Push current status + any pending notices to the new peer set.
            engine.publish_status();
            engine.publish_now_playing();
            last_published_revision = engine.status_revision();
            let mut dead = BTreeSet::new();
            drain_engine_events(engine, &mut clients, &mut dead);
            sweep_dead(&mut clients, &dead);
        }

        // Deferred SFX: prefer accepting backlog before a short decode.
        if let Some(sfx) = sfx_pending.take() {
            match engine.load_sfx_from_path(&sfx) {
                Ok(()) => eprintln!("plippa-engine: load SFX from {}", sfx.display()),
                Err(e) => eprintln!("plippa-engine: SFX load failed ({}): {e}", sfx.display()),
            }
        }

        let mut dead: BTreeSet<usize> = BTreeSet::new();
        let mut inbound: Vec<(usize, IpcMessage)> = Vec::new();
        let mut priority: Vec<(usize, IpcMessage)> = Vec::new();
        let mut latest_index: Option<(usize, plippa_ipc_proto::LibraryIndexUpdated)> = None;

        // Drain up to INBOUND_DRAIN_CAP frames per client so Pause/Play bursts
        // are not interleaved one-per-tick with the 500 ms status broadcast.
        for (i, c) in clients.iter_mut().enumerate() {
            for _ in 0..INBOUND_DRAIN_CAP {
                match c.try_recv() {
                    Ok(Some(msg)) => inbound.push((i, msg)),
                    Ok(None) => break,
                    Err(IpcTransportError::Disconnected) => {
                        dead.insert(i);
                        break;
                    }
                    Err(IpcTransportError::Frame(e)) => {
                        eprintln!(
                            "plippa-engine: bad IPC frame from client: {e} (frame drained; connection kept)"
                        );
                    }
                    Err(e) => {
                        eprintln!("plippa-engine: IPC client error: {e}");
                        dead.insert(i);
                        break;
                    }
                }
            }
        }

        for (from, msg) in inbound {
            if dead.contains(&from) {
                continue;
            }
            if is_relay(&msg) {
                send_all(&mut clients, &msg, Some(from), &mut dead);
                continue;
            }
            match msg {
                IpcMessage::LibraryIndexUpdated(u) => {
                    // Coalesce: keep only the newest generation this tick.
                    match &mut latest_index {
                        Some((_, prev)) if u.generation >= prev.generation => {
                            *prev = u;
                        }
                        None => latest_index = Some((from, u)),
                        Some(_) => {}
                    }
                }
                other if is_engine_inbound(&other) => {
                    priority.push((from, other));
                }
                _ => {}
            }
        }

        // Taps / transport first so USB index reopen cannot starve playback.
        for (from, msg) in priority {
            if dead.contains(&from) {
                continue;
            }
            engine.handle_ipc(msg);
        }
        if let Some((from, u)) = latest_index {
            if !dead.contains(&from) {
                engine.handle_ipc(IpcMessage::LibraryIndexUpdated(u));
            }
        }

        // Reconcile once per tick after all inbound commands (not per-message).
        engine.poll_stream_lifecycle();
        engine.poll_audio_route();
        engine.poll_library_retry();
        if let Some(elapsed) = engine.poll_tap_to_audio_metric() {
            eprintln!(
                "plippa-engine: tap→first-audio {}ms (Noubin §3.2.1 target <1000ms, ideal <300ms)",
                elapsed.as_millis()
            );
        }
        drain_engine_events(engine, &mut clients, &mut dead);

        // Periodic status: refresh clients without bumping revision unless
        // transport already changed this tick (events already drained above).
        let rev = engine.status_revision();
        let due = pacer.due(Instant::now(), engine.wants_fast_status()) && !clients.is_empty();
        if due && rev == last_published_revision {
            engine.publish_status();
            drain_engine_events(engine, &mut clients, &mut dead);
            pacer.mark(Instant::now());
        } else if rev != last_published_revision {
            last_published_revision = rev;
            pacer.mark(Instant::now());
        }

        if !dead.is_empty() {
            eprintln!("plippa-engine: dropping {} IPC client(s)", dead.len());
            sweep_dead(&mut clients, &dead);
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_is_500ms_normally_and_100ms_when_warped_scanning_or_seeking() {
        let t0 = Instant::now();
        let mut p = StatusPacer::new(t0);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        // Normal: not due before 500 ms, due at 500.
        assert!(!p.due(at(99), false));
        assert!(!p.due(at(499), false));
        assert!(p.due(at(500), false));
        // Fast: due from 100 ms.
        assert!(!p.due(at(99), true));
        assert!(p.due(at(100), true));
        assert!(p.due(at(250), true));
        // Marking restarts the window (event-driven pushes count too).
        p.mark(at(1_000));
        assert!(!p.due(at(1_099), true));
        assert!(p.due(at(1_100), true));
        assert!(!p.due(at(1_499), false));
        assert!(p.due(at(1_500), false));
        // A clock that appears to go backwards never panics and is not due.
        assert!(!p.due(at(900), true));
    }

    #[test]
    fn interval_constants_match_the_decision() {
        assert_eq!(StatusPacer::interval(false), Duration::from_millis(500));
        assert_eq!(StatusPacer::interval(true), Duration::from_millis(100));
    }
}
