//! The pairing agent we export on the system bus, and the owner-initiated pairing policy.
//!
//! # Policy (SYS-03)
//!
//! The agent is registered for the whole process lifetime (a paired accessory reconnecting
//! needs its `AuthorizeService` answered; unregister/register churn would add failure modes),
//! but it is **strict**: nothing is prompted for unless the owner started a pairing. The whole
//! decision is [`decide`], a pure function of
//!
//! - the request kind (confirm / passkey / PIN / authorise / authorise-a-service),
//! - the device asking, and whether it is already paired or trusted,
//! - the owner's pairing window (which device, still open?),
//! - the service UUID, for `AuthorizeService`.
//!
//! Rules: no window → every pairing request is rejected **without publishing a prompt**; a
//! window for another device → rejected the same way; a window for this device → prompt.
//! `AuthorizeService` for a paired/trusted device is answered yes only for
//! [`ALLOWED_SERVICE_UUIDS`]; an unknown device can never authorise a service.
//!
//! # Prompts
//!
//! Every prompt names the peer ([`PairPrompt`] carries a [`Peer`]): the address always, the
//! advertised name when there is one. The name is untrusted text from a nearby device; the UI
//! sanitises it for the screen. A missing answer after [`PROMPT_TIMEOUT`] cancels the exchange.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use zbus::zvariant::OwnedObjectPath;

use super::names::{bus_names, AgentError};
use super::{lock, AccessoryEvent, AccessoryId, PromptAnswer, Shared};
use crate::accessory::{normalise_address, PairPrompt, Peer};

/// How long the UI may take to answer one prompt before the exchange is cancelled.
pub(crate) const PROMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the owner's pairing window stays open. The UI arms its own countdown from the same
/// number, and the adapter's `PairableTimeout` is set to it so the service closes itself too.
pub(crate) const PAIRING_WINDOW: Duration = Duration::from_secs(60);

/// Service UUIDs an already-paired/trusted device may use without the owner being asked again.
///
/// Derived from what the accessory code handles today: the A/V remote-control profile and its
/// two roles (the transport buttons and the link that carries them). Written once, here, so the
/// policy is one constant to review. An audio sink, a hands-free link or a vendor service is
/// **not** on the list: answering yes to those would let a paired device pull audio or open a
/// microphone path the product never asked for (review SYS-03, owner answer 3).
pub(crate) const ALLOWED_SERVICE_UUIDS: [u16; 3] = [0x110c, 0x110e, 0x110f];

/// The 16-bit id of a base-range UUID (`0000110e-0000-1000-8000-00805f9b34fb`), or `None` for
/// a 32/128-bit vendor UUID. The short form (`110e`, `0x110E`) is accepted too.
fn short_uuid(uuid: &str) -> Option<u16> {
    let text = uuid.trim().trim_matches(|c| c == '{' || c == '}').trim();
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    let text = text.to_ascii_lowercase();
    let base = "-0000-1000-8000-00805f9b34fb";
    let short = match text.len() {
        4 => text.as_str(),
        // `0000110e-0000-1000-8000-00805f9b34fb`: the short id is the second group.
        36 if text.starts_with("0000") && text.ends_with(base) => &text[4..8],
        _ => return None,
    };
    u16::from_str_radix(short, 16).ok()
}

/// True when `service_uuid` is one the product itself uses.
pub(crate) fn service_allowed(service_uuid: &str) -> bool {
    short_uuid(service_uuid).is_some_and(|short| ALLOWED_SERVICE_UUIDS.contains(&short))
}

/// One request the service can make of the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestKind {
    Confirm,
    Passkey,
    Pin,
    Authorize,
    AuthorizeService,
}

/// What the agent must do with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Publish a prompt and wait for the owner.
    Prompt,
    /// Answer yes without asking (an already-paired device on an allowed service).
    Allow,
    /// Answer no without asking, and publish nothing.
    Reject,
}

/// The inputs of the policy. `window` is the device the owner selected **and** whose window is
/// still open; `None` means no owner-initiated pairing is running.
#[derive(Debug, Clone)]
pub(crate) struct Request<'a> {
    pub(crate) kind: RequestKind,
    pub(crate) device: &'a str,
    pub(crate) paired: bool,
    pub(crate) trusted: bool,
    pub(crate) window: Option<&'a str>,
    pub(crate) service_uuid: Option<&'a str>,
}

