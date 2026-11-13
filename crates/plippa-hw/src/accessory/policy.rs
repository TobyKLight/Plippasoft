//! Pure accessory policy: scan countdown, auto-connect choice, retry budget and address join.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::{normalise_address, Accessory, AccessoryId, AccessoryKind};

/// Longest scan the UI will ask the backend for.
pub const MAX_SCAN_SECONDS: u32 = 60;

/// Attempts allowed per accessory id for the lifetime of the process.
pub const MAX_TRIES: u32 = 3;

/// Minimum time between auto-connect attempts for one accessory.
pub const RETRY_SPACING: Duration = Duration::from_secs(5);

/// Countdown for an accessory scan, clamped to [`MAX_SCAN_SECONDS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanWindow {
    started: Duration,
    length: Duration,
}

impl ScanWindow {
    pub fn start(now: Duration, seconds: u32) -> Self {
        Self {
            started: now,
            length: Duration::from_secs(u64::from(seconds.min(MAX_SCAN_SECONDS))),
        }
    }

    pub fn is_open(&self, now: Duration) -> bool {
        now.saturating_sub(self.started) < self.length
    }

    pub fn remaining(&self, now: Duration) -> Duration {
        self.length
            .saturating_sub(now.saturating_sub(self.started))
    }

    /// Close the window now; `remaining` becomes zero.
    pub fn close(&mut self) {
        self.length = Duration::ZERO;
    }

    /// Requested window length after clamping.
    pub fn seconds(&self) -> u32 {
        self.length.as_secs() as u32
    }
}

/// Return the accessory that should be auto-connected, if any.
///
/// `Some` only when no accessory is connected and a paired, not-connected audio accessory is
/// marked in `auto_ids`. The lowest id wins so the same device is picked every boot.
pub fn auto_connect_target<'a>(
    accessories: &'a [Accessory],
    auto_ids: &BTreeSet<AccessoryId>,
) -> Option<&'a Accessory> {
    if accessories.iter().any(|a| a.connected) {
        return None;
    }
    accessories
        .iter()
        .filter(|a| {
            a.kind == AccessoryKind::Audio
                && a.paired
                && !a.connected
                && auto_ids.contains(&normalise_address(&a.id))
        })
        .min_by(|a, b| normalise_address(&a.id).cmp(&normalise_address(&b.id)))
}

/// Per-accessory auto-connect attempts for one process lifetime.
#[derive(Debug, Clone, Default)]
pub struct AutoConnectBudget {
    entries: BTreeMap<AccessoryId, BudgetEntry>,
}

#[derive(Debug, Clone, Copy, Default)]
struct BudgetEntry {
    tries: u32,
    last_try: Option<Duration>,
}

impl AutoConnectBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one attempt for `id` if it has tries left and enough time has passed.
    pub fn try_take(&mut self, now: Duration, id: &AccessoryId) -> bool {
        let id = normalise_address(id);
        let entry = self.entries.entry(id).or_default();
        if entry.tries >= MAX_TRIES {
            return false;
        }
        if let Some(last) = entry.last_try {
            if now.saturating_sub(last) < RETRY_SPACING {
                return false;
            }
        }
        entry.tries += 1;
        entry.last_try = Some(now);
        true
    }

    /// Attempts taken for `id` so far.
    pub fn tries(&self, id: &AccessoryId) -> u32 {
        self.entries
            .get(&normalise_address(id))
            .map_or(0, |entry| entry.tries)
    }
}

/// True when any property value in `props` is the normalised form of `address`.
///
/// Keys are deliberately ignored: this is a generic join between a radio device and whatever
/// output property happens to carry its address, not a dependency on a property name.
pub fn props_match_address(props: &BTreeMap<String, String>, address: &str) -> bool {
    let want = normalise_address(address);
    if want.is_empty() {
        return false;
    }
    props.values().any(|value| normalise_address(value) == want)
}
