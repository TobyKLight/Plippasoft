//! System > Audio (AUDIO_04): the persisted audio settings, the screen rows and the
//! engine-route mirror behind them.
//!
//! File (`PLIPPA_STATE_DIR/audio_outputs.json`, else `XDG_STATE_HOME/plippa/`, default
//! `~/.local/state/plippa/`), written atomically. Six keys:
//!
//! ```json
//! {"local_only":false,"extras":["alsa_output.usb"],"delay_auto":true,
//!  "delays_ms":{"alsa_output.usb":40},"default_volume":80,"waveform_vis_delay":true}
//! ```
//!
//! `extras` holds every ticked output **except the built-in one** (series decision 7: the
//! local tick is session-only, "Local only" persists). `route.enabled` is never persisted
//! wholesale. `delays_ms` is the user's stored manual trim per output, which is also what the
//! Delay screen shows (`AudioRoute::manual_delay_ms`), not the engine-applied
//! `AudioOutput::delay_ms` (0 for every output in Auto mode). `default_volume` is the main
//! volume sent once at start-up; the front knob changes the live volume, not this setting.
//!
//! Loading is lenient in the same way as `play_mode.json`: a missing, unreadable or garbage
//! file is the defaults, an unknown key is ignored and every key is judged on its own, so a
//! bad value resets only that setting.
//!
//! The screens render [`AudioView`], which mirrors the engine's last `AudioRoute` plus the
//! persisted settings. A toggle sends a command; the row changes when the engine publishes
//! the new route, never optimistically.

use crate::accessories::AccessoriesState;
use crate::assign::state_file_path;
use plippa_core::audio_route::{
    AudioOutput, AudioRoute, AudioRouteWanted, DELAY_MAX_MS, DELAY_STEP_MS,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// File name inside the state directory.
pub const AUDIO_FILE_NAME: &str = "audio_outputs.json";

/// The persisted audio choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    /// "Local only" mode: the built-in output, lowest latency, no delay.
    pub local_only: bool,
    /// Ids of every ticked output **except** the built-in one.
    pub extras: Vec<String>,
    /// Automatic compensation (engine default on).
    pub delay_auto: bool,
    /// User-set per-output trim, ms (`AudioRoute::manual_delay_ms`).
    pub delays_ms: BTreeMap<String, u32>,
    /// Main volume sent once at start-up (`0..=100`, default 80).
    pub default_volume: u8,
    /// UI-only waveform / jog visual delay (AUDIO_05 applies it; never an engine command).
    pub waveform_vis_delay: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            local_only: false,
            extras: Vec::new(),
            // Series decision 6 / the engine's first-run default.
            delay_auto: true,
            delays_ms: BTreeMap::new(),
            default_volume: 80,
            // The picture matches the sound unless the owner turns it off.
            waveform_vis_delay: true,
        }
    }
}

impl AudioSettings {
    /// File text. Every key is written, so a reload sees the same six choices.
    pub fn to_json(&self) -> String {
        let mut m = serde_json::Map::new();
        m.insert("local_only".into(), self.local_only.into());
        m.insert("extras".into(), self.extras.clone().into());
        m.insert("delay_auto".into(), self.delay_auto.into());
        let delays: serde_json::Map<String, Value> = self
            .delays_ms
            .iter()
            .map(|(id, ms)| (id.clone(), Value::from(*ms)))
            .collect();
        m.insert("delays_ms".into(), Value::Object(delays));
        m.insert("default_volume".into(), self.default_volume.into());
        m.insert("waveform_vis_delay".into(), self.waveform_vis_delay.into());
        serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_else(|_| "{}".into())
    }

    /// Parse leniently: each key is judged on its own, unknown keys are ignored, a bad value
    /// follows the default for that key only.
    pub fn from_json(text: &str) -> Self {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        let d = Self::default();
        let extras = v
            .get("extras")
            .and_then(Value::as_array)
            .map(|a| {
                let mut out: Vec<String> = Vec::new();
                for id in a.iter().filter_map(Value::as_str) {
                    let id = id.trim();
                    if !id.is_empty() && !out.iter().any(|k| k == id) {
                        out.push(id.to_string());
                    }
                }
                out
            })
            .unwrap_or_default();
        let delays_ms = v
            .get("delays_ms")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(id, ms)| {
                        let ms = ms.as_u64()?;
                        let ms = u32::try_from(ms).unwrap_or(u32::MAX).min(DELAY_MAX_MS);
                        Some((id.clone(), ms / DELAY_STEP_MS * DELAY_STEP_MS))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            local_only: v
                .get("local_only")
                .and_then(Value::as_bool)
                .unwrap_or(d.local_only),
            extras,
            delay_auto: v
                .get("delay_auto")
                .and_then(Value::as_bool)
                .unwrap_or(d.delay_auto),
            delays_ms,
            default_volume: v
                .get("default_volume")
                .and_then(Value::as_u64)
                .map(|n| n.min(100) as u8)
                .unwrap_or(d.default_volume),
            waveform_vis_delay: v
                .get("waveform_vis_delay")
                .and_then(Value::as_bool)
                .unwrap_or(d.waveform_vis_delay),
        }
    }
}