impl Request<'_> {
    fn in_window(&self) -> bool {
        self.window
            .is_some_and(|target| normalise_address(target) == normalise_address(self.device))
    }
}

/// The whole SYS-03 policy, pure.
pub(crate) fn decide(request: &Request<'_>) -> Decision {
    match request.kind {
        RequestKind::AuthorizeService => {
            if !(request.paired || request.trusted) {
                return Decision::Reject;
            }
            match request.service_uuid {
                Some(uuid) if service_allowed(uuid) => Decision::Allow,
                _ => Decision::Reject,
            }
        }
        // Re-authorisation of a device we already paired: BlueZ asks again on every reconnect,
        // and a prompt there would leave the profile connection waiting (and time out).
        RequestKind::Authorize => {
            if request.paired || request.trusted {
                Decision::Allow
            } else if request.in_window() {
                Decision::Prompt
            } else {
                Decision::Reject
            }
        }
        // The passkey exchange itself: only ever part of an owner-initiated pairing.
        RequestKind::Confirm | RequestKind::Passkey | RequestKind::Pin => {
            if request.in_window() {
                Decision::Prompt
            } else {
                Decision::Reject
            }
        }
    }
}

/// The adapter property calls the pairing window needs. The real implementation talks to the
/// selected adapter over D-Bus (see `mod.rs`); tests inject a recording one, so the window
/// rules are tested without a bus.
pub(crate) trait PairableControl: Send + Sync {
    /// Set `Adapter1.Pairable` (`PairableTimeout` too when opening).
    fn set_pairable(&self, pairable: bool) -> Result<(), String>;
}

/// The owner-initiated pairing window: one device, one minute, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairingWindow {
    pub(crate) target: AccessoryId,
    pub(crate) deadline: Instant,
}

impl PairingWindow {
    pub(crate) fn open(target: &str, now: Instant) -> Self {
        Self {
            target: normalise_address(target),
            deadline: now + PAIRING_WINDOW,
        }
    }

    pub(crate) fn is_open(&self, now: Instant) -> bool {
        now < self.deadline
    }

    /// True when `now` is inside the window and `device` is the device the owner selected.
    /// Test-only: the live path resolves the window to one address before a [`Request`] is
    /// built, so nothing outside the tests needs this comparison.
    #[cfg(test)]
    pub(crate) fn covers(&self, device: &str, now: Instant) -> bool {
        self.is_open(now) && self.target == normalise_address(device)
    }
}

/// The adapter flag at start-up: no window, and the adapter is not pairable. Until the owner
/// asks for a pairing, nothing can pair with Plippa.
pub(crate) fn initialise_pairable(shared: &Shared, pairable: &dyn PairableControl) {
    *lock(&shared.window) = None;
    if let Err(error) = pairable.set_pairable(false) {
        eprintln!("plippa-accessory: could not close the adapter's pairing window: {error}");
    }
}

/// Open the window for `target` and make the adapter pairable for [`PAIRING_WINDOW`].
pub(crate) fn open_window(
    shared: &Shared,
    pairable: &dyn PairableControl,
    target: &str,
    now: Instant,
) {
    *lock(&shared.window) = Some(PairingWindow::open(target, now));
    if let Err(error) = pairable.set_pairable(true) {
        eprintln!("plippa-accessory: could not make the adapter pairable: {error}");
    }
    eprintln!(
        "plippa-accessory: pairing window open for {} s ({})",
        PAIRING_WINDOW.as_secs(),
        normalise_address(target)
    );
}

/// Close the window and the adapter flag. Safe to call when nothing is open.
pub(crate) fn close_window(shared: &Shared, pairable: &dyn PairableControl) {
    let had_window = lock(&shared.window).take().is_some();
    if let Err(error) = pairable.set_pairable(false) {
        eprintln!("plippa-accessory: could not stop the adapter pairing: {error}");
    }
    if had_window {
        eprintln!("plippa-accessory: pairing window closed");
    }
}

