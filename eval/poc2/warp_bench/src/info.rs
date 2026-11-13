//! `info` sub-command: what the board is doing while the numbers are taken.

use std::process::Command;

pub fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `Model` (Raspberry Pi) or `model name` from /proc/cpuinfo.
pub fn parse_cpu_model(cpuinfo: &str) -> Option<String> {
    for key in ["Model", "model name", "Hardware"] {
        for line in cpuinfo.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim() == key && !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

/// Value in kB of a /proc/meminfo field.
pub fn parse_meminfo_kb(meminfo: &str, field: &str) -> Option<u64> {
    meminfo.lines().find_map(|l| {
        let rest = l.strip_prefix(field)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// `vcgencmd get_throttled` prints `throttled=0x0`; returns `0x0`.
pub fn parse_throttled(out: &str) -> Option<String> {
    out.trim()
        .strip_prefix("throttled=")
        .map(|s| s.trim().to_string())
}

/// `pactl info` as `(key, value)` pairs.
pub fn parse_pactl_info(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// `thermal_zone0/temp` is millidegrees.
pub fn parse_temp_c(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().map(|m| m / 1000.0)
}

/// Collect `(key, value)` pairs; missing items are `n/a`.
pub fn collect() -> Vec<(String, String)> {
    let na = || "n/a".to_string();
    let mut v: Vec<(String, String)> = Vec::new();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    v.push((
        "cpu_model".into(),
        parse_cpu_model(&cpuinfo).unwrap_or_else(na),
    ));
    v.push(("arch".into(), std::env::consts::ARCH.into()));
    v.push((
        "cores".into(),
        std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_else(|_| na()),
    ));
    v.push((
        "governor".into(),
        read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").unwrap_or_else(na),
    ));
    let mhz = |p: &str| {
        read_trim(p)
            .and_then(|s| s.parse::<f64>().ok())
            .map(|k| format!("{:.0}", k / 1000.0))
    };
    v.push((
        "cur_mhz".into(),
        mhz("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq").unwrap_or_else(na),
    ));
    v.push((
        "max_mhz".into(),
        mhz("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").unwrap_or_else(na),
    ));
    v.push((
        "temp_c".into(),
        read_trim("/sys/class/thermal/thermal_zone0/temp")
            .and_then(|s| parse_temp_c(&s))
            .map(|t| format!("{t:.1}"))
            .unwrap_or_else(na),
    ));
    let throttled = run("vcgencmd", &["get_throttled"])
        .and_then(|o| parse_throttled(&o))
        .unwrap_or_else(na);
    v.push(("throttled".into(), throttled));
    v.push((
        "kernel".into(),
        read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(na),
    ));
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    v.push((
        "mem_total_mb".into(),
        parse_meminfo_kb(&meminfo, "MemTotal")
            .map(|k| (k / 1024).to_string())
            .unwrap_or_else(na),
    ));
    v.push((
        "mem_available_mb".into(),
        parse_meminfo_kb(&meminfo, "MemAvailable")
            .map(|k| (k / 1024).to_string())
            .unwrap_or_else(na),
    ));
    v.push((
        "swap_used_mb".into(),
        match (
            parse_meminfo_kb(&meminfo, "SwapTotal"),
            parse_meminfo_kb(&meminfo, "SwapFree"),
        ) {
            (Some(t), Some(f)) => (t.saturating_sub(f) / 1024).to_string(),
            _ => na(),
        },
    ));
    v.push((
        "loadavg".into(),
        read_trim("/proc/loadavg").unwrap_or_else(na),
    ));
    v.push((
        "build_profile".into(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .into(),
    ));
    v.push((
        "build_pulse".into(),
        if cfg!(feature = "pulse") { "yes" } else { "no" }.into(),
    ));
    let pactl = run("pactl", &["info"])
        .map(|o| parse_pactl_info(&o))
        .unwrap_or_default();
    for (k, key) in [
        ("Server Name", "pulse_server"),
        ("Server Version", "pulse_version"),
        ("Default Sink", "pulse_default_sink"),
        ("Default Sample Specification", "pulse_sample_spec"),
    ] {
        let val = pactl
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, pv)| pv.clone())
            .unwrap_or_else(na);
        v.push((key.into(), val));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        let cpuinfo = "processor\t: 0\nmodel name\t: ARMv8 Processor rev 3 (v8l)\nModel\t\t: Raspberry Pi Compute Module 4 Rev 1.0\n";
        assert_eq!(
            parse_cpu_model(cpuinfo).unwrap(),
            "Raspberry Pi Compute Module 4 Rev 1.0"
        );
        assert_eq!(parse_cpu_model("model name : X Y\n").unwrap(), "X Y");
        assert!(parse_cpu_model("nothing").is_none());
        let mem = "MemTotal:        3884932 kB\nMemAvailable:    3000000 kB\n";
        assert_eq!(parse_meminfo_kb(mem, "MemTotal"), Some(3884932));
        assert_eq!(parse_meminfo_kb(mem, "SwapTotal"), None);
        assert_eq!(parse_throttled("throttled=0x50005\n").unwrap(), "0x50005");
        assert!(parse_throttled("error").is_none());
        let p = parse_pactl_info(
            "Server Name: PulseAudio (on PipeWire 0.3.65)\nDefault Sink: alsa_output.x\n",
        );
        assert_eq!(
            p[0],
            (
                "Server Name".into(),
                "PulseAudio (on PipeWire 0.3.65)".into()
            )
        );
        assert_eq!(p[1].1, "alsa_output.x");
        assert_eq!(parse_temp_c("51234\n"), Some(51.234));
    }

    #[test]
    fn collect_always_has_the_keys_suggest_reads() {
        let c = collect();
        for k in [
            "throttled",
            "governor",
            "build_profile",
            "pulse_server",
            "cpu_model",
        ] {
            assert!(c.iter().any(|(ck, _)| ck == k), "missing {k}");
        }
        let prof = c.iter().find(|(k, _)| k == "build_profile").unwrap();
        assert!(prof.1 == "debug" || prof.1 == "release");
    }
}
