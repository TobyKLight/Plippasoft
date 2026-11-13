//! SECFIX_01 (SRV-02, SRV-04): who may use the portal's scarce resources.
//!
//! Three small, separately testable pieces, none of which knows about HTTP parsing:
//!
//! * [`Connections`]: connection admission (total, loopback reserve, per client address) and a
//!   registry that can list and force-close live connections. It is the
//!   [`tiny_http::ConnectionGate`] the accept loop calls before a byte is read.
//! * [`UploadGate`]: the one upload slot per server, with an owner and a progress stamp so a
//!   stalled holder cannot keep it, and [`SlotReader`]/[`ActivityReader`] that stamp progress as
//!   the body arrives.
//! * [`place_session`]: the rule for opening an import or unite session when some exist.
//!
//! The on-device UI talks to the portal over loopback: loopback connections have capacity that
//! LAN clients can never use, so a flood from the network cannot lock the device out.

use crate::web::download::Cancel;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{ConnectionGate, ConnectionHandle, Refused};

/// The address a request is attributed to. A UNIX socket peer (no address) counts as local.
pub fn client_id(peer: Option<SocketAddr>) -> IpAddr {
    match peer {
        Some(a) => a.ip().to_canonical(),
        None => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    }
}

// ---------------------------------------------------------------------------
// Connection admission and registry
// ---------------------------------------------------------------------------

/// Caps for [`Connections`]. `Default` is the production set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionLimits {
    /// Live connections, all sources together.
    pub max_total: usize,
    /// Of `max_total`, how many only loopback (the on-device UI) may use. Other clients can hold
    /// at most `max_total - loopback_reserved`.
    pub loopback_reserved: usize,
    /// Live connections per non-loopback client (a browser opens six; two tabs, twelve).
    /// Loopback is not subject to it.
    pub max_per_client: usize,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            max_total: 64,
            loopback_reserved: 8,
            max_per_client: 16,
        }
    }
}

/// One live connection as the registry lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub id: u64,
    /// `None` for a UNIX socket.
    pub peer: Option<SocketAddr>,
}

impl ConnectionInfo {
    /// Loopback (or UNIX socket): the on-device UI and tools on the device itself.
    pub fn is_loopback(&self) -> bool {
        is_local(self.peer)
    }
}

fn is_local(peer: Option<SocketAddr>) -> bool {
    peer.is_none_or(|a| a.ip().to_canonical().is_loopback())
}

/// Clients are counted per address; an IPv6 client is counted per /64 (one customer owns the
/// whole prefix, so counting single addresses would make the cap meaningless).
pub(crate) fn client_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
        v4 => v4,
    }
}

struct Live {
    info: ConnectionInfo,
    key: IpAddr,
    close: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct Table {
    next_id: u64,
    live: Vec<Live>,
    per_client: HashMap<IpAddr, usize>,
    lan: usize,
}

struct Inner {
    limits: AdmissionLimits,
    table: Mutex<Table>,
}

/// Connection admission plus the registry of live connections. Cloning shares the registry.
#[derive(Clone)]
pub struct Connections(Arc<Inner>);

impl Connections {
    pub fn new(limits: AdmissionLimits) -> Self {
        Self(Arc::new(Inner {
            limits,
            table: Mutex::new(Table::default()),
        }))
    }

    pub fn limits(&self) -> AdmissionLimits {
        self.0.limits
    }

    /// Admit (or refuse) a connection from `peer`. `close` force-closes it later. The returned
    /// guard removes the connection from the registry when dropped.
    ///
    /// * loopback: admitted while `total < max_total`;
    /// * anyone else: admitted while the other non-loopback connections number fewer than
    ///   `max_total - loopback_reserved` **and** this client holds fewer than `max_per_client`.
    pub fn admit_peer(
        &self,
        peer: Option<SocketAddr>,
        close: Box<dyn Fn() + Send + Sync>,
    ) -> Result<ConnectionGuard, Refused> {
        let limits = self.0.limits;
        let mut t = self.0.table.lock().map_err(|_| Refused)?;
        let local = is_local(peer);
        let key = client_key(client_id(peer));
        if t.live.len() >= limits.max_total {
            return Err(Refused);
        }
        if !local {
            let lan_cap = limits.max_total.saturating_sub(limits.loopback_reserved);
            if t.lan >= lan_cap {
                return Err(Refused);
            }
            if t.per_client.get(&key).copied().unwrap_or(0) >= limits.max_per_client {
                return Err(Refused);
            }
            t.lan += 1;
            *t.per_client.entry(key).or_insert(0) += 1;
        }
        let id = t.next_id;
        t.next_id += 1;
        t.live.push(Live {
            info: ConnectionInfo { id, peer },
            key,
            close,
        });
        Ok(ConnectionGuard {
            inner: Arc::clone(&self.0),
            id,
        })
    }