/// Close the window when its minute is up. Returns true when it was open and has just closed.
pub(crate) fn expire_window(shared: &Shared, pairable: &dyn PairableControl, now: Instant) -> bool {
    let expired = lock(&shared.window)
        .as_ref()
        .is_some_and(|window| !window.is_open(now));
    if expired {
        close_window(shared, pairable);
    }
    expired
}

/// Which kind of answer the outstanding prompt accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptKind {
    Confirm,
    EnterPasskey,
    EnterPin,
    Authorise,
}

impl PromptKind {
    pub(crate) fn accepts(self, answer: &PromptAnswer) -> bool {
        matches!(
            (self, answer),
            (
                PromptKind::Confirm | PromptKind::Authorise,
                PromptAnswer::Accept | PromptAnswer::Reject
            ) | (PromptKind::EnterPasskey, PromptAnswer::Passkey(_))
                | (PromptKind::EnterPin, PromptAnswer::Pin(_))
        )
    }
}

/// The prompt the UI must answer before the service's call can return.
pub(crate) struct PendingPrompt {
    pub(crate) key: u64,
    pub(crate) kind: PromptKind,
    pub(crate) request: Arc<PromptRequest>,
}

struct PromptState {
    done: bool,
    answer: Option<PromptAnswer>,
    waker: Option<Waker>,
}

/// One prompt's answer slot. Resolved exactly once by the UI, the timeout thread, or a
/// replacement prompt.
pub(crate) struct PromptRequest {
    state: Mutex<PromptState>,
}

impl PromptRequest {
    fn new() -> Self {
        Self {
            state: Mutex::new(PromptState {
                done: false,
                answer: None,
                waker: None,
            }),
        }
    }

    pub(crate) fn resolve(&self, answer: Option<PromptAnswer>) {
        let mut state = lock_prompt(&self.state);
        if state.done {
            return;
        }
        state.done = true;
        state.answer = answer;
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }

    fn future(self: Arc<Self>) -> AnswerFuture {
        AnswerFuture { request: self }
    }
}

struct AnswerFuture {
    request: Arc<PromptRequest>,
}

