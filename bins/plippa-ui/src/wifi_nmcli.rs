//! Real Wi‑Fi backend via NetworkManager's `nmcli`.
//!
//! # Choice (MVP_11)
//!
//! **NetworkManager / `nmcli`** — available on Raspberry Pi OS desktop and most
//! Debian images used for POC2. Keeps the UI process free of D-Bus crates; we
//! shell out to `nmcli` for scan / connect / IP.
//!
//! Dry-run: set `PLIPPA_WIFI_DRY_RUN=1` to skip real `nmcli` and return mock
//! success (used by ignored hardware tests / desktop without wifi hardware).
//!
//! Fallback: if `nmcli` is missing, `scan` returns `[]` and `connect` fails
//! with a clear message — UI still runs.
//!
//! Credentials remain in NM after a successful connect — Plippa does not keep a
//! parallel password DB. [`WifiBackend::has_saved_credentials`] checks for an
//! existing NM connection profile matching the SSID.

use crate::wifi::{WifiBackend, WifiConnectResult};
use std::env;
use std::process::Command;

const ENV_DRY_RUN: &str = "PLIPPA_WIFI_DRY_RUN";
const DEFAULT_PORTAL_PORT: u16 = 8787;

/// NetworkManager-backed Wi‑Fi.
#[derive(Debug, Clone)]
pub struct NmcliWifi {
    pub portal_port: u16,
    pub dry_run: bool,
    /// Cached IP after a successful connect / refresh.
    ip: Option<String>,
    /// Dry-run association SSID.
    dry_ssid: Option<String>,
    dry_known: Vec<String>,
}

impl Default for NmcliWifi {
    fn default() -> Self {
        Self::new()
    }
}

impl NmcliWifi {
    pub fn new() -> Self {
        Self {
            portal_port: DEFAULT_PORTAL_PORT,
            dry_run: env::var_os(ENV_DRY_RUN).is_some(),
            ip: None,
            dry_ssid: None,
            dry_known: Vec::new(),
        }
    }

    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    fn refresh_ip(&mut self) {
        if self.dry_run {
            if self.ip.is_none() {
                self.ip = Some("192.168.1.50".into());
            }
            return;
        }
        self.ip = query_ips().into_iter().next();
    }
}

impl WifiBackend for NmcliWifi {
    fn scan(&mut self) -> Vec<String> {
        if self.dry_run {
            return vec!["DryRunLAN".into(), "LabAP".into()];
        }
        nmcli_scan().unwrap_or_default()
    }

    fn connect(&mut self, ssid: &str, password: &str) -> WifiConnectResult {
        if self.dry_run {
            self.ip = Some("192.168.1.50".into());
            self.dry_ssid = Some(ssid.to_string());
            if !self.dry_known.iter().any(|s| s == ssid) {
                self.dry_known.push(ssid.to_string());
            }
            return WifiConnectResult::Ok {
                ip: self.ip.clone().unwrap(),
            };
        }
        match nmcli_connect(ssid, password) {
            Ok(()) => {
                self.refresh_ip();
                match &self.ip {
                    Some(ip) => WifiConnectResult::Ok { ip: ip.clone() },
                    None => WifiConnectResult::Ok {
                        ip: "(connected, IP pending)".into(),
                    },
                }
            }
            Err(message) => WifiConnectResult::Failed { message },
        }
    }

    fn current_ip(&self) -> Option<String> {
        // Dry-run must answer from its own state: a real query would report the desk machine's
        // address and hide a disconnect.
        if self.dry_run {
            return self.ip.clone();
        }
        self.ip.clone().or_else(|| query_ips().into_iter().next())
    }

    fn portal_url(&self) -> Option<String> {
        self.current_ip()
            .map(|ip| format!("http://{ip}:{}", self.portal_port))
    }

    fn current_ssid(&self) -> Option<String> {
        if self.dry_run {
            return self.dry_ssid.clone();
        }
        nmcli_active_ssid()
    }

