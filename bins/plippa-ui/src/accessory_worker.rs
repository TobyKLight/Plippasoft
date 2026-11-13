//! Accessory radio worker (ACC_03): the only place the UI calls an accessory backend.
//!
//! The coordinator sends [`AccessoryCmd`]s on a channel and drains [`AccessoryMsg`]s; it never
//! calls a backend method itself. The worker polls with a 100 ms tick, batches the events the
//! backend produced into one message per tick and forwards a fresh adapters/known snapshot when
//! asked (`Refresh`, and once at start). See `docs/design/ui-threading.md` for the same pattern
//! used by the I²C service.

use crate::accessories::accessory_error_text;
use plippa_hw::{
    Accessory, AccessoryBackend, AccessoryEvent, AccessoryId, AdapterInfo, PromptAnswer,
};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

/// Worker tick and worst-case event latency. The backend poll is a cheap event-queue
/// drain, so a short tick keeps transport buttons responsive without measurable CPU cost.
pub const TICK: Duration = Duration::from_millis(20);

/// Commands the coordinator sends. They mirror [`AccessoryBackend`] plus `Refresh`, which is
/// answered with a snapshot instead of a backend event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessoryCmd {
    PowerOn(String),
    StartScan(u32),
    StopScan,
    Pair(AccessoryId),
    Answer(PromptAnswer),
    Connect(AccessoryId),
    Disconnect(AccessoryId),
    SetTrusted { id: AccessoryId, trusted: bool },
    Forget(AccessoryId),
    Refresh,
}

/// One worker-to-coordinator message.
#[derive(Debug, Clone)]
pub enum AccessoryMsg {
    Events(Vec<AccessoryEvent>),
    Snapshot {
        adapters: Vec<AdapterInfo>,
        known: Vec<Accessory>,
        unavailable: bool,
        /// Events drained in the same tick, applied before the snapshot.
        events: Vec<AccessoryEvent>,
    },
}

impl AccessoryMsg {
    pub fn is_empty(&self) -> bool {
        matches!(self, AccessoryMsg::Events(events) if events.is_empty())
    }
}

/// The coordinator's end of both channels.
#[derive(Debug)]
pub struct AccessoryHandle {
    pub cmd: Sender<AccessoryCmd>,
    pub evt: Receiver<AccessoryMsg>,
}

impl AccessoryHandle {
    pub fn new(cmd: Sender<AccessoryCmd>, evt: Receiver<AccessoryMsg>) -> Self {
        Self { cmd, evt }
    }

