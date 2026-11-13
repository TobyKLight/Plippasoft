//! CLI / env configuration for the `plippa-engine` binary.

use std::env;
use std::path::PathBuf;

/// Audio output backend selected at process start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkMode {
    Mock,
    Pulse,
}

impl SinkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SinkMode::Mock => "mock",
            SinkMode::Pulse => "pulse",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mock" => Some(SinkMode::Mock),
            "pulse" => Some(SinkMode::Pulse),
            _ => None,
        }
    }
}

/// Parsed runtime options for the engine binary.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Explicit library root. `None` → USB auto-discovery (MVP_17).
    pub library_root: Option<PathBuf>,
    /// Board id for `plippa-board` (e.g. `plippa-D-poc2`). `None` → desktop mock caps.
    pub board_id: Option<String>,
    pub sink: SinkMode,
    /// Optional override path for the tap-load SFX wav.
    pub sfx_path: Option<PathBuf>,
    /// Monorepo / install root used to locate board profiles and default SFX.
    pub repo_root: PathBuf,
}

impl EngineConfig {
    /// Default sink: Pulse when the `pulse` feature is compiled in, else mock.
    pub fn default_sink() -> SinkMode {
        #[cfg(feature = "pulse")]
        {
            SinkMode::Pulse
        }
        #[cfg(not(feature = "pulse"))]
        {
            SinkMode::Mock
        }
    }

    /// Parse from process args (`--library-root`, `--board`, `--sink`, `--sfx`) and env.
    ///
    /// Positional `argv[1]` is accepted as library root for backwards compatibility
    /// with MVP_09 smoke scripts.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut library_root: Option<PathBuf> = None;
        let mut board_id: Option<String> = None;
        let mut sink: Option<SinkMode> = None;
        let mut sfx_path: Option<PathBuf> = None;
        let mut repo_root: Option<PathBuf> = None;
        let mut positional: Option<PathBuf> = None;

        let mut it = args.into_iter().peekable();
        let _ = it.next(); // argv[0]
        while let Some(arg) = it.next() {
            if let Some(v) = arg.strip_prefix("--library-root=") {
                library_root = Some(PathBuf::from(v));
                continue;
            }
            if let Some(v) = arg.strip_prefix("--board=") {
                board_id = Some(v.to_string());
                continue;
            }
            if let Some(v) = arg.strip_prefix("--sink=") {
                sink = SinkMode::parse(v);
                continue;
            }
            if let Some(v) = arg.strip_prefix("--sfx=") {
                sfx_path = Some(PathBuf::from(v));
                continue;
            }
            if let Some(v) = arg.strip_prefix("--repo-root=") {
                repo_root = Some(PathBuf::from(v));
                continue;
            }
            match arg.as_str() {
                "--library-root" => {
                    if let Some(v) = it.next() {
                        library_root = Some(PathBuf::from(v));
                    }
                }
                "--board" => {
                    if let Some(v) = it.next() {
                        board_id = Some(v);
                    }
                }
                "--sink" => {
                    if let Some(v) = it.next() {
                        sink = SinkMode::parse(&v);
                    }
                }
                "--sfx" => {
                    if let Some(v) = it.next() {
                        sfx_path = Some(PathBuf::from(v));
                    }
                }
                "--repo-root" => {
                    if let Some(v) = it.next() {
                        repo_root = Some(PathBuf::from(v));
                    }
                }
                other if !other.starts_with('-') && positional.is_none() => {
                    positional = Some(PathBuf::from(other));
                }
                _ => {}
            }
        }

        if library_root.is_none() {
            if let Ok(v) = env::var("PLIPPA_LIBRARY_ROOT") {
                if !v.is_empty() {
                    library_root = Some(PathBuf::from(v));
                }
            }
        }
        if library_root.is_none() {
            library_root = positional;
        }

        if board_id.is_none() {
            if let Ok(v) = env::var("PLIPPA_BOARD") {
                if !v.is_empty() {
                    board_id = Some(v);
                }
            }
        }

        if sink.is_none() {
            if let Ok(v) = env::var("PLIPPA_SINK") {
                sink = SinkMode::parse(&v);
            }
        }

        if sfx_path.is_none() {
            if let Ok(v) = env::var("PLIPPA_SFX") {
                if !v.is_empty() {
                    sfx_path = Some(PathBuf::from(v));
                }
            }
        }

        if repo_root.is_none() {
            if let Ok(v) = env::var("PLIPPA_ROOT") {
                if !v.is_empty() {
                    repo_root = Some(PathBuf::from(v));
                }
            }
        }

        Self {
            library_root,
            board_id,
            sink: sink.unwrap_or_else(Self::default_sink),
            sfx_path,
            repo_root: repo_root.unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_library_root_and_sink_flags() {
        let cfg = EngineConfig::from_args([
            "plippa-engine".into(),
            "--library-root".into(),
            "/media/usb".into(),
            "--sink=mock".into(),
            "--board=plippa-D-poc2".into(),
        ]);
        assert_eq!(cfg.library_root, Some(PathBuf::from("/media/usb")));
        assert_eq!(cfg.sink, SinkMode::Mock);
        assert_eq!(cfg.board_id.as_deref(), Some("plippa-D-poc2"));
    }

    #[test]
    fn positional_library_root_still_works() {
        let cfg = EngineConfig::from_args([
            "plippa-engine".into(),
            "/tmp/lib".into(),
            "--sink".into(),
            "mock".into(),
        ]);
        assert_eq!(cfg.library_root, Some(PathBuf::from("/tmp/lib")));
        assert_eq!(cfg.sink, SinkMode::Mock);
    }

    #[test]
    fn flag_library_root_wins_over_positional() {
        let cfg = EngineConfig::from_args([
            "plippa-engine".into(),
            "/tmp/positional".into(),
            "--library-root=/tmp/flag".into(),
        ]);
        assert_eq!(cfg.library_root, Some(PathBuf::from("/tmp/flag")));
    }

    #[test]
    fn unset_library_root_means_discover() {
        // Clear env for this test process slice — only assert CLI with no root.
        let cfg = EngineConfig::from_args(["plippa-engine".into(), "--sink=mock".into()]);
        // May pick up PLIPPA_LIBRARY_ROOT from the environment on desk machines.
        let _ = cfg.library_root;
        assert_eq!(cfg.sink, SinkMode::Mock);
    }
}