/// Settings + optional persist path (same shape as `PlayModeStore`).
#[derive(Debug, Clone, Default)]
pub struct AudioStore {
    path: Option<PathBuf>,
    pub settings: AudioSettings,
}

impl AudioStore {
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            settings: AudioSettings::default(),
        }
    }

    /// `PLIPPA_STATE_DIR/audio_outputs.json`, else the XDG state dir.
    pub fn default_path() -> PathBuf {
        state_file_path(AUDIO_FILE_NAME)
    }

    /// Load `path`; a missing or unreadable file is "nothing chosen".
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let settings = match fs::read_to_string(path) {
            Ok(text) => AudioSettings::from_json(&text),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    eprintln!(
                        "plippa-ui: audio_outputs load `{}`: {e} - defaults",
                        path.display()
                    );
                }
                AudioSettings::default()
            }
        };
        Self {
            path: Some(path.to_path_buf()),
            settings,
        }
    }

    pub fn load_default() -> Self {
        Self::load_or_default(Self::default_path())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Atomic write (temp + rename). No-op without a path.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, self.settings.to_json())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Rows of the System > Audio screen, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRow {
    /// Opens `Route::SystemAudioOutputs`.
    Outputs,
    /// Opens `Route::SystemAudioDelay`.
    Delay,
    /// Edits the main volume sent once at start-up.
    DefaultVolume,
    /// ACC_03: opens `Route::SystemAudioAccessories`.
    Accessories,
    /// Toggles the UI-only visual delay.
    WaveformVisDelay,
}

/// One const list drives the Audio screen, its cursor wrap, Enter and the descriptions.
pub const AUDIO_ROWS: [AudioRow; 5] = [
    AudioRow::Outputs,
    AudioRow::Delay,
    AudioRow::DefaultVolume,
    AudioRow::Accessories,
    AudioRow::WaveformVisDelay,
];

impl AudioRow {
    pub fn label(self) -> &'static str {
        match self {
            AudioRow::Outputs => "Output devices",
            AudioRow::Delay => "Delay",
            AudioRow::DefaultVolume => "Default volume",
            AudioRow::Accessories => "Accessories",
            AudioRow::WaveformVisDelay => "Waveform vis delay",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            AudioRow::Outputs => {
                "Which outputs play. The built-in output always keeps the lowest latency."
            }
            AudioRow::Delay => {
                "Automatic compensation, or your own delay in ms per output. Only matters with more than one output."
            }
            AudioRow::DefaultVolume => {
                "The main volume at start-up. The front knob changes the live volume, not this setting."
            }
            AudioRow::Accessories => {
                "Paired 2.4 GHz accessories. A connected one can be chosen in Output devices."
            }
            AudioRow::WaveformVisDelay => {
                "Shift the waveform and jog display by the slowest output's delay, so the picture matches what you hear. Needs more than one output and Manual delay; a change steps the picture once."
            }
        }
    }

    pub fn value_text(self, view: &AudioView, accessories: &AccessoriesState) -> String {
        match self {
            AudioRow::Outputs => view.output_summary(),
            AudioRow::Delay => view.delay_mode_text().into(),
            AudioRow::DefaultVolume => format!("{}%", view.settings.default_volume),
            AudioRow::Accessories => String::new(),
            AudioRow::WaveformVisDelay => view.vis_delay_text().into(),
        }
    }
}

/// One detent on the Default volume row: ±5 %, clamped to `0..=100`, never wrapped.
pub fn volume_step(current: u8, delta: i8) -> u8 {
    let step = i32::from(delta) * 5;
    (i32::from(current) + step).clamp(0, 100) as u8
}