impl Future for AnswerFuture {
    type Output = Option<PromptAnswer>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = lock_prompt(&self.request.state);
        if state.done {
            Poll::Ready(state.answer.take())
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

fn lock_prompt(state: &Mutex<PromptState>) -> MutexGuard<'_, PromptState> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A device the cache knows, as the policy and the prompt need it.
struct KnownDevice {
    id: AccessoryId,
    peer: Peer,
    paired: bool,
    trusted: bool,
}

/// The exported pairing-agent object.
pub(crate) struct Agent {
    shared: Arc<Shared>,
}

impl Agent {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    /// Resolve the device path the service named. `None` means we have never seen it, which is
    /// always a rejection: an unknown device has no pairing and no allowed service.
    fn known(&self, device: &OwnedObjectPath) -> Option<KnownDevice> {
        let cache = lock(&self.shared.cache);
        let id = cache.device_id_by_path(device)?;
        let record = cache.device(&id)?;
        Some(KnownDevice {
            peer: Peer::new(Some(record.accessory.name.clone()), &id),
            paired: record.accessory.paired,
            trusted: record.accessory.trusted,
            id,
        })
    }

    /// The device whose window is open right now, if any (an expired window counts as closed).
    fn window_target(&self, now: Instant) -> Option<AccessoryId> {
        let window = lock(&self.shared.window);
        window
            .as_ref()
            .filter(|window| window.is_open(now))
            .map(|window| window.target.clone())
    }

    /// Apply the policy to one request.
    ///
    /// `Ok(Some(peer))` — publish a prompt and wait for the owner;
    /// `Ok(None)` — answer yes without asking;
    /// `Err(Rejected)` — answer no without asking and publish nothing.
    pub(crate) fn gate(
        &self,
        device: &OwnedObjectPath,
        kind: RequestKind,
        service_uuid: Option<&str>,
    ) -> Result<Option<Peer>, AgentError> {
        let Some(known) = self.known(device) else {
            eprintln!(
                "plippa-accessory: refused a {:?} request from an unknown device",
                kind
            );
            return Err(AgentError::Rejected);
        };
        let window = self.window_target(Instant::now());
        let decision = decide(&Request {
            kind,
            device: &known.id,
            paired: known.paired,
            trusted: known.trusted,
            window: window.as_deref(),
            service_uuid,
        });
        match decision {
            Decision::Prompt => Ok(Some(known.peer)),
            Decision::Allow => {
                if kind == RequestKind::AuthorizeService {
                    eprintln!(
                        "plippa-accessory: allowed a service for the paired device {}",
                        known.peer.address
                    );
                }
                Ok(None)
            }
            Decision::Reject => {
                eprintln!(
                    "plippa-accessory: refused a {:?} request for {} (no owner-initiated pairing)",
                    kind, known.peer.address
                );
                Err(AgentError::Rejected)
            }
        }
    }

    /// The service cancelled the exchange: clear any outstanding prompt and let the waiting
    /// agent call resolve as cancelled. Idempotent and safe when no prompt is pending.
    pub(crate) fn cancel_pending(&self) {
        let pending = lock(&self.shared.pending).take();
        if let Some(prompt) = pending {
            prompt.request.resolve(None);
        }
    }

    pub(crate) async fn ask(&self, kind: PromptKind, prompt: PairPrompt) -> Option<PromptAnswer> {
        let key = self.shared.next_prompt.fetch_add(1, Ordering::SeqCst);
        let request = Arc::new(PromptRequest::new());

        let previous = {
            let mut pending = lock(&self.shared.pending);
            pending.replace(PendingPrompt {
                key,
                kind,
                request: request.clone(),
            })
        };
        if let Some(previous) = previous {
            previous.request.resolve(None);
        }
        self.shared.push(AccessoryEvent::Prompt(prompt));

        let timeout_shared = self.shared.clone();
        let timeout_request = request.clone();
        let spawned = std::thread::Builder::new()
            .name("plippa-accessory-prompt".to_string())
            .spawn(move || {
                std::thread::sleep(PROMPT_TIMEOUT);
                let still_pending = {
                    let mut pending = lock(&timeout_shared.pending);
                    let matches = pending.as_ref().is_some_and(|prompt| prompt.key == key);
                    if matches {
                        *pending = None;
                    }
                    matches
                };
                if still_pending {
                    timeout_request.resolve(None);
                }
            });
        if spawned.is_err() {
            let mut pending = lock(&self.shared.pending);
            if pending.as_ref().is_some_and(|prompt| prompt.key == key) {
                *pending = None;
            }
            request.resolve(None);
        }

        request.future().await
    }
}

/// Register the exported agent with the service, then ask to be the default agent.
pub(crate) fn register_agent(
    connection: &zbus::blocking::Connection,
    shared: Arc<Shared>,
) -> Result<(), super::AccessoryError> {
    connection
        .object_server()
        .at(bus_names::AGENT_PATH, Agent::new(shared))
        .map_err(|error| {
            eprintln!("plippa-accessory: export agent object failed: {error}");
            super::errors::map_error(&error, bus_names::AGENT_PATH)
        })?;

    let manager = zbus::blocking::Proxy::new(
        connection,
        bus_names::SERVICE,
        bus_names::OBJECT_TREE,
        bus_names::AGENT_MANAGER,
    )
    .map_err(|error| {
        eprintln!("plippa-accessory: agent manager proxy failed: {error}");
        super::errors::map_error(&error, bus_names::AGENT_PATH)
    })?;
    let agent_path = zbus::zvariant::ObjectPath::try_from(bus_names::AGENT_PATH)
        .map_err(|_| super::AccessoryError::Unavailable)?;
    let _: () = manager
        .call(
            bus_names::REGISTER_AGENT,
            &(agent_path.clone(), bus_names::CAPABILITY),
        )
        .map_err(|error| {
            eprintln!("plippa-accessory: RegisterAgent failed: {error}");
            super::errors::map_error(&error, bus_names::AGENT_PATH)
        })?;
    let _: () = manager
        .call(bus_names::REQUEST_DEFAULT_AGENT, &(agent_path,))
        .map_err(|error| {
            eprintln!("plippa-accessory: RequestDefaultAgent failed: {error}");
            super::errors::map_error(&error, bus_names::AGENT_PATH)
        })?;
    eprintln!("plippa-accessory: pairing agent registered and default (owner-initiated only)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessory::canonical_address;

    #[test]
    fn cancel_clears_the_pending_prompt_and_resolves_it_as_cancelled() {
        let shared = Arc::new(Shared::default());
        let request = Arc::new(PromptRequest::new());
        *lock(&shared.pending) = Some(PendingPrompt {
            key: 7,
            kind: PromptKind::Confirm,
            request: request.clone(),
        });

        Agent::new(shared.clone()).cancel_pending();

        assert!(lock(&shared.pending).is_none());
        assert!(lock_prompt(&request.state).done);
        assert_eq!(lock_prompt(&request.state).answer, None);
    }

    #[test]
    fn cancel_without_a_prompt_is_harmless() {
        let shared = Arc::new(Shared::default());
        Agent::new(shared.clone()).cancel_pending();
        assert!(lock(&shared.pending).is_none());
    }

    // ---- SYS-03 policy ----

    const ME: &str = "aa:bb:cc:dd:ee:01";
    const OTHER: &str = "aa:bb:cc:dd:ee:02";

    fn request(kind: RequestKind, device: &str) -> Request<'_> {
        Request {
            kind,
            device,
            paired: false,
            trusted: false,
            window: None,
            service_uuid: None,
        }
    }

    /// Every request kind, with the window closed: nothing is prompted, nothing is allowed.
    #[test]
    fn no_pairing_window_means_every_request_is_rejected_without_a_prompt() {
        for kind in [
            RequestKind::Confirm,
            RequestKind::Passkey,
            RequestKind::Pin,
            RequestKind::Authorize,
            RequestKind::AuthorizeService,
        ] {
            let mut req = request(kind, ME);
            req.service_uuid = Some("0000110e-0000-1000-8000-00805f9b34fb");
            assert_eq!(decide(&req), Decision::Reject, "{kind:?} with no window");
        }
        // An unsolicited request from a device that is paired is still a reject: the window is
        // what decides, not how well we know the device.
        let mut paired = request(RequestKind::Confirm, ME);
        paired.paired = true;
        paired.trusted = true;
        assert_eq!(decide(&paired), Decision::Reject);
    }

    #[test]
    fn a_request_for_another_device_during_the_window_is_rejected() {
        for kind in [
            RequestKind::Confirm,
            RequestKind::Passkey,
            RequestKind::Pin,
            RequestKind::Authorize,
        ] {
            let mut req = request(kind, OTHER);
            req.window = Some(ME);
            assert_eq!(decide(&req), Decision::Reject, "{kind:?} for another device");
        }
    }

    #[test]
    fn a_matching_request_prompts_and_carries_name_and_address() {
        for kind in [
            RequestKind::Confirm,
            RequestKind::Passkey,
            RequestKind::Pin,
            RequestKind::Authorize,
        ] {
            let mut req = request(kind, ME);
            req.window = Some(ME);
            assert_eq!(decide(&req), Decision::Prompt, "{kind:?} in the window");
        }
        let peer = Peer::new(Some("Kitchen".into()), ME);
        assert_eq!(peer.address, canonical_address(ME));
        assert_eq!(peer.label(), "Kitchen  AA:BB:CC:DD:EE:01");
        // A name that is really the address is not a name: the address alone is shown.
        let nameless = Peer::new(Some("aa-bb-cc-dd-ee-01".into()), ME);
        assert_eq!(nameless.name, None);
        assert_eq!(nameless.label(), "AA:BB:CC:DD:EE:01");
    }

    #[test]
    fn an_unpaired_device_cannot_authorize_a_service() {
        let mut req = request(RequestKind::AuthorizeService, ME);
        req.service_uuid = Some("0000110e-0000-1000-8000-00805f9b34fb");
        assert_eq!(decide(&req), Decision::Reject);
        // Not even with the owner's window open: authorising a service is not pairing.
        req.window = Some(ME);
        assert_eq!(decide(&req), Decision::Reject);
    }

    #[test]
    fn a_paired_device_is_authorised_only_for_allow_listed_services() {
        for uuid in [
            "0000110c-0000-1000-8000-00805f9b34fb",
            "0000110e-0000-1000-8000-00805f9b34fb",
            "0000110f-0000-1000-8000-00805f9b34fb",
            "110e",
            "0x110C",
        ] {
            let mut req = request(RequestKind::AuthorizeService, ME);
            req.paired = true;
            req.service_uuid = Some(uuid);
            assert_eq!(decide(&req), Decision::Allow, "AVRCP uuid {uuid}");
        }
        for uuid in [
            "0000110b-0000-1000-8000-00805f9b34fb", // audio sink
            "00001108-0000-1000-8000-00805f9b34fb", // headset
            "0000111e-0000-1000-8000-00805f9b34fb", // hands-free
            "6e400001-b5a3-f393-e0a9-e50e24dcca9e", // vendor service
            "",
        ] {
            let mut req = request(RequestKind::AuthorizeService, ME);
            req.paired = true;
            req.service_uuid = Some(uuid);
            assert_eq!(decide(&req), Decision::Reject, "uuid {uuid:?}");
        }
        // Trusted counts like paired; a trusted device on an allowed service is fine.
        let mut trusted = request(RequestKind::AuthorizeService, ME);
        trusted.trusted = true;
        trusted.service_uuid = Some("110e");
        assert_eq!(decide(&trusted), Decision::Allow);
    }

    #[test]
    fn a_paired_device_reauthorises_without_a_prompt_on_reconnect() {
        let mut req = request(RequestKind::Authorize, ME);
        req.paired = true;
        assert_eq!(decide(&req), Decision::Allow);
        req.paired = false;
        req.trusted = true;
        assert_eq!(decide(&req), Decision::Allow);
    }

    /// A recorded adapter: the window rules are tested without a bus connection.
    #[derive(Default)]
    struct RecordingPairable {
        calls: Mutex<Vec<bool>>,
    }

    impl RecordingPairable {
        fn calls(&self) -> Vec<bool> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PairableControl for RecordingPairable {
        fn set_pairable(&self, pairable: bool) -> Result<(), String> {
            self.calls.lock().unwrap().push(pairable);
            Ok(())
        }
    }

    #[test]
    fn pairable_is_false_at_start_up_true_in_the_window_and_false_after_every_exit() {
        let shared = Shared::default();
        let adapter = RecordingPairable::default();
        // Start-up: nothing can pair with Plippa.
        initialise_pairable(&shared, &adapter);
        assert_eq!(adapter.calls(), vec![false]);
        assert!(lock(&shared.window).is_none());

        let now = Instant::now();
        // The owner picks a device: the window opens for that device only.
        open_window(&shared, &adapter, ME, now);
        assert_eq!(adapter.calls(), vec![false, true]);
        let window = lock(&shared.window).clone().unwrap();
        assert!(window.covers(ME, now));
        assert!(!window.covers(OTHER, now));
        assert!(!window.covers(ME, now + PAIRING_WINDOW));

        // Pairing finished (or was cancelled, or the owner rejected the prompt): closed.
        close_window(&shared, &adapter);
        assert_eq!(adapter.calls(), vec![false, true, false]);
        assert!(lock(&shared.window).is_none());
        // Closing again is safe and does not leave the adapter pairable.
        close_window(&shared, &adapter);
        assert_eq!(adapter.calls().last().copied(), Some(false));
    }

    #[test]
    fn the_window_closes_after_its_timeout() {
        let shared = Shared::default();
        let adapter = RecordingPairable::default();
        let now = Instant::now();
        open_window(&shared, &adapter, ME, now);
        assert!(!expire_window(
            &shared,
            &adapter,
            now + PAIRING_WINDOW - Duration::from_secs(1)
        ));
        assert!(lock(&shared.window).is_some());
        assert!(expire_window(&shared, &adapter, now + PAIRING_WINDOW));
        assert!(lock(&shared.window).is_none());
        assert_eq!(adapter.calls().last().copied(), Some(false));
        // An expired window is no window at all.
        let target = lock(&shared.window).as_ref().map(|w| w.target.clone());
        let mut req = request(RequestKind::Confirm, ME);
        req.window = target.as_deref();
        assert_eq!(decide(&req), Decision::Reject);
        assert!(!expire_window(&shared, &adapter, now + PAIRING_WINDOW));
    }
}
