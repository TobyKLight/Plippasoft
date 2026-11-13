//! WS2811 LEDs for POC2 — 30 FPS framebuffer refresh, sent to the `plippa-leds` helper.
//!
//! Which LED is which comes from the board profile (`[led_streams]` + `[leds]`,
//! see `LEDPATCH.csv`); composition lives in [`crate::led_compose`]. This file owns the
//! composition cadence and hands finished frames to [`LedsClient`]; the helper owns the DMA
//! engine and the `CAP_SYS_RAWIO` that driving it needs (SYS-02). Nothing here opens
//! `/dev/mem`, and the frame loop never waits for the helper (see `led_client`).

use crate::led::{LedCue, PlayButtonLed, Rgb, TransportPulse};
use crate::led_client::LedsClient;
use crate::led_compose::{LedComposer, LedLayout};
use crate::record_leds::RecordFrame;
use plippa_board::{BoardProfile, LedRole};
use std::time::{Duration, Instant};

const FRAME_INTERVAL: Duration = Duration::from_millis(33); // ~30 FPS

/// Hardware LED cue: every stream rendered together at ~30 FPS and sent to the helper.
pub struct Poc2LedCue {
    /// `None` on a board profile without `[led_streams]`: nothing to compose.
    client: Option<LedsClient>,
    composer: LedComposer,
    last_frame: Instant,
    /// Force a render on the next tick even if interval has not elapsed.
    force_frame: bool,
}

impl Poc2LedCue {
    pub fn open(profile: &BoardProfile) -> Self {
        let layout = LedLayout::from_profile(profile);
        let composer = LedComposer::new(layout);
        let wire = plippa_leds::Layout::from_profile(profile);
        let client = if wire.streams().is_empty() {
            eprintln!("plippa-ui: board profile defines no [led_streams]; LED cue disabled");
            None
        } else {
            let paths = plippa_ipc_proto::resolve_paths();
            eprintln!(
                "plippa-ui: WS2811 strips are driven by the LED helper at {} (~30 FPS)",
                plippa_leds::socket_path(&paths.dir).display()
            );
            Some(LedsClient::new(&paths.dir, wire, Instant::now()))
        };
        let mut s = Self {
            client,
            composer,
            last_frame: Instant::now()
                .checked_sub(FRAME_INTERVAL)
                .unwrap_or_else(Instant::now),
            force_frame: true,
        };
        s.composer.compose(Instant::now());
        s.render_now();
        s
    }

    /// ~30 FPS: compose steady + animation layers and send all strips together.
    pub fn tick(&mut self) {
        if self.client.is_none() {
            return;
        }
        let now = Instant::now();
        if self.composer.advance(now) {
            self.force_frame = true;
        }
        if !self.force_frame && now.duration_since(self.last_frame) < FRAME_INTERVAL {
            return;
        }
        self.composer.compose(now);
        self.render_now();
        self.force_frame = false;
    }

    fn render_now(&mut self) {
        let Some(client) = self.client.as_mut() else {
            return;
        };
        let frames: Vec<Vec<[u8; 3]>> = self
            .composer
            .driver_frames()
            .into_iter()
            .map(|frame| frame.into_iter().map(|c| [c.r, c.g, c.b]).collect())
            .collect();
        client.send_frames(&frames, Instant::now());
        self.last_frame = Instant::now();
    }

    fn clear_all(&mut self) {
        self.composer.clear_all();
        self.composer.compose(Instant::now());
        self.force_frame = true;
        self.render_now();
    }
}

impl LedCue for Poc2LedCue {
    fn on_tap_feedback(&mut self, tap_id: u64) {
        eprintln!("plippa-ui: LED tap feedback tap_id={tap_id} (whoosh → pulse → fade)");
        self.composer.on_tap_feedback(Instant::now());
        self.force_frame = true;
    }

    fn tick(&mut self) {
        Poc2LedCue::tick(self);
    }

    fn set_play_button(&mut self, state: PlayButtonLed) {
        if self.composer.play_button() == state {
            return;
        }
        self.composer.set_play_button(state);
        eprintln!("plippa-ui: play button LED → {state:?}");
        // The next 30 FPS tick picks this up — do not block on the socket here when called
        // from the IPC path; force so colour changes feel instant.
        self.force_frame = true;
        self.tick();
    }

    fn has_role(&self, role: LedRole) -> bool {
        self.composer.layout().has(role)
    }

    fn set_role_colour(&mut self, role: LedRole, colour: Option<Rgb>) {
        self.composer.set_role_colour(role, colour);
        self.force_frame = true;
    }

    fn set_role_pulse(&mut self, role: LedRole, pulse: Option<TransportPulse>) {
        self.composer.set_role_pulse(role, pulse);
        // The 30 FPS tick animates it; force the first frame so the change is immediate.
        self.force_frame = true;
    }

    fn flash_role(&mut self, role: LedRole, colour: Rgb) {
        // Timed layer in the composer; `tick` expires it and re-renders (never blocks input).
        self.composer.flash_role(role, colour, Instant::now());
        self.force_frame = true;
    }

    /// Direct backend: the frame is queued for the helper here and now (there is no queue in
    /// front of it that could refuse it), so it always counts as delivered.
    fn set_record_art(&mut self, frame: Option<RecordFrame>) -> bool {
        self.composer.set_record_art(frame, Instant::now());
        self.force_frame = true;
        true
    }

    fn clear_all(&mut self) {
        eprintln!("plippa-ui: clearing WS2811 LEDs");
        Poc2LedCue::clear_all(self);
    }
}

impl Drop for Poc2LedCue {
    fn drop(&mut self) {
        self.clear_all();
        if let Some(client) = self.client.as_mut() {
            // A blank frame plus a close: the helper also blanks when the client disappears,
            // this just makes the strips go dark before the process is gone.
            client.close(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_board::BoardProfile;

    fn profile() -> BoardProfile {
        BoardProfile::load(plippa_board::workspace_root_from_crate(), "plippa-D-poc2")
            .expect("plippa-D-poc2 board profile")
    }

    /// The cue converts logical RGB to the helper's wire order and sends whole streams.
    #[test]
    fn a_tick_sends_every_stream_of_the_profile() {
        let cue = Poc2LedCue::open(&profile());
        let client = cue.client.as_ref().expect("poc2 has LED streams");
        assert_eq!(client.layout().streams().len(), 2);
        assert_eq!(client.layout().led_count(), 63);
    }

    /// Without `[led_streams]` there is nothing to compose and no helper to connect to.
    #[test]
    fn a_board_without_streams_disables_the_cue() {
        let mut profile = profile();
        profile.led_streams.clear();
        let mut cue = Poc2LedCue::open(&profile);
        assert!(cue.client.is_none());
        cue.tick();
        cue.on_tap_feedback(1);
    }
}
