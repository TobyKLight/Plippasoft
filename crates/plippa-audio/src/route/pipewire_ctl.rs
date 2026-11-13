//! PipeWire output control client (AUDIO_02), speaking the PipeWire-Pulse protocol.
//!
//! # Blocking budget
//!
//! The engine hub ticks every 5 ms and also serves IPC, so nothing here may park it
//! for more than [`CALL_TIMEOUT`] (200 ms). All server work runs on one owned control
//! thread; the hub sends a request over a bounded channel and waits at most
//! [`CALL_TIMEOUT`]. Connection attempts happen on the control thread too, so a
//! missing audio server never stalls the hub; [`PipeWireControl::new`] waits at most
//! [`CONNECT_TIMEOUT`] (500 ms) for the first attempt.
//!
//! Playback itself stays on `libpulse-simple`; this module only enumerates sinks,
//! loads/unloads our routing modules, normalises sink volume and reports server
//! events (series decision 2 in `AUDIO-Oct26-packages-parallel-graph.md`).

use super::plan::{ops_for, ModuleHandle, ModuleTag, RouteOp};
use super::{OutputControl, RouteError, RouteEvent, SinkInfo};
use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet, State as ContextState};
use pulse::def::SinkState;
use pulse::mainloop::threaded::Mainloop;
use pulse::context::subscribe::{Facility, InterestMaskSet};
use pulse::volume::{ChannelVolumes, Volume};
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Longest the hub may wait for one request.
pub const CALL_TIMEOUT: Duration = Duration::from_millis(200);
/// Longest [`PipeWireControl::new`] waits for the first connection attempt.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// Retry period after a failed or dropped connection.
pub const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Budget handed to the control thread, leaving the hub a small margin.
const CALL_BUDGET: Duration = Duration::from_millis(190);
/// Longest one server operation may take on the control thread.
const OPERATION_TIMEOUT: Duration = Duration::from_millis(120);
/// Control-thread maintenance wake-up (also the request poll timeout).
const TICK: Duration = Duration::from_millis(50);
/// Pending events kept while the hub is not polling.
const MAX_PENDING_EVENTS: usize = 64;

const APP_NAME: &str = "plippa-routing";
const THREAD_NAME: &str = "plippa-route";
const FAILED_MESSAGE: &str = "audio server not available";

/// A control client for the PipeWire graph. See the module docs for the budget.
pub struct PipeWireControl {
    conn: Conn,
    /// Mirrors the control thread: true once the subscription is active.
    subscribed: bool,
    /// Modules we loaded, with the indices the server assigned.
    loaded: BTreeMap<ModuleTag, ModuleHandle>,
    /// Last good enumeration, returned when a request times out.
    cache: Vec<SinkInfo>,
    /// Events waiting to be returned by the next [`OutputControl::poll`].
    events: VecDeque<RouteEvent>,
    /// Next reconnect attempt (hub-side schedule).
    retry_at: Instant,
    /// Outage generation we last observed (resynchronises `retry_at`).
    seen_outages: u64,
}

impl PipeWireControl {
    /// Start the control thread. Never fails and never blocks longer than
    /// [`CONNECT_TIMEOUT`]; if the server is not up yet the client stays
    /// disconnected and retries from [`OutputControl::poll`].
    pub fn new() -> Self {
        let events = Arc::new(EventQueue::new());
        let state = Arc::new(SharedState::default());
        let (tx, rx) = mpsc::sync_channel(4);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);

        let thread_events = Arc::clone(&events);
        let thread_state = Arc::clone(&state);
        let join = thread::Builder::new()
            .name(THREAD_NAME.into())
            .spawn(move || run_control_loop(rx, startup_tx, thread_events, thread_state));

        let (inner, startup, seen_outages) = match join {
            Ok(join) => {
                let startup = startup_rx.recv_timeout(CONNECT_TIMEOUT);
                let outages = state.outages.load(Ordering::Acquire);
                (
                    Some(Inner {
                        tx,
                        events,
                        state,
                        join: Some(join),
                    }),
                    startup,
                    outages,
                )
            }
            Err(_) => (None, Err(mpsc::RecvTimeoutError::Disconnected), 0),
        };
        let connected = matches!(startup, Ok(true));
        let retry_at = if connected {
            Instant::now()
        } else {
            Instant::now() + RECONNECT_BACKOFF
        };

