use std::env;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Stable system PipeWire runtime directory.
pub const SYSTEM_AUDIO_RUNTIME: &str = "/run/plippa-audio";
const SYSTEM_PULSE_NATIVE: &str = "/run/plippa-audio/pulse/native";

pub fn effective_session_uid() -> u32 {
    env::var("SUDO_UID")
        .ok()
        .or_else(|| env::var("UID").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

pub fn pulse_session_available() -> bool {
    system_pulse_available() || user_pulse_available()
}

pub fn pulse_session_label() -> &'static str {
    if system_pulse_available() {
        "system /run/plippa-audio"
    } else if user_pulse_available() {
        "user session"
    } else {
        "none"
    }
}

fn system_pulse_available() -> bool {
    pulse_socket_alive(SYSTEM_PULSE_NATIVE)
}

fn user_pulse_available() -> bool {
    let uid = effective_session_uid();
    pulse_socket_alive(&format!("/run/user/{uid}/pulse/native"))
}

/// Socket files can outlive a crashed PipeWire-Pulse; verify something is listening.
fn pulse_socket_alive(path: &str) -> bool {
    Path::new(path).exists() && UnixStream::connect(path).is_ok()
}

pub fn pulse_session_env() -> Vec<(String, String)> {
    if system_pulse_available() {
        return vec![
            (
                "PULSE_SERVER".into(),
                format!("unix:{SYSTEM_PULSE_NATIVE}"),
            ),
            ("XDG_RUNTIME_DIR".into(), SYSTEM_AUDIO_RUNTIME.into()),
            (
                "PULSE_RUNTIME_PATH".into(),
                format!("{SYSTEM_AUDIO_RUNTIME}/pulse"),
            ),
        ];
    }

    let uid = effective_session_uid();
    let runtime = format!("/run/user/{uid}");
    let pulse_native = format!("{runtime}/pulse/native");
    if !pulse_socket_alive(&pulse_native) {
        return Vec::new();
    }
    vec![
        ("XDG_RUNTIME_DIR".into(), runtime.clone()),
        ("PULSE_RUNTIME_PATH".into(), format!("{runtime}/pulse")),
    ]
}

pub fn apply_pulse_env(override_server: Option<&str>) {
    if let Some(server) = override_server {
        env::set_var("PULSE_SERVER", server);
        return;
    }
    for (key, value) in pulse_session_env() {
        env::set_var(key, value);
    }
}
