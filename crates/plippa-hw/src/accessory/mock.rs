//! Deterministic in-memory accessory backend for unit tests.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use super::policy::MAX_SCAN_SECONDS;
use super::{
    normalise_address, Accessory, AccessoryBackend, AccessoryError, AccessoryEvent, AccessoryId,
    AccessoryKind, AdapterInfo, PairPrompt, Peer, PromptAnswer, TransportAction,
};

#[derive(Debug, Clone)]
struct PendingPair {
    id: AccessoryId,
    expected: Vec<PromptAnswer>,
    next: usize,
}

/// Scripted accessory backend: seed state, script outcomes, inspect the calls the UI made.
#[derive(Debug)]
pub struct MockAccessoryBackend {
    unavailable: bool,
    now: Duration,
    adapters: Vec<AdapterInfo>,
    known: Vec<Accessory>,
    scan_script: Vec<(AccessoryId, String, AccessoryKind)>,
    scan_queue: VecDeque<(AccessoryId, String, AccessoryKind)>,
    scan_active: bool,
    scan_deadline: Duration,
    discovered: BTreeMap<AccessoryId, (String, AccessoryKind)>,
    pair_scripts: BTreeMap<AccessoryId, Vec<PromptAnswer>>,
    pair_failures: BTreeMap<AccessoryId, String>,
    pending_pair: Option<PendingPair>,
    events: VecDeque<AccessoryEvent>,
    /// Every method call in order, e.g. `"connect aa:bb:cc:dd:ee:ff"`.
    pub calls: Vec<String>,
}

impl Default for MockAccessoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockAccessoryBackend {
    pub fn new() -> Self {
        Self {
            unavailable: false,
            now: Duration::ZERO,
            adapters: Vec::new(),
            known: Vec::new(),
            scan_script: Vec::new(),
            scan_queue: VecDeque::new(),
            scan_active: false,
            scan_deadline: Duration::ZERO,
            discovered: BTreeMap::new(),
            pair_scripts: BTreeMap::new(),
            pair_failures: BTreeMap::new(),
            pending_pair: None,
            events: VecDeque::new(),
            calls: Vec::new(),
        }
    }

    /// A board with no radio: every method returns [`AccessoryError::Unavailable`] and
    /// [`AccessoryBackend::poll`] is empty.
    pub fn unavailable() -> Self {
        Self {
            unavailable: true,
            ..Self::new()
        }
    }

    pub fn with_adapter(mut self, adapter: AdapterInfo) -> Self {
        self.adapters.push(adapter);
        self
    }

    pub fn with_accessory(mut self, accessory: Accessory) -> Self {
        let mut accessory = accessory;
        accessory.id = normalise_address(&accessory.id);
        if let Some(existing) = self.known.iter_mut().find(|a| a.id == accessory.id) {
            *existing = accessory;
        } else {
            self.known.push(accessory);
        }
        self
    }

    /// Injected clock: the current mock time.
    pub fn now(&self) -> Duration {
        self.now
    }

    pub fn set_now(&mut self, now: Duration) {
        self.now = now;
    }

    pub fn advance(&mut self, delta: Duration) {
        self.now = self.now.saturating_add(delta);
    }

    pub fn calls(&self) -> &[String] {
        &self.calls
    }

    /// Devices a subsequent scan reports, one per `poll`, then `ScanFinished`.
    pub fn script_scan(
        &mut self,
        devices: impl IntoIterator<Item = (AccessoryId, String, AccessoryKind)>,
    ) {
        self.scan_script = devices
            .into_iter()
            .map(|(id, name, kind)| (normalise_address(&id), name, kind))
            .collect();
    }

    /// Expected answers for a pairing attempt. The first answer also chooses the first prompt
    /// (`Passkey(n)` → `Confirm(n)`, `Pin(_)` → `EnterPin`, anything else → `Authorise`).
    pub fn script_pair(
        &mut self,
        id: impl AsRef<str>,
        answers: impl IntoIterator<Item = PromptAnswer>,
    ) {
        self.pair_scripts
            .insert(normalise_address(id.as_ref()), answers.into_iter().collect());
    }

