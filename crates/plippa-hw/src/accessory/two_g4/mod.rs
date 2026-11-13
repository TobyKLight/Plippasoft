//! 2.4 GHz accessory backend over the system D-Bus.
//!
//! One **radio thread** owns the bus connection and every blocking call. UI-facing methods
//! only queue [`Command`]s or read the cached snapshot; results arrive through
//! [`AccessoryBackend::poll`]. Pairing and connecting run on helper threads because the
//! service's calls block for the length of the exchange.
//!
//! Nothing here blocks the caller. The cache is updated by the object-manager and property
//! signals; `adapters()`/`known()` never make a round trip.
//!
//! Thread lifetime: the radio and signal threads are detached from their join handles. Every
//! thread owns an `Arc<Shared>` and any bus state it needs, so none borrows from
//! [`TwoG4Backend`] and a bus call that outlives the backend is safe. [`TwoG4Backend::drop`]
//! asks the radio thread to stop but deliberately does not join it: it may be inside a
//! blocking service call, and waiting there would block the UI teardown path. The detached
//! helper threads (pair/connect and the prompt timeout) follow the same rule.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

mod agent;
pub(crate) mod errors;
pub(crate) mod names;
mod parse;
mod player;

use super::policy::MAX_SCAN_SECONDS;
use super::{
    normalise_address, Accessory, AccessoryBackend, AccessoryError, AccessoryEvent, AccessoryId,
    AdapterInfo, PromptAnswer,
};
use agent::PendingPrompt;
use names::bus_names;
use parse::ManagedObjects;
use player::MediaPlayer;

const COMMAND_TICK: Duration = Duration::from_millis(100);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Overall event queue bound. Critical events are never dropped, so the queue may exceed this
/// only under a pathological critical-event flood.
const EVENT_QUEUE_CAP: usize = 512;
/// Discovered/ScanFinished are informational; keep the most recent few and drop older chatter.
const CHATTER_QUEUE_CAP: usize = 64;

type InterfaceMap = HashMap<String, HashMap<String, OwnedValue>>;

struct AdapterRecord {
    path: OwnedObjectPath,
    info: AdapterInfo,
}

struct DeviceRecord {
    path: OwnedObjectPath,
    adapter_path: OwnedObjectPath,
    accessory: Accessory,
}

#[derive(Default)]
struct Cache {
    adapters: Vec<AdapterRecord>,
    devices: BTreeMap<AccessoryId, DeviceRecord>,
}

impl Cache {
    fn clear(&mut self) {
        self.adapters.clear();
        self.devices.clear();
    }

    fn accessories(&self) -> Vec<Accessory> {
        self.devices
            .values()
            .map(|record| record.accessory.clone())
            .collect()
    }

    fn adapter(&self, id: &str) -> Option<&AdapterRecord> {
        self.adapters.iter().find(|adapter| adapter.info.id == id)
    }

    fn device(&self, id: &str) -> Option<&DeviceRecord> {
        self.devices.get(id)
    }

    fn device_id_by_path(&self, path: &OwnedObjectPath) -> Option<AccessoryId> {
        self.devices
            .iter()
            .find(|(_, record)| &record.path == path)
            .map(|(id, _)| id.clone())
    }
}

struct Shared {
    events: Mutex<VecDeque<AccessoryEvent>>,
    cache: Mutex<Cache>,
    pending: Mutex<Option<PendingPrompt>>,
    /// The owner-initiated pairing window (SYS-03): one device, one minute. Empty at start-up
    /// and after every exit path; nothing prompts while it is empty.
    window: Mutex<Option<agent::PairingWindow>>,
    next_prompt: AtomicU64,
    available: AtomicBool,
    pairing_available: AtomicBool,
    pairing_busy: AtomicBool,
    unavailable_reported: AtomicBool,
    adapters_dirty: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            cache: Mutex::new(Cache::default()),
            pending: Mutex::new(None),
            window: Mutex::new(None),
            next_prompt: AtomicU64::new(0),
            available: AtomicBool::new(false),
            pairing_available: AtomicBool::new(false),
            pairing_busy: AtomicBool::new(false),
            unavailable_reported: AtomicBool::new(false),
            adapters_dirty: AtomicBool::new(false),
        }
    }
}

impl Shared {
    /// Queue an event with a bounded, coalescing policy.
    ///
    /// `Prompt`, `Paired`, `Connected`, `Disconnected`, `Transport` and `Failed` are never
    /// dropped: losing them would leave the UI believing a pairing/connection is still
    /// pending, drop a button press, or never report an outage. `Adapters` is coalesced to the
    /// newest snapshot and the informational `Discovered`/`ScanFinished` chatter is capped.
    fn push(&self, event: AccessoryEvent) {
        let mut events = lock(&self.events);
        match &event {
            AccessoryEvent::Adapters(_) => {
                if let Some(slot) = events
                    .iter_mut()
                    .find(|queued| matches!(queued, AccessoryEvent::Adapters(_)))
                {
                    *slot = event;
                } else {
                    events.push_back(event);
                }
            }
            AccessoryEvent::Discovered { id, .. } => {
                events.retain(|queued| {
                    !matches!(
                        queued,
                        AccessoryEvent::Discovered { id: queued_id, .. } if queued_id == id
                    )
                });
                if chatter_len(&events) < CHATTER_QUEUE_CAP {
                    events.push_back(event);
                }
            }
            AccessoryEvent::ScanFinished => {
                if chatter_len(&events) < CHATTER_QUEUE_CAP {
                    events.push_back(event);
                }
            }
            _ => events.push_back(event),
        }
        trim_queue(&mut events);
    }

    fn push_failed(&self, error: &AccessoryError) {
        self.push(AccessoryEvent::Failed(errors::event_text(error).to_string()));
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
}

fn is_critical(event: &AccessoryEvent) -> bool {
    matches!(
        event,
        AccessoryEvent::Prompt(_)
            | AccessoryEvent::Paired { .. }
            | AccessoryEvent::Connected(_)
            | AccessoryEvent::Disconnected(_)
            | AccessoryEvent::Transport(_)
            | AccessoryEvent::Failed(_)
    )
}

fn chatter_len(events: &VecDeque<AccessoryEvent>) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, AccessoryEvent::Discovered { .. } | AccessoryEvent::ScanFinished))
        .count()
}

fn trim_queue(events: &mut VecDeque<AccessoryEvent>) {
    while events.len() > EVENT_QUEUE_CAP {
        let Some(position) = events.iter().position(|event| !is_critical(event)) else {
            break;
        };
        events.remove(position);
    }
}

/// A lock that never panics on a poisoned mutex: the thread that panicked is already gone and
/// the snapshot/event data is still usable.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum Command {
    PowerOn(String),
    StartScan(u32),
    StopScan,
    Pair(AccessoryId),
    Connect(AccessoryId),
    Disconnect(AccessoryId),
    SetTrusted(AccessoryId, bool),
    Forget(AccessoryId),
    Shutdown,
}

enum SignalEvent {
    PropertiesChanged {
        path: OwnedObjectPath,
        interface: String,
        changed: HashMap<String, OwnedValue>,
        invalidated: Vec<String>,
    },
    InterfacesAdded {
        path: OwnedObjectPath,
        interfaces: InterfaceMap,
    },
    InterfacesRemoved {
        path: OwnedObjectPath,
        interfaces: Vec<String>,
    },
    Closed,
}

