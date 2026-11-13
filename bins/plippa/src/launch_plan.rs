//! Build child argv / env for the three live processes (MVP_12).

use crate::watchdog::{ChildSpec, LauncherConfig, ProcessName};
use std::env;
use std::path::{Path, PathBuf};

/// Env keys the launcher always tries to establish for children.
pub const ENV_ROOT: &str = "PLIPPA_ROOT";
pub const ENV_LIBRARY_ROOT: &str = "PLIPPA_LIBRARY_ROOT";
pub const ENV_BOARD: &str = "PLIPPA_BOARD";
pub const ENV_SINK: &str = "PLIPPA_SINK";
pub const ENV_BIND: &str = "PLIPPA_BIND";
pub const ENV_UI_BACKEND: &str = "PLIPPA_UI_BACKEND";
/// When `1` / `true`, launcher runs host poweroff after stopping children.
pub const ENV_POWEROFF: &str = "PLIPPA_POWEROFF";

/// Parsed launcher CLI / env inputs.
#[derive(Debug, Clone, Default)]
pub struct LaunchArgs {
    pub board: Option<String>,
    pub library_root: Option<PathBuf>,
    pub sink: Option<String>,
    pub bind: Option<String>,
    pub ui_backend: Option<String>,
    pub root: Option<PathBuf>,
    /// Enable host poweroff after shutdown (lab only).
    pub poweroff: bool,
    pub test_stubs: bool,
}

impl LaunchArgs {
    /// Parse process argv (including argv[0]) plus environment.
    pub fn from_args_env(args: impl IntoIterator<Item = String>) -> Self {
        let mut out = Self::default();
        let mut it = args.into_iter().peekable();
        let _ = it.next(); // argv[0]
        while let Some(arg) = it.next() {
            if arg == "--test-stubs" {
                out.test_stubs = true;
                continue;
            }
            if arg == "--poweroff" {
                out.poweroff = true;
                continue;
            }
            if let Some(v) = arg.strip_prefix("--board=") {
                out.board = Some(v.to_string());
                continue;
            }
            if let Some(v) = arg.strip_prefix("--library-root=") {
                out.library_root = Some(PathBuf::from(v));
                continue;
            }
            if let Some(v) = arg.strip_prefix("--sink=") {
                out.sink = Some(v.to_string());
                continue;
            }
            if let Some(v) = arg.strip_prefix("--bind=") {
                out.bind = Some(v.to_string());
                continue;
            }
            if let Some(v) = arg.strip_prefix("--backend=") {
                out.ui_backend = Some(v.to_string());
                continue;
            }
            if let Some(v) = arg.strip_prefix("--root=") {
                out.root = Some(PathBuf::from(v));
                continue;
            }
            match arg.as_str() {
                "--board" => {
                    if let Some(v) = it.next() {
                        out.board = Some(v);
                    }
                }
                "--library-root" => {
                    if let Some(v) = it.next() {
                        out.library_root = Some(PathBuf::from(v));
                    }
                }
                "--sink" => {
                    if let Some(v) = it.next() {
                        out.sink = Some(v);
                    }
                }
                "--bind" => {
                    if let Some(v) = it.next() {
                        out.bind = Some(v);
                    }
                }
                "--backend" => {
                    if let Some(v) = it.next() {
                        out.ui_backend = Some(v);
                    }
                }
                "--root" => {
                    if let Some(v) = it.next() {
                        out.root = Some(PathBuf::from(v));
                    }
                }
                _ => {}
            }
        }

        if out.board.is_none() {
            out.board = env::var(ENV_BOARD).ok().filter(|s| !s.is_empty());
        }
        if out.library_root.is_none() {
            out.library_root = env::var_os(ENV_LIBRARY_ROOT).map(PathBuf::from);
        }
        if out.sink.is_none() {
            out.sink = env::var(ENV_SINK).ok().filter(|s| !s.is_empty());
        }
        if out.bind.is_none() {
            out.bind = env::var(ENV_BIND).ok().filter(|s| !s.is_empty());
        }
        if out.ui_backend.is_none() {
            out.ui_backend = env::var(ENV_UI_BACKEND).ok().filter(|s| !s.is_empty());
        }
        if out.root.is_none() {
            out.root = env::var_os(ENV_ROOT).map(PathBuf::from);
        }
        if !out.poweroff {
            out.poweroff = env_truthy(ENV_POWEROFF);
        }
        out
    }
}

