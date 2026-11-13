//! Audio routing state machine (AUDIO_01).
//!
//! [`RouteManager`] mirrors the UI's [`AudioRouteWanted`], resolves it against the
//! outputs the server reports, and turns the result into one [`RouteIntent`] for an
//! injected [`OutputControl`]. It owns no audio thread and no PipeWire handle
//! (AUDIO_02 adds the real backend, AUDIO_03 the stream re-open), so every rule here
//! is exercised by the mock backend in tests.

use plippa_audio::route::{
    intent_for, Member, OutputControl, RouteEvent, RouteIntent, SinkInfo, Target,
};
use plippa_core::audio_route::{
    sanitise_wanted, AudioOutput, AudioRoute, AudioRouteProblem, AudioRouteWanted,
    INTERNAL_SINK_PREFIX,
};
use plippa_ipc_proto::{
    NoticeSeverity, UserNotice, NOTICE_AUDIO_OUTPUT_LIMIT, NOTICE_AUDIO_OUTPUT_UNKNOWN,
    NOTICE_AUDIO_ROUTE_FAILED,
};

/// Owns the routed state and applies it through an [`OutputControl`].
pub struct RouteManager {
    control: Box<dyn OutputControl>,
    wanted: AudioRouteWanted,
    route: AudioRoute,
    applied: Option<RouteIntent>,
    local_sink: Option<String>,
    local_match: Option<String>,
    revision: u64,
    /// True when the last `apply` failed. The engine takes this once to re-open the
    /// stream at the kept route (a failure may have partially changed the graph).
    apply_failed: bool,
    /// Notices raised since the caller last drained them.
    notices: Vec<UserNotice>,
}

impl RouteManager {
    /// Build a manager. `local_sink` / `local_match` come from the board profile;
    /// both `None` falls back to the server default sink (see [`Self::refresh`]).
    pub fn new(
        control: Box<dyn OutputControl>,
        local_sink: Option<String>,
        local_match: Option<String>,
    ) -> Self {
        Self {
            control,
            // Automatic compensation is the sane first-run default (series decision 6).
            wanted: AudioRouteWanted {
                delay_auto: true,
                ..Default::default()
            },
            route: AudioRoute {
                delay_auto: true,
                ..Default::default()
            },
            applied: None,
            local_sink,
            local_match,
            revision: 0,
            apply_failed: false,
            notices: Vec::new(),
        }
    }

    /// Replace the board's local-output hints. Call [`Self::refresh`] afterwards.
    pub fn set_local_sink(&mut self, local_sink: Option<String>, local_match: Option<String>) {
        self.local_sink = local_sink;
        self.local_match = local_match;
    }

    pub fn wanted(&self) -> &AudioRouteWanted {
        &self.wanted
    }

    pub fn route(&self) -> &AudioRoute {
        &self.route
    }

    /// Drain notices raised by routing (apply failures, clamped values).
    pub fn take_notices(&mut self) -> Vec<UserNotice> {
        std::mem::take(&mut self.notices)
    }

    /// True once since the last call when an `apply` failed.
    ///
    /// The engine re-opens the stream once at the kept route: a failed apply may have
    /// left part of the graph changed even though the published route did not move
    /// (AUDIO_03 §4.8/§6.7).
    pub fn take_apply_failure(&mut self) -> bool {
        std::mem::take(&mut self.apply_failed)
    }

    /// Replace the enabled set and local-only mode in one step.
    pub fn set_outputs(&mut self, enabled: Vec<String>, local_only: bool) -> Vec<UserNotice> {
        if let Some(unknown) = enabled
            .iter()
            .find(|id| !self.known_ids().contains(id.as_str()))
        {
            return vec![unknown_notice(unknown)];
        }

        let mut candidate = self.wanted.clone();
        candidate.enabled = enabled;
        candidate.local_only = local_only;
        let known = self.known_outputs();
        let problems = sanitise_wanted(&mut candidate, &known);
        self.wanted = candidate;
        let previous = self.route.outputs.clone();
        self.resolve(&previous);
        let mut notices: Vec<UserNotice> = problems.iter().filter_map(problem_notice).collect();
        notices.extend(self.take_notices());
        notices
    }