        Self {
            conn: Conn { inner },
            subscribed: false,
            loaded: BTreeMap::new(),
            cache: Vec::new(),
            events: VecDeque::new(),
            retry_at,
            seen_outages,
        }
    }

    fn request_deadline() -> Instant {
        Instant::now() + CALL_BUDGET
    }

    fn send(&self, request: Request) -> Result<(), Request> {
        match self.conn.inner.as_ref() {
            Some(inner) => inner.tx.try_send(request).map_err(|error| match error {
                TrySendError::Full(request) | TrySendError::Disconnected(request) => request,
            }),
            None => Err(request),
        }
    }
}

impl Default for PipeWireControl {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputControl for PipeWireControl {
    fn sinks(&mut self) -> Result<Vec<SinkInfo>, RouteError> {
        let (reply, rx) = mpsc::sync_channel(1);
        let deadline = Self::request_deadline();
        if self.send(Request::Sinks { deadline, reply }).is_err() {
            return Ok(self.cache.clone());
        }
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(SinkListReply::List(list)) => {
                self.cache = list.clone();
                Ok(list)
            }
            Ok(SinkListReply::TimedOut) => Ok(self.cache.clone()),
            Ok(SinkListReply::Failed(message)) => Err(RouteError::Backend(message)),
            Err(_) => Ok(self.cache.clone()),
        }
    }

    fn apply(
        &mut self,
        intent: &super::RouteIntent,
        previous: Option<&super::RouteIntent>,
    ) -> Result<(), RouteError> {
        let ops = ops_for(intent, previous, &self.loaded);
        if ops.is_empty() {
            return Ok(());
        }
        let (reply, rx) = mpsc::sync_channel(1);
        let deadline = Self::request_deadline();
        let loaded = self.loaded.clone();
        if self
            .send(Request::Apply {
                ops,
                loaded,
                deadline,
                reply,
            })
            .is_err()
        {
            return Err(RouteError::Backend(FAILED_MESSAGE.into()));
        }
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(report) => {
                self.loaded = report.loaded;
                match report.error {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }
            Err(_) => Err(RouteError::Backend("audio control timeout".into())),
        }
    }

    fn poll(&mut self) -> Vec<RouteEvent> {
        let Some(inner) = self.conn.inner.as_ref() else {
            return Vec::new();
        };

        self.subscribed = inner.state.subscribed.load(Ordering::Acquire);
        let connected = inner.state.connected.load(Ordering::Acquire);
        let outages = inner.state.outages.load(Ordering::Acquire);
        if outages != self.seen_outages {
            self.seen_outages = outages;
            self.retry_at = Instant::now() + RECONNECT_BACKOFF;
            // A restarted server loses the modules we loaded; drop our handles so the
            // next apply reloads the graph instead of trying stale indices.
            self.loaded.clear();
        }

        for event in inner.events.drain() {
            if matches!(event, RouteEvent::SinksChanged)
                && self
                    .events
                    .iter()
                    .any(|queued| matches!(queued, RouteEvent::SinksChanged))
            {
                continue;
            }
            self.events.push_back(event);
        }

        if !connected && Instant::now() >= self.retry_at {
            // A full/closed request channel still waits a full backoff before the
            // next attempt; the control thread may simply be mid-connect.
            let _ = inner.tx.try_send(Request::Reconnect);
            self.retry_at = Instant::now() + RECONNECT_BACKOFF;
        }

        let mut out = Vec::new();
        let mut sinks_changed = false;
        while let Some(event) = self.events.pop_front() {
            if matches!(event, RouteEvent::SinksChanged) {
                if sinks_changed {
                    continue;
                }
                sinks_changed = true;
            }
            out.push(event);
        }
        out
    }
}

struct Conn {
    inner: Option<Inner>,
}

struct Inner {
    tx: SyncSender<Request>,
    events: Arc<EventQueue>,
    state: Arc<SharedState>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let Inner {
            tx,
            events: _,
            state: _,
            mut join,
        } = inner;
        let _ = tx.try_send(Request::Shutdown);
        drop(tx);
        if let Some(join) = join.take() {
            let _ = join.join();
        }
    }
}

