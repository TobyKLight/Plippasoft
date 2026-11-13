//! Audio output routing model (AUDIO_01).
//!
//! These types describe *what* the audio graph should do. The engine owns the state
//! machine (`plippa-engine`'s `RouteManager`); the UI mirrors
//! [`AudioRouteWanted`], the engine publishes [`AudioRoute`], and the PipeWire
//! backend (AUDIO_02) turns a [`Target`] into real modules. No PipeWire type is
//! referenced here, so the whole model is testable on a desktop.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Hard cap on enabled outputs: the CM4 has to convert per member (series decision 5).
pub const MAX_ENABLED_OUTPUTS: usize = 4;
/// Manual delay granularity (ms).
pub const DELAY_STEP_MS: u32 = 10;
/// Largest per-output delay the UI/engine accepts (ms).
pub const DELAY_MAX_MS: u32 = 1_000;
/// Sink-name prefix of our own plumbing nodes (e.g. the combining sink). These are
/// never offered to the user. Sink-name suffix / prefix rules live in
/// `plippa-audio::route`; this is the internal-name rule the core model needs.
pub const INTERNAL_SINK_PREFIX: &str = "plippa.";

/// One selectable audio output (a PipeWire sink).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioOutput {
    /// PipeWire sink name (`node.name`). Stable identity; this is the id everywhere.
    pub id: String,
    /// Sanitised `device.description` (SEC-12 sanitising happens in the UI, not here).
    pub description: String,
    /// Server reports it usable right now (not suspended-by-removal, not unplugged).
    pub available: bool,
    /// True for the board's built-in DAC.
    pub is_local: bool,
    /// Total delay this output adds, ms. Engine-computed (auto or manual). 0 = none.
    pub delay_ms: u32,
    /// Opaque sink properties (the accessory join reads `device.bus` / address here).
    pub props: BTreeMap<String, String>,
}

/// What the user asked for. Mirrored from the UI, validated by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioRouteWanted {
    /// Output ids to enable. Includes the local id unless the user unticked it this session.
    /// Order is not meaningful; the engine keeps local first.
    pub enabled: Vec<String>,
    pub local_only: bool,
    /// true = server compensates automatically, per-output manual delays ignored.
    pub delay_auto: bool,
    pub manual_delay_ms: BTreeMap<String, u32>,
}

/// What is actually routed. Broadcast to every client.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioRoute {
    pub outputs: Vec<AudioOutput>,
    /// Effective enabled ids, local first. Empty only while discovery has found no sink.
    pub enabled: Vec<String>,
    /// Enabled but currently paused by `local_only`, so the UI can show them greyed
    /// instead of pretending the user unticked them.
    pub paused: Vec<String>,
    pub local_only: bool,
    pub delay_auto: bool,
    pub manual_delay_ms: BTreeMap<String, u32>,
    /// Diagnostics for the Debug screen and one-line `problem` for the Audio screen.
    pub problem: Option<String>,
    pub revision: u64,
}

impl AudioRoute {
    /// The ids actually playing right now (already local-only collapsed by the engine).
    pub fn effective_enabled(&self) -> &[String] {
        &self.enabled
    }

    /// More than one output is effectively playing.
    pub fn is_multi(&self) -> bool {
        self.enabled.len() > 1
    }

    /// The board's built-in DAC, if discovery found it.
    pub fn local(&self) -> Option<&AudioOutput> {
        self.outputs.iter().find(|o| o.is_local)
    }

    /// Largest added delay among the effective outputs, 0 unless multi (AUDIO_05).
    pub fn vis_delay_ms(&self) -> u32 {
        if !self.is_multi() {
            return 0;
        }
        self.enabled
            .iter()
            .filter_map(|id| self.output(id))
            .map(|o| o.delay_ms)
            .max()
            .unwrap_or(0)
    }

    /// Look up one output by id.
    pub fn output(&self, id: &str) -> Option<&AudioOutput> {
        self.outputs.iter().find(|o| o.id == id)
    }

