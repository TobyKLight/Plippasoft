//! Debug > Audio info (AUDIO_05): the fixed-order diagnostic report.
//!
//! [`audio_report`] is a pure snapshot of the mirrored audio route and the UI settings. It
//! never queries the engine, so every value is exactly what the screens already show. Labels
//! are stable and outputs stay in route order, so two reports can be diffed. Detail lines carry
//! their two-space indent in the label; the renderer draws the label as-is and the journal dump
//! trims it.
//!
//! The audio lines come first and in a fixed order. ACC_03 appends the radio report: the radio
//! state and its adapters, one block per known accessory and the scan state. The accessory data
//! is the mirrored worker snapshot; the report never queries the radio itself.

use crate::accessories::{kind_text, AccessoriesState};
use crate::audio::AudioSettings;
use crate::screens::{screen_text, SCREEN_TEXT_MAX_CHARS};
use plippa_core::audio_route::{AudioOutput, AudioRoute};

/// Longest value kept. `screen_text` already bounds hostile input; this keeps a single value
/// from crowding the label out of its row and the journal line readable.
const VALUE_MAX_CHARS: usize = 40;

/// One diagnostic line: `label` left, `value` right. No colours, no icons, no prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagLine {
    pub label: String,
    pub value: String,
}

impl DiagLine {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }

    /// A line below an output / device heading, indented by two spaces.
    fn detail(label: &str, value: impl Into<String>) -> Self {
        Self::new(format!("  {label}"), value)
    }
}

fn clean(s: &str) -> String {
    screen_text(s)
        .chars()
        .take(SCREEN_TEXT_MAX_CHARS.min(VALUE_MAX_CHARS))
        .collect()
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

fn state_text(route: &AudioRoute, output: &AudioOutput) -> &'static str {
    if !output.available {
        "missing"
    } else if route.paused.iter().any(|id| id == &output.id) {
        "paused"
    } else {
        "ok"
    }
}

/// True once the engine or the radio has reported anything. The untouched default route means
/// "not connected yet", which the screen draws as the shared empty state rather than a fake
/// report.
fn has_data(
    route: &AudioRoute,
    accessories: &AccessoriesState,
    has_backend: bool,
) -> bool {
    !route.outputs.is_empty()
        || !route.enabled.is_empty()
        || route.revision != 0
        || route.problem.is_some()
        || has_backend
        || accessories.available
        || !accessories.known.is_empty()
}

fn accessory_state_text(a: &plippa_hw::Accessory) -> String {
    let mut flags = Vec::new();
    flags.push(if a.paired { "paired" } else { "not paired" });
    if a.trusted {
        flags.push("trusted");
    }
    if a.connected {
        flags.push("connected");
    }
    flags.join(", ")
}