    /// Number of live connections.
    pub fn live(&self) -> usize {
        self.0.table.lock().map(|t| t.live.len()).unwrap_or(0)
    }

    /// The live connections, oldest first.
    pub fn snapshot(&self) -> Vec<ConnectionInfo> {
        self.0
            .table
            .lock()
            .map(|t| t.live.iter().map(|l| l.info).collect())
            .unwrap_or_default()
    }

    /// Force-close every live connection `matches` selects (for example "everything that is not
    /// loopback" when the portal is switched off). Returns how many were closed. A closed
    /// connection leaves the registry when its thread notices and drops its guard.
    pub fn close_matching(&self, matches: impl Fn(&ConnectionInfo) -> bool) -> usize {
        // Closing only shuts a socket down (it never waits for the connection's thread), so the
        // table can stay locked meanwhile; the thread takes this lock only to deregister.
        let Ok(t) = self.0.table.lock() else { return 0 };
        let mut n = 0;
        for l in t.live.iter().filter(|l| matches(&l.info)) {
            (l.close)();
            n += 1;
        }
        n
    }
}

/// Removes its connection from the registry when dropped (the connection ended).
pub struct ConnectionGuard {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let Ok(mut t) = self.inner.table.lock() else { return };
        let Some(at) = t.live.iter().position(|l| l.info.id == self.id) else {
            return;
        };
        let gone = t.live.remove(at);
        if !is_local(gone.info.peer) {
            t.lan = t.lan.saturating_sub(1);
            if let Some(c) = t.per_client.get_mut(&gone.key) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    t.per_client.remove(&gone.key);
                }
            }
        }
    }
}

impl ConnectionGate for Connections {
    fn admit(
        &self,
        peer: Option<SocketAddr>,
        handle: ConnectionHandle,
    ) -> Result<Box<dyn Send>, Refused> {
        let guard = self.admit_peer(peer, Box::new(move || handle.shutdown()))?;
        Ok(Box::new(guard))
    }
}

// ---------------------------------------------------------------------------
// The upload slot
// ---------------------------------------------------------------------------

/// How long a holder may make no progress before the **same client** may take its slot over.
pub const SLOT_TAKEOVER_AFTER: Duration = Duration::from_secs(15);

struct Holder {
    id: u64,
    owner: IpAddr,
    last_progress: Instant,
    /// The body is fully received and the holder is only staging/registering it: not stalled.
    finishing: bool,
    cancel: Cancel,
}

struct GateInner {
    holder: Mutex<Option<Holder>>,
    next_id: AtomicU64,
}

/// One upload body is received at a time per server (each costs disk bandwidth and a staging
/// file on a small board). The holder is recorded with its owner and its last progress, so a
/// stalled holder cannot keep the slot: its own client may replace it after
/// [`SLOT_TAKEOVER_AFTER`] without progress (which cancels it), and any other client is told to
/// retry. Beyond that, the socket stall timeout and the minimum-progress rule of the streaming
/// code end a holder that has gone quiet or is only dripping bytes.
pub struct UploadGate {
    takeover_after: Duration,
    inner: Arc<GateInner>,
}

impl Default for UploadGate {
    fn default() -> Self {
        Self::new(SLOT_TAKEOVER_AFTER)
    }
}