    /// Flip one output in a wanted set, keeping the toggle rule in one place.
    ///
    /// * Ticking an output clears `local_only` (the user asked for more than local).
    /// * Under `local_only` the engine keeps the local id first and lists the extras
    ///   after it as `paused`; toggling one of those extras is a *tick*, so it clears
    ///   `local_only` and leaves the extra enabled instead of removing it.
    /// * Unticking the last remaining output keeps it enabled: the effective set is
    ///   never empty, so playback never becomes silent by an untick. Discovery finding
    ///   no sink at all is the only case where the enabled set may be empty (also
    ///   enforced by [`sanitise_wanted`]).
    pub fn toggled(wanted: &AudioRouteWanted, id: &str) -> AudioRouteWanted {
        let mut out = wanted.clone();
        if let Some(pos) = out.enabled.iter().position(|e| e == id) {
            if out.local_only && pos > 0 {
                out.local_only = false;
            } else if out.enabled.len() > 1 {
                out.enabled.remove(pos);
            }
        } else {
            out.enabled.push(id.to_string());
            out.local_only = false;
        }
        out
    }
}

/// Something the engine had to change about a wanted route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioRouteProblem {
    /// More ids than [`MAX_ENABLED_OUTPUTS`]; the tail was dropped.
    LimitExceeded,
    /// The id is not one of the currently known outputs.
    UnknownOutput(String),
    /// A manual delay was clamped to [`DELAY_MAX_MS`] and rounded down to
    /// [`DELAY_STEP_MS`].
    DelayClamped(String),
}