/// One detent on a delay row: ±[`DELAY_STEP_MS`], clamped to `0..=DELAY_MAX_MS`, never wrapped.
///
/// A delay is a plain quantity rather than a list, so turning the encoder towards the higher
/// end raises it; the enter-then-scroll interaction itself is the same as Fine time.
pub fn delay_step(current: u32, delta: i8) -> u32 {
    let step = i32::from(delta) * DELAY_STEP_MS as i32;
    (current as i32 + step).clamp(0, DELAY_MAX_MS as i32) as u32
}

/// Why the per-output delay rows cannot be edited right now, `None` when they can.
/// The same reason is drawn as the description and shown in the popup on Enter.
pub fn delay_blocked(route: &AudioRoute) -> Option<&'static str> {
    if route.local_only {
        Some("Local only is on. Turn it off to route more than one output.")
    } else if route.delay_auto {
        Some("Automatic compensation is on. Turn Auto off to set a delay.")
    } else if route.effective_enabled().len() < 2 {
        Some("Delay only matters with more than one output.")
    } else {
        None
    }
}

/// The engine's last route plus the persisted settings, as the Audio screens render it.
#[derive(Debug, Clone)]
pub struct AudioView {
    pub route: AudioRoute,
    pub settings: AudioSettings,
    /// Row of the Delay screen whose value is being edited by scrolling (`None` = none).
    pub editing: Option<usize>,
}

impl Default for AudioView {
    fn default() -> Self {
        Self {
            route: AudioRoute {
                delay_auto: true,
                ..AudioRoute::default()
            },
            settings: AudioSettings::default(),
            editing: None,
        }
    }
}

impl AudioView {
    /// The Audio row value for `Output devices`.
    pub fn output_summary(&self) -> String {
        if self.route.local_only {
            "Local only".into()
        } else {
            format!("{} on", self.route.effective_enabled().len())
        }
    }