    fn all_ips(&self) -> Vec<String> {
        if self.dry_run {
            return self.ip.iter().cloned().collect();
        }
        let mut ips = query_ips();
        if ips.is_empty() {
            if let Some(ip) = &self.ip {
                ips.push(ip.clone());
            }
        }
        ips
    }

    fn has_saved_credentials(&self, ssid: &str) -> bool {
        if self.dry_run {
            return self.dry_known.iter().any(|s| s == ssid);
        }
        nmcli_has_connection(ssid)
    }

    fn saved_networks(&self) -> Vec<String> {
        if self.dry_run {
            let mut names: Vec<String> = self
                .dry_known
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            names.sort();
            names.dedup();
            return names;
        }
        nmcli_connection_names()
    }

    fn disconnect(&mut self) -> Result<(), String> {
        if self.dry_run {
            self.dry_ssid = None;
            self.ip = None;
            return Ok(());
        }
        let iface = nmcli_wifi_device()?;
        nmcli_run(&disconnect_argv(&iface))?;
        // The address goes with the association; the next `current_ip` confirms.
        self.ip = None;
        Ok(())
    }

    fn forget(&mut self, ssid: &str) -> Result<(), String> {
        if self.dry_run {
            self.dry_known.retain(|s| s != ssid);
            if self.dry_ssid.as_deref() == Some(ssid) {
                self.dry_ssid = None;
                self.ip = None;
            }
            return Ok(());
        }
        nmcli_run(&delete_argv(ssid))
    }
}

/// The `nmcli` arguments that delete one saved profile. The profile name is the SSID, which is
/// what [`nmcli_has_connection`] already assumes.
fn delete_argv(ssid: &str) -> Vec<String> {
    vec!["connection".into(), "delete".into(), ssid.to_string()]
}

/// The `nmcli` arguments that drop one interface's association.
fn disconnect_argv(iface: &str) -> Vec<String> {
    vec!["dev".into(), "disconnect".into(), iface.to_string()]
}

/// Run one `nmcli` invocation whose only interesting output is success/failure.
///
/// The error never echoes stderr: it can carry the SSID and absolute paths (the same reason
/// [`auth_hint`] exists). A polkit refusal keeps its operator sentence.
fn nmcli_run(args: &[String]) -> Result<(), String> {
    let out = Command::new("nmcli")
        .args(args)
        .output()
        .map_err(|e| format!("nmcli not available: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(match auth_hint(stderr.trim()) {
        Some(hint) => hint.to_string(),
        None => {
            if args.first().map(String::as_str) == Some("connection") {
                "Could not remove the saved network".to_string()
            } else {
                "Could not disconnect from this network".to_string()
            }
        }
    })
}

