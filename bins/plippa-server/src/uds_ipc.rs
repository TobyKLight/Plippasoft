//! Live UDS implementation of [`IpcBus`], plus background reconnect.

use plippa_ipc_proto::{
    AssociateResponse, IpcConnection, IpcMessage, PlaybackStatus, ResumeWriteback,
    UnmatchedNoubin, UserNotice,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::ipc::{IpcBus, IpcMirror, ListSlot, NullIpc};

/// How long the portal keeps showing the engine's last notice.
///
/// The device shows a notice as a popup that clears itself after
/// `plippa_ui::ui::tokens`' 3 s; the portal's banner used to keep the last notice forever,
/// so a "no local media for this Noubin" banner stayed on screen long after the files were
/// added (or the tag associated) and made a working tag look broken. Mirrored here so the
/// two surfaces agree.
const NOTICE_MIRROR_TTL: Duration = Duration::from_secs(6);

/// The notice to mirror, or `None` once its window has passed.
fn fresh_notice(slot: &Option<(UserNotice, Instant)>) -> Option<UserNotice> {
    slot.clone()
        .filter(|(_, at)| at.elapsed() < NOTICE_MIRROR_TTL)
        .map(|(n, _)| n)
}

/// Shared connection used by the HTTP service to publish IPC frames.
#[derive(Clone)]
pub struct UdsIpc {
    conn: Arc<Mutex<IpcConnection>>,
    playback: Arc<Mutex<Option<PlaybackStatus>>>,
    /// Latest notice **with the instant it arrived**, so the portal can age it out.
    last_notice: Arc<Mutex<Option<(UserNotice, Instant)>>>,
    /// NB_08: latest unmatched URL tap.
    unmatched: Arc<Mutex<Option<UnmatchedNoubin>>>,
    /// Engine → server resume requests waiting to be written to `.noudata`.
    resume: Arc<Mutex<Vec<ResumeWriteback>>>,
    /// NB_10: latest `nowPlayingList` for the portal.
    list: ListSlot,
}

impl UdsIpc {
    pub fn new(conn: IpcConnection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            playback: Arc::new(Mutex::new(None)),
            last_notice: Arc::new(Mutex::new(None)),
            unmatched: Arc::new(Mutex::new(None)),
            resume: Arc::new(Mutex::new(Vec::new())),
            list: ListSlot::default(),
        }
    }

    pub fn shared(conn: Arc<Mutex<IpcConnection>>) -> Self {
        Self {
            conn,
            playback: Arc::new(Mutex::new(None)),
            last_notice: Arc::new(Mutex::new(None)),
            unmatched: Arc::new(Mutex::new(None)),
            resume: Arc::new(Mutex::new(Vec::new())),
            list: ListSlot::default(),
        }
    }

    fn note_inbound(
        playback: &Mutex<Option<PlaybackStatus>>,
        notice: &Mutex<Option<(UserNotice, Instant)>>,
        resume: &Mutex<Vec<ResumeWriteback>>,
        unmatched: &Mutex<Option<UnmatchedNoubin>>,
        list: &ListSlot,
        msg: &IpcMessage,
    ) {
        match msg {
            IpcMessage::NowPlayingListChanged(l) => list.set(l.clone()),
            IpcMessage::UnmatchedNoubin(u) => {
                if let Ok(mut g) = unmatched.lock() {
                    *g = Some(u.clone());
                }
            }
            IpcMessage::ResumeWriteback(w) => {
                if let Ok(mut g) = resume.lock() {
                    g.push(w.clone());
                }
            }
            IpcMessage::PlaybackStatus(s) => {
                if let Ok(mut g) = playback.lock() {
                    *g = Some(s.clone());
                }
            }
            IpcMessage::UserNotice(n) => {
                if let Ok(mut g) = notice.lock() {
                    *g = Some((n.clone(), Instant::now()));
                }
            }
            _ => {}
        }
    }
}