    pub fn set_delay_auto(&mut self, auto: bool) -> Vec<UserNotice> {
        self.wanted.delay_auto = auto;
        let previous = self.route.outputs.clone();
        self.resolve(&previous);
        self.take_notices()
    }

    pub fn set_output_delay(&mut self, id: &str, delay_ms: u32) -> Vec<UserNotice> {
        if !self.known_ids().contains(id) {
            return vec![unknown_notice(id)];
        }
        let mut candidate = self.wanted.clone();
        candidate.manual_delay_ms.insert(id.to_string(), delay_ms);
        let known = self.known_outputs();
        let problems = sanitise_wanted(&mut candidate, &known);
        self.wanted = candidate;
        let previous = self.route.outputs.clone();
        self.resolve(&previous);
        let mut notices: Vec<UserNotice> = problems.iter().filter_map(problem_notice).collect();
        notices.extend(self.take_notices());
        notices
    }

    /// Drain server events. Returns true when the published route changed.
    pub fn poll(&mut self) -> bool {
        let events = self.control.poll();
        if events.is_empty() {
            return false;
        }
        let mut refresh = false;
        let mut changed = false;
        for event in events {
            match event {
                RouteEvent::SinksChanged
                | RouteEvent::OutputAdded(_)
                | RouteEvent::OutputRemoved(_) => refresh = true,
                RouteEvent::Failed(message) => {
                    self.notices.push(UserNotice {
                        severity: NoticeSeverity::Error,
                        code: NOTICE_AUDIO_ROUTE_FAILED.into(),
                        message: "Audio output could not be changed; still playing on the last \
                                  working output."
                            .into(),
                    });
                    changed |= self.set_problem(Some(message));
                }
            }
        }
        if refresh {
            changed |= self.refresh();
        }
        changed
    }

    /// What the backend should currently be applying, if anything.
    pub fn intent(&self) -> Option<RouteIntent> {
        if self.route.enabled.is_empty() {
            None
        } else {
            Some(intent_for(&self.route))
        }
    }

    /// Re-enumerate sinks and re-resolve without a user command.
    ///
    /// Returns true when the published route changed.
    pub fn refresh(&mut self) -> bool {
        let sinks: Vec<SinkInfo> = match self.control.sinks() {
            Ok(sinks) => sinks
                .into_iter()
                .filter(|s| !s.id.starts_with(INTERNAL_SINK_PREFIX))
                .collect(),
            Err(e) => return self.set_problem(Some(format!("audio server unavailable: {e}"))),
        };
        let local = resolve_local_index(
            &sinks,
            self.local_sink.as_deref(),
            self.local_match.as_deref(),
        );

        let previous = self.route.outputs.clone();
        let outputs: Vec<AudioOutput> = sinks
            .into_iter()
            .enumerate()
            .map(|(index, sink)| AudioOutput {
                id: sink.id,
                description: sink.description,
                available: sink.available,
                is_local: local == Some(index),
                delay_ms: 0,
                props: sink.props,
            })
            .collect();
        self.route.outputs = outputs;

        // First discovery: no stored choice yet, so start on the local output.
        if self.wanted.enabled.is_empty() {
            if let Some(index) = local {
                self.wanted.enabled.push(self.route.outputs[index].id.clone());
            }
        }
        self.resolve(&previous)
    }

