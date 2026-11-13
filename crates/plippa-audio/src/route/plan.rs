//! Pure module-lifecycle planner for the PipeWire routing graph (AUDIO_02).
//!
//! [`ops_for`] turns the wanted [`RouteIntent`], the previously applied intent and
//! the modules we loaded into an ordered list of [`RouteOp`]s. No server type
//! appears here, so every transition is testable on a box with no audio server.
//!
//! Names and module arguments are kept in constants/builders so the tests can
//! assert the exact strings the server receives.

use super::{Member, RouteIntent, Target};
use std::collections::{BTreeMap, BTreeSet};

/// Module that fans one stream out to several sinks.
pub const MODULE_COMBINE_SINK: &str = "module-combine-sink";
/// Module that creates the per-output delay sink.
pub const MODULE_NULL_SINK: &str = "module-null-sink";
/// Module that delays the delayed output by feeding its monitor back into it.
pub const MODULE_LOOPBACK: &str = "module-loopback";

pub const MULTI_SINK_NAME: &str = "plippa.multi";
pub const MULTI_DESCRIPTION: &str = "Plippa_Multi";
pub const DELAY_DESCRIPTION: &str = "Plippa_Delay";
pub const DELAY_SINK_PREFIX: &str = "plippa.delay.";

/// Identity of one module we loaded, independent of its server index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModuleTag {
    /// The combining sink.
    Multi,
    /// The null sink feeding one delayed output.
    DelaySink(String),
    /// The loopback that delays one output (the output id is the key).
    DelayLoop(String),
}

/// The server index of a module we loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModuleHandle {
    pub index: u32,
}

/// One step in a route transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOp {
    Load {
        tag: ModuleTag,
        module: &'static str,
        args: String,
    },
    Unload {
        tag: ModuleTag,
    },
    /// Force one sink to 100 % / unmuted as it joins the route.
    Unity {
        sink: String,
    },
}

/// Sink name of the delay node for `sink`.
pub fn delay_sink_name(sink: &str) -> String {
    format!("{DELAY_SINK_PREFIX}{sink}")
}

/// `module-null-sink` arguments for the delay node of `sink`.
pub fn delay_sink_args(sink: &str) -> String {
    format!(
        "sink_name={} sink_properties=device.description={DELAY_DESCRIPTION}",
        delay_sink_name(sink)
    )
}

/// `module-loopback` arguments that delay `sink` by `delay_ms`.
pub fn delay_loop_args(sink: &str, delay_ms: u32) -> String {
    format!(
        "source={}.monitor sink={sink} latency_msec={delay_ms} source_dont_move=true sink_dont_move=true",
        delay_sink_name(sink)
    )
}