    /// Make the next pairing attempt for `id` fail with `error`.
    pub fn script_pair_failure(&mut self, id: impl AsRef<str>, error: impl Into<String>) {
        self.pair_failures
            .insert(normalise_address(id.as_ref()), error.into());
    }

    /// Queue one transport button press, as a connected accessory would send it.
    pub fn script_transport(&mut self, action: TransportAction) {
        self.events.push_back(AccessoryEvent::Transport(action));
    }

    /// Seed a paired accessory so `connect` has something to act on.
    pub fn script_connect(&mut self, id: impl AsRef<str>) {
        self.ensure_known(&normalise_address(id.as_ref()));
    }

    /// Seed a paired accessory so `disconnect` has something to act on.
    pub fn script_disconnect(&mut self, id: impl AsRef<str>) {
        self.ensure_known(&normalise_address(id.as_ref()));
    }

    /// Seed a paired accessory so `forget` has something to act on.
    pub fn script_forget(&mut self, id: impl AsRef<str>) {
        self.ensure_known(&normalise_address(id.as_ref()));
    }

    fn ensure_known(&mut self, id: &AccessoryId) {
        if let Some(accessory) = self.known.iter_mut().find(|a| a.id == *id) {
            accessory.paired = true;
            return;
        }
        let (name, kind) = self
            .discovered
            .get(id)
            .cloned()
            .unwrap_or_else(|| (id.clone(), AccessoryKind::Other));
        self.known.push(Accessory {
            id: id.clone(),
            name,
            kind,
            paired: true,
            trusted: false,
            connected: false,
            battery_pct: None,
        });
    }

    fn record(&mut self, call: impl Into<String>) {
        self.calls.push(call.into());
    }

    fn available(&self) -> Result<(), AccessoryError> {
        if self.unavailable {
            Err(AccessoryError::Unavailable)
        } else {
            Ok(())
        }
    }

    /// The peer a prompt names: the known name (or the discovered one) and the address.
    fn peer_for(&self, id: &AccessoryId) -> Peer {
        let name = self
            .known
            .iter()
            .find(|accessory| accessory.id == *id)
            .map(|accessory| accessory.name.clone())
            .or_else(|| self.discovered.get(id).map(|(name, _)| name.clone()));
        Peer::new(name, id)
    }

    fn finish_pairing(&mut self, id: &AccessoryId, error: Option<String>) {
        self.pending_pair = None;
        let ok = error.is_none();
        if ok {
            let discovered = self.discovered.get(id).cloned();
            if let Some(accessory) = self.known.iter_mut().find(|a| a.id == *id) {
                accessory.paired = true;
                if let Some((name, kind)) = discovered {
                    accessory.name = name;
                    accessory.kind = kind;
                }
            } else {
                let (name, kind) = discovered.unwrap_or_else(|| (id.clone(), AccessoryKind::Other));
                self.known.push(Accessory {
                    id: id.clone(),
                    name,
                    kind,
                    paired: true,
                    trusted: false,
                    connected: false,
                    battery_pct: None,
                });
            }
        } else if let Some(accessory) = self.known.iter_mut().find(|a| a.id == *id) {
            accessory.paired = false;
            accessory.connected = false;
        }
        self.events.push_back(AccessoryEvent::Paired {
            id: id.clone(),
            ok,
            error,
        });
    }
}

impl AccessoryBackend for MockAccessoryBackend {
    fn adapters(&mut self) -> Result<Vec<AdapterInfo>, AccessoryError> {
        self.record("adapters");
        self.available()?;
        Ok(self.adapters.clone())
    }

    fn known(&mut self) -> Result<Vec<Accessory>, AccessoryError> {
        self.record("known");
        self.available()?;
        Ok(self.known.clone())
    }