    /// Recompute the published route. `previous` supplies descriptions for outputs
    /// that have just disappeared (so the problem line can still name them).
    fn resolve(&mut self, previous: &[AudioOutput]) -> bool {
        let outputs = self.route.outputs.clone();
        let local = outputs.iter().find(|o| o.is_local).cloned();
        let local_id = local.as_ref().map(|o| o.id.clone());

        let mut wanted_ids: Vec<String> = Vec::new();
        for id in &self.wanted.enabled {
            if !wanted_ids.contains(id) {
                wanted_ids.push(id.clone());
            }
        }

        let mut problems: Vec<String> = Vec::new();
        let local_only = self.wanted.local_only && local_id.is_some();
        let mut enabled: Vec<String> = Vec::new();
        let mut paused: Vec<String> = Vec::new();

        if local_only {
            if local.as_ref().is_some_and(|o| o.available) {
                enabled.push(local_id.clone().expect("local output"));
            } else {
                problems.push("local output is not connected".into());
            }
            paused = wanted_ids
                .iter()
                .filter(|id| !enabled.contains(id))
                .cloned()
                .collect();
        } else {
            for id in &wanted_ids {
                match outputs.iter().find(|o| &o.id == id) {
                    Some(o) if o.available => enabled.push(id.clone()),
                    Some(o) => {
                        problems.push(format!("{} is not connected", known_name(previous, &o.id)))
                    }
                    None => problems.push(format!("{} is not connected", known_name(previous, id))),
                }
            }
            if let Some(local) = &local_id {
                if let Some(pos) = enabled.iter().position(|id| id == local) {
                    let local = enabled.remove(pos);
                    enabled.insert(0, local);
                }
            }
        }
        // Report the missing local sink regardless of what else is enabled: the
        // board's built-in output is how "local only" works and how a failed route
        // falls back, so its absence is always worth surfacing.
        if local_id.is_none() {
            problems.push("no local output".into());
        }

        // A single output has nothing to sync against: no delay, no combine sink.
        let multi = enabled.len() > 1;
        let mut routed_outputs = self.route.outputs.clone();
        for output in &mut routed_outputs {
            // Automatic compensation adds no per-output trim: the combining sink
            // compensates. The value is only the manual trim for a multi route.
            output.delay_ms = if multi && !self.wanted.delay_auto {
                self.wanted
                    .manual_delay_ms
                    .get(&output.id)
                    .copied()
                    .unwrap_or(0)
            } else {
                0
            };
        }

        let mut candidate = AudioRoute {
            outputs: routed_outputs,
            enabled,
            paused,
            local_only,
            delay_auto: self.wanted.delay_auto,
            manual_delay_ms: self.wanted.manual_delay_ms.clone(),
            problem: if problems.is_empty() {
                None
            } else {
                Some(problems.join("; "))
            },
            revision: self.route.revision,
        };

        let intent = if candidate.enabled.is_empty() {
            None
        } else {
            Some(intent_for(&candidate))
        };
        if intent != self.applied {
            if let Some(intent) = &intent {
                let previous_intent = self.applied.clone();
                match self.control.apply(intent, previous_intent.as_ref()) {
                    Ok(()) => self.applied = Some(intent.clone()),
                    Err(e) => {
                        self.apply_failed = true;
                        let mut message = format!("audio route failed: {e}");
                        // A broken route must never mean silence. If nothing was
                        // applied before, fall back to the built-in output. Otherwise
                        // keep the previous route: `AudioRoute` is what is actually
                        // routed, so the failed candidate must not be published.
                        if self.applied.is_none() {
                            if let Some(local) = candidate.local() {
                                let fallback = RouteIntent {
                                    target: Target::Direct(local.id.clone()),
                                    members: vec![Member {
                                        sink: local.id.clone(),
                                        delay_ms: 0,
                                    }],
                                    unity: vec![local.id.clone()],
                                };
                                if self.control.apply(&fallback, None).is_ok() {
                                    let local_id = local.id.clone();
                                    self.applied = Some(fallback);
                                    message.push_str("; playing through the built-in output");
                                    candidate.enabled = vec![local_id];
                                    candidate.paused.clear();
                                    for output in &mut candidate.outputs {
                                        output.delay_ms = 0;
                                    }
                                } else {
                                    candidate = self.route.clone();
                                }
                            } else {
                                candidate = self.route.clone();
                            }
                        } else {
                            candidate = self.route.clone();
                        }
                        self.notices.push(UserNotice {
                            severity: NoticeSeverity::Error,
                            code: NOTICE_AUDIO_ROUTE_FAILED.into(),
                            message: "Audio output could not be changed; still playing on the \
                                      last working output."
                                .into(),
                        });
                        candidate.problem = Some(message);
                    }
                }
            }
        }

        let changed = candidate != self.route;
        if changed {
            self.revision = self.revision.saturating_add(1);
            candidate.revision = self.revision;
            self.route = candidate;
        }
        changed
    }

