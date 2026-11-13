//! Logical input events (hardware backends map encoders/buttons → these).

/// Normalised UI input after role mapping (board profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Raw menu encoder detent (one signed step per mechanical detent). UI_16 §7 inverted how
    /// the UI reads it in menu-style lists: the negative sign now moves **down/next** there
    /// (`nav::menu_step`). The jog ring, the Adjust band and the held-scan ladder keep the raw
    /// sign, so one direction that reads "down" in a menu still reads "up" on those controls.
    MenuRotate(i8),
    /// Menu encoder push (enter / short click).
    MenuEnter,
    /// Raw menu-encoder push edges for hold-duration hooks.
    MenuEnterButton { down: bool },
    /// Assignable Encoder A rotate — drives bound control.
    AssignableARotate(i8),
    /// Assignable Encoder A push edges (hold-to-assign in UI_04).
    AssignableEncoderPush { down: bool },
    /// Back button.
    Back,
    /// Play/stop toggle (the `Play` control role).
    PlayStop,
    /// Volume encoder detent.
    VolumeRotate(i8),
    /// Power / volume push pressed or released.
    PowerButton { down: bool },
    /// A press of a control role that has no fixed behaviour yet (prev/next/
    /// rewind/FF/record/play-mode); NB_14+ bind these. Role absent on the board = never sent.
    ControlPressed(plippa_board::ControlRole),
    /// Both edges of Play / Rewind / FastForward / PlayMode with a timestamp (WARP_06):
    /// `at_ms` is stamped by the I²C service on the loop epoch (`MockInput` tests stamp
    /// explicitly). Emitted **in addition to** the legacy events of those roles.
    ControlButton {
        role: plippa_board::ControlRole,
        down: bool,
        at_ms: u64,
    },
    /// Wall-clock tick for hold timers / clock refresh (milliseconds since start).
    Tick { now_ms: u64 },
}

/// Poll source for the continuous UI loop.
pub trait InputSource {
    fn poll(&mut self) -> Vec<InputEvent>;
}

/// Injects scripted events for unit tests.
#[derive(Debug, Default)]
pub struct MockInput {
    queue: Vec<InputEvent>,
}

impl MockInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, ev: InputEvent) {
        self.queue.push(ev);
    }

    pub fn extend<I: IntoIterator<Item = InputEvent>>(&mut self, events: I) {
        self.queue.extend(events);
    }

    pub fn pop(&mut self) -> Option<InputEvent> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }
}

impl InputSource for MockInput {
    fn poll(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.queue)
    }
}