    fn power_on(&mut self, adapter: &str) -> Result<(), AccessoryError> {
        self.record(format!("power_on {adapter}"));
        self.available()?;
        if self.adapters.is_empty() {
            return Err(AccessoryError::NoAdapter);
        }
        match self.adapters.iter_mut().find(|a| a.id == adapter) {
            Some(found) => {
                found.powered = true;
                Ok(())
            }
            None => Err(AccessoryError::Unknown(adapter.to_string())),
        }
    }

    fn start_scan(&mut self, seconds: u32) -> Result<(), AccessoryError> {
        self.record(format!("start_scan {seconds}"));
        self.available()?;
        let seconds = seconds.min(MAX_SCAN_SECONDS);
        self.scan_queue = self.scan_script.iter().cloned().collect();
        self.scan_active = true;
        self.scan_deadline = self
            .now
            .saturating_add(Duration::from_secs(u64::from(seconds)));
        Ok(())
    }

    fn stop_scan(&mut self) -> Result<(), AccessoryError> {
        self.record("stop_scan");
        self.available()?;
        if self.scan_active {
            self.scan_active = false;
            self.scan_queue.clear();
            self.events.push_back(AccessoryEvent::ScanFinished);
        }
        Ok(())
    }

    fn pair(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.record(format!("pair {id}"));
        self.available()?;
        let id = normalise_address(id);
        if self.pending_pair.is_some() {
            return Err(AccessoryError::Refused(
                "pairing already in progress".to_string(),
            ));
        }
        if let Some(error) = self.pair_failures.get(&id).cloned() {
            self.events.push_back(AccessoryEvent::Paired {
                id,
                ok: false,
                error: Some(error),
            });
            return Ok(());
        }
        if self.known.iter().any(|a| a.id == id && a.paired) {
            self.events.push_back(AccessoryEvent::Paired {
                id,
                ok: true,
                error: None,
            });
            return Ok(());
        }
        if !self.known.iter().any(|a| a.id == id) && !self.discovered.contains_key(&id) {
            return Err(AccessoryError::Unknown(id));
        }
        let expected = self
            .pair_scripts
            .get(&id)
            .cloned()
            .unwrap_or_else(|| vec![PromptAnswer::Accept]);
        if expected.is_empty() {
            self.finish_pairing(&id, None);
            return Ok(());
        }
        let peer = self.peer_for(&id);
        let prompt = prompt_for(&expected[0], &peer);
        self.pending_pair = Some(PendingPair {
            id,
            expected,
            next: 0,
        });
        self.events.push_back(AccessoryEvent::Prompt(prompt));
        Ok(())
    }

    fn answer(&mut self, answer: PromptAnswer) -> Result<(), AccessoryError> {
        self.record("answer");
        self.available()?;
        let Some(id) = self.pending_pair.as_ref().map(|pending| pending.id.clone()) else {
            return Err(AccessoryError::NoPairing);
        };
        let peer = self.peer_for(&id);
        let Some(pending) = self.pending_pair.as_mut() else {
            return Err(AccessoryError::NoPairing);
        };
        let Some(expected) = pending.expected.get(pending.next).cloned() else {
            self.pending_pair = None;
            return Err(AccessoryError::NoPairing);
        };
        if matches!(answer, PromptAnswer::Reject) {
            self.finish_pairing(&id, Some("rejected".to_string()));
            return Ok(());
        }
        if !answer_matches(&expected, &answer) {
            let error = if matches!(expected, PromptAnswer::Passkey(_))
                || matches!(answer, PromptAnswer::Passkey(_))
            {
                "passkey did not match".to_string()
            } else {
                "answer did not match the request".to_string()
            };
            self.finish_pairing(&id, Some(error));
            return Ok(());
        }
        pending.next += 1;
        let next_prompt = pending
            .expected
            .get(pending.next)
            .map(|answer| prompt_for(answer, &peer));
        if let Some(prompt) = next_prompt {
            self.events.push_back(AccessoryEvent::Prompt(prompt));
        } else {
            self.finish_pairing(&id, None);
        }
        Ok(())
    }