impl UploadGate {
    pub fn new(takeover_after: Duration) -> Self {
        Self {
            takeover_after,
            inner: Arc::new(GateInner {
                holder: Mutex::new(None),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Take the slot for `owner`, or `None` when someone else holds it.
    pub fn try_take(&self, owner: IpAddr) -> Option<UploadSlot> {
        self.try_take_at(owner, Instant::now())
    }

    /// [`UploadGate::try_take`] with the clock passed in (tests).
    pub fn try_take_at(&self, owner: IpAddr, now: Instant) -> Option<UploadSlot> {
        let mut h = self.inner.holder.lock().ok()?;
        if let Some(cur) = h.as_ref() {
            let stale = !cur.finishing
                && now.saturating_duration_since(cur.last_progress) > self.takeover_after;
            if !(stale && cur.owner == owner) {
                return None;
            }
            // The stalled holder is superseded: it sees the cancel on its next read (or ends at
            // the socket stall timeout), deletes its temp file and releases its reservation.
            cur.cancel.cancel();
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = Cancel::new();
        *h = Some(Holder {
            id,
            owner,
            last_progress: now,
            finishing: false,
            cancel: cancel.clone(),
        });
        Some(UploadSlot {
            inner: Arc::clone(&self.inner),
            id,
            cancel,
        })
    }

    /// True while an upload holds the slot. Lets tests wait for "the first upload is really in
    /// flight" instead of probing with requests.
    pub fn in_flight(&self) -> bool {
        self.inner
            .holder
            .lock()
            .map(|h| h.is_some())
            .unwrap_or(false)
    }
}

/// Holds the upload slot; released on drop (also on early returns and panics). A holder that was
/// superseded does not release its successor's slot.
pub struct UploadSlot {
    inner: Arc<GateInner>,
    id: u64,
    cancel: Cancel,
}

impl UploadSlot {
    /// Record progress (bytes arrived).
    pub fn touch(&self) {
        self.touch_at(Instant::now());
    }

    /// [`UploadSlot::touch`] with the clock passed in (tests).
    pub fn touch_at(&self, now: Instant) {
        if let Ok(mut h) = self.inner.holder.lock() {
            if let Some(cur) = h.as_mut().filter(|c| c.id == self.id) {
                cur.last_progress = now;
            }
        }
    }

    /// The body is fully received; what remains is bounded work, not a stall.
    pub fn mark_finishing(&self) {
        if let Ok(mut h) = self.inner.holder.lock() {
            if let Some(cur) = h.as_mut().filter(|c| c.id == self.id) {
                cur.finishing = true;
            }
        }
    }

    /// Set when another upload from the same client took this slot over.
    pub fn is_superseded(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Wrap the request body so every chunk that arrives counts as progress, the end of the body
    /// marks the slot as finishing, and a superseded holder stops reading.
    pub fn reader<'a>(&'a self, inner: &'a mut dyn Read) -> SlotReader<'a> {
        SlotReader { slot: self, inner }
    }
}

impl Drop for UploadSlot {
    fn drop(&mut self) {
        if let Ok(mut h) = self.inner.holder.lock() {
            if h.as_ref().is_some_and(|c| c.id == self.id) {
                *h = None;
            }
        }
    }
}

/// See [`UploadSlot::reader`].
pub struct SlotReader<'a> {
    slot: &'a UploadSlot,
    inner: &'a mut dyn Read,
}

impl Read for SlotReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.slot.is_superseded() {
            return Err(std::io::Error::other("upload superseded by a newer one"));
        }
        let n = self.inner.read(buf)?;
        if n == 0 {
            self.slot.mark_finishing();
        } else {
            self.slot.touch();
        }
        Ok(n)
    }
}

/// How often [`ActivityReader`] reports activity at most. Far below any session lifetime.
const ACTIVITY_INTERVAL: Duration = Duration::from_millis(100);

/// Calls `on_activity` at most every [`ACTIVITY_INTERVAL`] while data arrives (a session keeps
/// itself alive while its own upload is streaming).
pub struct ActivityReader<'a, F: FnMut()> {
    inner: &'a mut dyn Read,
    on_activity: F,
    last: Option<Instant>,
}

impl<'a, F: FnMut()> ActivityReader<'a, F> {
    pub fn new(inner: &'a mut dyn Read, on_activity: F) -> Self {
        Self {
            inner,
            on_activity,
            last: None,
        }
    }
}

impl<F: FnMut()> Read for ActivityReader<'_, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0
            && self
                .last
                .is_none_or(|t| t.elapsed() >= ACTIVITY_INTERVAL)
        {
            self.last = Some(Instant::now());
            (self.on_activity)();
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Session placement
// ---------------------------------------------------------------------------

/// Caps for opening import and unite sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// Sessions of one kind, all clients together.
    pub max_total: usize,
    /// Sessions of one kind per client.
    pub max_per_client: usize,
    /// A session with no progress for this long is *idle*: it may be replaced. Anything newer is
    /// live and is never evicted.
    pub live_window: Duration,
}

impl SessionLimits {
    pub const DEFAULT_LIVE_WINDOW: Duration = Duration::from_secs(10 * 60);

    pub fn new(max_total: usize) -> Self {
        Self {
            max_total,
            max_per_client: 2,
            live_window: Self::DEFAULT_LIVE_WINDOW,
        }
    }
}

/// One existing session as the placement rule sees it.
#[derive(Debug, Clone, Copy)]
pub struct SessionRow {
    pub owner: IpAddr,
    /// Time since the session last made progress. A session that is working right now (finishing,
    /// downloading) is reported with a zero idle time.
    pub idle: Duration,
}

/// Where a new session goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// There is room.
    Add,
    /// There is room once the session at this index is dropped.
    Replace(usize),
    /// Every slot belongs to live sessions: refuse (`429 too_many`).
    Refuse,
}

/// Decide where `client`'s new session goes. Rules, in order:
///
/// 1. A client at its own cap replaces **its own** longest-idle idle session, or is refused when
///    all of its own are live.
/// 2. With the total cap reached, the longest-idle idle session of **anyone** is replaced; a live
///    session is never evicted, so when all are live the new one is refused.
pub fn place_session(rows: &[SessionRow], client: IpAddr, limits: &SessionLimits) -> Placement {
    let longest_idle = |own_only: bool| {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| (!own_only || r.owner == client) && r.idle >= limits.live_window)
            .max_by_key(|(_, r)| r.idle)
            .map(|(i, _)| i)
    };
    let own = rows.iter().filter(|r| r.owner == client).count();
    if own >= limits.max_per_client {
        return longest_idle(true).map_or(Placement::Refuse, Placement::Replace);
    }
    if rows.len() >= limits.max_total {
        return longest_idle(false).map_or(Placement::Refuse, Placement::Replace);
    }
    Placement::Add
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn lan(n: u8) -> Option<SocketAddr> {
        Some(SocketAddr::from(([192, 168, 1, n], 40000 + u16::from(n))))
    }