    fn set_problem(&mut self, problem: Option<String>) -> bool {
        if self.route.problem == problem {
            return false;
        }
        self.route.problem = problem;
        self.revision = self.revision.saturating_add(1);
        self.route.revision = self.revision;
        true
    }

    fn known_ids(&self) -> std::collections::BTreeSet<&str> {
        self.route
            .outputs
            .iter()
            .map(|o| o.id.as_str())
            .chain(self.wanted.enabled.iter().map(String::as_str))
            .collect()
    }

    /// Outputs the model knows about, including enabled-but-unplugged ids (so a
    /// validation pass does not drop the user's choice just because the device left).
    fn known_outputs(&self) -> Vec<AudioOutput> {
        let mut outputs = self.route.outputs.clone();
        for id in &self.wanted.enabled {
            if !outputs.iter().any(|o| &o.id == id) {
                outputs.push(AudioOutput {
                    id: id.clone(),
                    description: id.clone(),
                    available: false,
                    ..Default::default()
                });
            }
        }
        outputs
    }
}

/// Which output is local: exact board name, then the board's substring (id or a
/// property value), then the server default, then no local output.
fn resolve_local_index(
    sinks: &[SinkInfo],
    local_sink: Option<&str>,
    local_match: Option<&str>,
) -> Option<usize> {
    if let Some(name) = local_sink {
        if let Some(index) = sinks.iter().position(|s| s.id == name) {
            return Some(index);
        }
    }
    if let Some(needle) = local_match {
        if let Some(index) = sinks.iter().position(|s| {
            s.id.contains(needle) || s.props.values().any(|value| value.contains(needle))
        }) {
            return Some(index);
        }
    }
    sinks.iter().position(|s| s.is_default)
}

fn output_name(output: &AudioOutput) -> String {
    if output.description.trim().is_empty() {
        output.id.clone()
    } else {
        output.description.clone()
    }
}

fn known_name(previous: &[AudioOutput], id: &str) -> String {
    previous
        .iter()
        .find(|o| o.id == id)
        .map(output_name)
        .unwrap_or_else(|| id.to_string())
}

fn unknown_notice(id: &str) -> UserNotice {
    UserNotice {
        severity: NoticeSeverity::Warning,
        code: NOTICE_AUDIO_OUTPUT_UNKNOWN.into(),
        message: format!("`{id}` is not a connected output."),
    }
}