/// Saved wireless profile names, sorted and deduplicated.
fn nmcli_connection_names() -> Vec<String> {
    let out = match Command::new("nmcli")
        .args(["-t", "-f", "NAME,TYPE", "connection", "show"])
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => return Vec::new(),
    };
    parse_connection_list(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `nmcli -t -f NAME,TYPE connection show` output into wireless profile names.
///
/// `-t` escapes a literal `:` inside a field as `\:`, so the line is split on the first
/// **unescaped** separator, then the fields are unescaped. Non-wireless profiles (VPNs,
/// ethernet) are dropped.
fn parse_connection_list(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (name, ty) = split_escaped(line);
        let ty = ty.to_lowercase();
        if !(ty.contains("wireless") || ty.contains("wifi") || ty.contains("802-11")) {
            continue;
        }
        let name = name.trim().to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// Split a `nmcli -t` line on its first unescaped `:`, unescaping both halves.
fn split_escaped(line: &str) -> (String, String) {
    let mut name = String::new();
    let mut rest = String::new();
    let mut escaped = false;
    let mut in_rest = false;
    for ch in line.chars() {
        if escaped {
            if in_rest {
                rest.push(ch);
            } else {
                name.push(ch);
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            ':' if !in_rest => in_rest = true,
            _ => {
                if in_rest {
                    rest.push(ch);
                } else {
                    name.push(ch);
                }
            }
        }
    }
    if escaped {
        // A dangling backslash is literal content, not an escape.
        if in_rest {
            rest.push('\\');
        } else {
            name.push('\\');
        }
    }
    (name, rest)
}

/// The first wireless device name from `nmcli -t -f DEVICE,TYPE dev status`.
fn nmcli_wifi_device() -> Result<String, String> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,TYPE", "dev", "status"])
        .output()
        .map_err(|e| format!("nmcli not available: {e}"))?;
    if !out.status.success() {
        return Err("Could not read the Wi-Fi adapter".to_string());
    }
    parse_wifi_device(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "No Wi-Fi adapter to disconnect".to_string())
}

/// Pick the wireless device out of `nmcli -t -f DEVICE,TYPE dev status` output.
fn parse_wifi_device(text: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.trim_end().splitn(2, ':');
        let device = parts.next().unwrap_or("").trim();
        let ty = parts.next().unwrap_or("").to_lowercase();
        if !device.is_empty() && (ty.contains("wifi") || ty.contains("802-11")) {
            return Some(device.to_string());
        }
    }
    None
}

fn nmcli_scan() -> Result<Vec<String>, String> {
    let out = Command::new("nmcli")
        .args([
            "-t",
            "-f",
            "SSID",
            "dev",
            "wifi",
            "list",
            "--rescan",
            "yes",
        ])
        .output()
        .map_err(|e| format!("nmcli not available: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "nmcli scan failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let mut ssids = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let ssid = line.trim();
        if ssid.is_empty() || ssids.iter().any(|s: &String| s == ssid) {
            continue;
        }
        ssids.push(ssid.to_string());
    }
    Ok(ssids)
}

fn nmcli_connect(ssid: &str, password: &str) -> Result<(), String> {
    let mut cmd = Command::new("nmcli");
    cmd.args(["dev", "wifi", "connect", ssid]);
    if !password.is_empty() {
        cmd.args(["password", password]);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("nmcli not available: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        Err(match auth_hint(stderr) {
            Some(hint) => hint.to_string(),
            None => format!("connect failed: {stderr}"),
        })
    }
}

/// polkit/NM refusals worded for the screen; `None` = use stderr as is. The result never
/// repeats the stderr text, so an SSID or path in it cannot reach the screen.
fn auth_hint(stderr: &str) -> Option<&'static str> {
    let lower = stderr.to_lowercase();
    let refused = lower.contains("not authorized")
        || lower.contains("not authorised")
        || lower.contains("insufficient privileges")
        || (lower.contains("authorization") && lower.contains("denied"));
    refused.then_some(
        "Wi-Fi settings are blocked by the system. Install the operator polkit rule (50-plippa-network.rules).",
    )
}

fn query_ips() -> Vec<String> {
    let mut ips = Vec::new();
    // Prefer nmcli device show; fall back to `hostname -I`.
    if let Ok(out) = Command::new("nmcli")
        .args(["-t", "-f", "IP4.ADDRESS", "dev", "show"])
        .output()
    {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let addr = line
                    .trim()
                    .trim_start_matches("IP4.ADDRESS:")
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .trim();
                if !addr.is_empty() && addr != "--" && !ips.iter().any(|x| x == addr) {
                    ips.push(addr.to_string());
                }
            }
        }
    }
    if !ips.is_empty() {
        return ips;
    }
    if let Ok(out) = Command::new("hostname").arg("-I").output() {
        if out.status.success() {
            for addr in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                if !ips.iter().any(|x| x == addr) {
                    ips.push(addr.to_string());
                }
            }
        }
    }
    ips
}

fn nmcli_active_ssid() -> Option<String> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "ACTIVE,SSID", "dev", "wifi"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(2, ':');
        let active = parts.next().unwrap_or("");
        let ssid = parts.next().unwrap_or("").trim();
        if (active == "yes" || active == "ja") && !ssid.is_empty() {
            return Some(ssid.to_string());
        }
    }
    None
}

