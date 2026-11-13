//! `suggest`: pure mapping from measured numbers to suggested tuning constants.
//!
//! The rules are the measurable rows of the README "Result -> constant" table (WARP_02) and of the
//! constants register (WARP README section 7). Input is the `key = value` map of a results file.
//! Nothing here measures anything; it is plain arithmetic on numbers the owner measured on the CM4.

use crate::report::Table;
use std::collections::HashMap;

/// Budget: the stretcher alone may use at most this share of one core (README).
pub const STRETCH_BUDGET_PCT: f64 = 25.0;
/// Decode headroom: scan caps must satisfy `rate <= D x DECODE_SAFETY_FACTOR`.
pub const DECODE_SAFETY_FACTOR: f64 = 0.5;
/// `WindowConfig::WINDOW_CAP_BYTES` (6 MiB).
pub const WINDOW_CAP_BYTES: f64 = 6.0 * 1024.0 * 1024.0;
/// Shipped `WindowConfig::LOOKAHEAD_S`.
pub const LOOKAHEAD_DEFAULT_S: f64 = 9.0;
/// Smallest look-ahead the window may be trimmed to.
pub const LOOKAHEAD_MIN_S: f64 = 2.0;
/// Back-window rule: the fetch of a window must use at most this share of its playing time.
pub const BACK_WINDOW_SHARE: f64 = 0.7;

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub constant: &'static str,
    /// `Struct (file)` as named in README section 7.
    pub owner: &'static str,
    pub value: String,
    pub why: String,
}

#[derive(Debug, Clone, Default)]
pub struct SuggestResult {
    /// `false` when the run is invalid (throttled): no constants are suggested.
    pub valid: bool,
    pub suggestions: Vec<Suggestion>,
    pub flags: Vec<String>,
    pub notes: Vec<String>,
}

impl SuggestResult {
    #[cfg(test)]
    pub fn value_of(&self, constant: &str) -> Option<&str> {
        self.suggestions
            .iter()
            .find(|s| s.constant == constant)
            .map(|s| s.value.as_str())
    }

    fn add(&mut self, constant: &'static str, owner: &'static str, value: String, why: String) {
        self.suggestions.push(Suggestion {
            constant,
            owner,
            value,
            why,
        });
    }

    #[cfg(test)]
    pub fn has_flag(&self, needle: &str) -> bool {
        self.flags.iter().any(|f| f.contains(needle))
    }
}