/// Snapshot of everything the UI knows, in a fixed order.
///
/// `has_backend` is false when the build/board has no accessory radio at all (feature off or
/// the device screen injected no handle); the report then says `radio = unavailable` instead of
/// inventing adapters.
pub fn audio_report(
    route: &AudioRoute,
    settings: &AudioSettings,
    accessories: &AccessoriesState,
    has_backend: bool,
    now_ms: u64,
) -> Vec<DiagLine> {
    if !has_data(route, accessories, has_backend) {
        return Vec::new();
    }

    let mut out = vec![
        DiagLine::new(
            "audio server",
            match &route.problem {
                Some(problem) => clean(problem),
                None => "ok".into(),
            },
        ),
        DiagLine::new("revision", route.revision.to_string()),
        DiagLine::new("local only", on_off(route.local_only)),
        DiagLine::new("delay auto", on_off(route.delay_auto)),
        DiagLine::new(
            "vis delay",
            if settings.waveform_vis_delay {
                format!("On ({} ms)", route.vis_delay_ms())
            } else {
                "Off".into()
            },
        ),
    ];

    for (index, output) in route.outputs.iter().enumerate() {
        out.push(DiagLine::new(
            format!("out {}", index + 1),
            clean(&output.id),
        ));
        out.push(DiagLine::detail("desc", clean(&output.description)));
        out.push(DiagLine::detail("state", state_text(route, output)));
        out.push(DiagLine::detail(
            "local",
            if output.is_local { "yes" } else { "no" },
        ));
        out.push(DiagLine::detail("delay", format!("{} ms", output.delay_ms)));
        if let Some(bus) = output.props.get("device.bus") {
            out.push(DiagLine::detail("bus", clean(bus)));
        }
        if let Some(address) = output.props.get("device.address") {
            out.push(DiagLine::detail("address", clean(address)));
        }
    }

    // ---- ACC_03: radio / accessories / scan ------------------------------------------
    let radio_ok = has_backend && accessories.radio_present;
    if !radio_ok {
        out.push(DiagLine::new("radio", "unavailable"));
    } else if accessories.adapters.is_empty() {
        out.push(DiagLine::new("radio", "no adapters"));
    } else {
        out.push(DiagLine::new("radio", "ok"));
        for (index, adapter) in accessories.adapters.iter().enumerate() {
            out.push(DiagLine::new(
                format!("adapter {}", index + 1),
                clean(&adapter.id),
            ));
            out.push(DiagLine::detail("address", clean(&adapter.address)));
            out.push(DiagLine::detail("powered", on_off(adapter.powered)));
        }
    }
    for (index, accessory) in accessories.sorted_known().iter().enumerate() {
        out.push(DiagLine::new(
            format!("accessory {}", index + 1),
            clean(&accessory.name),
        ));
        out.push(DiagLine::detail("id", clean(&accessory.id)));
        out.push(DiagLine::detail("kind", kind_text(accessory.kind)));
        out.push(DiagLine::detail("state", accessory_state_text(accessory)));
        out.push(DiagLine::detail(
            "auto connect",
            on_off(accessories.is_auto_connect(&accessory.id)),
        ));
    }
    let scan = if accessories.is_scanning() {
        let mut state = accessories.clone();
        state.now_ms = now_ms;
        format!("{} s left", state.scan_seconds_left())
    } else {
        "idle".into()
    };
    out.push(DiagLine::new("scan", scan));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn output(id: &str, available: bool, local: bool, delay_ms: u32) -> AudioOutput {
        AudioOutput {
            id: id.into(),
            description: format!("{id} description"),
            available,
            is_local: local,
            delay_ms,
            props: BTreeMap::new(),
        }
    }

    fn route() -> AudioRoute {
        AudioRoute {
            outputs: vec![
                output("local", true, true, 0),
                output("usb", false, false, 300),
            ],
            enabled: vec!["local".into(), "usb".into()],
            paused: vec!["usb".into()],
            revision: 7,
            ..AudioRoute::default()
        }
    }

    fn labels(report: &[DiagLine]) -> Vec<&str> {
        report.iter().map(|line| line.label.trim_start()).collect()
    }

    #[test]
    fn report_has_the_fixed_line_order_and_every_output_once() {
        let settings = AudioSettings::default();
        let accessories = AccessoriesState::default();
        let report = audio_report(&route(), &settings, &accessories, false, 0);
        assert_eq!(
            labels(&report),
            [
                "audio server",
                "revision",
                "local only",
                "delay auto",
                "vis delay",
                "out 1",
                "desc",
                "state",
                "local",
                "delay",
                "out 2",
                "desc",
                "state",
                "local",
                "delay",
                "radio",
                "scan",
            ]
        );
        assert_eq!(
            report
                .iter()
                .filter(|line| line.label.starts_with("out "))
                .count(),
            2,
            "each output exactly once"
        );
        assert_eq!(report[4].value, "On (300 ms)");
        assert_eq!(report[5].value, "local");
        assert_eq!(report[10].value, "usb");
        assert_eq!(report[7].value, "ok");
        let usb_state = &report[labels(&report).iter().position(|l| *l == "state").unwrap() + 5];
        assert_eq!(usb_state.value, "missing", "unavailable wins over paused");
        assert_eq!(report.last().unwrap().label, "scan");
        assert_eq!(report.last().unwrap().value, "idle");
        let radio = report
            .iter()
            .find(|line| line.label == "radio")
            .expect("radio line");
        assert_eq!(radio.value, "unavailable");
    }

    #[test]
    fn radio_adapters_accessories_and_scan_follow_the_audio_lines() {
        let mut accessories = AccessoriesState::default();
        accessories.available = true;
        accessories.adapters = vec![plippa_hw::AdapterInfo {
            id: "hci0".into(),
            address: "11:22:33:44:55:66".into(),
            name: "radio".into(),
            powered: true,
        }];
        accessories.known = vec![plippa_hw::Accessory {
            id: "aa:bb:cc:dd:ee:ff".into(),
            name: "Speaker".into(),
            kind: plippa_hw::AccessoryKind::Audio,
            paired: true,
            trusted: true,
            connected: true,
            battery_pct: None,
        }];
        accessories.settings.auto_connect.insert("aa:bb:cc:dd:ee:ff".into());
        accessories.stage = crate::accessories::PairStage::Scanning;
        accessories.scan_window = Some(plippa_hw::ScanWindow::start(
            std::time::Duration::ZERO,
            30,
        ));
        let report = audio_report(
            &route(),
            &AudioSettings::default(),
            &accessories,
            true,
            1_000,
        );
        let labels = labels(&report);
        assert_eq!(labels[0], "audio server", "audio lines stay on top");
        let radio = report.iter().position(|l| l.label == "radio").unwrap();
        let adapter = report
            .iter()
            .position(|l| l.label == "adapter 1")
            .unwrap();
        let accessory = report
            .iter()
            .position(|l| l.label == "accessory 1")
            .unwrap();
        let scan = report.iter().position(|l| l.label == "scan").unwrap();
        assert!(radio < adapter && adapter < accessory && accessory < scan);
        assert_eq!(report[radio].value, "ok");
        assert_eq!(report[adapter].value, "hci0");
        assert_eq!(report[accessory].value, "Speaker");
        assert_eq!(report[scan].value, "29 s left");
        assert!(report.iter().any(|l| l.label.trim() == "auto connect" && l.value == "On"));
        assert!(
            report
                .iter()
                .any(|l| l.label.trim() == "state" && l.value == "paired, trusted, connected")
        );

        let unavailable = audio_report(
            &route(),
            &AudioSettings::default(),
            &AccessoriesState::default(),
            false,
            0,
        );
        assert_eq!(
            unavailable
                .iter()
                .find(|l| l.label == "radio")
                .unwrap()
                .value,
            "unavailable"
        );
    }

    #[test]
    fn wireless_property_lines_appear_only_when_present() {
        let mut route = route();
        route.outputs[0]
            .props
            .insert("device.bus".into(), "usb".into());
        route.outputs[0]
            .props
            .insert("device.address".into(), "AA-BB-CC-DD-EE-FF".into());
        let report = audio_report(
            &route,
            &AudioSettings::default(),
            &AccessoriesState::default(),
            false,
            0,
        );
        assert_eq!(
            report
                .iter()
                .filter(|line| line.label.trim() == "bus")
                .count(),
            1
        );
        assert_eq!(
            report
                .iter()
                .filter(|line| line.label.trim() == "address")
                .count(),
            1
        );
        // The second output has no properties, so its block ends at `delay`.
        let second = report
            .iter()
            .position(|line| line.label == "out 2")
            .unwrap();
        assert!(
            report[second..]
                .iter()
                .all(|line| !matches!(line.label.trim(), "bus" | "address")),
            "{report:?}"
        );
    }

    #[test]
    fn long_and_hostile_descriptions_are_sanitised_and_truncated() {
        let mut route = route();
        route.outputs[0].description = format!("{}\u{202E}\u{0007}", "A".repeat(200));
        let report = audio_report(
            &route,
            &AudioSettings::default(),
            &AccessoriesState::default(),
            false,
            0,
        );
        let desc = report
            .iter()
            .find(|line| line.label.trim() == "desc")
            .unwrap();
        assert!(desc.value.chars().count() <= VALUE_MAX_CHARS, "{}", desc.value);
        assert!(
            !desc
                .value
                .chars()
                .any(|c| c.is_control() || "\u{202E}".contains(c)),
            "{:?}",
            desc.value
        );
    }

    #[test]
    fn a_route_problem_is_reported_and_the_untouched_default_is_empty() {
        let mut with_problem = route();
        with_problem.problem = Some("audio server offline".into());
        let report = audio_report(
            &with_problem,
            &AudioSettings::default(),
            &AccessoriesState::default(),
            false,
            0,
        );
        assert_eq!(report[0].value, "audio server offline");

        let off = AudioSettings {
            waveform_vis_delay: false,
            ..AudioSettings::default()
        };
        let report = audio_report(&route(), &off, &AccessoriesState::default(), false, 0);
        assert_eq!(report[4].value, "Off");

        assert!(audio_report(
            &AudioRoute::default(),
            &AudioSettings::default(),
            &AccessoriesState::default(),
            false,
            0
        )
        .is_empty());
    }
}