struct ScanState {
    deadline: Instant,
    seen: BTreeMap<AccessoryId, String>,
}

/// Real 2.4 GHz accessory backend.
///
/// Construction starts the radio thread and returns immediately. Until the bus and the radio
/// service are both reachable every method returns [`AccessoryError::Unavailable`], exactly
/// like [`super::MockAccessoryBackend::unavailable`].
pub struct TwoG4Backend {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Default for TwoG4Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl TwoG4Backend {
    pub fn new() -> Self {
        let shared = Arc::new(Shared::default());
        let (commands, receiver) = mpsc::channel();
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("plippa-accessory-radio".to_string())
            .spawn(move || run_radio(worker_shared, receiver))
            .ok();
        Self {
            shared,
            commands,
            worker,
        }
    }

    fn send(&self, command: Command) -> Result<(), AccessoryError> {
        self.commands
            .send(command)
            .map_err(|_| AccessoryError::Unavailable)
    }

    fn require_available(&self) -> Result<(), AccessoryError> {
        if self.shared.is_available() {
            Ok(())
        } else {
            Err(AccessoryError::Unavailable)
        }
    }

    fn cached_device(&self, id: &str) -> Option<Accessory> {
        let id = normalise_address(id);
        lock(&self.shared.cache)
            .device(&id)
            .map(|record| record.accessory.clone())
    }
}

impl Drop for TwoG4Backend {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        // Deliberately detach instead of joining: the radio thread may be inside a blocking
        // service call, and Drop runs on the UI worker thread. It owns an Arc<Shared> and its
        // own connection, so it can finish (or observe Shutdown on the next tick) without any
        // borrowed state; the OS reclaims detached threads at process exit.
        let _ = self.worker.take();
    }
}

impl AccessoryBackend for TwoG4Backend {
    fn adapters(&mut self) -> Result<Vec<AdapterInfo>, AccessoryError> {
        self.require_available()?;
        Ok(lock(&self.shared.cache)
            .adapters
            .iter()
            .map(|adapter| adapter.info.clone())
            .collect())
    }

    fn known(&mut self) -> Result<Vec<Accessory>, AccessoryError> {
        self.require_available()?;
        Ok(lock(&self.shared.cache).accessories())
    }

    fn power_on(&mut self, adapter: &str) -> Result<(), AccessoryError> {
        self.require_available()?;
        {
            let cache = lock(&self.shared.cache);
            if cache.adapters.is_empty() {
                return Err(AccessoryError::NoAdapter);
            }
            let Some(found) = cache.adapter(adapter) else {
                return Err(AccessoryError::Unknown(adapter.to_string()));
            };
            if found.info.powered {
                return Ok(());
            }
        }
        self.send(Command::PowerOn(adapter.to_string()))
    }

    fn start_scan(&mut self, seconds: u32) -> Result<(), AccessoryError> {
        self.require_available()?;
        self.send(Command::StartScan(seconds.min(MAX_SCAN_SECONDS)))
    }

    fn stop_scan(&mut self) -> Result<(), AccessoryError> {
        self.require_available()?;
        self.send(Command::StopScan)
    }

    fn pair(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.require_available()?;
        if !self.shared.pairing_available.load(Ordering::SeqCst) {
            eprintln!("plippa-accessory: pair rejected: pairing agent is unavailable");
            return Err(AccessoryError::Refused("pairing unavailable".to_string()));
        }
        let id = normalise_address(id);
        if lock(&self.shared.cache).device(&id).is_none() {
            eprintln!("plippa-accessory: pair rejected: {id} is not in the cache");
            return Err(AccessoryError::Unknown(id));
        }
        if self
            .shared
            .pairing_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            eprintln!("plippa-accessory: pair rejected: another pairing is in progress");
            return Err(AccessoryError::Refused(
                "pairing already in progress".to_string(),
            ));
        }
        if let Err(error) = self.send(Command::Pair(id)) {
            self.shared.pairing_busy.store(false, Ordering::SeqCst);
            return Err(error);
        }
        Ok(())
    }

    fn answer(&mut self, answer: PromptAnswer) -> Result<(), AccessoryError> {
        self.require_available()?;
        let mut pending = lock(&self.shared.pending);
        let Some(prompt) = pending.as_ref() else {
            return Err(AccessoryError::NoPairing);
        };
        if !prompt.kind.accepts(&answer) {
            return Err(AccessoryError::NoPairing);
        }
        let Some(prompt) = pending.take() else {
            return Err(AccessoryError::NoPairing);
        };
        prompt.request.resolve(Some(answer));
        Ok(())
    }

    fn connect(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.require_available()?;
        let id = normalise_address(id);
        let Some(accessory) = self.cached_device(&id) else {
            return Err(AccessoryError::Unknown(id));
        };
        if !accessory.paired {
            return Err(AccessoryError::Refused("not paired".to_string()));
        }
        if accessory.connected {
            return Ok(());
        }
        self.send(Command::Connect(id))
    }

    fn disconnect(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.require_available()?;
        let id = normalise_address(id);
        let Some(accessory) = self.cached_device(&id) else {
            return Err(AccessoryError::Unknown(id));
        };
        if !accessory.connected {
            return Ok(());
        }
        self.send(Command::Disconnect(id))
    }

    fn set_trusted(&mut self, id: &AccessoryId, trusted: bool) -> Result<(), AccessoryError> {
        self.require_available()?;
        let id = normalise_address(id);
        if lock(&self.shared.cache).device(&id).is_none() {
            return Err(AccessoryError::Unknown(id));
        }
        self.send(Command::SetTrusted(id, trusted))
    }

    fn forget(&mut self, id: &AccessoryId) -> Result<(), AccessoryError> {
        self.require_available()?;
        let id = normalise_address(id);
        if lock(&self.shared.cache).device(&id).is_none() {
            return Err(AccessoryError::Unknown(id));
        }
        self.send(Command::Forget(id))
    }

    fn poll(&mut self) -> Vec<AccessoryEvent> {
        let mut events: Vec<AccessoryEvent> = lock(&self.shared.events).drain(..).collect();
        if self.shared.adapters_dirty.swap(false, Ordering::SeqCst) {
            let adapters: Vec<AdapterInfo> = lock(&self.shared.cache)
                .adapters
                .iter()
                .map(|adapter| adapter.info.clone())
                .collect();
            let event = AccessoryEvent::Adapters(adapters);
            if let Some(slot) = events
                .iter_mut()
                .find(|queued| matches!(queued, AccessoryEvent::Adapters(_)))
            {
                *slot = event;
            } else {
                events.push(event);
            }
        }
        events
    }
}

fn run_radio(shared: Arc<Shared>, commands: Receiver<Command>) {
    loop {
        match setup(&shared) {
            Some((connection, signals)) => {
                shared.available.store(true, Ordering::SeqCst);
                shared.unavailable_reported.store(false, Ordering::SeqCst);
                let lost = run_connected(&shared, &connection, signals, &commands);
                shared.available.store(false, Ordering::SeqCst);
                shared.pairing_available.store(false, Ordering::SeqCst);
                shared.pairing_busy.store(false, Ordering::SeqCst);
                let pending = lock(&shared.pending).take();
                if let Some(prompt) = pending {
                    prompt.request.resolve(None);
                }
                *lock(&shared.window) = None;
                lock(&shared.cache).clear();
                shared.adapters_dirty.store(false, Ordering::SeqCst);
                if lost {
                    report_unavailable(&shared);
                }
            }
            None => report_unavailable(&shared),
        }

        if wait_before_retry(&commands, &shared) {
            return;
        }
    }
}