fn num(m: &HashMap<String, String>, key: &str) -> Option<f64> {
    m.get(key)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

fn fmt_rate(v: f64) -> String {
    format!("{v:.1}")
}

/// Largest of {2, 4, 6, 8} that is `<= d x DECODE_SAFETY_FACTOR` (`None` when even 2 is too fast).
pub fn scan_cap_from_decode(d: f64) -> Option<f64> {
    let limit = d * DECODE_SAFETY_FACTOR;
    [8.0, 6.0, 4.0, 2.0].into_iter().find(|c| *c <= limit)
}

/// Stretcher caps from the seek-voice CPU at input rates 2/3/4: `(scan, chase, over_at_2x)`.
/// The largest rate within budget, contiguous from 2.0; chase is limited to 3.0.
pub fn stretch_caps(cpu2: f64, cpu3: Option<f64>, cpu4: Option<f64>) -> (f64, f64, bool) {
    if cpu2 > STRETCH_BUDGET_PCT {
        return (2.0, 2.0, true);
    }
    let mut scan = 2.0;
    if cpu3.is_some_and(|c| c <= STRETCH_BUDGET_PCT) {
        scan = 3.0;
        if cpu4.is_some_and(|c| c <= STRETCH_BUDGET_PCT) {
            scan = 4.0;
        }
    }
    (scan, scan.min(3.0), false)
}

/// Smallest `W` in {1, 2, 4, 8} s with `S_i + W/D <= 0.7 x W / M`.
pub fn back_window_s(s_indexed: f64, decode_x: f64, max_scan: f64) -> Option<f64> {
    [1.0, 2.0, 4.0, 8.0]
        .into_iter()
        .find(|w| s_indexed + w / decode_x <= BACK_WINDOW_SHARE * w / max_scan)
}

/// Highest source rate a W = 8 s back-window can sustain: `S_i + 8/D <= 0.7 x 8 / N`.
pub fn sustainable_rate_at_8s(s_indexed: f64, decode_x: f64) -> f64 {
    BACK_WINDOW_SHARE * 8.0 / (s_indexed + 8.0 / decode_x)
}

/// `(history_s, lookahead_s, window_s, clipped)` from the worst Accurate backward-seek cost.
pub fn history_and_lookahead(s_accurate: f64, max_scan: f64, window_s: f64) -> (f64, f64, bool) {
    let mut history = (s_accurate * max_scan).ceil().max(8.0);
    let mut look = LOOKAHEAD_DEFAULT_S;
    let mut clipped = false;
    if history + look > window_s {
        look = (window_s - history).max(LOOKAHEAD_MIN_S);
        if history + look > window_s {
            history = window_s - look;
            clipped = true;
        }
    }
    (history, look, clipped)
}

pub fn suggest(m: &HashMap<String, String>) -> SuggestResult {
    let mut r = SuggestResult {
        valid: true,
        ..Default::default()
    };

    // ---- info: throttled flag
    match m
        .get("info.throttled")
        .map(|s| s.trim().to_ascii_lowercase())
    {
        Some(t) if t == "0x0" || t == "0" => {}
        Some(t) if t == "n/a" || t.is_empty() => {
            r.notes.push(
                "info.throttled is n/a (no vcgencmd): could not verify the run was unthrottled"
                    .into(),
            );
        }
        Some(t) => {
            r.valid = false;
            r.flags.push(format!(
                "THROTTLED: vcgencmd get_throttled = {t} (not 0x0): the run is INVALID. Let the board cool down, power it from a good supply, and repeat; no constants are suggested."
            ));
            return r;
        }
        None => r
            .notes
            .push("info.throttled missing: run `warp_bench info` to record throttling".into()),
    }

    // ---- a debug build makes every number meaningless
    if m.get("info.build_profile")
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
        == Some("debug")
    {
        r.valid = false;
        r.flags.push(
            "DEBUG BUILD: warp_bench was built without --release, so every CPU/decode number is invalid. Rebuild with `PROFILE=release scripts/deploy.sh eval warp_bench`, rerun, no constants are suggested."
                .into(),
        );
        return r;
    }

    // ---- decode -> scan cap, chase cap
    let d = num(m, "decode.worst_median_x");
    let mut scan_cap: Option<f64> = None;
    match d {
        Some(d) => match scan_cap_from_decode(d) {
            Some(cap) => {
                scan_cap = Some(cap);
                r.add(
                    "MAX_SCAN_VARI",
                    "ScanProfile (scan.rs)",
                    fmt_rate(cap),
                    format!("worst decode median {d:.1}x realtime x {DECODE_SAFETY_FACTOR} = {:.1}; largest of 2/4/6/8 within it", d * DECODE_SAFETY_FACTOR),
                );
            }
            None => {
                scan_cap = Some(2.0);
                r.flags.push(format!(
                    "DECODE TOO SLOW: worst decode median {d:.1}x x {DECODE_SAFETY_FACTOR} is below 2.0, so no scan rung (2/4/6/8) is fast enough; MAX_SCAN_VARI set to the minimum 2.0 but scan on this media will not be reliable (check `decode` per file; drop to a faster format or rate)"
                ));
                r.add(
                    "MAX_SCAN_VARI",
                    "ScanProfile (scan.rs)",
                    "2.0".into(),
                    "minimum rung; decode is too slow for any scan rung".into(),
                );
            }
        },
        None => r
            .notes
            .push("decode.worst_median_x missing: run `warp_bench decode <files>`".into()),
    }

    // ---- cpu: varispeed / boxcar at the scan rates
    let (v2, b4, b8) = (
        num(m, "cpu.vari.2.0"),
        num(m, "cpu.boxcar.4.0"),
        num(m, "cpu.boxcar.8.0"),
    );
    if let (Some(v2), Some(b4), Some(b8)) = (v2, b4, b8) {
        let cpu_cap = if v2 > STRETCH_BUDGET_PCT {
            r.flags.push(format!(
                "VARISPEED OVER BUDGET at 2.0x ({v2:.1}% of a core): scan capped at 2.0"
            ));
            2.0
        } else if b4 > STRETCH_BUDGET_PCT {
            2.0
        } else if b8 > STRETCH_BUDGET_PCT {
            4.0
        } else {
            8.0
        };
        let new_cap = scan_cap.map_or(cpu_cap, |c| c.min(cpu_cap));
        if cpu_cap < 8.0 {
            r.flags.push(format!(
                "varispeed/boxcar CPU (2.0x: {v2:.1}%, 4x: {b4:.1}%, 8x: {b8:.1}%, budget {STRETCH_BUDGET_PCT:.0}%) lowers MAX_SCAN_VARI to {cpu_cap:.1}"
            ));
        }
        if scan_cap != Some(new_cap) {
            r.suggestions.retain(|s| s.constant != "MAX_SCAN_VARI");
            r.add(
                "MAX_SCAN_VARI",
                "ScanProfile (scan.rs)",
                fmt_rate(new_cap),
                format!("min of decode cap and varispeed/boxcar CPU cap ({cpu_cap:.1})"),
            );
        }
        scan_cap = Some(new_cap);
    } else {
        r.notes.push(
            "cpu.vari.2.0 / cpu.boxcar.4.0 / cpu.boxcar.8.0 missing: run `warp_bench cpu`".into(),
        );
    }
    if let Some(cap) = scan_cap {
        r.add(
            "R_MAX_VARI",
            "ChaseConfig (seek.rs)",
            fmt_rate(cap.min(6.0)),
            "min(6, MAX_SCAN_VARI)".into(),
        );
        r.add(
            "DECODE_SAFETY_FACTOR",
            "WindowConfig (window.rs)",
            "0.5".into(),
            "keep 0.5; use 0.4 only if the product still underruns during scans".into(),
        );
    }

    // ---- cpu: stretcher at seek-voice input rates
    if let Some(c2) = num(m, "cpu.stretch_seek.2.0") {
        let (scan, chase, over) = stretch_caps(
            c2,
            num(m, "cpu.stretch_seek.3.0"),
            num(m, "cpu.stretch_seek.4.0"),
        );
        if over {
            r.flags.push(format!(
                "STRETCH OVER BUDGET AT 2x: stretcher uses {c2:.1}% of a core at input rate 2.0 (budget {STRETCH_BUDGET_PCT:.0}%); pitch-correct seek caps set to 2.0 and flagged. Try the cheaper preset (STRETCH_PRESET) or keep seek mode on pitch shift"
            ));
        }
        let why = format!("seek-voice stretcher CPU at 2/3/4x within {STRETCH_BUDGET_PCT:.0}% of a core (2.0x: {c2:.1}%)");
        r.add(
            "MAX_SEEK_STRETCH_SCAN",
            "StretchConfig (stretch.rs)",
            fmt_rate(scan),
            why.clone(),
        );
        r.add(
            "MAX_SCAN_STRETCH",
            "ScanProfile (scan.rs)",
            fmt_rate(scan),
            "same value as MAX_SEEK_STRETCH_SCAN (the effective cap is the smaller of the two)"
                .into(),
        );
        r.add(
            "MAX_SEEK_STRETCH_CHASE",
            "StretchConfig (stretch.rs)",
            fmt_rate(chase),
            why,
        );
        r.add(
            "R_MAX_STRETCH",
            "ChaseConfig (seek.rs)",
            fmt_rate(chase),
            "same value as MAX_SEEK_STRETCH_CHASE".into(),
        );
    } else {
        r.notes
            .push("cpu.stretch_seek.2.0 missing: run `warp_bench cpu`".into());
    }

    // ---- cpu: stretcher as the speed voice
    let speed_keys = [
        "cpu.stretch_speed.0.5",
        "cpu.stretch_speed.1.0",
        "cpu.stretch_speed.2.0",
    ];
    let speed: Vec<Option<f64>> = speed_keys.iter().map(|k| num(m, k)).collect();
    if speed.iter().all(|v| v.is_some()) {
        let worst = speed.iter().flatten().cloned().fold(0.0, f64::max);
        if worst > STRETCH_BUDGET_PCT {
            let cheaper_keys = [
                "cpu.stretch_speed_cheaper.0.5",
                "cpu.stretch_speed_cheaper.1.0",
                "cpu.stretch_speed_cheaper.2.0",
            ];
            let cheaper: Vec<Option<f64>> = cheaper_keys.iter().map(|k| num(m, k)).collect();
            let cheaper_worst = if cheaper.iter().all(|v| v.is_some()) {
                Some(cheaper.iter().flatten().cloned().fold(0.0, f64::max))
            } else {
                None
            };
            match cheaper_worst {
                Some(cw) if cw <= STRETCH_BUDGET_PCT => {
                    r.add(
                        "STRETCH_PRESET",
                        "StretchConfig (stretch.rs)",
                        "cheaper".into(),
                        format!("default preset worst {worst:.1}% > {STRETCH_BUDGET_PCT:.0}% budget; cheaper preset worst {cw:.1}% fits"),
                    );
                }
                Some(cw) => {
                    r.flags.push(format!(
                        "SPEED STRETCH OVER BUDGET: default preset {worst:.1}%, cheaper preset {cw:.1}% of a core (budget {STRETCH_BUDGET_PCT:.0}%): set SPEED_WARP_DEFAULT back to PitchShift (plippa-core) and also raise STRETCH_BLOCK_FRAMES only if listening says it is acceptable"
                    ));
                    r.add(
                        "STRETCH_PRESET",
                        "StretchConfig (stretch.rs)",
                        "cheaper".into(),
                        "cheapest available; still over budget".into(),
                    );
                    r.add(
                        "SPEED_WARP_DEFAULT",
                        "plippa-core (warp.rs)",
                        "PitchShift".into(),
                        "stretcher too expensive as the default speed voice".into(),
                    );
                }
                None => {
                    r.flags.push(format!(
                        "speed-voice stretcher {worst:.1}% > {STRETCH_BUDGET_PCT:.0}% budget and the cheaper-preset numbers are missing: rerun `warp_bench cpu`"
                    ));
                }
            }
        }
    } else {
        r.notes
            .push("cpu.stretch_speed.* missing: run `warp_bench cpu`".into());
    }

    // ---- BACK_WINDOW_S
    let s_i = num(m, "usbread.indexed_seek_s");
    match (s_i, d) {
        (Some(s_i), Some(d)) if d > 0.0 => {
            let m_cap = scan_cap.unwrap_or(8.0);
            match back_window_s(s_i, d, m_cap) {
                Some(w) => r.add(
                    "BACK_WINDOW_S",
                    "WindowConfig (window.rs)",
                    format!("{w:.0}"),
                    format!("smallest W with S_i + W/D <= 0.7 x W/MAX_SCAN_VARI (S_i {:.0} ms, D {d:.1}x, MAX_SCAN_VARI {m_cap:.1})", s_i * 1000.0),
                ),
                None => {
                    let n = sustainable_rate_at_8s(s_i, d);
                    r.flags.push(format!(
                        "NO BACK WINDOW FITS: MP3 reverse above {n:.1}x will stall even with W = 8 s: lower MAX_SCAN_VARI for MP3 to {:.1} (or accept stalls / hold-and-fade)",
                        (n * 2.0).floor() / 2.0
                    ));
                    r.add(
                        "BACK_WINDOW_S",
                        "WindowConfig (window.rs)",
                        "8".into(),
                        "no size satisfies the rule; use the largest".into(),
                    );
                }
            }
        }
        _ => r.notes.push(
            "usbread.indexed_seek_s or decode.worst_median_x missing: BACK_WINDOW_S not suggested"
                .into(),
        ),
    }

    // ---- HISTORY_S / LOOKAHEAD_S
    if let Some(s_a) = num(m, "seek.worst_backward_accurate_s") {
        let m_cap = scan_cap.unwrap_or(8.0);
        let bytes_per_s = num(m, "decode.max_frame_bytes_per_s")
            .filter(|b| *b > 0.0)
            .unwrap_or(44100.0 * 2.0 * 4.0);
        let window_s = (WINDOW_CAP_BYTES / bytes_per_s).floor();
        let (history, look, clipped) = history_and_lookahead(s_a, m_cap, window_s);
        r.add(
            "HISTORY_S",
            "WindowConfig (window.rs)",
            format!("{history:.0}"),
            format!("max(8, ceil(S_a {s_a:.2} s x MAX_SCAN_VARI {m_cap:.1})) within the {window_s:.0} s window cap"),
        );
        r.add(
            "LOOKAHEAD_S",
            "WindowConfig (window.rs)",
            format!("{look:.0}"),
            format!("HISTORY_S + LOOKAHEAD_S <= {window_s:.0} s (6 MiB at the largest measured frame size)"),
        );
        if clipped {
            r.flags.push(format!(
                "HISTORY CLIPPED: an Accurate backward seek costs {s_a:.2} s, so hiding it at {m_cap:.1}x needs {:.0} s of history but the window only holds {window_s:.0} s: the first rewinds before the MP3 index exists use the slow hold-and-fade path",
                (s_a * m_cap).ceil()
            ));
        }
    } else {
        r.notes
            .push("seek.worst_backward_accurate_s missing: run `warp_bench seek <files>`".into());
    }

    // ---- MP3 index build time (informational)
    if let Some(b) = num(m, "usbread.index_build_s") {
        r.notes.push(format!(
            "estimated MP3 index build time B = {b:.1} s: a rewind within B s of loading an MP3 uses the slow path (hold-and-fade)"
        ));
        if b > 20.0 {
            r.flags.push(format!(
                "INDEX BUILD SLOW: B = {b:.1} s > 20 s: raise the index worker priority or MP3_INDEX_STRIDE_FRAMES (WindowConfig, window.rs), e.g. 100 -> 200"
            ));
        }
    }

    // ---- latency
    let mut tls: Vec<u32> = m
        .keys()
        .filter_map(|k| {
            k.strip_prefix("latency.tl_")?
                .strip_suffix(".underruns")?
                .parse()
                .ok()
        })
        .collect();
    tls.sort_unstable();
    if tls.is_empty() {
        r.notes
            .push("latency.* missing: run `warp_bench latency` (needs the pulse build)".into());
    } else {
        let good = tls
            .iter()
            .find(|tl| num(m, &format!("latency.tl_{tl}.underruns")) == Some(0.0));
        match good {
            Some(tl) => {
                let avg = num(m, &format!("latency.tl_{tl}.avg_ms"));
                r.add(
                    "PULSE_TLENGTH_MS",
                    "PulseConfig (sink/pulse.rs)",
                    format!("{tl}"),
                    "smallest tlength with 0 underruns in 10 s; add 40 ms if the product still underruns under load".into(),
                );
                r.add(
                    "PULSE_MINREQ_MS",
                    "PulseConfig (sink/pulse.rs)",
                    format!("{}", (*tl as f64 / 4.0).round()),
                    "tlength / 4".into(),
                );
                match avg {
                    Some(a) => r.add(
                        "SINK_LATENCY_MS",
                        "PulseConfig (sink/pulse.rs)",
                        format!("{a:.0}"),
                        format!("measured average Simple::get_latency at tlength {tl}"),
                    ),
                    None => r.notes.push(format!(
                        "latency.tl_{tl}.avg_ms missing: SINK_LATENCY_MS not suggested"
                    )),
                }
            }
            None => {
                r.flags.push("LATENCY: every tested tlength underran in 10 s: use the largest tested value and investigate (CPU load, PipeWire quantum)".into());
                let last = tls.last().copied().unwrap_or(240);
                r.add(
                    "PULSE_TLENGTH_MS",
                    "PulseConfig (sink/pulse.rs)",
                    format!("{last}"),
                    "largest tested; all underran".into(),
                );
                r.add(
                    "PULSE_MINREQ_MS",
                    "PulseConfig (sink/pulse.rs)",
                    format!("{}", (last as f64 / 4.0).round()),
                    "tlength / 4".into(),
                );
            }
        }
    }

    r
}

/// Human table + flags + machine lines (`suggest.<CONSTANT> = value`).
pub fn render(r: &SuggestResult) -> String {
    let mut out = String::new();
    if r.suggestions.is_empty() {
        out.push_str("(no constants suggested)\n");
    } else {
        let mut t = Table::new(&["Constant", "Struct (file)", "Suggested", "Why"]);
        for s in &r.suggestions {
            t.row(vec![
                s.constant.to_string(),
                s.owner.to_string(),
                s.value.clone(),
                s.why.clone(),
            ]);
        }
        out.push_str(&t.render());
    }
    if !r.flags.is_empty() {
        out.push_str("\nFLAGS\n");
        for f in &r.flags {
            out.push_str(&format!("  ! {f}\n"));
        }
    }
    if !r.notes.is_empty() {
        out.push_str("\nNOTES\n");
        for n in &r.notes {
            out.push_str(&format!("  - {n}\n"));
        }
    }
    out.push('\n');
    out.push_str(&format!("suggest.valid = {}\n", r.valid));
    for s in &r.suggestions {
        out.push_str(&format!("suggest.{} = {}\n", s.constant, s.value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn good_run() -> Vec<(&'static str, &'static str)> {
        vec![
            ("info.throttled", "0x0"),
            ("decode.worst_median_x", "40.0"),
            ("decode.max_frame_bytes_per_s", "352800"),
            ("cpu.vari.2.0", "6.0"),
            ("cpu.boxcar.4.0", "7.0"),
            ("cpu.boxcar.8.0", "9.0"),
            ("cpu.stretch_seek.2.0", "12.0"),
            ("cpu.stretch_seek.3.0", "18.0"),
            ("cpu.stretch_seek.4.0", "24.0"),
            ("cpu.stretch_speed.0.5", "10.0"),
            ("cpu.stretch_speed.1.0", "11.0"),
            ("cpu.stretch_speed.2.0", "12.0"),
            ("usbread.indexed_seek_s", "0.08"),
            ("usbread.index_build_s", "12.0"),
            ("seek.worst_backward_accurate_s", "0.5"),
            ("latency.tl_60.underruns", "3"),
            ("latency.tl_80.underruns", "0"),
            ("latency.tl_80.avg_ms", "71.4"),
            ("latency.tl_120.underruns", "0"),
            ("latency.tl_120.avg_ms", "108.0"),
        ]
    }

    fn with(
        base: Vec<(&'static str, &'static str)>,
        over: &[(&'static str, &'static str)],
    ) -> HashMap<String, String> {
        let mut m = data(&base);
        for (k, v) in over {
            m.insert(k.to_string(), v.to_string());
        }
        m
    }

    #[test]
    fn decode_rule_table() {
        for (d, expect) in [
            (100.0, Some(8.0)),
            (16.0, Some(8.0)),
            (15.9, Some(6.0)),
            (12.0, Some(6.0)),
            (11.9, Some(4.0)),
            (8.0, Some(4.0)),
            (7.9, Some(2.0)),
            (4.0, Some(2.0)),
            (3.9, None),
            (0.0, None),
        ] {
            assert_eq!(scan_cap_from_decode(d), expect, "D = {d}");
        }
    }

    #[test]
    fn stretch_caps_table() {
        assert_eq!(
            stretch_caps(12.0, Some(18.0), Some(24.0)),
            (4.0, 3.0, false)
        );
        assert_eq!(
            stretch_caps(12.0, Some(18.0), Some(26.0)),
            (3.0, 3.0, false)
        );
        assert_eq!(
            stretch_caps(12.0, Some(26.0), Some(30.0)),
            (2.0, 2.0, false)
        );
        assert_eq!(
            stretch_caps(12.0, Some(26.0), Some(20.0)),
            (2.0, 2.0, false),
            "contiguous from 2.0"
        );
        assert_eq!(
            stretch_caps(25.0, Some(20.0), Some(20.0)),
            (4.0, 3.0, false),
            "exactly at budget is within"
        );
        assert_eq!(stretch_caps(25.1, Some(1.0), Some(1.0)), (2.0, 2.0, true));
        assert_eq!(stretch_caps(10.0, None, None), (2.0, 2.0, false));
    }

    #[test]
    fn back_window_table() {
        // D = 100x, M = 8: W = 2 needs S_i <= 0.7*2/8 - 0.02 = 0.155
        assert_eq!(back_window_s(0.10, 100.0, 8.0), Some(2.0));
        assert_eq!(back_window_s(0.01, 100.0, 8.0), Some(1.0));
        assert_eq!(back_window_s(0.30, 100.0, 8.0), Some(4.0));
        assert_eq!(back_window_s(0.60, 100.0, 8.0), Some(8.0));
        assert_eq!(back_window_s(2.0, 100.0, 8.0), None);
        assert!(sustainable_rate_at_8s(2.0, 100.0) < 3.0);
    }

    #[test]
    fn history_table() {
        // 17 s window (44.1 kHz stereo f32)
        assert_eq!(history_and_lookahead(0.5, 8.0, 17.0), (8.0, 9.0, false));
        assert_eq!(history_and_lookahead(1.5, 8.0, 17.0), (12.0, 5.0, false));
        assert_eq!(history_and_lookahead(2.0, 8.0, 17.0), (15.0, 2.0, true));
        let (h, l, clipped) = history_and_lookahead(30.0, 8.0, 17.0);
        assert!(clipped);
        assert_eq!((h, l), (15.0, 2.0));
        // 48 kHz stereo: 16 s window
        assert_eq!(history_and_lookahead(0.5, 8.0, 16.0), (8.0, 8.0, false));
    }

    #[test]
    fn good_run_end_to_end() {
        let r = suggest(&with(good_run(), &[]));
        assert!(r.valid);
        assert_eq!(r.value_of("MAX_SCAN_VARI"), Some("8.0"));
        assert_eq!(r.value_of("R_MAX_VARI"), Some("6.0"));
        assert_eq!(r.value_of("DECODE_SAFETY_FACTOR"), Some("0.5"));
        assert_eq!(r.value_of("MAX_SEEK_STRETCH_SCAN"), Some("4.0"));
        assert_eq!(r.value_of("MAX_SEEK_STRETCH_CHASE"), Some("3.0"));
        assert_eq!(r.value_of("R_MAX_STRETCH"), Some("3.0"));
        assert_eq!(r.value_of("MAX_SCAN_STRETCH"), Some("4.0"));
        assert_eq!(r.value_of("BACK_WINDOW_S"), Some("2"));
        assert_eq!(r.value_of("HISTORY_S"), Some("8"));
        assert_eq!(r.value_of("LOOKAHEAD_S"), Some("9"));
        assert_eq!(r.value_of("PULSE_TLENGTH_MS"), Some("80"));
        assert_eq!(r.value_of("PULSE_MINREQ_MS"), Some("20"));
        assert_eq!(r.value_of("SINK_LATENCY_MS"), Some("71"));
        assert_eq!(r.value_of("STRETCH_PRESET"), None);
        assert!(r.flags.is_empty(), "{:?}", r.flags);
    }

    #[test]
    fn throttled_run_is_invalid() {
        let r = suggest(&with(good_run(), &[("info.throttled", "0x50005")]));
        assert!(!r.valid);
        assert!(r.suggestions.is_empty());
        assert!(r.has_flag("THROTTLED"));
        assert!(render(&r).contains("suggest.valid = false"));
    }

    #[test]
    fn debug_build_is_invalid() {
        let r = suggest(&with(good_run(), &[("info.build_profile", "debug")]));
        assert!(!r.valid && r.suggestions.is_empty());
        assert!(r.has_flag("DEBUG BUILD"));
        let r = suggest(&with(good_run(), &[("info.build_profile", "release")]));
        assert!(r.valid && !r.suggestions.is_empty());
    }

    #[test]
    fn nothing_is_fast_enough() {
        let r = suggest(&with(good_run(), &[("decode.worst_median_x", "3.0")]));
        assert!(r.valid);
        assert!(r.has_flag("DECODE TOO SLOW"));
        assert_eq!(r.value_of("MAX_SCAN_VARI"), Some("2.0"));
        assert_eq!(r.value_of("R_MAX_VARI"), Some("2.0"));
    }

    #[test]
    fn stretch_over_budget_at_2x() {
        let r = suggest(&with(good_run(), &[("cpu.stretch_seek.2.0", "40.0")]));
        assert!(r.has_flag("STRETCH OVER BUDGET AT 2x"));
        assert_eq!(r.value_of("MAX_SEEK_STRETCH_SCAN"), Some("2.0"));
        assert_eq!(r.value_of("MAX_SEEK_STRETCH_CHASE"), Some("2.0"));
        assert_eq!(r.value_of("R_MAX_STRETCH"), Some("2.0"));
    }

    #[test]
    fn speed_voice_over_budget_uses_cheaper_preset_then_falls_back() {
        let over = [("cpu.stretch_speed.2.0", "31.0")];
        // cheaper preset fits
        let mut base = good_run();
        base.extend([
            ("cpu.stretch_speed_cheaper.0.5", "8.0"),
            ("cpu.stretch_speed_cheaper.1.0", "9.0"),
            ("cpu.stretch_speed_cheaper.2.0", "20.0"),
        ]);
        let r = suggest(&with(base.clone(), &over));
        assert_eq!(r.value_of("STRETCH_PRESET"), Some("cheaper"));
        assert_eq!(r.value_of("SPEED_WARP_DEFAULT"), None);
        // cheaper also over budget
        let r = suggest(&with(
            base,
            &[
                ("cpu.stretch_speed.2.0", "31.0"),
                ("cpu.stretch_speed_cheaper.2.0", "27.0"),
            ],
        ));
        assert_eq!(r.value_of("SPEED_WARP_DEFAULT"), Some("PitchShift"));
        assert!(r.has_flag("SPEED STRETCH OVER BUDGET"));
        // cheaper not measured
        let r = suggest(&with(good_run(), &over));
        assert!(r.has_flag("cheaper-preset numbers are missing"));
    }

    #[test]
    fn varispeed_cpu_lowers_scan_cap() {
        let r = suggest(&with(good_run(), &[("cpu.boxcar.8.0", "30.0")]));
        assert_eq!(r.value_of("MAX_SCAN_VARI"), Some("4.0"));
        assert_eq!(r.value_of("R_MAX_VARI"), Some("4.0"));
        let r = suggest(&with(good_run(), &[("cpu.boxcar.4.0", "30.0")]));
        assert_eq!(r.value_of("MAX_SCAN_VARI"), Some("2.0"));
        // the decode cap still wins when it is lower
        let r = suggest(&with(good_run(), &[("decode.worst_median_x", "8.0")]));
        assert_eq!(r.value_of("MAX_SCAN_VARI"), Some("4.0"));
        assert_eq!(
            r.suggestions
                .iter()
                .filter(|s| s.constant == "MAX_SCAN_VARI")
                .count(),
            1
        );
    }

    #[test]
    fn no_back_window_satisfies_the_rule() {
        let r = suggest(&with(good_run(), &[("usbread.indexed_seek_s", "2.5")]));
        assert_eq!(r.value_of("BACK_WINDOW_S"), Some("8"));
        assert!(r.has_flag("NO BACK WINDOW FITS"));
        assert!(r.has_flag("MP3 reverse above"));
    }

    #[test]
    fn history_is_clipped_for_slow_accurate_seeks() {
        let r = suggest(&with(
            good_run(),
            &[("seek.worst_backward_accurate_s", "12.0")],
        ));
        assert_eq!(r.value_of("HISTORY_S"), Some("15"));
        assert_eq!(r.value_of("LOOKAHEAD_S"), Some("2"));
        assert!(r.has_flag("HISTORY CLIPPED"));
        // 48 kHz material has a 16 s window
        let r = suggest(&with(
            good_run(),
            &[
                ("seek.worst_backward_accurate_s", "1.5"),
                ("decode.max_frame_bytes_per_s", "384000"),
            ],
        ));
        assert_eq!(r.value_of("HISTORY_S"), Some("12"));
        assert_eq!(r.value_of("LOOKAHEAD_S"), Some("4"));
    }

    #[test]
    fn slow_index_build_is_flagged() {
        let r = suggest(&with(good_run(), &[("usbread.index_build_s", "35.0")]));
        assert!(r.has_flag("INDEX BUILD SLOW"));
        assert!(r.notes.iter().any(|n| n.contains("35.0")));
    }

    #[test]
    fn latency_rules() {
        let r = suggest(&with(good_run(), &[]));
        assert_eq!(r.value_of("PULSE_TLENGTH_MS"), Some("80"));
        let mut m = data(&good_run());
        for tl in [60, 80, 120] {
            m.insert(format!("latency.tl_{tl}.underruns"), "2".into());
        }
        let r = suggest(&m);
        assert!(r.has_flag("LATENCY"));
        assert_eq!(r.value_of("PULSE_TLENGTH_MS"), Some("120"));
    }

    #[test]
    fn missing_data_only_produces_notes() {
        let r = suggest(&HashMap::new());
        assert!(r.valid);
        assert!(r.suggestions.is_empty());
        assert!(r.flags.is_empty());
        assert!(!r.notes.is_empty());
        assert!(render(&r).contains("(no constants suggested)"));
    }

    #[test]
    fn render_lists_machine_lines() {
        let r = suggest(&with(good_run(), &[]));
        let text = render(&r);
        assert!(text.contains("suggest.MAX_SCAN_VARI = 8.0"));
        assert!(text.contains("ScanProfile (scan.rs)"));
        let parsed = crate::report::parse_results(&text);
        assert_eq!(parsed.get("suggest.R_MAX_VARI").unwrap(), "6.0");
    }
}