enum Request {
    Sinks {
        deadline: Instant,
        reply: SyncSender<SinkListReply>,
    },
    Apply {
        ops: Vec<RouteOp>,
        loaded: BTreeMap<ModuleTag, ModuleHandle>,
        deadline: Instant,
        reply: SyncSender<ApplyReport>,
    },
    Reconnect,
    Shutdown,
}

struct ApplyReport {
    loaded: BTreeMap<ModuleTag, ModuleHandle>,
    error: Option<RouteError>,
}

/// Result of an enumeration request. `TimedOut` lets the hub return its cache
/// without treating a slow server as a routing error.
enum SinkListReply {
    List(Vec<SinkInfo>),
    TimedOut,
    Failed(String),
}

#[derive(Default)]
struct SharedState {
    connected: AtomicBool,
    subscribed: AtomicBool,
    default_dirty: AtomicBool,
    /// Incremented once per outage so the hub can resynchronise its retry clock.
    outages: AtomicU64,
}

struct EventQueue {
    items: Mutex<VecDeque<RouteEvent>>,
}

impl EventQueue {
    fn new() -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, event: RouteEvent) {
        let mut items = self.lock();
        if matches!(event, RouteEvent::SinksChanged)
            && items
                .iter()
                .any(|queued| matches!(queued, RouteEvent::SinksChanged))
        {
            return;
        }
        if items.len() >= MAX_PENDING_EVENTS {
            items.pop_front();
        }
        items.push_back(event);
    }

    fn drain(&self) -> Vec<RouteEvent> {
        self.lock().drain(..).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<RouteEvent>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

enum OpChange {
    Insert(ModuleTag, ModuleHandle),
    Remove(ModuleTag),
    None,
}

impl OpChange {
    fn apply(self, loaded: &mut BTreeMap<ModuleTag, ModuleHandle>) {
        match self {
            Self::Insert(tag, handle) => {
                loaded.insert(tag, handle);
            }
            Self::Remove(tag) => {
                loaded.remove(&tag);
            }
            Self::None => {}
        }
    }
}

fn run_control_loop(
    requests: Receiver<Request>,
    startup: SyncSender<bool>,
    events: Arc<EventQueue>,
    state: Arc<SharedState>,
) {
    let Some(mut mainloop) = Mainloop::new() else {
        events.push(RouteEvent::Failed(FAILED_MESSAGE.into()));
        state.outages.fetch_add(1, Ordering::AcqRel);
        let _ = startup.send(false);
        return;
    };
    if mainloop.start().is_err() {
        events.push(RouteEvent::Failed(FAILED_MESSAGE.into()));
        state.outages.fetch_add(1, Ordering::AcqRel);
        let _ = startup.send(false);
        return;
    }

    let mut context: Option<Context> = None;
    let mut connected = false;
    let mut subscribed = false;
    // Remove modules left behind by a previous engine process before we load ours.
    let mut swept_stale_modules = false;
    let mut default_sink: Option<String> = None;
    let mut outage_reported = false;

    if let Some(open) = try_connect(&mut mainloop) {
        context = Some(open);
        connected = true;
    }
    state.connected.store(connected, Ordering::Release);
    if !connected {
        events.push(RouteEvent::Failed(FAILED_MESSAGE.into()));
        outage_reported = true;
        state.outages.fetch_add(1, Ordering::AcqRel);
    }
    let _ = startup.send(connected);

    loop {
        if connected && !context_alive(&mut mainloop, context.as_ref()) {
            if let Some(mut stale) = context.take() {
                mainloop.lock();
                stale.disconnect();
                mainloop.unlock();
                drop(stale);
            }
            connected = false;
            subscribed = false;
            default_sink = None;
            state.connected.store(false, Ordering::Release);
            state.subscribed.store(false, Ordering::Release);
            if !outage_reported {
                events.push(RouteEvent::Failed(FAILED_MESSAGE.into()));
                outage_reported = true;
                state.outages.fetch_add(1, Ordering::AcqRel);
            }
        }

        if connected && !subscribed {
            if let Some(open) = context.as_mut() {
                if subscribe(&mut mainloop, open, &events, &state) {
                    subscribed = true;
                    state.subscribed.store(true, Ordering::Release);
                    state.default_dirty.store(true, Ordering::Release);
                    if !swept_stale_modules {
                        sweep_stale_modules(
                            &mut mainloop,
                            open,
                            Instant::now() + OPERATION_TIMEOUT,
                        );
                        swept_stale_modules = true;
                    }
                    events.push(RouteEvent::SinksChanged);
                }
            }
        }

        if connected && state.default_dirty.load(Ordering::Acquire) {
            if let Some(open) = context.as_mut() {
                let deadline = Instant::now() + OPERATION_TIMEOUT;
                if let Ok(name) = fetch_default_sink(&mut mainloop, open, deadline) {
                    default_sink = name;
                    state.default_dirty.store(false, Ordering::Release);
                }
            }
        }

        let request = match requests.recv_timeout(TICK) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        match request {
            Request::Shutdown => {
                // Leave no routing modules behind while the engine is down.
                if let Some(open) = context.as_mut() {
                    sweep_stale_modules(
                        &mut mainloop,
                        open,
                        Instant::now() + OPERATION_TIMEOUT,
                    );
                }
                break;
            }
            Request::Reconnect => {
                if connected {
                    continue;
                }
                match try_connect(&mut mainloop) {
                    Some(open) => {
                        context = Some(open);
                        connected = true;
                        subscribed = false;
                        outage_reported = false;
                        state.connected.store(true, Ordering::Release);
                        state.subscribed.store(false, Ordering::Release);
                    }
                    None => {
                        state.connected.store(false, Ordering::Release);
                        state.subscribed.store(false, Ordering::Release);
                        if !outage_reported {
                            events.push(RouteEvent::Failed(FAILED_MESSAGE.into()));
                            outage_reported = true;
                            state.outages.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                }
            }
            Request::Sinks { deadline, reply } => {
                let result = match context.as_mut() {
                    Some(open) if connected => {
                        // A server event may have arrived while we were waiting; make
                        // sure `is_default` reflects it even if the tick has not run.
                        if state.default_dirty.load(Ordering::Acquire) {
                            if let Ok(name) = fetch_default_sink(&mut mainloop, open, deadline) {
                                default_sink = name;
                                state.default_dirty.store(false, Ordering::Release);
                            }
                        }
                        enumerate_sinks(&mut mainloop, open, &default_sink, deadline)
                    }
                    _ => SinkListReply::Failed(FAILED_MESSAGE.into()),
                };
                let _ = reply.send(result);
            }
            Request::Apply {
                ops,
                mut loaded,
                deadline,
                reply,
            } => {
                let mut error = None;
                if !connected {
                    error = Some(RouteError::Backend(FAILED_MESSAGE.into()));
                } else if let Some(open) = context.as_mut() {
                    // A lost report or a client-only reconnect can leave modules loaded while
                    // our handle map is empty; clear the old graph before loading a new one.
                    if loaded.is_empty() && ops.iter().any(|op| matches!(op, RouteOp::Load { .. })) {
                        sweep_stale_modules(
                            &mut mainloop,
                            open,
                            Instant::now() + OPERATION_TIMEOUT,
                        );
                    }
                    for op in ops {
                        if Instant::now() >= deadline {
                            error = Some(RouteError::Backend("audio control timeout".into()));
                            break;
                        }
                        match apply_op(&mut mainloop, open, &op, &loaded, deadline) {
                            Ok(change) => change.apply(&mut loaded),
                            Err(failure) => {
                                error = Some(failure);
                                break;
                            }
                        }
                    }
                } else {
                    error = Some(RouteError::Backend(FAILED_MESSAGE.into()));
                }
                let _ = reply.send(ApplyReport { loaded, error });
            }
        }
    }

    if let Some(mut open) = context.take() {
        mainloop.lock();
        open.disconnect();
        mainloop.unlock();
        drop(open);
    }
    mainloop.stop();
    state.connected.store(false, Ordering::Release);
    state.subscribed.store(false, Ordering::Release);
}

fn context_alive(mainloop: &mut Mainloop, context: Option<&Context>) -> bool {
    let Some(context) = context else {
        return false;
    };
    mainloop.lock();
    let state = context.get_state();
    mainloop.unlock();
    !matches!(
        state,
        ContextState::Failed | ContextState::Terminated | ContextState::Unconnected
    )
}

fn try_connect(mainloop: &mut Mainloop) -> Option<Context> {
    let mut context = Context::new(mainloop, APP_NAME)?;
    let server = env::var("PULSE_SERVER").ok();
    mainloop.lock();
    let connected = context.connect(server.as_deref(), FlagSet::NOAUTOSPAWN, None);
    mainloop.unlock();
    connected.ok()?;

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        mainloop.lock();
        let state = context.get_state();
        mainloop.unlock();
        match state {
            ContextState::Ready => return Some(context),
            ContextState::Failed | ContextState::Terminated => return None,
            _ if Instant::now() >= deadline => return None,
            _ => thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn subscribe(
    mainloop: &mut Mainloop,
    context: &mut Context,
    events: &Arc<EventQueue>,
    state: &Arc<SharedState>,
) -> bool {
    let callback_events = Arc::clone(events);
    let callback_state = Arc::clone(state);
    mainloop.lock();
    context.set_subscribe_callback(Some(Box::new(
        move |facility: Option<Facility>, _operation, _index| match facility {
            Some(Facility::Sink) => callback_events.push(RouteEvent::SinksChanged),
            Some(Facility::Server) => {
                callback_state.default_dirty.store(true, Ordering::Release);
                callback_events.push(RouteEvent::SinksChanged);
            }
            _ => {}
        },
    )));
    mainloop.unlock();

    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    mainloop.lock();
    let mut op = context.subscribe(
        InterestMaskSet::SINK | InterestMaskSet::SERVER,
        move |ok| {
            if let Some(tx) = tx.take() {
                let _ = tx.send(ok);
            }
        },
    );
    mainloop.unlock();

    match rx.recv_timeout(OPERATION_TIMEOUT) {
        Ok(true) => true,
        _ => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            false
        }
    }
}

fn fetch_default_sink(
    mainloop: &mut Mainloop,
    context: &mut Context,
    deadline: Instant,
) -> Result<Option<String>, ()> {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    mainloop.lock();
    let introspector = context.introspect();
    let mut op = introspector.get_server_info(move |info| {
        if let Some(tx) = tx.take() {
            let name = info.default_sink_name.as_ref().map(|name| name.to_string());
            let _ = tx.send(name);
        }
    });
    mainloop.unlock();

    match rx.recv_timeout(wait_budget(deadline)) {
        Ok(name) => Ok(name),
        Err(_) => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            Err(())
        }
    }
}

fn enumerate_sinks(
    mainloop: &mut Mainloop,
    context: &mut Context,
    default_sink: &Option<String>,
    deadline: Instant,
) -> SinkListReply {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    let mut items: Option<Vec<SinkInfo>> = Some(Vec::new());
    let default_sink = default_sink.clone();

    mainloop.lock();
    let introspector = context.introspect();
    let mut op = introspector.get_sink_info_list(move |result| match result {
        ListResult::Item(info) => {
            if let Some(items) = items.as_mut() {
                items.push(map_sink(info, default_sink.as_deref()));
            }
        }
        ListResult::End => {
            if let (Some(tx), Some(items)) = (tx.take(), items.take()) {
                let _ = tx.send(SinkListReply::List(items));
            }
        }
        ListResult::Error => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(SinkListReply::Failed("sink enumeration failed".into()));
            }
        }
    });
    mainloop.unlock();

    match rx.recv_timeout(wait_budget(deadline)) {
        Ok(result) => result,
        Err(_) => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            SinkListReply::TimedOut
        }
    }
}