/// `module-combine-sink` arguments for a member list.
pub fn multi_args(members: &[Member], compensate: bool) -> String {
    let slaves = members
        .iter()
        .map(|m| {
            if m.delay_ms > 0 {
                delay_sink_name(&m.sink)
            } else {
                m.sink.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "sink_name={MULTI_SINK_NAME} slaves={slaves} latency_compensate={} sink_properties=device.description={MULTI_DESCRIPTION}",
        u8::from(compensate)
    )
}

/// Plan the ordered ops that move the graph from `previous`/`loaded` to `intent`.
///
/// `loaded` is the set of modules we created and still hold. Delays are loaded
/// before the combining sink references them and unloaded after it stops using
/// them, so no module ever dangles.
pub fn ops_for(
    intent: &RouteIntent,
    previous: Option<&RouteIntent>,
    loaded: &BTreeMap<ModuleTag, ModuleHandle>,
) -> Vec<RouteOp> {
    let mut ops = Vec::new();
    let mut active: BTreeSet<ModuleTag> = loaded.keys().cloned().collect();

    // 1. Drop delay modules whose output left the route, lost its delay, or had
    //    its delay changed (loopback latency is fixed at load time).
    if matches!(intent.target, Target::Multi { .. }) {
        let mut stale_sinks: BTreeSet<String> = BTreeSet::new();
        for tag in loaded.keys() {
            let sink = match tag {
                ModuleTag::DelaySink(sink) | ModuleTag::DelayLoop(sink) => sink,
                ModuleTag::Multi => continue,
            };
            let wanted = member_delay(intent, sink);
            let current = previous.and_then(|p| member_delay(p, sink));
            let keep = matches!((wanted, current), (Some(w), Some(c)) if w == c && w > 0);
            if !keep {
                stale_sinks.insert(sink.clone());
            }
        }
        for sink in stale_sinks {
            push_delay_unload(&mut ops, &mut active, &sink);
        }
    }

    match &intent.target {
        Target::Direct(_) => {
            // 3. Direct playback never keeps our plumbing nodes.
            if active.remove(&ModuleTag::Multi) {
                ops.push(RouteOp::Unload {
                    tag: ModuleTag::Multi,
                });
            }
            let mut delay_sinks: BTreeSet<String> = BTreeSet::new();
            for tag in &active {
                if let ModuleTag::DelaySink(sink) | ModuleTag::DelayLoop(sink) = tag {
                    delay_sinks.insert(sink.clone());
                }
            }
            for sink in delay_sinks {
                push_delay_unload(&mut ops, &mut active, &sink);
            }
        }
        Target::Multi { compensate } => {
            // 2. Load missing delay pairs before the combining sink references them.
            for member in &intent.members {
                if member.delay_ms == 0 {
                    continue;
                }
                let sink_tag = ModuleTag::DelaySink(member.sink.clone());
                let loop_tag = ModuleTag::DelayLoop(member.sink.clone());
                if !active.contains(&sink_tag) {
                    active.insert(sink_tag.clone());
                    ops.push(RouteOp::Load {
                        tag: sink_tag,
                        module: MODULE_NULL_SINK,
                        args: delay_sink_args(&member.sink),
                    });
                }
                if !active.contains(&loop_tag) {
                    active.insert(loop_tag.clone());
                    ops.push(RouteOp::Load {
                        tag: loop_tag,
                        module: MODULE_LOOPBACK,
                        args: delay_loop_args(&member.sink, member.delay_ms),
                    });
                }
            }

            // 4. Reload the combining sink only when its inputs changed.
            let unchanged = previous.is_some_and(|p| {
                p.target == intent.target && p.members == intent.members
            });
            if !unchanged || !active.contains(&ModuleTag::Multi) {
                if active.remove(&ModuleTag::Multi) {
                    ops.push(RouteOp::Unload {
                        tag: ModuleTag::Multi,
                    });
                }
                ops.push(RouteOp::Load {
                    tag: ModuleTag::Multi,
                    module: MODULE_COMBINE_SINK,
                    args: multi_args(&intent.members, *compensate),
                });
                active.insert(ModuleTag::Multi);
            }
        }
    }

    // 5. Normalise sinks that just joined the route.
    let previous_unity = previous.map(|p| p.unity.as_slice()).unwrap_or(&[]);
    for sink in &intent.unity {
        if !previous_unity.iter().any(|s| s == sink) {
            ops.push(RouteOp::Unity { sink: sink.clone() });
        }
    }

    ops
}

fn member_delay(intent: &RouteIntent, sink: &str) -> Option<u32> {
    intent
        .members
        .iter()
        .find(|m| m.sink == sink)
        .map(|m| m.delay_ms)
}

fn push_delay_unload(
    ops: &mut Vec<RouteOp>,
    active: &mut BTreeSet<ModuleTag>,
    sink: &str,
) {
    let loop_tag = ModuleTag::DelayLoop(sink.to_string());
    if active.remove(&loop_tag) {
        ops.push(RouteOp::Unload { tag: loop_tag });
    }
    let sink_tag = ModuleTag::DelaySink(sink.to_string());
    if active.remove(&sink_tag) {
        ops.push(RouteOp::Unload { tag: sink_tag });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(sink: &str) -> RouteIntent {
        RouteIntent {
            target: Target::Direct(sink.into()),
            members: vec![Member {
                sink: sink.into(),
                delay_ms: 0,
            }],
            unity: vec![sink.into()],
        }
    }

    fn multi(members: &[(&str, u32)], compensate: bool) -> RouteIntent {
        RouteIntent {
            target: Target::Multi { compensate },
            members: members
                .iter()
                .map(|(sink, delay_ms)| Member {
                    sink: (*sink).into(),
                    delay_ms: *delay_ms,
                })
                .collect(),
            unity: members.iter().map(|(sink, _)| (*sink).into()).collect(),
        }
    }

    fn loaded(tags: &[ModuleTag]) -> BTreeMap<ModuleTag, ModuleHandle> {
        tags.iter()
            .enumerate()
            .map(|(i, tag)| (tag.clone(), ModuleHandle { index: i as u32 + 1 }))
            .collect()
    }

    fn pair(sink: &str) -> [ModuleTag; 2] {
        [
            ModuleTag::DelaySink(sink.into()),
            ModuleTag::DelayLoop(sink.into()),
        ]
    }

    #[test]
    fn local_only_to_multi_loads_delays_then_multi_then_new_unity() {
        let previous = direct("local");
        let intent = multi(&[("local", 0), ("usb", 50)], true);
        let ops = ops_for(&intent, Some(&previous), &BTreeMap::new());

        assert_eq!(
            ops,
            vec![
                RouteOp::Load {
                    tag: ModuleTag::DelaySink("usb".into()),
                    module: MODULE_NULL_SINK,
                    args: "sink_name=plippa.delay.usb sink_properties=device.description=Plippa_Delay"
                        .into(),
                },
                RouteOp::Load {
                    tag: ModuleTag::DelayLoop("usb".into()),
                    module: MODULE_LOOPBACK,
                    args: "source=plippa.delay.usb.monitor sink=usb latency_msec=50 source_dont_move=true sink_dont_move=true".into(),
                },
                RouteOp::Load {
                    tag: ModuleTag::Multi,
                    module: MODULE_COMBINE_SINK,
                    args: "sink_name=plippa.multi slaves=local,plippa.delay.usb latency_compensate=1 sink_properties=device.description=Plippa_Multi".into(),
                },
                RouteOp::Unity {
                    sink: "usb".into(),
                },
            ]
        );
    }

    #[test]
    fn multi_to_direct_unloads_multi_then_the_delay_pair() {
        let previous = multi(&[("local", 0), ("usb", 50)], true);
        let mut map = loaded(&[ModuleTag::Multi, ModuleTag::DelaySink("usb".into()), ModuleTag::DelayLoop("usb".into())]);
        let intent = direct("local");
        let ops = ops_for(&intent, Some(&previous), &map);

        assert_eq!(
            ops,
            vec![
                RouteOp::Unload {
                    tag: ModuleTag::Multi
                },
                RouteOp::Unload {
                    tag: ModuleTag::DelayLoop("usb".into())
                },
                RouteOp::Unload {
                    tag: ModuleTag::DelaySink("usb".into())
                },
            ]
        );

        // Repeating the transition from the resulting state is a no-op.
        let [sink, lp] = pair("usb");
        map.remove(&ModuleTag::Multi);
        map.remove(&sink);
        map.remove(&lp);
        assert!(ops_for(&intent, Some(&intent), &map).is_empty());
    }

    #[test]
    fn adding_a_member_loads_one_pair_then_reloads_multi_once() {
        let previous = multi(&[("local", 0), ("usb", 50)], true);
        let [sink, lp] = pair("usb");
        let map = loaded(&[ModuleTag::Multi, sink, lp]);
        let intent = multi(&[("local", 0), ("usb", 50), ("aux", 30)], true);
        let ops = ops_for(&intent, Some(&previous), &map);

        assert_eq!(
            ops,
            vec![
                RouteOp::Load {
                    tag: ModuleTag::DelaySink("aux".into()),
                    module: MODULE_NULL_SINK,
                    args: delay_sink_args("aux"),
                },
                RouteOp::Load {
                    tag: ModuleTag::DelayLoop("aux".into()),
                    module: MODULE_LOOPBACK,
                    args: delay_loop_args("aux", 30),
                },
                RouteOp::Unload {
                    tag: ModuleTag::Multi
                },
                RouteOp::Load {
                    tag: ModuleTag::Multi,
                    module: MODULE_COMBINE_SINK,
                    args: multi_args(&intent.members, true),
                },
                RouteOp::Unity {
                    sink: "aux".into(),
                },
            ]
        );
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, RouteOp::Unload { tag: ModuleTag::Multi }))
                .count(),
            1
        );
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, RouteOp::Load { tag: ModuleTag::Multi, .. }))
                .count(),
            1
        );
    }

    #[test]
    fn changing_one_delay_reloads_that_pair_and_multi() {
        let previous = multi(&[("local", 0), ("usb", 50)], true);
        let [sink, lp] = pair("usb");
        let map = loaded(&[ModuleTag::Multi, sink, lp]);
        let intent = multi(&[("local", 0), ("usb", 80)], true);
        let ops = ops_for(&intent, Some(&previous), &map);

        assert_eq!(
            ops,
            vec![
                RouteOp::Unload {
                    tag: ModuleTag::DelayLoop("usb".into())
                },
                RouteOp::Unload {
                    tag: ModuleTag::DelaySink("usb".into())
                },
                RouteOp::Load {
                    tag: ModuleTag::DelaySink("usb".into()),
                    module: MODULE_NULL_SINK,
                    args: delay_sink_args("usb"),
                },
                RouteOp::Load {
                    tag: ModuleTag::DelayLoop("usb".into()),
                    module: MODULE_LOOPBACK,
                    args: delay_loop_args("usb", 80),
                },
                RouteOp::Unload {
                    tag: ModuleTag::Multi
                },
                RouteOp::Load {
                    tag: ModuleTag::Multi,
                    module: MODULE_COMBINE_SINK,
                    args: multi_args(&intent.members, true),
                },
            ]
        );
    }

    #[test]
    fn compensate_flip_reloads_only_multi() {
        let previous = multi(&[("local", 0), ("usb", 50)], true);
        let [sink, lp] = pair("usb");
        let map = loaded(&[ModuleTag::Multi, sink, lp]);
        let intent = multi(&[("local", 0), ("usb", 50)], false);
        let ops = ops_for(&intent, Some(&previous), &map);

        assert_eq!(
            ops,
            vec![
                RouteOp::Unload {
                    tag: ModuleTag::Multi
                },
                RouteOp::Load {
                    tag: ModuleTag::Multi,
                    module: MODULE_COMBINE_SINK,
                    args: multi_args(&intent.members, false),
                },
            ]
        );
        assert!(ops.iter().all(
            |op| !matches!(op, RouteOp::Unload { tag: ModuleTag::DelaySink(_) | ModuleTag::DelayLoop(_) })
        ));
    }

    #[test]
    fn a_member_that_loses_its_delay_drops_its_pair_and_reloads_multi() {
        let previous = multi(&[("local", 0), ("usb", 50)], false);
        let [sink, lp] = pair("usb");
        let map = loaded(&[ModuleTag::Multi, sink, lp]);
        let intent = multi(&[("local", 0), ("usb", 0)], false);
        let ops = ops_for(&intent, Some(&previous), &map);

        assert_eq!(
            ops,
            vec![
                RouteOp::Unload {
                    tag: ModuleTag::DelayLoop("usb".into())
                },
                RouteOp::Unload {
                    tag: ModuleTag::DelaySink("usb".into())
                },
                RouteOp::Unload {
                    tag: ModuleTag::Multi
                },
                RouteOp::Load {
                    tag: ModuleTag::Multi,
                    module: MODULE_COMBINE_SINK,
                    args: "sink_name=plippa.multi slaves=local,usb latency_compensate=0 sink_properties=device.description=Plippa_Multi".into(),
                },
            ]
        );
    }

    #[test]
    fn repeated_identical_intent_produces_no_ops() {
        let intent = multi(&[("local", 0), ("usb", 50)], true);
        let [sink, lp] = pair("usb");
        let map = loaded(&[ModuleTag::Multi, sink, lp]);
        assert!(ops_for(&intent, Some(&intent), &map).is_empty());
    }

    #[test]
    fn direct_without_previous_still_keeps_no_plumbing() {
        let intent = direct("local");
        let [sink, lp] = pair("usb");
        let map = loaded(&[ModuleTag::Multi, sink, lp]);
        let ops = ops_for(&intent, None, &map);
        assert_eq!(
            ops,
            vec![
                RouteOp::Unload {
                    tag: ModuleTag::Multi
                },
                RouteOp::Unload {
                    tag: ModuleTag::DelayLoop("usb".into())
                },
                RouteOp::Unload {
                    tag: ModuleTag::DelaySink("usb".into())
                },
                RouteOp::Unity {
                    sink: "local".into(),
                },
            ]
        );
    }

    #[test]
    fn ops_for_is_deterministic_for_the_same_inputs() {
        let previous = multi(&[("local", 0), ("usb", 50), ("aux", 20)], false);
        let [usb_sink, usb_loop] = pair("usb");
        let [aux_sink, aux_loop] = pair("aux");
        let map = loaded(&[ModuleTag::Multi, usb_sink, usb_loop, aux_sink, aux_loop]);
        let intent = multi(&[("local", 0), ("usb", 70), ("aux", 20), ("phone", 40)], true);

        let first = ops_for(&intent, Some(&previous), &map);
        let second = ops_for(&intent, Some(&previous), &map);
        assert_eq!(first, second);
    }
}
