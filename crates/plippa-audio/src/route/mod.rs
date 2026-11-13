//! Output-control seam for audio routing (AUDIO_01).
//!
//! This module is pure: it names what the engine wants ([`RouteIntent`]) and the
//! interface a real PipeWire backend implements ([`OutputControl`]). AUDIO_02 adds
//! the client behind that trait; the engine and its tests only ever see this seam,
//! so no PipeWire dependency leaks into the routing state machine.

mod mock;
mod plan;

#[cfg(feature = "pulse")]
mod pipewire_ctl;

pub use mock::MockOutputControl;
pub use plan::{
    delay_loop_args, delay_sink_args, delay_sink_name, multi_args, ops_for, ModuleHandle,
    ModuleTag, RouteOp, DELAY_DESCRIPTION, DELAY_SINK_PREFIX, MODULE_COMBINE_SINK,
    MODULE_LOOPBACK, MODULE_NULL_SINK, MULTI_DESCRIPTION, MULTI_SINK_NAME,
};
#[cfg(feature = "pulse")]
pub use pipewire_ctl::PipeWireControl;

use plippa_core::AudioRoute;
use std::collections::BTreeMap;
use thiserror::Error;

/// The upstream PipeWire property value that marks a wireless/radio sink
/// (`device.bus`). This is the only permitted spelling (trademark rule), and it is
/// used solely to skip volume/mute normalisation — never as UI text.
pub const RADIO_BUS_VALUE: &str = "bluetooth";

/// Errors from an output-control backend.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RouteError {
    /// The audio server refused or could not apply the request.
    #[error("{0}")]
    Backend(String),
    /// The requested sink is no longer known to the server.
    #[error("unknown output `{0}`")]
    UnknownOutput(String),
}

/// One sink as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SinkInfo {
    pub id: String,
    pub description: String,
    pub available: bool,
    /// The sink's own configured latency (ms); automatic compensation publishes this.
    pub latency_ms: u32,
    pub props: BTreeMap<String, String>,
    /// The server's current default sink. Used only when the board profile cannot
    /// name the local output (AUDIO_01 §5.4).
    pub is_default: bool,
}

/// What the audio graph should be doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Play straight into this sink.
    Direct(String),
    /// Play into our combining sink; it fans out to these members.
    Multi { compensate: bool },
}

/// One member of a [`Target::Multi`] route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub sink: String,
    pub delay_ms: u32,
}

/// A complete routing request for [`OutputControl::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteIntent {
    pub target: Target,
    /// Ignored for [`Target::Direct`]; kept so the delay is ready when a second
    /// output joins.
    pub members: Vec<Member>,
    /// Sinks to force to 100 % / unmuted when they join the route.
    pub unity: Vec<String>,
}

/// Server-side audio routing backend (AUDIO_02 implements the PipeWire client).
pub trait OutputControl: Send {
    /// Enumerate sinks (including internal ones; the engine filters those).
    fn sinks(&mut self) -> Result<Vec<SinkInfo>, RouteError>;
    /// Make the graph match `intent`, given what is currently applied.
    fn apply(&mut self, intent: &RouteIntent, previous: Option<&RouteIntent>)
        -> Result<(), RouteError>;
    /// Non-blocking: server events since the last call.
    fn poll(&mut self) -> Vec<RouteEvent>;
}

/// Something the audio server reported since the last poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteEvent {
    SinksChanged,
    OutputAdded(String),
    OutputRemoved(String),
    /// Human-readable; the engine puts it in `problem` and raises a notice.
    Failed(String),
}