    /// Start the worker thread with `backend`. Returns immediately; the backend's construction
    /// contract is that it does too. `None` when the thread cannot be spawned, so the caller
    /// reports no radio rather than holding a dead channel.
    pub fn spawn<B: AccessoryBackend + 'static>(backend: B) -> Option<Self> {
        let (cmd, cmd_rx) = mpsc::channel();
        let (evt_tx, evt) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("plippa-accessory-worker".to_string())
            .spawn(move || {
                let mut worker = AccessoryWorker::new(backend);
                let _ = evt_tx.send(worker.snapshot());
                loop {
                    match cmd_rx.recv_timeout(TICK) {
                        Ok(command) => {
                            let msg = worker.apply(command);
                            if !msg.is_empty() && evt_tx.send(msg).is_err() {
                                return;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    let msg = worker.poll();
                    if !msg.is_empty() && evt_tx.send(msg).is_err() {
                        return;
                    }
                }
            });
        match spawned {
            Ok(_) => Some(Self::new(cmd, evt)),
            Err(error) => {
                eprintln!("plippa-ui: accessory worker spawn failed: {error}");
                None
            }
        }
    }
}

/// The backend-owning half, split out so the integration tests can drive it synchronously.
pub struct AccessoryWorker<B: AccessoryBackend> {
    backend: B,
}

impl<B: AccessoryBackend> AccessoryWorker<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Test / embedding access to the backend (clock, scripts). The production loop never uses
    /// this; it only calls [`Self::apply`] and [`Self::poll`].
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn snapshot(&mut self) -> AccessoryMsg {
        let adapters = self.backend.adapters();
        let known = self.backend.known();
        let unavailable = adapters.is_err() || known.is_err();
        AccessoryMsg::Snapshot {
            adapters: adapters.unwrap_or_default(),
            known: known.unwrap_or_default(),
            unavailable,
            events: Vec::new(),
        }
    }

    /// Run one command, then batch everything the backend has produced since the last poll.
    pub fn apply(&mut self, command: AccessoryCmd) -> AccessoryMsg {
        let mut events = Vec::new();
        let refresh = matches!(command, AccessoryCmd::Refresh);
        let label = format!("{command:?}");
        if let Err(error) = self.run(command) {
            eprintln!("plippa-ui: accessory command {label} failed: {error:?}");
            events.push(AccessoryEvent::Failed(
                accessory_error_text(&error).to_string(),
            ));
        }
        events.extend(self.backend.poll());
        if refresh {
            let mut snapshot = self.snapshot();
            if let AccessoryMsg::Snapshot {
                events: ref mut snapshot_events,
                ..
            } = snapshot
            {
                *snapshot_events = events;
            }
            snapshot
        } else {
            AccessoryMsg::Events(events)
        }
    }

    /// Drain events without a command (the production tick).
    pub fn poll(&mut self) -> AccessoryMsg {
        AccessoryMsg::Events(self.backend.poll())
    }

    fn run(&mut self, command: AccessoryCmd) -> Result<(), plippa_hw::AccessoryError> {
        match command {
            AccessoryCmd::PowerOn(adapter) => self.backend.power_on(&adapter),
            AccessoryCmd::StartScan(seconds) => self.backend.start_scan(seconds),
            AccessoryCmd::StopScan => self.backend.stop_scan(),
            AccessoryCmd::Pair(id) => self.backend.pair(&id),
            AccessoryCmd::Answer(answer) => self.backend.answer(answer),
            AccessoryCmd::Connect(id) => self.backend.connect(&id),
            AccessoryCmd::Disconnect(id) => self.backend.disconnect(&id),
            AccessoryCmd::SetTrusted { id, trusted } => self.backend.set_trusted(&id, trusted),
            AccessoryCmd::Forget(id) => self.backend.forget(&id),
            // Answered by `apply` after the event drain.
            AccessoryCmd::Refresh => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_hw::{AccessoryKind, MockAccessoryBackend, PairPrompt};

    fn adapter() -> AdapterInfo {
        AdapterInfo {
            id: "hci0".into(),
            address: "11:22:33:44:55:66".into(),
            name: "radio".into(),
            powered: true,
        }
    }

    fn accessory(id: &str, connected: bool) -> Accessory {
        Accessory {
            id: id.into(),
            name: "Speaker".into(),
            kind: AccessoryKind::Audio,
            paired: true,
            trusted: true,
            connected,
            battery_pct: None,
        }
    }

    #[test]
    fn commands_reach_the_backend_and_events_come_back_batched() {
        let mut unpaired = accessory("aa:bb:cc:dd:ee:01", false);
        unpaired.paired = false;
        let mut mock = MockAccessoryBackend::new()
            .with_adapter(adapter())
            .with_accessory(unpaired);
        mock.script_pair("aa:bb:cc:dd:ee:01", [PromptAnswer::Accept]);
        let mut worker = AccessoryWorker::new(mock);
        let snapshot = worker.snapshot();
        match snapshot {
            AccessoryMsg::Snapshot {
                adapters,
                unavailable,
                ..
            } => {
                assert_eq!(adapters.len(), 1);
                assert!(!unavailable);
            }
            _ => panic!("expected a snapshot"),
        }

        let msg = worker.apply(AccessoryCmd::Pair("aa:bb:cc:dd:ee:01".into()));
        match msg {
            AccessoryMsg::Events(events) => {
                assert!(
                    events.iter().any(|e| matches!(e, AccessoryEvent::Prompt(_))),
                    "{events:?}"
                );
            }
            _ => panic!("expected events"),
        }
        assert!(worker.backend_mut().calls().iter().any(|c| c == "pair aa:bb:cc:dd:ee:01"));
    }

    #[test]
    fn a_failed_command_becomes_one_human_failure_event() {
        let mut worker = AccessoryWorker::new(MockAccessoryBackend::unavailable());
        let msg = worker.apply(AccessoryCmd::Connect("aa:bb:cc:dd:ee:01".into()));
        match msg {
            AccessoryMsg::Events(events) => {
                assert_eq!(events.len(), 1, "{events:?}");
                match &events[0] {
                    AccessoryEvent::Failed(text) => {
                        assert!(!text.contains("Unavailable"), "{text}");
                    }
                    other => panic!("expected Failed, got {other:?}"),
                }
            }
            _ => panic!("expected events"),
        }
    }

    #[test]
    fn refresh_returns_a_snapshot_after_the_events() {
        let mut mock = MockAccessoryBackend::new()
            .with_adapter(adapter())
            .with_accessory(accessory("aa:bb:cc:dd:ee:01", true));
        mock.script_scan([]);
        let mut worker = AccessoryWorker::new(mock);
        let msg = worker.apply(AccessoryCmd::Refresh);
        match msg {
            AccessoryMsg::Snapshot { known, .. } => {
                assert_eq!(known.len(), 1);
                assert!(known[0].connected);
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn prompt_keyboard_is_digits_only_for_a_passkey() {
        let prompt = PairPrompt::EnterPasskey {
            peer: plippa_hw::Peer {
                name: None,
                address: "58:8A:D2:D8:22:7F".into(),
            },
        };
        let kb = crate::accessories::prompt_keyboard(&prompt).expect("keyboard");
        assert!(kb.current_char().is_ascii_digit());
    }
}