impl IpcBus for UdsIpc {
    fn send(&mut self, msg: IpcMessage) {
        if let Ok(mut guard) = self.conn.lock() {
            if let Err(e) = guard.send(&msg) {
                eprintln!("plippa-server: IPC send failed: {e}");
            } else {
                eprintln!("plippa-server: IPC sent {}", brief_msg(&msg));
            }
        }
    }

    fn wait_associate_response(
        &mut self,
        request_id: u64,
        timeout: Duration,
    ) -> Option<AssociateResponse> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let slice = remaining.min(Duration::from_millis(200));
            let Ok(mut guard) = self.conn.lock() else {
                return None;
            };
            match guard.recv_timeout(slice) {
                Ok(Some(IpcMessage::AssociateResponse(resp))) if resp.request_id == request_id => {
                    return Some(resp);
                }
                Ok(Some(msg)) => {
                    Self::note_inbound(
                        &self.playback,
                        &self.last_notice,
                        &self.resume,
                        &self.unmatched,
                        &self.list,
                        &msg,
                    );
                }
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        None
    }

    fn supports_live_associate(&self) -> bool {
        true
    }

    fn is_live(&self) -> bool {
        true
    }

    fn poll_inbound(&mut self) {
        let Ok(mut guard) = self.conn.lock() else {
            return;
        };
        for _ in 0..32 {
            match guard.try_recv() {
                Ok(Some(msg)) => {
                    Self::note_inbound(
                        &self.playback,
                        &self.last_notice,
                        &self.resume,
                        &self.unmatched,
                        &self.list,
                        &msg,
                    );
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    fn take_resume_writebacks(&mut self) -> Vec<ResumeWriteback> {
        self.resume
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    fn mirror(&self) -> IpcMirror {
        let (list_seq, list) = self.list.get();
        IpcMirror {
            list,
            list_seq,
            playback: self.playback.lock().ok().and_then(|g| g.clone()),
            // Aged out so a resolved notice cannot outlive its cause in the portal.
            last_notice: self.last_notice.lock().ok().and_then(|g| fresh_notice(&g)),
            unmatched: self.unmatched.lock().ok().and_then(|g| g.clone()),
            live: true,
        }
    }
}

/// IPC bus that retries connecting to `engine.sock` in the background.
///
/// Avoids permanent [`NullIpc`] after a transient miss at process start
/// (engine listen-before-Pulse race). First connect attempt uses `initial_timeout`;
/// later attempts use a short poll every ~1 s until live or process exit.
pub struct ReconnectingIpc {
    sock: PathBuf,
    inner: Arc<Mutex<Option<UdsIpc>>>,
    stop: Arc<AtomicBool>,
}

impl ReconnectingIpc {
    pub fn start(sock: PathBuf, initial_timeout: Duration) -> Self {
        let inner = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));

        if !initial_timeout.is_zero() {
            if let Ok(conn) = IpcConnection::connect_timeout(&sock, initial_timeout) {
                eprintln!("plippa-server: IPC connected to {}", sock.display());
                if let Ok(mut g) = inner.lock() {
                    *g = Some(UdsIpc::new(conn));
                }
            } else {
                eprintln!(
                    "plippa-server: IPC not ready at {} — background reconnect enabled",
                    sock.display()
                );
            }
        }

        let sock_bg = sock.clone();
        let inner_bg = Arc::clone(&inner);
        let stop_bg = Arc::clone(&stop);
        thread::Builder::new()
            .name("plippa-server-ipc-reconnect".into())
            .spawn(move || {
                while !stop_bg.load(Ordering::Relaxed) {
                    let need = inner_bg
                        .lock()
                        .ok()
                        .map(|g| g.is_none())
                        .unwrap_or(true);
                    if need {
                        match IpcConnection::connect_timeout(
                            &sock_bg,
                            Duration::from_millis(500),
                        ) {
                            Ok(conn) => {
                                eprintln!(
                                    "plippa-server: IPC reconnected to {}",
                                    sock_bg.display()
                                );
                                if let Ok(mut g) = inner_bg.lock() {
                                    *g = Some(UdsIpc::new(conn));
                                }
                            }
                            Err(_) => {
                                thread::sleep(Duration::from_secs(1));
                            }
                        }
                    } else {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            })
            .expect("spawn ipc reconnect");

        // Continuously drain engine broadcasts so a closed/idle portal cannot
        // fill the UDS buffer and block the engine hub on send.
        let inner_drain = Arc::clone(&inner);
        let stop_drain = Arc::clone(&stop);
        thread::Builder::new()
            .name("plippa-server-ipc-drain".into())
            .spawn(move || {
                while !stop_drain.load(Ordering::Relaxed) {
                    if let Ok(mut g) = inner_drain.lock() {
                        if let Some(ipc) = g.as_mut() {
                            ipc.poll_inbound();
                        }
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            })
            .expect("spawn ipc drain");

        Self { sock, inner, stop }
    }

    /// Shared handle so HTTP can wait on associate without holding `ServerService`.
    pub fn handle(&self) -> Arc<Mutex<Option<UdsIpc>>> {
        Arc::clone(&self.inner)
    }
}

impl Drop for ReconnectingIpc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl IpcBus for ReconnectingIpc {
    fn send(&mut self, msg: IpcMessage) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(ipc) = guard.as_mut() {
            ipc.send(msg);
        } else {
            eprintln!(
                "plippa-server: IPC send dropped (not connected to {} yet)",
                self.sock.display()
            );
        }
    }

    fn wait_associate_response(
        &mut self,
        request_id: u64,
        timeout: Duration,
    ) -> Option<AssociateResponse> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let slice = remaining.min(Duration::from_millis(200));
            let mut guard = self.inner.lock().ok()?;
            if let Some(ipc) = guard.as_mut() {
                // Borrow UdsIpc wait for one slice — drop lock between slices so
                // status handlers can poll_inbound / mirror.
                if let Some(resp) = ipc.wait_associate_response(request_id, slice) {
                    return Some(resp);
                }
            } else {
                drop(guard);
                thread::sleep(slice.min(Duration::from_millis(200)));
            }
        }
        None
    }

    fn supports_live_associate(&self) -> bool {
        true
    }

    fn is_live(&self) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|i| i.is_live()))
            .unwrap_or(false)
    }

    fn poll_inbound(&mut self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(ipc) = g.as_mut() {
                ipc.poll_inbound();
            }
        }
    }

