//! AW9523 switch IDs and edge debounce (no I²C — usable in mock/tests).

use std::time::{Duration, Instant};

/// Logical switch on the expander (POC2 wiring).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchId {
    BtSw1,
    BtSw2,
    BtSw3,
    BtSw4,
    BtSw5,
    BtSw6,
    Sw2,
    Sw6,
    Sw7,
    Swe1,
    Swe3,
    Swe4,
    Swe5,
}

impl SwitchId {
    pub const ALL: [SwitchId; 13] = [
        SwitchId::BtSw1,
        SwitchId::BtSw2,
        SwitchId::BtSw3,
        SwitchId::BtSw4,
        SwitchId::BtSw5,
        SwitchId::BtSw6,
        SwitchId::Sw2,
        SwitchId::Sw6,
        SwitchId::Sw7,
        SwitchId::Swe1,
        SwitchId::Swe3,
        SwitchId::Swe4,
        SwitchId::Swe5,
    ];

    pub fn port_bit(self) -> (u8, u8) {
        match self {
            SwitchId::BtSw1 => (0, 1 << 0),
            SwitchId::BtSw2 => (0, 1 << 1),
            SwitchId::BtSw3 => (0, 1 << 2),
            SwitchId::BtSw4 => (0, 1 << 3),
            SwitchId::BtSw5 => (0, 1 << 4),
            SwitchId::BtSw6 => (0, 1 << 5),
            SwitchId::Sw2 => (0, 1 << 6),
            SwitchId::Sw6 => (0, 1 << 7),
            SwitchId::Sw7 => (1, 1 << 0),
            SwitchId::Swe1 => (1, 1 << 1),
            SwitchId::Swe3 => (1, 1 << 2),
            SwitchId::Swe4 => (1, 1 << 3),
            SwitchId::Swe5 => (1, 1 << 4),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SwitchId::BtSw1 => "BT_SW1",
            SwitchId::BtSw2 => "BT_SW2",
            SwitchId::BtSw3 => "BT_SW3",
            SwitchId::BtSw4 => "BT_SW4",
            SwitchId::BtSw5 => "BT_SW5",
            SwitchId::BtSw6 => "BT_SW6",
            SwitchId::Sw2 => "SW2",
            SwitchId::Sw6 => "SW6",
            SwitchId::Sw7 => "SW7",
            SwitchId::Swe1 => "SWE1",
            SwitchId::Swe3 => "SWE3",
            SwitchId::Swe4 => "SWE4",
            SwitchId::Swe5 => "SWE5",
        }
    }

    /// Map expander pin name from board.toml (`SW2`, `SWE1_SW`, `AW9523_PA6`, …).
    pub fn from_board_name(name: &str) -> Option<Self> {
        match name {
            "SW2" | "AW9523_PA6" => Some(Self::Sw2),
            "SW6" | "AW9523_PA7" => Some(Self::Sw6),
            "SW7" | "AW9523_PB0" => Some(Self::Sw7),
            "SWE1" | "SWE1_SW" | "AW9523_PB1" => Some(Self::Swe1),
            "SWE3" | "SWE3_SW" | "AW9523_PB2" => Some(Self::Swe3),
            "SWE4" | "SWE4_SW" | "AW9523_PB3" => Some(Self::Swe4),
            "SWE5" | "SWE5_SW" | "AW9523_PB4" => Some(Self::Swe5),
            "BT_SW1" | "AW9523_PA0" => Some(Self::BtSw1),
            "BT_SW2" | "AW9523_PA1" => Some(Self::BtSw2),
            "BT_SW3" | "AW9523_PA2" => Some(Self::BtSw3),
            "BT_SW4" | "AW9523_PA3" => Some(Self::BtSw4),
            "BT_SW5" | "AW9523_PA4" => Some(Self::BtSw5),
            "BT_SW6" | "AW9523_PA5" => Some(Self::BtSw6),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchEvent {
    Pressed(SwitchId),
    Released(SwitchId),
}

/// Edge-triggered with cooldown. Fires on the first sample of a new level so
/// short taps are not lost when the consumer is slow. Debounce lives in the
/// I²C service — the coordinator never samples raw ports.
pub struct SwitchDebouncer {
    debounce: Duration,
    last_reported: [bool; 13],
    last_edge_at: [Option<Instant>; 13],
    primed: bool,
}

impl SwitchDebouncer {
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            debounce: Duration::from_millis(debounce_ms),
            last_reported: [false; 13],
            last_edge_at: [None; 13],
            primed: false,
        }
    }

    pub fn poll_events(&mut self, p0: u8, p1: u8) -> Vec<SwitchEvent> {
        self.poll_events_at(p0, p1, Instant::now())
    }

    /// Test seam: inject `now` for deterministic latency checks.
    pub fn poll_events_at(&mut self, p0: u8, p1: u8, now: Instant) -> Vec<SwitchEvent> {
        let ports = [p0, p1];
        let mut events = Vec::new();
        for (i, id) in SwitchId::ALL.iter().copied().enumerate() {
            let (port, bit) = id.port_bit();
            let pressed = ports[port as usize] & bit == 0;
            if pressed == self.last_reported[i] {
                continue;
            }
            if let Some(at) = self.last_edge_at[i] {
                if now.duration_since(at) < self.debounce {
                    continue;
                }
            }
            self.last_edge_at[i] = Some(now);
            if pressed {
                events.push(SwitchEvent::Pressed(id));
            } else if self.primed {
                events.push(SwitchEvent::Released(id));
            }
            self.last_reported[i] = pressed;
        }
        self.primed = true;
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt_sw3_maps_from_board_name() {
        assert_eq!(SwitchId::from_board_name("BT_SW3"), Some(SwitchId::BtSw3));
    }

    #[test]
    fn edge_fires_on_first_pressed_sample() {
        let mut d = SwitchDebouncer::new(20);
        let t0 = Instant::now();
        // All high (released), active-low: press BT_SW3 = clear bit 2 on port 0.
        let idle = (0xFF, 0xFF);
        assert!(d.poll_events_at(idle.0, idle.1, t0).is_empty());
        let pressed = (0xFF & !(1 << 2), 0xFF);
        let ev = d.poll_events_at(pressed.0, pressed.1, t0 + Duration::from_millis(1));
        assert_eq!(ev, vec![SwitchEvent::Pressed(SwitchId::BtSw3)]);
    }
}