fn problem_notice(problem: &AudioRouteProblem) -> Option<UserNotice> {
    match problem {
        AudioRouteProblem::LimitExceeded => Some(UserNotice {
            severity: NoticeSeverity::Warning,
            code: NOTICE_AUDIO_OUTPUT_LIMIT.into(),
            message: "You can play to at most 4 outputs. Extra outputs were not enabled.".into(),
        }),
        AudioRouteProblem::UnknownOutput(id) => Some(unknown_notice(id)),
        // A clamp is benign: the nearest allowed value is already published in
        // `manual_delay_ms`, so it needs no user-facing notice and is certainly not
        // a route failure.
        AudioRouteProblem::DelayClamped(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_audio::route::MockOutputControl;
    use std::collections::BTreeMap;

    fn sink(
        id: &str,
        description: &str,
        available: bool,
        latency_ms: u32,
        is_default: bool,
    ) -> SinkInfo {
        SinkInfo {
            id: id.into(),
            description: description.into(),
            available,
            latency_ms,
            props: BTreeMap::new(),
            is_default,
        }
    }

    fn local_control() -> MockOutputControl {
        MockOutputControl::new(vec![
            sink("local", "Built-in output", true, 0, true),
            sink("usb", "USB headset", true, 40, false),
        ])
    }

    fn manager(mock: &MockOutputControl) -> RouteManager {
        RouteManager::new(Box::new(mock.clone()), Some("local".into()), None)
    }

    fn multi_for(mock: &MockOutputControl) -> RouteIntent {
        mock.last_applied().expect("an applied intent")
    }

    fn direct_id(intent: &RouteIntent) -> Option<&str> {
        match &intent.target {
            Target::Direct(id) => Some(id),
            Target::Multi { .. } => None,
        }
    }

    #[test]
    fn local_resolution_prefers_exact_then_substring_then_default_then_none() {
        let exact = vec![
            sink("first", "First", true, 0, true),
            sink("board.local", "Board", true, 0, false),
        ];
        assert_eq!(
            resolve_local_index(&exact, Some("board.local"), None),
            Some(1),
            "the board name wins over the server default"
        );

        let substring = vec![
            sink("usb", "USB", true, 0, false),
            sink("alsa.builtin-output", "Built-in", true, 0, false),
        ];
        assert_eq!(
            resolve_local_index(&substring, None, Some("builtin")),
            Some(1),
            "substring matches the sink id"
        );
        let mut with_prop = sink("card", "Card", true, 0, false);
        with_prop.props.insert("device.bus".into(), "platform-soc".into());
        let prop_sinks = vec![sink("usb", "USB", true, 0, false), with_prop];
        assert_eq!(
            resolve_local_index(&prop_sinks, None, Some("platform-soc")),
            Some(1),
            "substring matches a property value"
        );

        let default = vec![
            sink("usb", "USB", true, 0, false),
            sink("fallback", "Fallback", true, 0, true),
        ];
        assert_eq!(resolve_local_index(&default, None, None), Some(1));

        let no_local = vec![
            sink("usb", "USB", true, 0, false),
            sink("monitor", "Monitor", true, 0, false),
        ];
        assert_eq!(resolve_local_index(&no_local, Some("gone"), Some("nope")), None);
    }

    #[test]
    fn enabling_a_second_output_applies_multi_once_not_once_per_poll() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        assert_eq!(mock.applied_count(), 1, "refresh applies the local-only route");
        assert_eq!(direct_id(&multi_for(&mock)), Some("local"));

        manager.set_outputs(vec!["local".into(), "usb".into()], false);
        assert_eq!(mock.applied_count(), 2, "one apply for the new route");

        let intent = multi_for(&mock);
        assert!(matches!(intent.target, Target::Multi { compensate: true }));
        let members: Vec<&str> = intent.members.iter().map(|m| m.sink.as_str()).collect();
        assert_eq!(members, ["local", "usb"], "members are local-first");

        assert!(!manager.poll(), "a poll with no server event changes nothing");
        assert_eq!(mock.applied_count(), 2, "not re-applied per poll");
    }

    #[test]
    fn disabling_returns_to_direct_and_carries_the_previous_intent() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_outputs(vec!["local".into(), "usb".into()], false);
        manager.set_outputs(vec!["local".into()], false);

        assert_eq!(direct_id(&multi_for(&mock)), Some("local"));
        let previous = mock.last_previous().expect("previous intent");
        assert!(
            matches!(previous.target, Target::Multi { .. }),
            "the backend is told what it is moving away from"
        );
    }

    #[test]
    fn a_sink_disappearing_keeps_the_route_valid_and_reports_a_problem() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_outputs(vec!["local".into(), "usb".into()], false);

        mock.set_sinks(vec![sink("local", "Built-in output", true, 0, true)]);
        mock.push_event(RouteEvent::SinksChanged);
        assert!(manager.poll());

        assert_eq!(manager.route().enabled, ["local"]);
        let problem = manager.route().problem.clone().expect("problem line");
        assert!(
            problem.contains("USB headset") && problem.contains("not connected"),
            "{problem}"
        );
        assert_eq!(
            manager.wanted().enabled,
            ["local", "usb"],
            "the user's choice is kept for when the device returns"
        );
        assert_eq!(direct_id(&multi_for(&mock)), Some("local"));
    }

    #[test]
    fn a_sink_reappearing_reapplies_multi_without_a_user_command() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_outputs(vec!["local".into(), "usb".into()], false);
        mock.set_sinks(vec![sink("local", "Built-in output", true, 0, true)]);
        mock.push_event(RouteEvent::SinksChanged);
        assert!(manager.poll());

        mock.set_sinks(vec![
            sink("local", "Built-in output", true, 0, true),
            sink("usb", "USB headset", true, 40, false),
        ]);
        mock.push_event(RouteEvent::OutputAdded("usb".into()));
        assert!(manager.poll());

        assert_eq!(manager.route().enabled, ["local", "usb"]);
        assert!(
            matches!(multi_for(&mock).target, Target::Multi { .. }),
            "the route is re-applied from a server event alone"
        );
    }

    #[test]
    fn apply_failure_keeps_the_previous_route_and_raises_a_notice() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        let revision = manager.route().revision;

        mock.fail_next_apply("no such module");
        let notices = manager.set_outputs(vec!["local".into(), "usb".into()], false);

        assert!(
            notices.iter().any(|n| n.code == NOTICE_AUDIO_ROUTE_FAILED),
            "{notices:?}"
        );
        assert_eq!(
            direct_id(&multi_for(&mock)),
            Some("local"),
            "the last working route is still applied (never silent)"
        );
        assert_eq!(
            manager.route().enabled,
            ["local"],
            "the failed candidate is not published; the route stays what is routed"
        );
        assert!(manager.route().paused.is_empty());
        let problem = manager.route().problem.clone().expect("problem line");
        assert!(problem.contains("no such module"), "{problem}");
        assert!(manager.route().revision > revision, "the failure is published");
    }

    #[test]
    fn a_failed_apply_after_multi_keeps_publishing_the_multi_route() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_outputs(vec!["local".into(), "usb".into()], false);

        mock.fail_next_apply("combine module gone");
        manager.set_outputs(vec!["local".into()], false);

        assert_eq!(
            manager.route().enabled,
            ["local", "usb"],
            "the applied multi route is still what is routed"
        );
        assert!(
            matches!(multi_for(&mock).target, Target::Multi { .. }),
            "the prior multi intent remains applied"
        );
        let problem = manager.route().problem.clone().expect("problem line");
        assert!(problem.contains("combine module gone"), "{problem}");
    }

    #[test]
    fn an_empty_enabled_request_resolves_to_local_while_a_sink_exists() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();

        manager.set_outputs(Vec::new(), false);

        assert_eq!(manager.route().enabled, ["local"]);
        assert_eq!(
            manager.wanted().enabled,
            ["local"],
            "the last-output rule is applied in sanitise_wanted"
        );
    }

    #[test]
    fn a_clamped_delay_does_not_raise_a_route_failed_notice() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_outputs(vec!["local".into(), "usb".into()], false);

        let notices = manager.set_output_delay("usb", 1_033);

        assert!(
            !notices.iter().any(|n| n.code == NOTICE_AUDIO_ROUTE_FAILED),
            "a clamp is benign, not a route failure: {notices:?}"
        );
        assert_eq!(
            manager.route().manual_delay_ms.get("usb").copied(),
            Some(1_000),
            "the clamped value is still published"
        );
    }

    #[test]
    fn a_failed_first_apply_falls_back_to_the_local_output() {
        let mock = MockOutputControl::new(vec![sink("local", "Built-in output", true, 0, true)]);
        mock.fail_next_apply("module missing");
        let mut manager = manager(&mock);
        manager.refresh();

        assert_eq!(mock.attempts().len(), 2, "the failed try, then the fallback");
        assert_eq!(mock.applied_count(), 1);
        assert_eq!(direct_id(&multi_for(&mock)), Some("local"));
        let problem = manager.route().problem.clone().expect("problem line");
        assert!(problem.contains("module missing"), "{problem}");
    }

    #[test]
    fn local_only_lists_the_extras_as_paused() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_outputs(vec!["local".into(), "usb".into()], false);
        manager.set_outputs(vec!["local".into(), "usb".into()], true);

        assert!(manager.route().local_only);
        assert_eq!(manager.route().enabled, ["local"]);
        assert_eq!(manager.route().paused, ["usb"]);
        assert_eq!(direct_id(&multi_for(&mock)), Some("local"));
    }

    #[test]
    fn revision_bumps_once_per_real_change_and_not_on_a_noop_poll() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        let revision = manager.route().revision;

        assert!(!manager.poll());
        assert_eq!(manager.route().revision, revision);

        manager.set_outputs(vec!["local".into()], false);
        assert_eq!(
            manager.route().revision,
            revision,
            "resending the same route is a no-op"
        );

        manager.set_outputs(vec!["local".into(), "usb".into()], false);
        assert_eq!(manager.route().revision, revision + 1);

        assert!(!manager.poll());
        assert_eq!(manager.route().revision, revision + 1);
    }

    #[test]
    fn a_manual_delay_on_a_single_output_leaves_the_intent_direct() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_delay_auto(false);
        manager.set_output_delay("local", 120);

        assert_eq!(direct_id(&multi_for(&mock)), Some("local"));
        assert_eq!(
            manager.route().output("local").map(|o| o.delay_ms),
            Some(0),
            "one output has nothing to sync against"
        );
        assert_eq!(
            manager.route().manual_delay_ms.get("local").copied(),
            Some(120),
            "the value is kept for the second output"
        );
    }

    #[test]
    fn manual_delays_are_used_only_when_auto_is_off_and_multi() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        manager.set_outputs(vec!["local".into(), "usb".into()], false);
        manager.set_delay_auto(false);
        manager.set_output_delay("usb", 120);
        manager.set_output_delay("local", 30);

        let members: BTreeMap<String, u32> = multi_for(&mock)
            .members
            .into_iter()
            .map(|m| (m.sink, m.delay_ms))
            .collect();
        assert_eq!(members["local"], 30);
        assert_eq!(members["usb"], 120);

        manager.set_delay_auto(true);
        let members: BTreeMap<String, u32> = multi_for(&mock)
            .members
            .into_iter()
            .map(|m| (m.sink, m.delay_ms))
            .collect();
        assert_eq!(
            members["local"], 0,
            "auto leaves compensation to the combine sink"
        );
        assert_eq!(
            members["usb"], 0,
            "auto must not create a per-output loopback"
        );
        assert_eq!(
            manager.route().output("usb").map(|o| o.delay_ms),
            Some(0),
            "auto adds no per-output trim, so the published delay is 0"
        );
        assert_eq!(
            manager.route().output("local").map(|o| o.delay_ms),
            Some(0),
            "auto is 0 for every output, including the local one"
        );
    }

    #[test]
    fn no_local_output_is_reported_without_forcing_local_only() {
        let mock = MockOutputControl::new(vec![sink("usb", "USB headset", true, 0, false)]);
        let mut manager = RouteManager::new(Box::new(mock), None, None);
        manager.refresh();

        assert!(manager.route().local().is_none());
        assert!(!manager.route().local_only);
        assert_eq!(manager.route().problem.as_deref(), Some("no local output"));
    }

    #[test]
    fn no_local_output_is_reported_even_with_extras_enabled() {
        let mock = MockOutputControl::new(vec![sink("usb", "USB headset", true, 0, false)]);
        let mut manager = RouteManager::new(Box::new(mock), None, None);
        manager.refresh();
        manager.set_outputs(vec!["usb".into()], false);

        assert_eq!(manager.route().enabled, ["usb"]);
        assert!(!manager.route().local_only);
        assert_eq!(manager.route().problem.as_deref(), Some("no local output"));
    }

    #[test]
    fn unknown_output_id_leaves_the_route_unchanged() {
        let mock = local_control();
        let mut manager = manager(&mock);
        manager.refresh();
        let toggled = manager.route().revision;

        let notices = manager.set_outputs(vec!["local".into(), "ghost".into()], false);

        assert!(notices.iter().any(|n| n.code == NOTICE_AUDIO_OUTPUT_UNKNOWN));
        assert_eq!(manager.route().revision, toggled);
        assert_eq!(manager.wanted().enabled, ["local"]);
    }

    #[test]
    fn internal_sinks_are_hidden_from_the_model() {
        let mock = MockOutputControl::new(vec![
            sink("local", "Built-in output", true, 0, true),
            sink("plippa.multi", "Combine", true, 0, false),
        ]);
        let mut manager = manager(&mock);
        manager.refresh();

        assert!(manager.route().output("plippa.multi").is_none());
        assert_eq!(manager.route().outputs.len(), 1);
    }
}