/// Validate a wanted route against the outputs that currently exist.
///
/// Drops unknown ids (reporting each once), dedupes, puts the local output first,
/// truncates to [`MAX_ENABLED_OUTPUTS`], clamps manual delays to
/// [`DELAY_MAX_MS`] / [`DELAY_STEP_MS`], and keeps the enabled set non-empty while a
/// sink exists. Mutates `wanted` in place and returns what it had to change; the
/// engine turns the actionable problems into a `UserNotice` (a clamp is benign, and
/// the clamped value is already published).
pub fn sanitise_wanted(
    wanted: &mut AudioRouteWanted,
    outputs: &[AudioOutput],
) -> Vec<AudioRouteProblem> {
    let mut problems = Vec::new();
    let local_id = outputs.iter().find(|o| o.is_local).map(|o| o.id.clone());

    let mut kept: Vec<String> = Vec::new();
    for id in std::mem::take(&mut wanted.enabled) {
        if !outputs.iter().any(|o| o.id == id) {
            problems.push(AudioRouteProblem::UnknownOutput(id));
        } else if !kept.iter().any(|k| k == &id) {
            kept.push(id);
        }
    }
    if let Some(local) = &local_id {
        if let Some(pos) = kept.iter().position(|id| id == local) {
            let local = kept.remove(pos);
            kept.insert(0, local);
        }
    }
    if kept.len() > MAX_ENABLED_OUTPUTS {
        kept.truncate(MAX_ENABLED_OUTPUTS);
        problems.push(AudioRouteProblem::LimitExceeded);
    }
    // The enabled set is empty only while discovery has found no sink. If there is
    // something to play to, fall back to the local output (then any available sink),
    // so an empty request can never mean silence and the last-output rule lives here.
    if kept.is_empty() {
        if let Some(local) = outputs.iter().find(|o| o.is_local && o.available) {
            kept.push(local.id.clone());
        } else if let Some(available) = outputs.iter().find(|o| o.available) {
            kept.push(available.id.clone());
        }
    }
    wanted.enabled = kept;

    for (id, ms) in wanted.manual_delay_ms.iter_mut() {
        let clamped = (*ms).min(DELAY_MAX_MS) / DELAY_STEP_MS * DELAY_STEP_MS;
        if clamped != *ms {
            problems.push(AudioRouteProblem::DelayClamped(id.clone()));
            *ms = clamped;
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(id: &str, is_local: bool) -> AudioOutput {
        AudioOutput {
            id: id.into(),
            description: id.into(),
            available: true,
            is_local,
            ..Default::default()
        }
    }

    fn outputs() -> Vec<AudioOutput> {
        vec![
            out("a", false),
            out("b", false),
            out("c", false),
            out("d", false),
            out("local", true),
        ]
    }

    #[test]
    fn sanitise_keeps_four_with_local_first_and_reports_the_limit() {
        let mut wanted = AudioRouteWanted {
            enabled: vec!["a".into(), "b".into(), "c".into(), "d".into(), "local".into()],
            ..Default::default()
        };
        let problems = sanitise_wanted(&mut wanted, &outputs());
        assert_eq!(wanted.enabled, ["local", "a", "b", "c"]);
        assert!(problems.contains(&AudioRouteProblem::LimitExceeded));
    }

    #[test]
    fn sanitise_clamps_delays_and_drops_unknown_ids() {
        let mut wanted = AudioRouteWanted {
            enabled: vec!["a".into(), "ghost".into(), "a".into()],
            ..Default::default()
        };
        wanted.manual_delay_ms.insert("a".into(), 1_033);
        wanted.manual_delay_ms.insert("b".into(), 3);
        let problems = sanitise_wanted(&mut wanted, &outputs());
        assert_eq!(wanted.enabled, ["a"], "deduped and the unknown id is gone");
        assert!(problems.contains(&AudioRouteProblem::UnknownOutput("ghost".into())));
        assert!(problems.contains(&AudioRouteProblem::DelayClamped("a".into())));
        assert!(problems.contains(&AudioRouteProblem::DelayClamped("b".into())));
        assert_eq!(wanted.manual_delay_ms["a"], 1_000);
        assert_eq!(wanted.manual_delay_ms["b"], 0, "3 ms rounds down to 0");
    }

    #[test]
    fn sanitise_an_empty_request_falls_back_to_local_then_any_available_sink() {
        let mut wanted = AudioRouteWanted::default();
        let problems = sanitise_wanted(&mut wanted, &outputs());
        assert_eq!(
            wanted.enabled,
            ["local"],
            "an empty request resolves to the local output while a sink exists"
        );
        assert!(problems.is_empty(), "{problems:?}");

        let no_local = vec![out("a", false), out("b", false)];
        let mut wanted = AudioRouteWanted::default();
        sanitise_wanted(&mut wanted, &no_local);
        assert_eq!(wanted.enabled, ["a"], "otherwise the first available sink");

        let mut wanted = AudioRouteWanted::default();
        sanitise_wanted(&mut wanted, &[]);
        assert!(
            wanted.enabled.is_empty(),
            "empty is allowed only when discovery found no sink"
        );
    }

    #[test]
    fn toggled_keeps_a_valid_set_and_clears_local_only() {
        let base = AudioRouteWanted {
            enabled: vec!["local".into(), "a".into(), "b".into()],
            ..Default::default()
        };
        let no_local = AudioRoute::toggled(&base, "local");
        assert_eq!(no_local.enabled, ["a", "b"], "extras stay on without local");

        let one_extra = AudioRouteWanted {
            enabled: vec!["local".into(), "a".into()],
            ..Default::default()
        };
        assert_eq!(AudioRoute::toggled(&one_extra, "a").enabled, ["local"]);

        let only_local = AudioRouteWanted {
            enabled: vec!["local".into()],
            ..Default::default()
        };
        assert_eq!(
            AudioRoute::toggled(&only_local, "local").enabled,
            ["local"],
            "unticking the last output never empties the set"
        );

        let local_only = AudioRouteWanted {
            enabled: vec!["local".into()],
            local_only: true,
            ..Default::default()
        };
        let with_extra = AudioRoute::toggled(&local_only, "a");
        assert_eq!(with_extra.enabled, ["local", "a"]);
        assert!(!with_extra.local_only, "ticking an extra leaves local-only mode");
    }

    #[test]
    fn toggled_ticks_a_paused_extra_and_clears_local_only() {
        let paused_extra = AudioRouteWanted {
            enabled: vec!["local".into(), "a".into()],
            local_only: true,
            ..Default::default()
        };
        let ticked = AudioRoute::toggled(&paused_extra, "a");
        assert_eq!(
            ticked.enabled,
            ["local", "a"],
            "the paused extra stays enabled when it is ticked"
        );
        assert!(!ticked.local_only, "ticking it leaves local-only mode");
    }

    #[test]
    fn route_helpers_report_multi_delay_and_local() {
        let mut route = AudioRoute {
            outputs: vec![
                AudioOutput {
                    id: "local".into(),
                    is_local: true,
                    delay_ms: 10,
                    ..Default::default()
                },
                AudioOutput {
                    id: "a".into(),
                    delay_ms: 40,
                    ..Default::default()
                },
            ],
            enabled: vec!["local".into(), "a".into()],
            ..Default::default()
        };
        assert!(route.is_multi());
        assert_eq!(route.local().map(|o| o.id.as_str()), Some("local"));
        assert_eq!(route.effective_enabled(), ["local", "a"]);
        assert_eq!(route.vis_delay_ms(), 40);
        assert!(route.output("a").is_some());
        assert!(route.output("nope").is_none());

        route.enabled = vec!["local".into()];
        assert!(!route.is_multi());
        assert_eq!(route.vis_delay_ms(), 0, "one output has nothing to sync against");
    }
}