    fn take_resume_writebacks(&mut self) -> Vec<ResumeWriteback> {
        match self.inner.lock() {
            Ok(mut g) => g
                .as_mut()
                .map(|ipc| ipc.take_resume_writebacks())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn mirror(&self) -> IpcMirror {
        if let Ok(g) = self.inner.lock() {
            if let Some(ipc) = g.as_ref() {
                return ipc.mirror();
            }
        }
        IpcMirror {
            live: false,
            ..IpcMirror::default()
        }
    }
}

/// Connect with background reconnect (preferred for launcher-supervised runs).
///
/// `PLIPPA_IPC_CONNECT_SECS=0` skips waiting and returns [`NullIpc`] (offline /
/// smoke without an engine).
pub fn connect_or_null(sock: &Path, timeout: Duration) -> Box<dyn IpcBus> {
    if timeout.is_zero() {
        eprintln!("plippa-server: IPC connect skipped (0s); using NullIpc");
        return Box::new(NullIpc);
    }
    Box::new(ReconnectingIpc::start(sock.to_path_buf(), timeout))
}

/// Resolve connect timeout from env / default.
///
/// Default 20s for the *first* attempt; reconnect continues afterward if needed.
pub fn ipc_connect_timeout() -> Duration {
    let secs = std::env::var("PLIPPA_IPC_CONNECT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(20);
    Duration::from_secs(secs)
}

/// How long the portal waits for UI NFC write during associate (default 60s).
pub fn associate_wait_timeout() -> Duration {
    let secs = std::env::var("PLIPPA_ASSOCIATE_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

fn brief_msg(msg: &IpcMessage) -> &'static str {
    match msg {
        IpcMessage::TapNdef(_) => "TapNdef",
        IpcMessage::TapKey(_) => "TapKey",
        IpcMessage::PlaybackCommand(_) => "PlaybackCommand",
        IpcMessage::PlaybackStatus(_) => "PlaybackStatus",
        IpcMessage::LibraryIndexUpdated(_) => "LibraryIndexUpdated",
        IpcMessage::UserNotice(_) => "UserNotice",
        IpcMessage::AssociateRequest(_) => "AssociateRequest",
        IpcMessage::AssociateResponse(_) => "AssociateResponse",
        IpcMessage::NowPlayingListChanged(_) => "NowPlayingListChanged",
        IpcMessage::ResumeWriteback(_) => "ResumeWriteback",
        IpcMessage::UnmatchedNoubin(_) => "UnmatchedNoubin",
        IpcMessage::AudioRouteChanged(_) => "AudioRouteChanged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::NowPlayingListKind;
    use plippa_ipc_proto::NowPlayingListChanged;

    /// NB_10: inbound `NowPlayingListChanged` frames land in the mirror, newest wins, and the
    /// sequence number counts them (the portal refetches rows when it changes).
    #[test]
    fn inbound_now_playing_list_is_mirrored_with_a_sequence() {
        let (playback, notice, resume, unmatched, list) = (
            Mutex::new(None),
            Mutex::new(None),
            Mutex::new(Vec::new()),
            Mutex::new(None),
            ListSlot::default(),
        );
        assert_eq!(list.get().0, 0);
        for n in 0..2u32 {
            UdsIpc::note_inbound(
                &playback,
                &notice,
                &resume,
                &unmatched,
                &list,
                &IpcMessage::NowPlayingListChanged(NowPlayingListChanged {
                    entries: Vec::new(),
                    current_index: n,
                    list_kind: NowPlayingListKind::Album,
                    list_id: 1,
                }),
            );
        }
        let (seq, l) = list.get();
        assert_eq!(seq, 2);
        assert_eq!(l.unwrap().current_index, 1);
    }

    /// The portal banner must not outlive its cause: a notice is mirrored while it is fresh and
    /// dropped once its window passes, so "no local media found" cannot sit on the page after
    /// the files have been added or the tag associated.
    #[test]
    fn a_mirrored_notice_expires_after_its_window() {
        let (playback, notice, resume, unmatched, list) = (
            Mutex::new(None),
            Mutex::new(None),
            Mutex::new(Vec::new()),
            Mutex::new(None),
            ListSlot::default(),
        );
        UdsIpc::note_inbound(
            &playback,
            &notice,
            &resume,
            &unmatched,
            &list,
            &IpcMessage::UserNotice(UserNotice {
                severity: plippa_ipc_proto::NoticeSeverity::Warning,
                code: plippa_ipc_proto::NOTICE_LIBRARY_MISS.into(),
                message: "No local media found".into(),
            }),
        );
        let (n, at) = notice.lock().unwrap().clone().expect("mirrored");
        assert_eq!(n.message, "No local media found");
        assert!(at.elapsed() < NOTICE_MIRROR_TTL, "fresh notices are shown");

        // Driving the same helper `mirror()` uses.
        assert_eq!(
            fresh_notice(&Some((n.clone(), Instant::now()))).map(|n| n.message),
            Some("No local media found".to_string()),
            "a fresh notice is mirrored"
        );
        assert!(
            fresh_notice(&Some((n, Instant::now() - NOTICE_MIRROR_TTL))).is_none(),
            "an expired notice is dropped"
        );
        assert!(fresh_notice(&None).is_none(), "nothing to show");
    }
}