fn map_sink(info: &pulse::context::introspect::SinkInfo<'_>, default_sink: Option<&str>) -> SinkInfo {
    let mut props = BTreeMap::new();
    if let Some(monitor) = &info.monitor_source_name {
        props.insert("monitor_source_name".into(), monitor.to_string());
    }
    for key in info.proplist.iter() {
        if let Some(value) = info.proplist.get_str(&key) {
            props.insert(key, value);
        }
    }
    let is_monitor = props
        .get("device.class")
        .is_some_and(|class| class == "monitor");
    SinkInfo {
        id: info.name.as_deref().unwrap_or_default().to_string(),
        description: info.description.as_deref().unwrap_or_default().to_string(),
        available: !is_monitor && !matches!(info.state, SinkState::Invalid),
        latency_ms: (info.latency.0 / 1000).min(u64::from(u32::MAX)) as u32,
        props,
        is_default: default_sink.is_some() && default_sink == info.name.as_deref(),
    }
}

fn apply_op(
    mainloop: &mut Mainloop,
    context: &mut Context,
    op: &RouteOp,
    loaded: &BTreeMap<ModuleTag, ModuleHandle>,
    deadline: Instant,
) -> Result<OpChange, RouteError> {
    match op {
        RouteOp::Load { tag, module, args } => {
            let handle = load_module(mainloop, context, module, args, deadline)?;
            Ok(OpChange::Insert(tag.clone(), handle))
        }
        RouteOp::Unload { tag } => {
            if let Some(handle) = loaded.get(tag) {
                unload_module(mainloop, context, tag, handle.index, deadline)?;
            }
            Ok(OpChange::Remove(tag.clone()))
        }
        RouteOp::Unity { sink } => {
            set_unity(mainloop, context, sink, deadline)?;
            Ok(OpChange::None)
        }
    }
}