    fn loopback() -> Option<SocketAddr> {
        Some(SocketAddr::from(([127, 0, 0, 1], 41000)))
    }

    fn nop() -> Box<dyn Fn() + Send + Sync> {
        Box::new(|| {})
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, n])
    }

    #[test]
    fn a_lan_flood_cannot_lock_out_loopback() {
        let c = Connections::new(AdmissionLimits::default());
        let mut guards = Vec::new();
        // Many distinct LAN clients, each below its own cap, until the LAN share is full.
        let mut host = 1u8;
        let mut admitted = 0;
        'fill: loop {
            for _ in 0..16 {
                match c.admit_peer(lan(host), nop()) {
                    Ok(g) => {
                        guards.push(g);
                        admitted += 1;
                    }
                    Err(Refused) => break 'fill,
                }
            }
            host += 1;
            assert!(host < 200, "the LAN share never filled");
        }
        assert_eq!(admitted, 64 - 8, "LAN clients may hold total minus the loopback reserve");
        // The LAN is shut out ...
        assert!(c.admit_peer(lan(250), nop()).is_err());
        // ... but the on-device UI (loopback) is still admitted, up to the full total.
        for _ in 0..8 {
            guards.push(c.admit_peer(loopback(), nop()).expect("loopback has reserved capacity"));
        }
        assert_eq!(c.live(), 64);
        assert!(c.admit_peer(loopback(), nop()).is_err(), "even loopback stops at the total");
    }

    #[test]
    fn one_lan_client_is_capped_and_loopback_is_not_subject_to_the_per_client_cap() {
        let c = Connections::new(AdmissionLimits::default());
        let mut guards: Vec<_> = (0..16)
            .map(|_| c.admit_peer(lan(7), nop()).expect("16 are fine"))
            .collect();
        assert!(c.admit_peer(lan(7), nop()).is_err(), "the 17th from one address is refused");
        guards.push(c.admit_peer(lan(8), nop()).expect("another client is unaffected"));
        for _ in 0..20 {
            guards.push(c.admit_peer(loopback(), nop()).expect("loopback has no per-client cap"));
        }
        // A browser (six connections, two tabs: twelve) is comfortably inside.
        drop(guards);
        let browser: Vec<_> = (0..12).map(|_| c.admit_peer(lan(9), nop())).collect();
        assert!(browser.iter().all(Result::is_ok));
    }

    #[test]
    fn slots_come_back_when_connections_end() {
        let c = Connections::new(AdmissionLimits::default());
        let guards: Vec<_> = (0..16).map(|_| c.admit_peer(lan(7), nop()).unwrap()).collect();
        assert_eq!(c.live(), 16);
        drop(guards);
        assert_eq!(c.live(), 0);
        assert!(c.admit_peer(lan(7), nop()).is_ok());
    }

    #[test]
    fn ipv6_clients_are_counted_per_prefix_and_mapped_loopback_is_loopback() {
        let c = Connections::new(AdmissionLimits::default());
        let mut guards = Vec::new();
        for i in 0..16u16 {
            let a = SocketAddr::new(format!("2001:db8:0:1::{i:x}").parse().unwrap(), 5000);
            guards.push(c.admit_peer(Some(a), nop()).unwrap());
        }
        let same_prefix = SocketAddr::new("2001:db8:0:1::ffff".parse().unwrap(), 5000);
        assert!(c.admit_peer(Some(same_prefix), nop()).is_err());
        let mapped = SocketAddr::new("::ffff:127.0.0.1".parse().unwrap(), 5000);
        assert!(ConnectionInfo { id: 0, peer: Some(mapped) }.is_loopback());
        assert!(ConnectionInfo { id: 0, peer: None }.is_loopback(), "a UNIX socket is local");
    }

    #[test]
    fn connection_registry_can_force_close_matching_connections() {
        let c = Connections::new(AdmissionLimits::default());
        let closed_lan = Arc::new(AtomicUsize::new(0));
        let closed_local = Arc::new(AtomicUsize::new(0));
        let mut guards = Vec::new();
        for n in 1..=3u8 {
            let k = Arc::clone(&closed_lan);
            guards.push(
                c.admit_peer(lan(n), Box::new(move || { k.fetch_add(1, Ordering::SeqCst); }))
                    .unwrap(),
            );
        }
        for _ in 0..2 {
            let k = Arc::clone(&closed_local);
            guards.push(
                c.admit_peer(loopback(), Box::new(move || { k.fetch_add(1, Ordering::SeqCst); }))
                    .unwrap(),
            );
        }
        assert_eq!(c.snapshot().len(), 5);
        let n = c.close_matching(|i| !i.is_loopback());
        assert_eq!(n, 3);
        assert_eq!(closed_lan.load(Ordering::SeqCst), 3);
        assert_eq!(closed_local.load(Ordering::SeqCst), 0, "loopback is left alone");
        // Matching nothing closes nothing.
        assert_eq!(c.close_matching(|_| false), 0);
    }

    #[test]
    fn a_stale_slot_is_taken_over_by_the_same_client_but_not_by_another() {
        let gate = UploadGate::new(Duration::from_secs(15));
        let t0 = Instant::now();
        let first = gate.try_take_at(ip(1), t0).expect("free slot");
        // Fresh: nobody gets it, not even the owner.
        assert!(gate.try_take_at(ip(1), t0 + Duration::from_secs(10)).is_none());
        assert!(gate.try_take_at(ip(2), t0 + Duration::from_secs(10)).is_none());
        // Progress re-arms the window.
        first.touch_at(t0 + Duration::from_secs(14));
        assert!(gate.try_take_at(ip(1), t0 + Duration::from_secs(20)).is_none());
        // Stale (more than 15 s without progress): another client still cannot ...
        assert!(gate.try_take_at(ip(2), t0 + Duration::from_secs(40)).is_none());
        assert!(!first.is_superseded());
        // ... but the same client can, which cancels the stale holder.
        let second = gate.try_take_at(ip(1), t0 + Duration::from_secs(40)).expect("takeover");
        assert!(first.is_superseded());
        // The superseded holder ending does not free its successor's slot.
        drop(first);
        assert!(gate.in_flight());
        assert!(gate.try_take_at(ip(1), t0 + Duration::from_secs(41)).is_none());
        drop(second);
        assert!(!gate.in_flight());
        assert!(gate.try_take_at(ip(2), t0 + Duration::from_secs(42)).is_some());
    }

    #[test]
    fn a_holder_that_has_received_its_whole_body_is_not_stalled() {
        let gate = UploadGate::new(Duration::from_secs(15));
        let t0 = Instant::now();
        let slot = gate.try_take_at(ip(1), t0).unwrap();
        slot.mark_finishing();
        assert!(gate.try_take_at(ip(1), t0 + Duration::from_secs(120)).is_none());
    }

    #[test]
    fn the_slot_reader_counts_progress_and_stops_a_superseded_holder() {
        let gate = UploadGate::default();
        let slot = gate.try_take(ip(1)).unwrap();
        let mut src = std::io::Cursor::new(vec![1u8; 10]);
        let mut r = slot.reader(&mut src);
        let mut buf = [0u8; 4];
        assert_eq!(r.read(&mut buf).unwrap(), 4);
        // The end of the body marks the slot as finishing.
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(rest.len(), 6);
        assert!(gate
            .try_take_at(ip(1), Instant::now() + Duration::from_secs(600))
            .is_none());
        // A superseded holder reads no more.
        let gate = UploadGate::new(Duration::ZERO);
        let old = gate.try_take(ip(1)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let _new = gate.try_take(ip(1)).unwrap();
        let mut src = std::io::Cursor::new(vec![1u8; 10]);
        assert!(old.reader(&mut src).read(&mut buf).is_err());
    }

    fn rows(spec: &[(u8, u64)]) -> Vec<SessionRow> {
        spec.iter()
            .map(|&(owner, idle)| SessionRow {
                owner: ip(owner),
                idle: Duration::from_secs(idle),
            })
            .collect()
    }

    #[test]
    fn sessions_never_evict_another_clients_live_session() {
        let lim = SessionLimits::new(4);
        let live = 30;
        let idle = 20 * 60;
        // Room: just add.
        assert_eq!(place_session(&rows(&[(1, live)]), ip(2), &lim), Placement::Add);
        // Full of live sessions of two other clients: refused, nobody is evicted.
        let full_live = rows(&[(1, live), (1, live), (3, live), (3, live)]);
        assert_eq!(place_session(&full_live, ip(2), &lim), Placement::Refuse);
        // Full, one of them idle: that idle one (of whoever) is replaced.
        let one_idle = rows(&[(1, live), (1, idle), (3, live), (3, live)]);
        assert_eq!(place_session(&one_idle, ip(2), &lim), Placement::Replace(1));
        // The longest-idle one goes first.
        let two_idle = rows(&[(1, idle), (1, idle * 2), (3, live), (3, live)]);
        assert_eq!(place_session(&two_idle, ip(2), &lim), Placement::Replace(1));
    }

    #[test]
    fn a_client_at_its_own_cap_replaces_only_its_own_idle_session() {
        let lim = SessionLimits::new(4);
        let live = 30;
        let idle = 20 * 60;
        // Two live of its own: refused, even though the total has room.
        assert_eq!(
            place_session(&rows(&[(2, live), (2, live)]), ip(2), &lim),
            Placement::Refuse
        );
        // One of its own idle: replaced, not someone else's idle session.
        let r = rows(&[(1, idle * 3), (2, live), (2, idle)]);
        assert_eq!(place_session(&r, ip(2), &lim), Placement::Replace(2));
    }
}