/// Turn a resolved [`AudioRoute`] into the backend request.
///
/// One effective output is played straight into that sink: a one-member combine
/// sink would only add latency. More than one goes through the combining sink, with
/// automatic compensation following `delay_auto`. `members` and `unity` are always
/// local-first (the engine already orders `enabled` that way).
///
/// In automatic mode the combining sink compensates (`latency_compensate=1`) and no
/// member carries a delay, so the planner creates no per-output loopback modules.
/// The engine publishes `delay_ms = 0` for every output in that mode; manual mode
/// instead carries the user's per-output ms (series decision 6).
pub fn intent_for(route: &AudioRoute) -> RouteIntent {
    let enabled = route.effective_enabled();
    let unity = enabled
        .iter()
        .filter(|id| {
            route.output(id).is_none_or(|o| {
                o.props.get("device.bus").map(String::as_str) != Some(RADIO_BUS_VALUE)
            })
        })
        .cloned()
        .collect();
    let members: Vec<Member> = enabled
        .iter()
        .filter_map(|id| route.output(id))
        .map(|o| Member {
            sink: o.id.clone(),
            delay_ms: if route.delay_auto { 0 } else { o.delay_ms },
        })
        .collect();
    let target = match enabled {
        [only] => Target::Direct(only.clone()),
        _ => Target::Multi {
            compensate: route.delay_auto,
        },
    };
    RouteIntent {
        target,
        members,
        unity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::audio_route::AudioOutput;

    fn output(id: &str, is_local: bool, delay_ms: u32) -> AudioOutput {
        AudioOutput {
            id: id.into(),
            description: id.into(),
            available: true,
            is_local,
            delay_ms,
            props: BTreeMap::new(),
        }
    }

    fn output_with_bus(id: &str, is_local: bool, bus: Option<&str>) -> AudioOutput {
        let mut out = output(id, is_local, 0);
        if let Some(bus) = bus {
            out.props.insert("device.bus".into(), bus.into());
        }
        out
    }

    fn route(enabled: &[&str], outputs: Vec<AudioOutput>, delay_auto: bool) -> AudioRoute {
        AudioRoute {
            outputs,
            enabled: enabled.iter().map(|s| s.to_string()).collect(),
            delay_auto,
            ..Default::default()
        }
    }

    #[test]
    fn one_output_plays_direct_with_that_sink_in_unity() {
        let r = route(&["usb"], vec![output("usb", false, 0)], true);
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Direct("usb".into()));
        assert_eq!(intent.unity, ["usb"]);
        assert_eq!(
            intent.members,
            [Member {
                sink: "usb".into(),
                delay_ms: 0
            }]
        );
    }

    #[test]
    fn local_plus_one_uses_multi_with_local_first() {
        let r = route(
            &["local", "usb"],
            vec![output("local", true, 10), output("usb", false, 30)],
            false,
        );
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Multi { compensate: false });
        assert_eq!(
            intent.members,
            [
                Member {
                    sink: "local".into(),
                    delay_ms: 10
                },
                Member {
                    sink: "usb".into(),
                    delay_ms: 30
                }
            ]
        );
        assert_eq!(intent.unity, ["local", "usb"]);
    }

    #[test]
    fn four_outputs_all_reach_the_intent_and_unity() {
        let outs = vec![
            output("local", true, 0),
            output("a", false, 0),
            output("b", false, 0),
            output("c", false, 0),
        ];
        let r = route(&["local", "a", "b", "c"], outs, true);
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Multi { compensate: true });
        assert_eq!(intent.members.len(), 4);
        assert_eq!(intent.unity, ["local", "a", "b", "c"]);
    }

    #[test]
    fn local_only_route_is_direct_to_local() {
        let r = route(&["local"], vec![output("local", true, 0)], true);
        assert_eq!(intent_for(&r).target, Target::Direct("local".into()));
    }

    #[test]
    fn a_radio_sink_in_a_multi_route_is_not_normalised_to_unity() {
        let r = route(
            &["local", "usb", "bt"],
            vec![
                output_with_bus("local", true, None),
                output_with_bus("usb", false, Some("usb")),
                output_with_bus("bt", false, Some(RADIO_BUS_VALUE)),
            ],
            false,
        );
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Multi { compensate: false });
        assert_eq!(intent.unity, ["local", "usb"]);
        assert!(
            intent.members.iter().any(|m| m.sink == "bt"),
            "the radio sink still plays: {:?}",
            intent.members
        );
    }

    #[test]
    fn direct_into_a_radio_sink_has_empty_unity() {
        let r = route(
            &["bt"],
            vec![output_with_bus("bt", false, Some(RADIO_BUS_VALUE))],
            true,
        );
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Direct("bt".into()));
        assert!(intent.unity.is_empty());
        assert_eq!(intent.members.len(), 1);
    }

    #[test]
    fn a_manual_delay_reaches_the_member_even_for_one_output_until_resolve_zeroes_it() {
        // `intent_for` is pure: it copies whatever delay the route carries. The engine
        // zeroes the delay when only one output is effective (nothing to sync against).
        let r = route(&["local"], vec![output("local", true, 50)], false);
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Direct("local".into()));
        assert_eq!(intent.members[0].delay_ms, 50);
    }

    #[test]
    fn automatic_mode_leaves_delay_to_the_combine_sink() {
        use super::plan::{ops_for, RouteOp};
        use std::collections::BTreeMap;

        // Even if the route still carries a per-output value, auto mode must zero the
        // members; the engine publishes delay_ms = 0 for every output in this mode.
        let r = route(
            &["local", "usb"],
            vec![output("local", true, 24), output("usb", false, 40)],
            true,
        );
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Multi { compensate: true });
        assert!(
            intent.members.iter().all(|m| m.delay_ms == 0),
            "auto mode must not request a per-output delay: {:?}",
            intent.members
        );
        assert_eq!(
            r.output("usb").map(|o| o.delay_ms),
            Some(40),
            "intent_for does not read the route's display delay in auto mode"
        );

        let ops = ops_for(&intent, None, &BTreeMap::new());
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                RouteOp::Load {
                    tag: ModuleTag::DelaySink(_) | ModuleTag::DelayLoop(_),
                    ..
                }
            )),
            "auto mode must not load delay modules: {ops:?}"
        );
        assert_eq!(
            ops,
            vec![
                RouteOp::Load {
                    tag: ModuleTag::Multi,
                    module: MODULE_COMBINE_SINK,
                    args: "sink_name=plippa.multi slaves=local,usb latency_compensate=1 sink_properties=device.description=Plippa_Multi".into(),
                },
                RouteOp::Unity {
                    sink: "local".into()
                },
                RouteOp::Unity {
                    sink: "usb".into()
                },
            ]
        );
    }

    #[test]
    fn manual_mode_requests_per_output_loopbacks() {
        use super::plan::{ops_for, RouteOp};
        use std::collections::BTreeMap;

        let r = route(
            &["local", "usb"],
            vec![output("local", true, 10), output("usb", false, 30)],
            false,
        );
        let intent = intent_for(&r);
        assert_eq!(intent.target, Target::Multi { compensate: false });
        assert_eq!(
            intent
                .members
                .iter()
                .map(|m| (m.sink.as_str(), m.delay_ms))
                .collect::<Vec<_>>(),
            [("local", 10), ("usb", 30)]
        );

        let ops = ops_for(&intent, None, &BTreeMap::new());
        let loaded: Vec<&ModuleTag> = ops
            .iter()
            .filter_map(|op| match op {
                RouteOp::Load { tag, .. } => Some(tag),
                _ => None,
            })
            .collect();
        for want in [
            ModuleTag::DelaySink("local".into()),
            ModuleTag::DelayLoop("local".into()),
            ModuleTag::DelaySink("usb".into()),
            ModuleTag::DelayLoop("usb".into()),
        ] {
            assert!(loaded.contains(&&want), "missing {want:?} in {ops:?}");
        }
    }

    #[test]
    fn compensate_follows_delay_auto() {
        let outs = || vec![output("local", true, 0), output("usb", false, 0)];
        assert_eq!(
            intent_for(&route(&["local", "usb"], outs(), true)).target,
            Target::Multi { compensate: true }
        );
        assert_eq!(
            intent_for(&route(&["local", "usb"], outs(), false)).target,
            Target::Multi { compensate: false }
        );
    }
}