fn load_module(
    mainloop: &mut Mainloop,
    context: &mut Context,
    module: &'static str,
    args: &str,
    deadline: Instant,
) -> Result<ModuleHandle, RouteError> {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    mainloop.lock();
    let mut introspector = context.introspect();
    let mut op = introspector.load_module(module, args, move |index| {
        if let Some(tx) = tx.take() {
            let _ = tx.send(index);
        }
    });
    mainloop.unlock();

    match rx.recv_timeout(wait_budget(deadline)) {
        Ok(index) if index != pulse::def::INVALID_INDEX => Ok(ModuleHandle { index }),
        Ok(_) => {
            let detail = server_error(mainloop, context);
            Err(RouteError::Backend(format!("{module} load failed: {detail}")))
        }
        Err(_) => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            Err(RouteError::Backend(format!("{module} load timed out")))
        }
    }
}

fn unload_module(
    mainloop: &mut Mainloop,
    context: &mut Context,
    tag: &ModuleTag,
    index: u32,
    deadline: Instant,
) -> Result<(), RouteError> {
    let module = module_name(tag);
    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    mainloop.lock();
    let mut introspector = context.introspect();
    let mut op = introspector.unload_module(index, move |ok| {
        if let Some(tx) = tx.take() {
            let _ = tx.send(ok);
        }
    });
    mainloop.unlock();

    match rx.recv_timeout(wait_budget(deadline)) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let detail = server_error(mainloop, context);
            Err(RouteError::Backend(format!(
                "{module} unload failed for {tag:?}: {detail}"
            )))
        }
        Err(_) => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            Err(RouteError::Backend(format!(
                "{module} unload timed out for {tag:?}"
            )))
        }
    }
}