fn report_unavailable(shared: &Shared) {
    if !shared.unavailable_reported.swap(true, Ordering::SeqCst) {
        shared.push(AccessoryEvent::Failed(
            errors::event_text(&AccessoryError::Unavailable).to_string(),
        ));
    }
}

/// Wait for the reconnect delay while still reacting to shutdown and commands.
fn wait_before_retry(commands: &Receiver<Command>, shared: &Shared) -> bool {
    let deadline = Instant::now() + RECONNECT_DELAY;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let wait = (deadline - now).min(COMMAND_TICK);
        match commands.recv_timeout(wait) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => return true,
            Ok(command) => reject_while_unavailable(shared, command),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Give a command an explicit outcome while we have no bus connection. A command must never
/// disappear silently: the UI is waiting for its `poll()` result.
fn reject_while_unavailable(shared: &Shared, command: Command) {
    match command {
        Command::Shutdown => {}
        Command::StartScan(_) | Command::StopScan => {
            shared.push(AccessoryEvent::ScanFinished);
        }
        Command::Pair(id) => {
            shared.pairing_busy.store(false, Ordering::SeqCst);
            shared.push(AccessoryEvent::Paired {
                id,
                ok: false,
                error: Some("unavailable".to_string()),
            });
        }
        Command::PowerOn(_)
        | Command::Connect(_)
        | Command::Disconnect(_)
        | Command::SetTrusted(_, _)
        | Command::Forget(_) => shared.push_failed(&AccessoryError::Unavailable),
    }
}

fn setup(shared: &Arc<Shared>) -> Option<(Connection, Receiver<SignalEvent>)> {
    let connection = Connection::system().ok()?;
    if !radio_service_present(&connection) {
        return None;
    }

    // Register the match rule before reading the object tree: anything that changes after the
    // tree is read is queued for the radio loop, and anything before it is already in the tree.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(bus_names::SERVICE)
        .ok()?
        .build();
    let iterator = MessageIterator::for_match_rule(rule, &connection, Some(256)).ok()?;
    let objects = fetch_objects(&connection).ok()?;
    rebuild_cache(shared, &objects);

    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("plippa-accessory-signals".to_string())
        .spawn(move || read_signals(iterator, sender))
        .ok()?;

    if let Some(adapter) = selected_adapter(shared) {
        if !adapter.info.powered {
            if let Err(error) = set_adapter_powered(&connection, &adapter.path, true) {
                shared.push_failed(&error);
            }
        }
        // SYS-03: nothing pairs with Plippa until the owner starts a pairing.
        agent::initialise_pairable(shared, &AdapterPairable::new(&connection, &adapter.path));
    } else {
        shared.push_failed(&AccessoryError::NoAdapter);
    }

    match agent::register_agent(&connection, shared.clone()) {
        Ok(()) => shared.pairing_available.store(true, Ordering::SeqCst),
        Err(_) => {
            shared.pairing_available.store(false, Ordering::SeqCst);
            shared.push(AccessoryEvent::Failed("pairing unavailable".to_string()));
        }
    }

    register_player(&connection, shared);

    shared.adapters_dirty.store(true, Ordering::SeqCst);
    Some((connection, receiver))
}

/// Export the transport player and register it with the selected adapter. Failure never stops
/// pairing or connecting: the accessory still works without its buttons.
fn register_player(connection: &Connection, shared: &Arc<Shared>) {
    let Some(adapter) = selected_adapter(shared) else {
        return;
    };
    if let Err(error) = connection
        .object_server()
        .at(bus_names::PLAYER_PATH, MediaPlayer::new(shared.clone()))
    {
        eprintln!("plippa-accessory: export player object failed: {error}");
        return;
    }
    let properties = match player_properties() {
        Ok(properties) => properties,
        Err(error) => {
            eprintln!("plippa-accessory: player properties failed: {error}");
            return;
        }
    };
    let result = (|| -> zbus::Result<()> {
        let media = Proxy::new(
            connection,
            bus_names::SERVICE,
            adapter.path.as_str(),
            bus_names::MEDIA1,
        )?;
        let player = zbus::zvariant::ObjectPath::try_from(bus_names::PLAYER_PATH)
            .map_err(zbus::Error::Variant)?;
        let _: () = media.call(bus_names::REGISTER_PLAYER, &(player, properties))?;
        Ok(())
    })();
    match result {
        Ok(()) => eprintln!(
            "plippa-accessory: media player registered on {}",
            adapter.info.id
        ),
        Err(error) => eprintln!("plippa-accessory: RegisterPlayer failed: {error}"),
    }
}

fn player_properties() -> zbus::zvariant::Result<HashMap<String, OwnedValue>> {
    let mut properties = HashMap::new();
    properties.insert(
        "Identity".to_string(),
        OwnedValue::try_from(Value::from("Plippa"))?,
    );
    properties.insert(
        "PlaybackStatus".to_string(),
        OwnedValue::try_from(Value::from("Stopped"))?,
    );
    properties.insert("Position".to_string(), OwnedValue::from(0i64));
    for key in [
        "CanPlay",
        "CanPause",
        "CanGoNext",
        "CanGoPrevious",
        "CanControl",
    ] {
        properties.insert(key.to_string(), OwnedValue::from(true));
    }
    Ok(properties)
}

fn radio_service_present(connection: &Connection) -> bool {
    let Ok(proxy) = Proxy::new(
        connection,
        bus_names::DBUS_SERVICE,
        bus_names::DBUS_PATH,
        bus_names::DBUS_INTERFACE,
    ) else {
        return false;
    };
    let owner: bool = proxy
        .call(bus_names::NAME_HAS_OWNER, &(bus_names::SERVICE,))
        .unwrap_or(false);
    owner
}

fn read_signals(iterator: MessageIterator, sender: Sender<SignalEvent>) {
    for message in iterator {
        let Ok(message) = message else {
            break;
        };
        if let Some(event) = parse_signal(&message) {
            if sender.send(event).is_err() {
                return;
            }
        }
    }
    let _ = sender.send(SignalEvent::Closed);
}

fn parse_signal(message: &zbus::Message) -> Option<SignalEvent> {
    let header = message.header();
    let interface = header.interface()?.as_str();
    let member = header.member()?.as_str();
    let path = OwnedObjectPath::try_from(header.path()?.as_str().to_string()).ok()?;
    match (interface, member) {
        (bus_names::PROPERTIES, bus_names::PROPERTIES_CHANGED) => {
            let (interface, changed, invalidated) = message
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .ok()?;
            Some(SignalEvent::PropertiesChanged {
                path,
                interface,
                changed,
                invalidated,
            })
        }
        (bus_names::OBJECT_MANAGER, bus_names::INTERFACES_ADDED) => {
            let (path, interfaces) = message
                .body()
                .deserialize::<(OwnedObjectPath, InterfaceMap)>()
                .ok()?;
            Some(SignalEvent::InterfacesAdded { path, interfaces })
        }
        (bus_names::OBJECT_MANAGER, bus_names::INTERFACES_REMOVED) => {
            let (path, interfaces) = message
                .body()
                .deserialize::<(OwnedObjectPath, Vec<String>)>()
                .ok()?;
            Some(SignalEvent::InterfacesRemoved { path, interfaces })
        }
        _ => None,
    }
}

fn run_connected(
    shared: &Arc<Shared>,
    connection: &Connection,
    signals: Receiver<SignalEvent>,
    commands: &Receiver<Command>,
) -> bool {
    let mut scan: Option<ScanState> = None;
    loop {
        while let Ok(signal) = signals.try_recv() {
            match signal {
                SignalEvent::Closed => return true,
                other => handle_signal(connection, shared, &mut scan, other),
            }
        }

        if let Some(state) = &scan {
            if Instant::now() >= state.deadline {
                end_scan(connection, shared, &mut scan);
            }
        }

        // The owner's pairing window lasts one minute, whatever the UI is doing.
        if let Some(pairable) = adapter_pairable(shared, connection) {
            agent::expire_window(shared, &pairable, Instant::now());
        }

        match commands.recv_timeout(COMMAND_TICK) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => return false,
            Ok(command) => handle_command(connection, shared, &mut scan, command),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn handle_command(
    connection: &Connection,
    shared: &Arc<Shared>,
    scan: &mut Option<ScanState>,
    command: Command,
) {
    match command {
        Command::Shutdown => {}
        Command::PowerOn(adapter_id) => {
            let adapter = lock(&shared.cache)
                .adapter(&adapter_id)
                .map(|adapter| (adapter.path.clone(), adapter.info.clone()));
            match adapter {
                Some((path, _)) => {
                    if let Err(error) = set_adapter_powered(connection, &path, true) {
                        shared.push_failed(&error);
                    } else {
                        mark_adapter_powered(shared, &path, true);
                    }
                }
                None => shared.push_failed(&AccessoryError::Unknown(adapter_id)),
            }
        }
        Command::StartScan(seconds) => start_scan(connection, shared, scan, seconds),
        Command::StopScan => end_scan(connection, shared, scan),
        Command::Pair(id) => {
            let device = lock(&shared.cache)
                .device(&id)
                .map(|record| record.path.clone());
            match device {
                Some(path) => {
                    // Owner-initiated: open the window *before* the call, so it covers both an
                    // outgoing pair and the device's own pairing request (owner answer 2), and
                    // so the agent accepts the prompt this call will produce.
                    let pairable = adapter_pairable(shared, connection);
                    match &pairable {
                        Some(pairable) => {
                            agent::open_window(shared, pairable, &id, Instant::now())
                        }
                        None => eprintln!(
                            "plippa-accessory: no adapter to open the pairing window on"
                        ),
                    }
                    let connection = connection.clone();
                    let worker_shared = shared.clone();
                    let spawn_id = id.clone();
                    let spawned = thread::Builder::new()
                        .name("plippa-accessory-pair".to_string())
                        .spawn(move || pair_worker(worker_shared, connection, id, path));
                    if spawned.is_err() {
                        if let Some(pairable) = &pairable {
                            agent::close_window(shared, pairable);
                        }
                        shared.pairing_busy.store(false, Ordering::SeqCst);
                        shared.push(AccessoryEvent::Paired {
                            id: spawn_id,
                            ok: false,
                            error: Some("failed".to_string()),
                        });
                    }
                }
                None => {
                    eprintln!("plippa-accessory: pair failed: {id} vanished from the cache");
                    shared.pairing_busy.store(false, Ordering::SeqCst);
                    shared.push(AccessoryEvent::Paired {
                        id,
                        ok: false,
                        error: Some("failed".to_string()),
                    });
                }
            }
        }
        Command::Connect(id) => spawn_device_call(connection, shared, id, true),
        Command::Disconnect(id) => spawn_device_call(connection, shared, id, false),
        Command::SetTrusted(id, trusted) => set_trusted(connection, shared, &id, trusted),
        Command::Forget(id) => forget(connection, shared, &id),
    }
}

fn spawn_device_call(connection: &Connection, shared: &Arc<Shared>, id: AccessoryId, connect: bool) {
    let path = lock(&shared.cache)
        .device(&id)
        .map(|record| record.path.clone());
    let Some(path) = path else {
        shared.push_failed(&AccessoryError::Unknown(id));
        return;
    };
    let connection = connection.clone();
    let worker_shared = shared.clone();
    let spawned = thread::Builder::new()
        .name("plippa-accessory-device".to_string())
        .spawn(move || device_worker(worker_shared, connection, id, path, connect));
    if spawned.is_err() {
        shared.push(AccessoryEvent::Failed(
            "accessory operation could not start".to_string(),
        ));
    }
}

fn pair_worker(
    shared: Arc<Shared>,
    connection: Connection,
    id: AccessoryId,
    path: OwnedObjectPath,
) {
    let result = (|| -> zbus::Result<()> {
        let proxy = Proxy::new(&connection, bus_names::SERVICE, path.as_str(), bus_names::DEVICE)?;
        let _: () = proxy.call(bus_names::PAIR, &())?;
        Ok(())
    })();

    let outcome = match result {
        Ok(()) => Some((true, None)),
        Err(error) => match errors::method_error(&error).and_then(|(name, _)| errors::pair_outcome(name)) {
            Some(errors::PairOutcome::Success) => Some((true, None)),
            Some(errors::PairOutcome::Failed(error)) => {
                Some((false, error.map(str::to_string)))
            }
            None => {
                let mapped = errors::map_error(&error, &id);
                eprintln!("plippa-accessory: pair {id} failed: {error}");
                shared.push_failed(&mapped);
                Some((false, Some("failed".to_string())))
            }
        },
    };

    if let Some((ok, error)) = outcome {
        if ok {
            if let Some(record) = lock(&shared.cache).devices.get_mut(&id) {
                record.accessory.paired = true;
            }
        }
        shared.push(AccessoryEvent::Paired { id, ok, error });
    }
    let pending = lock(&shared.pending).take();
    if let Some(prompt) = pending {
        prompt.request.resolve(None);
    }
    // Every pairing end — success, refusal, cancellation, the owner rejecting the prompt —
    // closes the window: the adapter only ever accepts pairing while the owner is asking.
    if let Some(pairable) = adapter_pairable(&shared, &connection) {
        agent::close_window(&shared, &pairable);
    }
    shared.pairing_busy.store(false, Ordering::SeqCst);
}

fn device_worker(
    shared: Arc<Shared>,
    connection: Connection,
    id: AccessoryId,
    path: OwnedObjectPath,
    connect: bool,
) {
    let result = (|| -> zbus::Result<()> {
        let proxy = Proxy::new(&connection, bus_names::SERVICE, path.as_str(), bus_names::DEVICE)?;
        let method = if connect {
            bus_names::CONNECT
        } else {
            bus_names::DISCONNECT
        };
        let _: () = proxy.call(method, &())?;
        Ok(())
    })();
    if let Err(error) = result {
        let ignorable = errors::method_error(&error).is_some_and(|(name, _)| {
            (connect && name == bus_names::ERR_ALREADY_CONNECTED)
                || (!connect && name == bus_names::ERR_NOT_CONNECTED)
        });
        if !ignorable {
            shared.push_failed(&errors::map_error(&error, &id));
        }
    }
}

fn set_trusted(connection: &Connection, shared: &Arc<Shared>, id: &str, trusted: bool) {
    let path = lock(&shared.cache)
        .device(id)
        .map(|record| record.path.clone());
    let Some(path) = path else {
        shared.push_failed(&AccessoryError::Unknown(id.to_string()));
        return;
    };
    let result = (|| -> zbus::Result<()> {
        let proxy = Proxy::new(connection, bus_names::SERVICE, path.as_str(), bus_names::DEVICE)?;
        proxy
            .set_property(bus_names::TRUSTED, trusted)
            .map_err(zbus::Error::from)
    })();
    match result {
        Ok(()) => {
            if let Some(record) = lock(&shared.cache).devices.get_mut(id) {
                record.accessory.trusted = trusted;
            }
        }
        Err(error) => shared.push_failed(&errors::map_error(&error, id)),
    }
}

fn forget(connection: &Connection, shared: &Arc<Shared>, id: &str) {
    let (adapter_path, device_path, was_connected) = {
        let cache = lock(&shared.cache);
        let Some(record) = cache.device(id) else {
            shared.push_failed(&AccessoryError::Unknown(id.to_string()));
            return;
        };
        (
            record.adapter_path.clone(),
            record.path.clone(),
            record.accessory.connected,
        )
    };
    let result = (|| -> zbus::Result<()> {
        let proxy = Proxy::new(
            connection,
            bus_names::SERVICE,
            adapter_path.as_str(),
            bus_names::ADAPTER,
        )?;
        let device = zbus::zvariant::ObjectPath::try_from(device_path.as_str().to_string())
            .map_err(zbus::Error::Variant)?;
        let _: () = proxy.call(bus_names::REMOVE_DEVICE, &(device,))?;
        Ok(())
    })();
    match result {
        Ok(()) => remove_device(shared, id, was_connected),
        Err(error) => {
            let missing = errors::method_error(&error)
                .is_some_and(|(name, _)| name == bus_names::ERR_DOES_NOT_EXIST);
            if missing {
                remove_device(shared, id, was_connected);
            } else {
                shared.push_failed(&errors::map_error(&error, id));
            }
        }
    }
}

fn remove_device(shared: &Shared, id: &str, was_connected: bool) {
    lock(&shared.cache).devices.remove(id);
    if was_connected {
        shared.push(AccessoryEvent::Disconnected(id.to_string()));
    }
}

fn start_scan(
    connection: &Connection,
    shared: &Arc<Shared>,
    scan: &mut Option<ScanState>,
    seconds: u32,
) {
    if scan.is_some() {
        stop_discovery(connection, shared);
        *scan = None;
    }
    let Some(adapter) = selected_adapter(shared) else {
        eprintln!("plippa-accessory: scan rejected: no adapter");
        shared.push_failed(&AccessoryError::NoAdapter);
        return;
    };
    let result = (|| -> zbus::Result<()> {
        let proxy = Proxy::new(
            connection,
            bus_names::SERVICE,
            adapter.path.as_str(),
            bus_names::ADAPTER,
        )?;
        let _: () = proxy.call(bus_names::START_DISCOVERY, &())?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("plippa-accessory: StartDiscovery failed: {error}");
        shared.push_failed(&errors::map_error(&error, &adapter.info.id));
        return;
    }

    let seconds = seconds.min(MAX_SCAN_SECONDS);
    *scan = Some(ScanState {
        deadline: Instant::now() + Duration::from_secs(u64::from(seconds)),
        seen: BTreeMap::new(),
    });
    // Do not seed Discovered from the cache: GetManagedObjects also lists remembered devices
    // that are out of range. A device is present for this scan only when the service reports
    // it on the device interface while discovery is active (InterfacesAdded or
    // PropertiesChanged) — those paths call `note_discovered`.
}

fn end_scan(connection: &Connection, shared: &Shared, scan: &mut Option<ScanState>) {
    if scan.take().is_none() {
        return;
    }
    stop_discovery(connection, shared);
    shared.push(AccessoryEvent::ScanFinished);
}

fn stop_discovery(connection: &Connection, shared: &Shared) {
    if let Some(adapter) = selected_adapter(shared) {
        if let Ok(proxy) = Proxy::new(
            connection,
            bus_names::SERVICE,
            adapter.path.as_str(),
            bus_names::ADAPTER,
        ) {
            let _: zbus::Result<()> = proxy.call(bus_names::STOP_DISCOVERY, &());
        }
    }
}

fn note_discovered(shared: &Shared, scan: &mut Option<ScanState>, accessory: &Accessory) {
    let Some(state) = scan.as_mut() else {
        return;
    };
    if let Some(previous) = state.seen.get(&accessory.id) {
        if previous == &accessory.name {
            return;
        }
    }
    state
        .seen
        .insert(accessory.id.clone(), accessory.name.clone());
    shared.push(AccessoryEvent::Discovered {
        id: accessory.id.clone(),
        name: accessory.name.clone(),
        kind: accessory.kind,
    });
}

fn handle_signal(
    connection: &Connection,
    shared: &Arc<Shared>,
    scan: &mut Option<ScanState>,
    signal: SignalEvent,
) {
    match signal {
        SignalEvent::PropertiesChanged {
            path,
            interface,
            changed,
            invalidated,
        } => {
            let structural = !invalidated.is_empty()
                || (interface == bus_names::DEVICE
                    && (changed.contains_key(bus_names::UUIDS)
                        || changed.contains_key(bus_names::CLASS)));
            if structural {
                refresh_and_reconcile(connection, shared, scan, &path, &changed, &interface);
                return;
            }
            match interface.as_str() {
                bus_names::ADAPTER => {
                    apply_adapter_properties(shared, &path, &changed);
                }
                bus_names::DEVICE => {
                    apply_device_properties(connection, shared, scan, &path, &changed);
                }
                bus_names::BATTERY => {
                    apply_battery_properties(shared, &path, &changed);
                }
                _ => {}
            }
        }
        SignalEvent::InterfacesAdded { path, interfaces } => {
            handle_interfaces_added(shared, scan, path, interfaces);
        }
        SignalEvent::InterfacesRemoved { path, interfaces } => {
            handle_interfaces_removed(shared, scan, &path, &interfaces);
        }
        SignalEvent::Closed => {}
    }
}

fn apply_adapter_properties(
    shared: &Shared,
    path: &OwnedObjectPath,
    changed: &HashMap<String, OwnedValue>,
) {
    let mut cache = lock(&shared.cache);
    let Some(record) = cache.adapters.iter_mut().find(|adapter| &adapter.path == path) else {
        drop(cache);
        shared.adapters_dirty.store(true, Ordering::SeqCst);
        return;
    };
    if let Some(value) = changed.get(bus_names::POWERED) {
        if let Ok(powered) = bool::try_from(value) {
            record.info.powered = powered;
        }
    }
    if let Some(value) = changed.get(bus_names::ADDRESS) {
        if let Some(address) = parse::string_value(value) {
            record.info.address = normalise_address(&address);
        }
    }
    let mut name = None;
    if let Some(value) = changed.get(bus_names::ALIAS) {
        name = parse::string_value(value);
    }
    if name.is_none() {
        if let Some(value) = changed.get(bus_names::NAME) {
            name = parse::string_value(value);
        }
    }
    if let Some(name) = name {
        record.info.name = name;
    }
    drop(cache);
    shared.adapters_dirty.store(true, Ordering::SeqCst);
}

fn apply_device_properties(
    connection: &Connection,
    shared: &Arc<Shared>,
    scan: &mut Option<ScanState>,
    path: &OwnedObjectPath,
    changed: &HashMap<String, OwnedValue>,
) {
    let id = lock(&shared.cache).device_id_by_path(path);
    let Some(id) = id else {
        refresh_and_reconcile(connection, shared, scan, path, changed, bus_names::DEVICE);
        return;
    };

    let transition = {
        let mut cache = lock(&shared.cache);
        apply_device_scalars(&mut cache, &id, changed)
    };
    let Some((was_connected, updated)) = transition else {
        return;
    };
    emit_connection_transition(shared, was_connected, &updated);
    note_discovered(shared, scan, &updated);
}

/// Apply the scalar device properties from a `PropertiesChanged` batch. Structural changes
/// (`UUIDs`, `Class`) trigger a cache refresh elsewhere; this function still applies any
/// connection/name/flag values batched with them so nothing is lost.
fn apply_device_scalars(
    cache: &mut Cache,
    id: &str,
    changed: &HashMap<String, OwnedValue>,
) -> Option<(bool, Accessory)> {
    let record = cache.devices.get_mut(id)?;
    let was_connected = record.accessory.connected;
    if let Some(value) = changed.get(bus_names::PAIRED) {
        if let Ok(paired) = bool::try_from(value) {
            record.accessory.paired = paired;
        }
    }
    if let Some(value) = changed.get(bus_names::TRUSTED) {
        if let Ok(trusted) = bool::try_from(value) {
            record.accessory.trusted = trusted;
        }
    }
    if let Some(value) = changed.get(bus_names::CONNECTED) {
        if let Ok(connected) = bool::try_from(value) {
            record.accessory.connected = connected;
        }
    }
    let mut name = None;
    if let Some(value) = changed.get(bus_names::ALIAS) {
        name = parse::string_value(value);
    }
    if name.is_none() {
        if let Some(value) = changed.get(bus_names::NAME) {
            name = parse::string_value(value);
        }
    }
    if let Some(name) = name {
        record.accessory.name = name;
    }
    Some((was_connected, record.accessory.clone()))
}

fn emit_connection_transition(shared: &Shared, was_connected: bool, updated: &Accessory) {
    if was_connected != updated.connected {
        if updated.connected {
            shared.push(AccessoryEvent::Connected(updated.id.clone()));
        } else {
            shared.push(AccessoryEvent::Disconnected(updated.id.clone()));
        }
    }
}

fn connection_snapshot(shared: &Shared) -> BTreeMap<AccessoryId, bool> {
    lock(&shared.cache)
        .devices
        .iter()
        .map(|(id, record)| (id.clone(), record.accessory.connected))
        .collect()
}

/// Emit a `Connected`/`Disconnected` for every device whose connection state differs between
/// the snapshot and the current cache, plus a disconnect for a connected device that vanished.
fn reconcile_connections(shared: &Shared, previous: &BTreeMap<AccessoryId, bool>) {
    let current: Vec<(AccessoryId, bool)> = lock(&shared.cache)
        .devices
        .iter()
        .map(|(id, record)| (id.clone(), record.accessory.connected))
        .collect();
    for (id, connected) in &current {
        let was = previous.get(id).copied().unwrap_or(false);
        if was != *connected {
            if *connected {
                shared.push(AccessoryEvent::Connected(id.clone()));
            } else {
                shared.push(AccessoryEvent::Disconnected(id.clone()));
            }
        }
    }
    for (id, was_connected) in previous {
        if *was_connected && !current.iter().any(|(current_id, _)| current_id == id) {
            shared.push(AccessoryEvent::Disconnected(id.clone()));
        }
    }
}

/// A structural property change (or an invalidation) means the cached shape may be stale.
/// Refresh from the object tree, re-apply any scalar values from the same batch, then derive
/// connection transitions by comparing the old and new state rather than trusting that the
/// batch contained `Connected`.
fn refresh_and_reconcile(
    connection: &Connection,
    shared: &Arc<Shared>,
    scan: &mut Option<ScanState>,
    path: &OwnedObjectPath,
    changed: &HashMap<String, OwnedValue>,
    interface: &str,
) {
    let previous = connection_snapshot(shared);
    if refresh_cache(connection, shared).is_err() {
        return;
    }
    let refreshed = if interface == bus_names::DEVICE {
        let mut cache = lock(&shared.cache);
        cache
            .device_id_by_path(path)
            .and_then(|id| apply_device_scalars(&mut cache, &id, changed))
    } else {
        None
    };
    reconcile_connections(shared, &previous);
    if let Some((_, accessory)) = refreshed {
        note_discovered(shared, scan, &accessory);
    }
}

fn apply_battery_properties(
    shared: &Shared,
    path: &OwnedObjectPath,
    changed: &HashMap<String, OwnedValue>,
) {
    let Some(value) = changed.get(bus_names::PERCENTAGE) else {
        return;
    };
    let Some(percentage) = parse::percentage_value(value) else {
        return;
    };
    let id = lock(&shared.cache).device_id_by_path(path);
    if let Some(id) = id {
        if let Some(record) = lock(&shared.cache).devices.get_mut(&id) {
            record.accessory.battery_pct = Some(percentage);
        }
    }
}


fn handle_interfaces_added(
    shared: &Shared,
    scan: &mut Option<ScanState>,
    path: OwnedObjectPath,
    interfaces: InterfaceMap,
) {
    if interfaces.contains_key(bus_names::DEVICE) {
        let objects = BTreeMap::from([(path.clone().into_inner(), interfaces.clone())]);
        if let Some((_, mut accessory)) = parse::devices_from(&objects).into_iter().next() {
            let adapter_path = parent_path(&path);
            let mut cache = lock(&shared.cache);
            let was_connected = cache
                .devices
                .get(&accessory.id)
                .is_some_and(|record| record.accessory.connected);
            if accessory.battery_pct.is_none() {
                accessory.battery_pct = cache
                    .devices
                    .get(&accessory.id)
                    .and_then(|record| record.accessory.battery_pct);
            }
            cache.devices.insert(
                accessory.id.clone(),
                DeviceRecord {
                    path: path.clone(),
                    adapter_path,
                    accessory: accessory.clone(),
                },
            );
            drop(cache);
            emit_connection_transition(shared, was_connected, &accessory);
            note_discovered(shared, scan, &accessory);
        }
    }
    if interfaces.contains_key(bus_names::BATTERY) {
        if let Some(percentage) = interfaces
            .get(bus_names::BATTERY)
            .and_then(|battery| battery.get(bus_names::PERCENTAGE))
            .and_then(parse::percentage_value)
        {
            let id = lock(&shared.cache).device_id_by_path(&path);
            if let Some(id) = id {
                if let Some(record) = lock(&shared.cache).devices.get_mut(&id) {
                    record.accessory.battery_pct = Some(percentage);
                }
            }
        }
    }
    if interfaces.contains_key(bus_names::ADAPTER) {
        let objects = BTreeMap::from([(path.into_inner(), interfaces)]);
        if let Some((path, info)) = parse::adapters_from(&objects).into_iter().next() {
            let mut cache = lock(&shared.cache);
            if let Some(record) = cache
                .adapters
                .iter_mut()
                .find(|adapter| adapter.info.id == info.id)
            {
                record.path = path;
                record.info = info;
            } else {
                cache.adapters.push(AdapterRecord { path, info });
            }
            cache.adapters.sort_by(|a, b| a.info.id.cmp(&b.info.id));
            drop(cache);
            shared.adapters_dirty.store(true, Ordering::SeqCst);
        }
    }
}

fn handle_interfaces_removed(
    shared: &Shared,
    scan: &mut Option<ScanState>,
    path: &OwnedObjectPath,
    interfaces: &[String],
) {
    if interfaces.iter().any(|name| name == bus_names::DEVICE) {
        let removed = {
            let mut cache = lock(&shared.cache);
            if let Some(id) = cache.device_id_by_path(path) {
                let record = cache.devices.remove(&id);
                if let Some(state) = scan.as_mut() {
                    state.seen.remove(&id);
                }
                record.map(|record| (id, record.accessory.connected))
            } else {
                None
            }
        };
        if let Some((id, was_connected)) = removed {
            if was_connected {
                shared.push(AccessoryEvent::Disconnected(id));
            }
        }
    }
    if interfaces.iter().any(|name| name == bus_names::ADAPTER) {
        let mut cache = lock(&shared.cache);
        cache.adapters.retain(|adapter| &adapter.path != path);
        drop(cache);
        shared.adapters_dirty.store(true, Ordering::SeqCst);
    }
    if interfaces.iter().any(|name| name == bus_names::BATTERY) {
        let id = lock(&shared.cache).device_id_by_path(path);
        if let Some(id) = id {
            if let Some(record) = lock(&shared.cache).devices.get_mut(&id) {
                record.accessory.battery_pct = None;
            }
        }
    }
}

fn parent_path(path: &OwnedObjectPath) -> OwnedObjectPath {
    let text = path.as_str();
    let parent = text.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("/");
    OwnedObjectPath::try_from(parent.to_string()).unwrap_or_else(|_| path.clone())
}

fn selected_adapter(shared: &Shared) -> Option<AdapterRecord> {
    lock(&shared.cache)
        .adapters
        .first()
        .map(|adapter| AdapterRecord {
            path: adapter.path.clone(),
            info: adapter.info.clone(),
        })
}

fn mark_adapter_powered(shared: &Shared, path: &OwnedObjectPath, powered: bool) {
    let mut cache = lock(&shared.cache);
    if let Some(record) = cache.adapters.iter_mut().find(|adapter| &adapter.path == path) {
        record.info.powered = powered;
    }
    drop(cache);
    shared.adapters_dirty.store(true, Ordering::SeqCst);
}

/// The selected adapter, as the pairing window writes to it. The trait keeps the window rules
/// testable without a bus; this is the only implementation that talks D-Bus.
struct AdapterPairable<'a> {
    connection: &'a Connection,
    path: OwnedObjectPath,
}

impl<'a> AdapterPairable<'a> {
    fn new(connection: &'a Connection, path: &OwnedObjectPath) -> Self {
        Self {
            connection,
            path: path.clone(),
        }
    }
}

impl agent::PairableControl for AdapterPairable<'_> {
    fn set_pairable(&self, pairable: bool) -> Result<(), String> {
        let proxy = Proxy::new(
            self.connection,
            bus_names::SERVICE,
            self.path.as_str(),
            bus_names::ADAPTER,
        )
        .map_err(|error| error.to_string())?;
        if pairable {
            // The timeout goes on first: BlueZ's default for it is "never expires", so writing
            // Pairable=true before it (and then failing) would leave the adapter open until
            // something else closed it — including after this process has gone.
            let seconds: u32 = agent::PAIRING_WINDOW.as_secs() as u32;
            proxy
                .set_property(bus_names::PAIRABLE_TIMEOUT, seconds)
                .map_err(|error| error.to_string())?;
            proxy
                .set_property(bus_names::PAIRABLE, true)
                .map_err(|error| {
                    // Do not leave it pairable without the bound the write above was for.
                    let _ = proxy.set_property(bus_names::PAIRABLE, false);
                    error.to_string()
                })?;
        } else {
            proxy
                .set_property(bus_names::PAIRABLE, false)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

/// The adapter the window writes to, if there is one.
fn adapter_pairable<'a>(shared: &Shared, connection: &'a Connection) -> Option<AdapterPairable<'a>> {
    let adapter = selected_adapter(shared)?;
    Some(AdapterPairable::new(connection, &adapter.path))
}

fn set_adapter_powered(
    connection: &Connection,
    path: &OwnedObjectPath,
    powered: bool,
) -> Result<(), AccessoryError> {
    let proxy = Proxy::new(
        connection,
        bus_names::SERVICE,
        path.as_str(),
        bus_names::ADAPTER,
    )
    .map_err(|error| errors::map_error(&error, ""))?;
    proxy
        .set_property(bus_names::POWERED, powered)
        .map_err(|error| errors::map_error(&zbus::Error::from(error), ""))
}

fn refresh_cache(connection: &Connection, shared: &Shared) -> Result<(), AccessoryError> {
    let objects = fetch_objects(connection)?;
    rebuild_cache(shared, &objects);
    Ok(())
}

fn rebuild_cache(shared: &Shared, objects: &ManagedObjects) {
    let adapters: Vec<AdapterRecord> = parse::adapters_from(objects)
        .into_iter()
        .map(|(path, info)| AdapterRecord { path, info })
        .collect();
    let devices: BTreeMap<AccessoryId, DeviceRecord> = parse::devices_from(objects)
        .into_iter()
        .map(|(path, accessory)| {
            (
                accessory.id.clone(),
                DeviceRecord {
                    adapter_path: parent_path(&path),
                    path,
                    accessory,
                },
            )
        })
        .collect();
    {
        let mut cache = lock(&shared.cache);
        cache.adapters = adapters;
        cache.devices = devices;
    }
    shared.adapters_dirty.store(true, Ordering::SeqCst);
}

fn fetch_objects(connection: &Connection) -> Result<ManagedObjects, AccessoryError> {
    let manager = Proxy::new(
        connection,
        bus_names::SERVICE,
        bus_names::ROOT_PATH,
        bus_names::OBJECT_MANAGER,
    )
    .map_err(|error| errors::map_error(&error, ""))?;
    let objects: HashMap<OwnedObjectPath, InterfaceMap> = manager
        .call(bus_names::GET_MANAGED_OBJECTS, &())
        .map_err(|error| errors::map_error(&error, ""))?;
    Ok(objects
        .into_iter()
        .map(|(path, interfaces)| (path.into_inner(), interfaces))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::agent::PromptKind;
    use crate::accessory::{AccessoryKind, PairPrompt};
    use zbus::zvariant::Value;

    fn device_string(id: &str) -> OwnedValue {
        OwnedValue::try_from(Value::from(id)).expect("string value")
    }

    fn device_path(id: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(format!(
            "{}/hci0/dev_{}",
            bus_names::OBJECT_TREE,
            id.replace(':', "_").to_uppercase()
        ))
        .expect("object path")
    }

    fn managed_objects(ids: &[&str]) -> ManagedObjects {
        let mut objects = ManagedObjects::new();
        for id in ids {
            let mut props = HashMap::new();
            props.insert(bus_names::ADDRESS.to_string(), device_string(id));
            props.insert(bus_names::NAME.to_string(), device_string("device"));
            props.insert(bus_names::CONNECTED.to_string(), OwnedValue::from(false));
            objects.insert(
                device_path(id).into_inner(),
                HashMap::from([(bus_names::DEVICE.to_string(), props)]),
            );
        }
        objects
    }

    fn accessory(id: &str, connected: bool) -> Accessory {
        Accessory {
            id: id.to_string(),
            name: "device".to_string(),
            kind: AccessoryKind::Other,
            paired: true,
            trusted: false,
            connected,
            battery_pct: None,
        }
    }

    fn record(id: &str, connected: bool) -> DeviceRecord {
        DeviceRecord {
            path: device_path(id),
            adapter_path: OwnedObjectPath::try_from(format!("{}/hci0", bus_names::OBJECT_TREE))
                .expect("adapter path"),
            accessory: accessory(id, connected),
        }
    }

    #[test]
    fn prompt_kinds_only_accept_their_own_answers() {
        assert!(PromptKind::Confirm.accepts(&PromptAnswer::Accept));
        assert!(PromptKind::Confirm.accepts(&PromptAnswer::Reject));
        assert!(!PromptKind::Confirm.accepts(&PromptAnswer::Passkey(1)));
        assert!(PromptKind::EnterPasskey.accepts(&PromptAnswer::Passkey(1234)));
        assert!(!PromptKind::EnterPasskey.accepts(&PromptAnswer::Accept));
        assert!(PromptKind::EnterPin.accepts(&PromptAnswer::Pin("1234".to_string())));
        assert!(!PromptKind::EnterPin.accepts(&PromptAnswer::Passkey(1234)));
        assert!(PromptKind::Authorise.accepts(&PromptAnswer::Accept));
        assert!(!PromptKind::Authorise.accepts(&PromptAnswer::Pin("1".to_string())));
    }

    #[test]
    fn an_outage_reports_exactly_one_failure() {
        let shared = Shared::default();
        report_unavailable(&shared);
        report_unavailable(&shared);
        let failures = lock(&shared.events)
            .iter()
            .filter(|event| matches!(event, AccessoryEvent::Failed(_)))
            .count();
        assert_eq!(failures, 1);
    }

    #[test]
    fn removed_device_does_not_persist_in_known() {
        let shared = Shared::default();
        rebuild_cache(
            &shared,
            &managed_objects(&["aa:bb:cc:dd:ee:01", "aa:bb:cc:dd:ee:02"]),
        );
        assert_eq!(lock(&shared.cache).accessories().len(), 2);

        rebuild_cache(&shared, &managed_objects(&["aa:bb:cc:dd:ee:02"]));
        let known = lock(&shared.cache).accessories();
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].id, "aa:bb:cc:dd:ee:02");
    }

    #[test]
    fn a_uuid_batch_still_applies_a_batched_connected_transition() {
        let shared = Shared::default();
        let id = "aa:bb:cc:dd:ee:01";
        lock(&shared.cache)
            .devices
            .insert(id.to_string(), record(id, false));
        let previous = connection_snapshot(&shared);

        let mut changed = HashMap::new();
        changed.insert(bus_names::CONNECTED.to_string(), OwnedValue::from(true));
        changed.insert(bus_names::UUIDS.to_string(), OwnedValue::from(0u32));
        {
            let mut cache = lock(&shared.cache);
            let (was, updated) =
                apply_device_scalars(&mut cache, id, &changed).expect("device in cache");
            assert!(!was);
            assert!(updated.connected);
        }
        reconcile_connections(&shared, &previous);

        let events: Vec<AccessoryEvent> = lock(&shared.events).drain(..).collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AccessoryEvent::Connected(connected) if connected == id))
                .count(),
            1,
            "the batched connection transition must survive the structural refresh"
        );
    }

    #[test]
    fn commands_while_unavailable_always_produce_an_outcome() {
        let shared = Shared::default();
        let id = "aa:bb:cc:dd:ee:01".to_string();
        reject_while_unavailable(&shared, Command::Pair(id.clone()));
        reject_while_unavailable(&shared, Command::StartScan(5));
        reject_while_unavailable(&shared, Command::Connect(id));
        reject_while_unavailable(&shared, Command::Forget("aa:bb:cc:dd:ee:02".to_string()));

        let events: Vec<AccessoryEvent> = lock(&shared.events).drain(..).collect();
        assert!(matches!(
            events[0],
            AccessoryEvent::Paired { ok: false, .. }
        ));
        assert!(matches!(events[1], AccessoryEvent::ScanFinished));
        assert!(matches!(events[2], AccessoryEvent::Failed(_)));
        assert!(matches!(events[3], AccessoryEvent::Failed(_)));
        assert!(!shared.pairing_busy.load(Ordering::SeqCst));
    }

    #[test]
    fn queue_coalesces_adapters_caps_chatter_and_keeps_critical_events() {
        let shared = Shared::default();
        for index in 0..(CHATTER_QUEUE_CAP + 20) {
            shared.push(AccessoryEvent::Discovered {
                id: format!("id-{index}"),
                name: "name".to_string(),
                kind: AccessoryKind::Other,
            });
        }
        shared.push(AccessoryEvent::Adapters(Vec::new()));
        shared.push(AccessoryEvent::Adapters(vec![AdapterInfo {
            id: "hci0".to_string(),
            address: "11:22:33:44:55:66".to_string(),
            name: "radio".to_string(),
            powered: true,
        }]));
        shared.push(AccessoryEvent::Prompt(PairPrompt::Confirm {
            peer: crate::accessory::Peer::new(Some("device".into()), "aa:bb:cc:dd:ee:01"),
            passkey: 1234,
        }));
        shared.push(AccessoryEvent::Paired {
            id: "aa:bb:cc:dd:ee:01".to_string(),
            ok: false,
            error: None,
        });
        shared.push(AccessoryEvent::Connected("aa:bb:cc:dd:ee:01".to_string()));
        shared.push(AccessoryEvent::Disconnected("aa:bb:cc:dd:ee:01".to_string()));
        shared.push(AccessoryEvent::Failed("boom".to_string()));

        let events = lock(&shared.events);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AccessoryEvent::Adapters(_)))
                .count(),
            1,
            "repeated adapter snapshots are coalesced"
        );
        assert!(
            events
                .iter()
                .filter(|event| matches!(event, AccessoryEvent::Discovered { .. }))
                .count()
                <= CHATTER_QUEUE_CAP
        );
        for critical in [
            is_critical(&AccessoryEvent::Prompt(PairPrompt::Confirm {
                peer: crate::accessory::Peer::new(None, "aa:bb:cc:dd:ee:01"),
                passkey: 1,
            })),
            is_critical(&AccessoryEvent::Paired {
                id: String::new(),
                ok: false,
                error: None,
            }),
            is_critical(&AccessoryEvent::Connected(String::new())),
            is_critical(&AccessoryEvent::Disconnected(String::new())),
            is_critical(&AccessoryEvent::Transport(
                crate::accessory::TransportAction::Play,
            )),
            is_critical(&AccessoryEvent::Failed(String::new())),
        ] {
            assert!(critical);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            AccessoryEvent::Prompt(_)
                | AccessoryEvent::Paired { .. }
                | AccessoryEvent::Connected(_)
                | AccessoryEvent::Disconnected(_)
                | AccessoryEvent::Failed(_)
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, AccessoryEvent::Failed(_))));
        assert!(events
            .iter()
            .any(|event| matches!(event, AccessoryEvent::Paired { .. })));
    }

    #[test]
    fn trimming_the_queue_keeps_critical_events_over_chatter() {
        let mut queue: VecDeque<AccessoryEvent> =
            (0..(EVENT_QUEUE_CAP + 10)).map(|_| AccessoryEvent::ScanFinished).collect();
        queue.push_back(AccessoryEvent::Failed("outage".to_string()));
        trim_queue(&mut queue);
        assert_eq!(queue.len(), EVENT_QUEUE_CAP);
        assert!(queue
            .iter()
            .any(|event| matches!(event, AccessoryEvent::Failed(_))));
    }
}
