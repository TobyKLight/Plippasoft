//! Postcard IPC bus abstraction (live UDS: [`crate::UdsIpc`]).

use plippa_ipc_proto::{
    AssociateResponse, IpcMessage, NowPlayingListChanged, PlaybackStatus, ResumeWriteback,
    UnmatchedNoubin, UserNotice,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Snapshot of engine state mirrored for the portal.
#[derive(Debug, Clone, Default)]
pub struct IpcMirror {
    pub playback: Option<PlaybackStatus>,
    pub last_notice: Option<UserNotice>,
    /// NB_08: latest URL tap that matched nothing locally.
    pub unmatched: Option<UnmatchedNoubin>,
    /// NB_10: the engine's `nowPlayingList` as last published (shared, up to 1500 rows).
    pub list: Option<Arc<NowPlayingListChanged>>,
    /// Counts every `NowPlayingListChanged` received (0 = none yet); lets callers wait for
    /// "a new list arrived" and lets the portal know when to refetch the rows.
    pub list_seq: u64,
    pub live: bool,
}

/// Shared slot for the latest list + its sequence number (used by the IPC implementations).
#[derive(Debug, Default, Clone)]
pub struct ListSlot(Arc<Mutex<(u64, Option<Arc<NowPlayingListChanged>>)>>);

impl ListSlot {
    pub fn set(&self, list: NowPlayingListChanged) {
        if let Ok(mut g) = self.0.lock() {
            g.0 += 1;
            g.1 = Some(Arc::new(list));
        }
    }

    pub fn get(&self) -> (u64, Option<Arc<NowPlayingListChanged>>) {
        self.0.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Outbound IPC from server → engine / UI.
pub trait IpcBus: Send {
    fn send(&mut self, msg: IpcMessage);

    /// Wait for an [`AssociateResponse`] matching `request_id` (portal live path).
    ///
    /// Default: `None` — caller falls back to the local NFC callback (tests / offline).
    fn wait_associate_response(
        &mut self,
        _request_id: u64,
        _timeout: Duration,
    ) -> Option<AssociateResponse> {
        None
    }

    /// True when [`wait_associate_response`] actually listens on the bus (UDS).
    /// When false, a `None` wait means “use the mock/local NFC callback”.
    fn supports_live_associate(&self) -> bool {
        false
    }

    /// Connected to a live engine hub (not [`NullIpc`]).
    fn is_live(&self) -> bool {
        false
    }

    /// Drain inbound frames (status / notices / stray associate responses).
    fn poll_inbound(&mut self) {}

    /// Resume write-back requests the engine sent since the last call (NB_04).
    fn take_resume_writebacks(&mut self) -> Vec<ResumeWriteback> {
        Vec::new()
    }

    /// Latest mirrored engine status after [`poll_inbound`].
    fn mirror(&self) -> IpcMirror {
        IpcMirror::default()
    }
}

/// No-op bus for runs without peers attached.
#[derive(Debug, Default)]
pub struct NullIpc;

impl IpcBus for NullIpc {
    fn send(&mut self, _msg: IpcMessage) {}
}

/// Records every message for unit tests / mock harnesses.
#[derive(Debug, Default, Clone)]
pub struct RecordingIpc {
    inner: Arc<Mutex<Vec<IpcMessage>>>,
    resume: Arc<Mutex<Vec<ResumeWriteback>>>,
    unmatched: Arc<Mutex<Option<UnmatchedNoubin>>>,
    playback: Arc<Mutex<Option<PlaybackStatus>>>,
    list: ListSlot,
}

impl RecordingIpc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> Vec<IpcMessage> {
        self.inner.lock().expect("ipc lock").clone()
    }

    pub fn clear(&self) {
        self.inner.lock().expect("ipc lock").clear();
    }

    /// Simulate the engine reporting an unmatched URL tap (tests).
    pub fn inject_unmatched(&self, u: UnmatchedNoubin) {
        *self.unmatched.lock().expect("ipc lock") = Some(u);
    }

    /// Simulate the engine's status frame (tests).
    pub fn inject_playback(&self, status: PlaybackStatus) {
        *self.playback.lock().expect("ipc lock") = Some(status);
    }

    /// Simulate the engine publishing its `nowPlayingList` (tests).
    pub fn inject_list(&self, list: NowPlayingListChanged) {
        self.list.set(list);
    }

    /// Simulate the engine sending a [`ResumeWriteback`] (tests).
    pub fn inject_resume_writeback(&self, wb: ResumeWriteback) {
        self.resume.lock().expect("ipc lock").push(wb);
    }
}

impl IpcBus for RecordingIpc {
    fn send(&mut self, msg: IpcMessage) {
        self.inner.lock().expect("ipc lock").push(msg);
    }

    fn is_live(&self) -> bool {
        // Tests treat recording as a connected peer.
        true
    }

    fn take_resume_writebacks(&mut self) -> Vec<ResumeWriteback> {
        std::mem::take(&mut *self.resume.lock().expect("ipc lock"))
    }

    fn mirror(&self) -> IpcMirror {
        let (list_seq, list) = self.list.get();
        IpcMirror {
            live: true,
            unmatched: self.unmatched.lock().expect("ipc lock").clone(),
            playback: self.playback.lock().expect("ipc lock").clone(),
            list,
            list_seq,
            ..IpcMirror::default()
        }
    }
}

/// Alias used in docs / tests.
pub type MockIpcBus = RecordingIpc;