fn set_unity(
    mainloop: &mut Mainloop,
    context: &mut Context,
    sink: &str,
    deadline: Instant,
) -> Result<(), RouteError> {
    let channels = sink_channels(mainloop, context, sink, deadline).unwrap_or(2);
    let mut volume = ChannelVolumes::default();
    volume.set(channels.max(1), Volume::NORMAL);

    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    mainloop.lock();
    let mut introspector = context.introspect();
    let mut op = introspector.set_sink_volume_by_name(
        sink,
        &volume,
        Some(Box::new(move |ok| {
            if let Some(tx) = tx.take() {
                let _ = tx.send(ok);
            }
        })),
    );
    mainloop.unlock();
    match rx.recv_timeout(wait_budget(deadline)) {
        Ok(true) => {}
        Ok(false) => {
            let detail = server_error(mainloop, context);
            return Err(RouteError::Backend(format!(
                "set volume for {sink}: {detail}"
            )));
        }
        Err(_) => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            return Err(RouteError::Backend(format!("set volume for {sink} timed out")));
        }
    }

    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    mainloop.lock();
    let mut introspector = context.introspect();
    let mut op = introspector.set_sink_mute_by_name(
        sink,
        false,
        Some(Box::new(move |ok| {
            if let Some(tx) = tx.take() {
                let _ = tx.send(ok);
            }
        })),
    );
    mainloop.unlock();
    match rx.recv_timeout(wait_budget(deadline)) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let detail = server_error(mainloop, context);
            Err(RouteError::Backend(format!("unmute {sink}: {detail}")))
        }
        Err(_) => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            Err(RouteError::Backend(format!("unmute {sink} timed out")))
        }
    }
}