fn nmcli_has_connection(ssid: &str) -> bool {
    nmcli_connection_names().iter().any(|name| name == ssid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_hint_recognises_polkit_refusals_in_any_case() {
        for s in [
            "Error: Connection activation failed: Not authorized to control networking.",
            "Error: not authorised to add wifi",
            "Error: Insufficient privileges.",
            "Error: Authorization request DENIED for org.freedesktop.NetworkManager.settings.modify.system",
        ] {
            let hint = auth_hint(s).unwrap_or_else(|| panic!("no hint for {s}"));
            assert!(hint.contains("50-plippa-network.rules"));
            assert!(!hint.contains("org.freedesktop"));
        }
    }

    #[test]
    fn auth_hint_leaves_ordinary_failures_alone() {
        assert_eq!(auth_hint("Error: Secrets were required, but not provided."), None);
        assert_eq!(auth_hint("Error: No network with SSID 'Cafe' found."), None);
        assert_eq!(auth_hint(""), None);
    }

    #[test]
    fn dry_run_connect_sets_ip_and_portal() {
        let mut wifi = NmcliWifi::new().with_dry_run(true);
        let ssids = wifi.scan();
        assert!(!ssids.is_empty());
        let r = wifi.connect("DryRunLAN", "x");
        assert!(matches!(r, WifiConnectResult::Ok { .. }));
        assert_eq!(wifi.current_ip().as_deref(), Some("192.168.1.50"));
        assert_eq!(
            wifi.portal_url().as_deref(),
            Some("http://192.168.1.50:8787")
        );
    }

    #[test]
    #[ignore = "requires NetworkManager + wifi hardware"]
    fn hardware_nmcli_scan() {
        let mut wifi = NmcliWifi::new().with_dry_run(false);
        let _ = wifi.scan();
    }

    const CONNECTIONS: &str = "\
HomeLAN:802-11-wireless
Wired connection 1:802-3-ethernet
Work VPN:vpn
Cafe\\:Corner:802-11-wireless
LabAP:802-11-wireless
HomeLAN:802-11-wireless
";

    #[test]
    fn saved_networks_filters_to_wireless_and_strips_escapes() {
        assert_eq!(
            parse_connection_list(CONNECTIONS),
            vec![
                "Cafe:Corner".to_string(),
                "HomeLAN".to_string(),
                "LabAP".to_string()
            ],
            "ethernet and VPN profiles dropped, the escaped colon kept, duplicates merged"
        );
        assert_eq!(parse_connection_list(""), Vec::<String>::new());
    }

    #[test]
    fn disconnect_and_forget_build_the_expected_argv() {
        assert_eq!(
            delete_argv("Cafe:Corner"),
            vec!["connection", "delete", "Cafe:Corner"]
        );
        assert_eq!(disconnect_argv("wlan0"), vec!["dev", "disconnect", "wlan0"]);

        // The device name comes from `dev status`, where the type column picks the radio.
        let status = "eth0:ethernet\nwlan0:wifi\nlo:loopback\n";
        assert_eq!(parse_wifi_device(status).as_deref(), Some("wlan0"));
        assert_eq!(parse_wifi_device("eth0:ethernet\n"), None);
    }

    #[test]
    fn dry_run_disconnect_and_forget_move_the_mock_state() {
        let mut wifi = NmcliWifi::new().with_dry_run(true);
        wifi.connect("DryRunLAN", "x");
        assert_eq!(wifi.current_ssid().as_deref(), Some("DryRunLAN"));
        assert_eq!(wifi.saved_networks(), vec!["DryRunLAN".to_string()]);

        wifi.disconnect().unwrap();
        assert_eq!(wifi.current_ssid(), None);
        assert_eq!(wifi.current_ip(), None);
        assert_eq!(
            wifi.saved_networks(),
            vec!["DryRunLAN".to_string()],
            "disconnecting keeps the profile"
        );

        wifi.forget("DryRunLAN").unwrap();
        assert_eq!(wifi.saved_networks(), Vec::<String>::new());
        assert_eq!(wifi.current_ssid(), None);
    }
}