    /// `Auto` / `Manual`.
    pub fn delay_mode_text(&self) -> &'static str {
        if self.route.delay_auto {
            "Auto"
        } else {
            "Manual"
        }
    }

    /// `On` / `Off`.
    pub fn vis_delay_text(&self) -> &'static str {
        on_off(self.settings.waveform_vis_delay)
    }

    /// Outputs in list order: the built-in one first, the rest by display name.
    pub fn sorted_outputs(&self) -> Vec<AudioOutput> {
        let mut outs = self.route.outputs.clone();
        outs.sort_by(|a, b| {
            b.is_local
                .cmp(&a.is_local)
                .then_with(|| a.description.to_lowercase().cmp(&b.description.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });
        outs
    }

    /// The outputs the engine reports as effectively enabled, in route order.
    pub fn enabled_outputs(&self) -> Vec<AudioOutput> {
        self.route
            .effective_enabled()
            .iter()
            .filter_map(|id| self.route.output(id).cloned())
            .collect()
    }

    /// The user's stored manual delay for `id` (`0` when unset).
    pub fn delay_ms(&self, id: &str) -> u32 {
        self.route
            .manual_delay_ms
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// Whether `id` is effectively enabled right now.
    pub fn is_enabled(&self, id: &str) -> bool {
        self.route.enabled.iter().any(|e| e == id)
    }

    /// A non-local output enabled by the user but paused by `local_only`.
    pub fn is_paused(&self, id: &str) -> bool {
        self.route.local_only && self.route.paused.iter().any(|p| p == id)
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

/// Build the wanted set implied by the last route, for `AudioRoute::toggled`.
pub fn wanted_from_route(route: &AudioRoute) -> AudioRouteWanted {
    AudioRouteWanted {
        enabled: route.enabled.clone(),
        local_only: route.local_only,
        delay_auto: route.delay_auto,
        manual_delay_ms: route.manual_delay_ms.clone(),
    }
}

/// The session state behind the screens: the engine's route, the delay or default-volume row
/// being edited and whether the startup reconciler has run.
#[derive(Debug, Clone)]
pub struct AudioState {
    pub route: AudioRoute,
    /// Row of the Delay screen being edited (`1..=enabled.len()`; row 0 is Auto).
    pub edit: Option<usize>,
    /// The Default volume row on the Audio screen is in its enter-then-scroll editor.
    pub volume_edit: bool,
    /// The persisted extras/local-only were sent once after the first route.
    pub sent_initial: bool,
}

impl Default for AudioState {
    fn default() -> Self {
        Self {
            route: AudioRoute {
                delay_auto: true,
                ..AudioRoute::default()
            },
            edit: None,
            volume_edit: false,
            sent_initial: false,
        }
    }
}

impl AudioState {
    /// Replace the mirrored route. An edit stops when its row no longer exists or the route
    /// has made the delay rows uneditable (Auto on, one output, Local only).
    pub fn apply_route(&mut self, route: AudioRoute) {
        self.route = route;
        let gone = self
            .edit
            .is_some_and(|row| row == 0 || row > self.route.effective_enabled().len());
        if gone || delay_blocked(&self.route).is_some() {
            self.edit = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn out(id: &str, description: &str, local: bool) -> AudioOutput {
        AudioOutput {
            id: id.into(),
            description: description.into(),
            available: true,
            is_local: local,
            ..AudioOutput::default()
        }
    }

    fn route() -> AudioRoute {
        AudioRoute {
            outputs: vec![
                out("usb", "USB DAC", false),
                out("local", "Built-in", true),
                out("hdmi", "HDMI", false),
            ],
            enabled: vec!["local".into(), "usb".into(), "hdmi".into()],
            delay_auto: false,
            ..AudioRoute::default()
        }
    }

    #[test]
    fn defaults_are_auto_with_visual_delay_on_and_80_percent() {
        let d = AudioSettings::default();
        assert!(!d.local_only);
        assert!(d.extras.is_empty());
        assert!(d.delay_auto, "automatic compensation is the first-run default");
        assert!(d.delays_ms.is_empty());
        assert_eq!(d.default_volume, 80);
        assert!(d.waveform_vis_delay);
    }

    #[test]
    fn round_trip_keeps_all_six_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(AUDIO_FILE_NAME);
        let mut store = AudioStore::with_path(&path);
        store.settings.local_only = true;
        store.settings.extras = vec!["usb".into(), "hdmi".into()];
        store.settings.delay_auto = false;
        store.settings.delays_ms.insert("usb".into(), 40);
        store.settings.delays_ms.insert("hdmi".into(), 120);
        store.settings.default_volume = 65;
        store.settings.waveform_vis_delay = false;
        store.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        for key in [
            "local_only",
            "extras",
            "delay_auto",
            "delays_ms",
            "default_volume",
            "waveform_vis_delay",
        ] {
            assert!(text.contains(key), "`{key}` missing: {text}");
        }
        assert!(!path.with_extension("json.tmp").exists(), "temp renamed");
        let loaded = AudioStore::load_or_default(&path);
        assert_eq!(loaded.settings, store.settings);
    }

    #[test]
    fn garbage_only_resets_the_bad_key_and_unknown_keys_are_ignored() {
        let s = AudioSettings::from_json(
            r#"{"local_only":"yes","extras":["usb"],"delay_auto":false,
                "delays_ms":{"usb":40,"bogus":"loud"},"default_volume":"loud",
                "waveform_vis_delay":1,"future_key":{"nested":true}}"#,
        );
        assert!(!s.local_only, "garbage bool follows the default");
        assert_eq!(s.extras, vec!["usb".to_string()], "valid key survives");
        assert!(!s.delay_auto, "valid key survives");
        assert_eq!(s.delays_ms.get("usb"), Some(&40));
        assert_eq!(s.delays_ms.len(), 1, "a garbage entry is dropped");
        assert_eq!(s.default_volume, 80, "a garbage volume follows the default");
        assert!(s.waveform_vis_delay, "garbage bool follows the default");
        assert_eq!(
            AudioSettings::from_json("not json {{"),
            AudioSettings::default()
        );
        let delayed = AudioSettings::from_json(r#"{"delays_ms":{"usb":1234}}"#);
        assert_eq!(delayed.delays_ms.get("usb"), Some(&1_000), "clamped");
        let huge =
            AudioSettings::from_json(r#"{"delays_ms":{"usb":18446744073709551615}}"#);
        assert_eq!(
            huge.delays_ms.get("usb"),
            Some(&1_000),
            "a u64 past u32::MAX clamps instead of truncating"
        );
        let off_ladder = AudioSettings::from_json(r#"{"delays_ms":{"usb":37}}"#);
        assert_eq!(off_ladder.delays_ms.get("usb"), Some(&30), "10 ms steps");
        let loud = AudioSettings::from_json(r#"{"default_volume":250}"#);
        assert_eq!(loud.default_volume, 100, "clamped to 100");
        let negative = AudioSettings::from_json(r#"{"default_volume":-5}"#);
        assert_eq!(negative.default_volume, 80, "a non-u64 falls back to 80");
    }

    #[test]
    fn row_texts_follow_the_engine_route_and_settings() {
        let view = AudioView {
            route: route(),
            settings: AudioSettings::default(),
            editing: None,
        };
        let accessories = AccessoriesState::default();
        assert_eq!(
            AUDIO_ROWS,
            [
                AudioRow::Outputs,
                AudioRow::Delay,
                AudioRow::DefaultVolume,
                AudioRow::Accessories,
                AudioRow::WaveformVisDelay
            ]
        );
        assert_eq!(AudioRow::Outputs.value_text(&view, &accessories), "3 on");
        assert_eq!(AudioRow::Delay.value_text(&view, &accessories), "Manual");
        assert_eq!(
            AudioRow::DefaultVolume.value_text(&view, &accessories),
            "80%"
        );
        assert_eq!(
            AudioRow::Accessories.value_text(&view, &accessories),
            "",
            "the Accessories row has no summary value"
        );
        assert_eq!(
            AudioRow::WaveformVisDelay.value_text(&view, &accessories),
            "On"
        );
        assert_eq!(AUDIO_ROWS[0].label(), "Output devices");
        assert_eq!(AUDIO_ROWS[1].label(), "Delay");
        assert_eq!(AUDIO_ROWS[2].label(), "Default volume");
        assert_eq!(AUDIO_ROWS[3].label(), "Accessories");
        assert_eq!(AUDIO_ROWS[4].label(), "Waveform vis delay");
        let mut local = view.clone();
        local.route.local_only = true;
        assert_eq!(
            AudioRow::Outputs.value_text(&local, &accessories),
            "Local only"
        );
        let mut auto = view.clone();
        auto.route.delay_auto = true;
        assert_eq!(AudioRow::Delay.value_text(&auto, &accessories), "Auto");
    }

    #[test]
    fn sorted_outputs_put_local_first_then_display_name() {
        let view = AudioView {
            route: route(),
            ..AudioView::default()
        };
        let sorted = view.sorted_outputs();
        let ids: Vec<&str> = sorted.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["local", "hdmi", "usb"]);
        let enabled = view.enabled_outputs();
        let enabled: Vec<&str> = enabled.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(enabled, ["local", "usb", "hdmi"], "route order kept");
    }

    #[test]
    fn delay_step_is_ten_ms_and_clamps_without_wrapping() {
        assert_eq!(delay_step(0, 1), 10);
        assert_eq!(delay_step(0, 4), 40);
        assert_eq!(delay_step(40, -4), 0);
        assert_eq!(delay_step(0, -1), 0, "clamped at 0");
        assert_eq!(delay_step(1_000, 1), 1_000, "clamped at the max");
        assert_eq!(delay_step(990, 127), 1_000);
        assert_eq!(delay_step(10, -128), 0);
    }

    #[test]
    fn volume_step_is_five_percent_and_clamps_without_wrapping() {
        assert_eq!(volume_step(80, 1), 85);
        assert_eq!(volume_step(80, -1), 75);
        assert_eq!(volume_step(80, 0), 80);
        assert_eq!(volume_step(0, -1), 0, "clamped at 0");
        assert_eq!(volume_step(100, 1), 100, "clamped at 100");
        assert_eq!(volume_step(5, -128), 0);
        assert_eq!(volume_step(95, 127), 100);
    }

    #[test]
    fn delay_rows_are_blocked_in_the_documented_cases() {
        let mut r = route();
        assert_eq!(delay_blocked(&r), None);
        r.delay_auto = true;
        assert!(delay_blocked(&r).unwrap().contains("Automatic"));
        r.delay_auto = false;
        r.enabled.truncate(1);
        assert!(delay_blocked(&r).unwrap().contains("one output"));
        r.enabled = vec!["local".into(), "usb".into()];
        r.local_only = true;
        assert!(delay_blocked(&r).unwrap().contains("Local only"));
    }

    #[test]
    fn apply_route_drops_an_edit_row_that_no_longer_exists() {
        let mut state = AudioState::default();
        state.apply_route(route());
        state.edit = Some(3);
        let mut shrunk = route();
        shrunk.enabled.truncate(1);
        state.apply_route(shrunk);
        assert_eq!(state.edit, None);
    }
}