fn server_error(mainloop: &mut Mainloop, context: &Context) -> String {
    mainloop.lock();
    let detail = context.errno().to_string().unwrap_or_default();
    mainloop.unlock();
    if detail.is_empty() {
        "server refused".into()
    } else {
        detail
    }
}

fn sink_channels(
    mainloop: &mut Mainloop,
    context: &mut Context,
    sink: &str,
    deadline: Instant,
) -> Option<u8> {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    mainloop.lock();
    let introspector = context.introspect();
    let mut op = introspector.get_sink_info_by_name(sink, move |result| match result {
        ListResult::Item(info) => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(info.sample_spec.channels);
            }
        }
        ListResult::End => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(0);
            }
        }
        ListResult::Error => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(0);
            }
        }
    });
    mainloop.unlock();

    match rx.recv_timeout(wait_budget(deadline)) {
        Ok(channels) if channels > 0 => Some(channels),
        Ok(_) => None,
        Err(_) => {
            mainloop.lock();
            op.cancel();
            mainloop.unlock();
            None
        }
    }
}

fn module_name(tag: &ModuleTag) -> &'static str {
    match tag {
        ModuleTag::Multi => super::MODULE_COMBINE_SINK,
        ModuleTag::DelaySink(_) => super::MODULE_NULL_SINK,
        ModuleTag::DelayLoop(_) => super::MODULE_LOOPBACK,
    }
}

/// True for a server module that belongs to our routing graph (`plippa.multi`,
/// `plippa.delay.*`). Our module arguments are the only place these names appear.
fn is_stale_module(argument: &str) -> bool {
    argument.contains(plippa_core::audio_route::INTERNAL_SINK_PREFIX)
}

/// Unload routing modules left by a previous process. The server keeps modules alive after
/// the client disconnects, so a restart would otherwise accumulate a second `plippa.multi`.
fn sweep_stale_modules(mainloop: &mut Mainloop, context: &mut Context, deadline: Instant) {
    let (tx, rx) = mpsc::sync_channel(1);
    let mut tx = Some(tx);
    let mut indexes = Vec::new();

    mainloop.lock();
    let introspector = context.introspect();
    let mut list_op = introspector.get_module_info_list(move |result| match result {
        ListResult::Item(info) => {
            if is_stale_module(info.argument.as_deref().unwrap_or_default()) {
                indexes.push(info.index);
            }
        }
        ListResult::End => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(std::mem::take(&mut indexes));
            }
        }
        ListResult::Error => {
            if let Some(tx) = tx.take() {
                let _ = tx.send(Vec::new());
            }
        }
    });
    mainloop.unlock();

    let indexes = match rx.recv_timeout(wait_budget(deadline)) {
        Ok(indexes) => indexes,
        Err(_) => {
            mainloop.lock();
            list_op.cancel();
            mainloop.unlock();
            return;
        }
    };

    for index in indexes {
        let (tx, rx) = mpsc::sync_channel(1);
        mainloop.lock();
        let mut introspector = context.introspect();
        let mut unload_op = introspector.unload_module(index, move |unloaded| {
            let _ = tx.send(unloaded);
        });
        mainloop.unlock();
        match rx.recv_timeout(wait_budget(deadline)) {
            Ok(_) => eprintln!("plippa-audio: removed stale routing module {index}"),
            Err(_) => {
                mainloop.lock();
                unload_op.cancel();
                mainloop.unlock();
            }
        }
    }
}

fn wait_budget(deadline: Instant) -> Duration {
    let now = Instant::now();
    if deadline <= now {
        Duration::ZERO
    } else {
        (deadline - now).min(OPERATION_TIMEOUT)
    }
}

#[cfg(test)]
mod stale_module_tests {
    use super::is_stale_module;

    #[test]
    fn only_our_routing_modules_are_swept() {
        assert!(is_stale_module(
            "sink_name=plippa.multi latency_compensate=1 sink_properties=device.description=Plippa_Multi"
        ));
        assert!(is_stale_module(
            "source=plippa.delay.aa:bb.monitor sink=alsa_output latency_msec=40"
        ));
        assert!(!is_stale_module("sink_name=alsa_output.platform-soc_sound"));
        assert!(!is_stale_module(""));
    }
}