fn env_truthy(key: &str) -> bool {
    matches!(
        env::var(key).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

/// Shared env map applied to every child (plus process-specific args).
#[derive(Debug, Clone, Default)]
pub struct ChildEnv {
    pub pairs: Vec<(String, String)>,
}

impl ChildEnv {
    pub fn push(&mut self, key: impl Into<String>, val: impl Into<String>) {
        self.pairs.push((key.into(), val.into()));
    }
}

/// Result of planning how to spawn engine / ui / server.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub children: Vec<ChildSpec>,
    pub env: ChildEnv,
    pub poweroff: bool,
}

impl LaunchPlan {
    /// Build specs from parsed args. `bin_dir` is where sibling binaries live (optional).
    pub fn build(args: &LaunchArgs, bin_dir: Option<&Path>) -> Self {
        let mut env = ChildEnv::default();
        if let Some(ref root) = args.root {
            env.push(ENV_ROOT, root.to_string_lossy());
        }
        if let Some(ref lib) = args.library_root {
            env.push(ENV_LIBRARY_ROOT, lib.to_string_lossy());
        }
        if let Some(ref board) = args.board {
            env.push(ENV_BOARD, board);
        }
        if let Some(ref sink) = args.sink {
            env.push(ENV_SINK, sink);
        }
        // PWRSW_01: the UI words its acknowledgement from this flag ("Powering off…" vs
        // "Shutting down…") and marks the request as a host power-off.
        if args.poweroff {
            env.push(ENV_POWEROFF, "1");
        }
        // PULSE_SERVER: pass through when already set so Pulse-backed engine inherits it.
        if let Ok(pulse) = env::var("PULSE_SERVER") {
            if !pulse.is_empty() {
                env.push("PULSE_SERVER", pulse);
            }
        }

        let resolve = |name: &str| -> String {
            if let Some(dir) = bin_dir {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
            name.to_string()
        };

        let mut leds_args = Vec::new();
        let mut engine_args = Vec::new();
        let mut ui_args = Vec::new();
        let mut server_args = Vec::new();

        if let Some(ref lib) = args.library_root {
            let s = lib.to_string_lossy().into_owned();
            engine_args.push("--library-root".into());
            engine_args.push(s.clone());
            server_args.push("--library-root".into());
            server_args.push(s);
        }
        if let Some(ref board) = args.board {
            leds_args.push(format!("--board={board}"));
            engine_args.push("--board".into());
            engine_args.push(board.clone());
            ui_args.push(board.clone());
        }
        if let Some(ref sink) = args.sink {
            engine_args.push(format!("--sink={sink}"));
        }
        let backend = args.ui_backend.as_deref().unwrap_or("mock");
        ui_args.insert(0, format!("--backend={backend}"));
        if let Some(ref bind) = args.bind {
            server_args.push("--bind".into());
            server_args.push(bind.clone());
        }

        // The LED helper first: it blanks the strips on start-up, so they are dark from the
        // first moment instead of latching the previous run's last frame, and it is listening
        // before the UI composes its first frame (the UI also reconnects, so this is ordering
        // for the owner's eyes, not a correctness requirement).
        let children = vec![
            ChildSpec {
                name: ProcessName::Leds,
                program: resolve("plippa-leds"),
                args: leds_args,
                env: env.pairs.clone(),
            },
            ChildSpec {
                name: ProcessName::Engine,
                program: resolve("plippa-engine"),
                args: engine_args,
                env: env.pairs.clone(),
            },
            ChildSpec {
                name: ProcessName::Ui,
                program: resolve("plippa-ui"),
                args: ui_args,
                env: env.pairs.clone(),
            },
            ChildSpec {
                name: ProcessName::Server,
                program: resolve("plippa-server"),
                args: server_args,
                env: env.pairs.clone(),
            },
        ];

        Self {
            children,
            env,
            poweroff: args.poweroff,
        }
    }

    pub fn into_config(self) -> LauncherConfig {
        LauncherConfig {
            children: self.children,
            ..LauncherConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_passes_library_board_sink_bind_backend() {
        let args = LaunchArgs {
            board: Some("plippa-D-poc2".into()),
            library_root: Some(PathBuf::from("/media/usb")),
            sink: Some("mock".into()),
            bind: Some("0.0.0.0:8787".into()),
            ui_backend: Some("mock".into()),
            root: Some(PathBuf::from("/opt/plippa")),
            poweroff: false,
            test_stubs: false,
        };
        let plan = LaunchPlan::build(&args, None);
        let engine = plan
            .children
            .iter()
            .find(|c| c.name == ProcessName::Engine)
            .unwrap();
        assert!(engine.args.windows(2).any(|w| w[0] == "--library-root" && w[1] == "/media/usb"));
        assert!(engine.args.windows(2).any(|w| w[0] == "--board" && w[1] == "plippa-D-poc2"));
        assert!(engine.args.iter().any(|a| a == "--sink=mock"));
        assert!(engine
            .env
            .iter()
            .any(|(k, v)| k == ENV_ROOT && v == "/opt/plippa"));
        assert!(engine
            .env
            .iter()
            .any(|(k, v)| k == ENV_LIBRARY_ROOT && v == "/media/usb"));

        let ui = plan
            .children
            .iter()
            .find(|c| c.name == ProcessName::Ui)
            .unwrap();
        assert_eq!(ui.args[0], "--backend=mock");
        assert!(ui.args.iter().any(|a| a == "plippa-D-poc2"));

        let server = plan
            .children
            .iter()
            .find(|c| c.name == ProcessName::Server)
            .unwrap();
        assert!(server
            .args
            .windows(2)
            .any(|w| w[0] == "--library-root" && w[1] == "/media/usb"));
        assert!(server
            .args
            .windows(2)
            .any(|w| w[0] == "--bind" && w[1] == "0.0.0.0:8787"));
    }

    /// SYS-02: the LED helper is planned before the UI (strips dark before the UI composes)
    /// and is told the same board and root as everyone else.
    #[test]
    fn leds_starts_before_the_ui_and_gets_the_board() {
        let args = LaunchArgs {
            board: Some("plippa-D-poc2".into()),
            root: Some(PathBuf::from("/opt/plippa")),
            ..LaunchArgs::default()
        };
        let plan = LaunchPlan::build(&args, None);
        let at = |name: ProcessName| {
            plan.children
                .iter()
                .position(|c| c.name == name)
                .expect("child planned")
        };
        assert!(
            at(ProcessName::Leds) < at(ProcessName::Ui),
            "the helper must start before the UI: {:?}",
            plan.children.iter().map(|c| c.name).collect::<Vec<_>>()
        );
        let leds = &plan.children[at(ProcessName::Leds)];
        assert_eq!(leds.program, "plippa-leds");
        assert_eq!(leds.args, vec!["--board=plippa-D-poc2"]);
        assert!(leds.env.iter().any(|(k, v)| k == ENV_ROOT && v == "/opt/plippa"));
    }

    #[test]
    fn poweroff_defaults_off() {
        let args = LaunchArgs::from_args_env(vec!["plippa".into()]);
        assert!(!args.poweroff);
    }
}
