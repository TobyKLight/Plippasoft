//! Clock / thermal / fan telemetry providers (injectable for tests).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Supplies wall-clock HH:MM for the main screen.
pub trait ClockProvider {
    fn time_hhmm(&self) -> String;
}

/// SoC / board temperature in °C.
pub trait TempProvider {
    fn temperature_c(&self) -> Option<i32>;
}

/// Fan status string for the Debug screen.
pub trait FanProvider {
    fn fan_status(&self) -> String;
}

/// Bundle used by the UI loop.
pub struct SysProviders {
    pub clock: Box<dyn ClockProvider>,
    pub temp: Box<dyn TempProvider>,
    pub fan: Box<dyn FanProvider>,
}

impl SysProviders {
    pub fn mock(time_hhmm: impl Into<String>, temp_c: Option<i32>, fan: impl Into<String>) -> Self {
        Self {
            clock: Box::new(MockClock {
                time_hhmm: time_hhmm.into(),
            }),
            temp: Box::new(MockTemp { c: temp_c }),
            fan: Box::new(MockFan {
                status: fan.into(),
            }),
        }
    }

    /// Real sysfs / vcgencmd / hwmon backends for the device.
    pub fn system(fan_fallback: impl Into<String>) -> Self {
        Self {
            clock: Box::new(SystemClock),
            temp: Box::new(SysfsTemp::default()),
            fan: Box::new(SysfsFan::new(fan_fallback.into())),
        }
    }

    pub fn snapshot(&self) -> (String, Option<i32>, String) {
        (
            self.clock.time_hhmm(),
            self.temp.temperature_c(),
            self.fan.fan_status(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct MockClock {
    pub time_hhmm: String,
}

impl ClockProvider for MockClock {
    fn time_hhmm(&self) -> String {
        self.time_hhmm.clone()
    }
}

#[derive(Debug, Clone)]
pub struct MockTemp {
    pub c: Option<i32>,
}

impl TempProvider for MockTemp {
    fn temperature_c(&self) -> Option<i32> {
        self.c
    }
}

#[derive(Debug, Clone)]
pub struct MockFan {
    pub status: String,
}

impl FanProvider for MockFan {
    fn fan_status(&self) -> String {
        self.status.clone()
    }
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl ClockProvider for SystemClock {
    fn time_hhmm(&self) -> String {
        // Local time via libc-less path: UTC offset from env is fine for MVP;
        // prefer `chrono` later. Use process-local formatting from epoch + TZ.
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Apply crude local offset from `date` when available; else UTC.
        if let Ok(out) = Command::new("date").args(["+%H:%M"]).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.len() == 5 {
                    return s;
                }
            }
        }
        let mins = (secs / 60) % (24 * 60);
        format!("{:02}:{:02}", mins / 60, mins % 60)
    }
}

/// Reads `/sys/class/thermal/thermal_zone*/temp` (millidegrees) or `vcgencmd`.
#[derive(Debug, Clone)]
pub struct SysfsTemp {
    pub zone_glob: PathBuf,
}

impl Default for SysfsTemp {
    fn default() -> Self {
        Self {
            zone_glob: PathBuf::from("/sys/class/thermal"),
        }
    }
}

impl TempProvider for SysfsTemp {
    fn temperature_c(&self) -> Option<i32> {
        if let Some(c) = read_thermal_zone(&self.zone_glob) {
            return Some(c);
        }
        read_vcgencmd_temp()
    }
}

fn read_thermal_zone(thermal_root: &Path) -> Option<i32> {
    let entries = fs::read_dir(thermal_root).ok()?;
    for ent in entries.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let temp_path = ent.path().join("temp");
        let raw = fs::read_to_string(temp_path).ok()?;
        let milli: i32 = raw.trim().parse().ok()?;
        return Some(milli / 1000);
    }
    None
}

fn read_vcgencmd_temp() -> Option<i32> {
    let out = Command::new("vcgencmd").arg("measure_temp").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // e.g. temp=48.5'C
    let start = s.find('=')? + 1;
    let end = s[start..].find(|c: char| !c.is_ascii_digit() && c != '.')? + start;
    s[start..end].parse::<f32>().ok().map(|t| t as i32)
}

/// Fan status from hwmon PWM / RPM, or a documented stub string when absent.
#[derive(Debug, Clone)]
pub struct SysfsFan {
    pub fallback: String,
    pub hwmon_root: PathBuf,
}

impl SysfsFan {
    pub fn new(fallback: impl Into<String>) -> Self {
        Self {
            fallback: fallback.into(),
            hwmon_root: PathBuf::from("/sys/class/hwmon"),
        }
    }
}

impl FanProvider for SysfsFan {
    fn fan_status(&self) -> String {
        if let Some(s) = read_hwmon_fan(&self.hwmon_root) {
            return s;
        }
        if self.fallback.is_empty() {
            "absent".into()
        } else {
            format!("{} (stub)", self.fallback)
        }
    }
}

fn read_hwmon_fan(hwmon_root: &Path) -> Option<String> {
    let entries = fs::read_dir(hwmon_root).ok()?;
    for ent in entries.flatten() {
        let path = ent.path();
        // Prefer fan1_input (RPM) then pwm1.
        let rpm_path = path.join("fan1_input");
        if let Ok(raw) = fs::read_to_string(&rpm_path) {
            if let Ok(rpm) = raw.trim().parse::<u32>() {
                return Some(format!("{rpm} RPM"));
            }
        }
        let pwm_path = path.join("pwm1");
        if let Ok(raw) = fs::read_to_string(&pwm_path) {
            if let Ok(pwm) = raw.trim().parse::<u32>() {
                // Typical 0–255.
                let pct = (pwm * 100) / 255;
                return Some(format!("pwm {pct}%"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_providers_snapshot() {
        let sys = SysProviders::mock("15:30", Some(48), "pwm 40%");
        let (t, c, f) = sys.snapshot();
        assert_eq!(t, "15:30");
        assert_eq!(c, Some(48));
        assert_eq!(f, "pwm 40%");
    }
}