    fn connect(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.record(format!("connect {id}"));
        self.available()?;
        let id = normalise_address(id);
        let Some(accessory) = self.known.iter_mut().find(|a| a.id == id) else {
            return Err(AccessoryError::Unknown(id));
        };
        if !accessory.paired {
            return Err(AccessoryError::Refused("not paired".to_string()));
        }
        if accessory.connected {
            return Ok(());
        }
        accessory.connected = true;
        let event_id = accessory.id.clone();
        self.events.push_back(AccessoryEvent::Connected(event_id));
        Ok(())
    }

    fn disconnect(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.record(format!("disconnect {id}"));
        self.available()?;
        let id = normalise_address(id);
        let Some(accessory) = self.known.iter_mut().find(|a| a.id == id) else {
            return Err(AccessoryError::Unknown(id));
        };
        if !accessory.connected {
            return Ok(());
        }
        accessory.connected = false;
        let event_id = accessory.id.clone();
        self.events
            .push_back(AccessoryEvent::Disconnected(event_id));
        Ok(())
    }

    fn set_trusted(&mut self, id: &AccessoryId, trusted: bool) -> Result<(), AccessoryError> {
        self.record(format!("set_trusted {id} {trusted}"));
        self.available()?;
        let id = normalise_address(id);
        let Some(accessory) = self.known.iter_mut().find(|a| a.id == id) else {
            return Err(AccessoryError::Unknown(id));
        };
        accessory.trusted = trusted;
        Ok(())
    }

    fn forget(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.record(format!("forget {id}"));
        self.available()?;
        let id = normalise_address(id);
        let Some(position) = self.known.iter().position(|a| a.id == id) else {
            return Err(AccessoryError::Unknown(id));
        };
        let removed = self.known.remove(position);
        self.discovered.remove(&id);
        if self.pending_pair.as_ref().is_some_and(|p| p.id == id) {
            self.pending_pair = None;
        }
        if removed.connected {
            self.events.push_back(AccessoryEvent::Disconnected(id));
        }
        Ok(())
    }

    fn poll(&mut self) -> Vec<AccessoryEvent> {
        self.record("poll");
        if self.unavailable {
            return Vec::new();
        }
        let mut out: Vec<AccessoryEvent> = self.events.drain(..).collect();
        if self.scan_active {
            if self.now >= self.scan_deadline {
                // The scan window is over: drop anything still queued and finish.
                self.scan_active = false;
                self.scan_queue.clear();
                out.push(AccessoryEvent::ScanFinished);
            } else if let Some((id, name, kind)) = self.scan_queue.pop_front() {
                self.discovered
                    .insert(id.clone(), (name.clone(), kind));
                out.push(AccessoryEvent::Discovered { id, name, kind });
            }
        }
        out
    }
}

fn prompt_for(answer: &PromptAnswer, peer: &Peer) -> PairPrompt {
    match answer {
        PromptAnswer::Passkey(n) => PairPrompt::Confirm {
            peer: peer.clone(),
            passkey: *n,
        },
        PromptAnswer::Pin(_) => PairPrompt::EnterPin { peer: peer.clone() },
        PromptAnswer::Accept | PromptAnswer::Reject => PairPrompt::Authorise { peer: peer.clone() },
    }
}

fn answer_matches(expected: &PromptAnswer, answer: &PromptAnswer) -> bool {
    match (expected, answer) {
        (PromptAnswer::Accept, PromptAnswer::Accept) => true,
        (PromptAnswer::Passkey(a), PromptAnswer::Passkey(b)) => a == b,
        (PromptAnswer::Pin(a), PromptAnswer::Pin(b)) => a == b,
        _ => false,
    }
}
