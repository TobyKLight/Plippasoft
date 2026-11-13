//! Runtime configuration for `plippa-server`.

use plippa_board::{workspace_root_from_crate, BoardProfile};
use plippa_core::Capabilities;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Where store credentials live on an installed system (BCS_03).
///
/// The directory is created `0700` on first save and its files are `0600`. On a dev
/// box `--secrets-dir` / `PLIPPA_SECRETS_DIR` points this somewhere writable; when it
/// cannot be used the store feature degrades and everything else keeps running.
pub const DEFAULT_SECRETS_DIR: &str = "/var/lib/plippa/secrets";

/// Server settings. The LAN portal (`bind`) is switched on and off from the device menu and
/// asks for a PIN (SECFIX_02); the defaults here only say where things listen.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// HTTP bind address (default `0.0.0.0:8787`).
    pub bind: SocketAddr,
    /// Absolute or relative path to the Noubin media library root.
    ///
    /// When missing/absent, the server still runs and surfaces a warning.
    pub library_root: Option<PathBuf>,
    /// Where store credentials are stored (`--secrets-dir` / `PLIPPA_SECRETS_DIR`).
    ///
    /// `None` means "no store credentials can be stored at all": the stores panel
    /// reports itself unavailable and the rest of the portal is unaffected.
    pub secrets_dir: Option<PathBuf>,
    /// Address the device advertises for its portal (`--portal-url` / `PLIPPA_PORTAL_URL`).
    ///
    /// `None` means "work it out from this machine's interfaces". An override is not
    /// trusted blindly: `crate::portal` refuses credentials, queries and fragments, and
    /// never recommends a loopback or link-local address even when one is configured.
    pub portal_url: Option<String>,
    /// The loopback control listener the on-device UI uses (`--control-bind` /
    /// `PLIPPA_CONTROL_BIND`, default `127.0.0.1:8786`). `None` runs no control listener (tests
    /// that only exercise the portal). It must stay a loopback address: it serves the whole API
    /// to anyone who holds the token, and it is plain HTTP.
    pub control_bind: Option<SocketAddr>,
    /// Where the per-boot control token is written for the UI to read (`<ipc dir>/portal.token`,
    /// in the private runtime directory). `None` keeps the token in memory only.
    pub control_token_path: Option<PathBuf>,
}

/// Default address of the loopback control listener.
pub const DEFAULT_CONTROL_BIND: &str = "127.0.0.1:8786";

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8787".parse().expect("valid default bind"),
            library_root: None,
            secrets_dir: Some(PathBuf::from(DEFAULT_SECRETS_DIR)),
            portal_url: None,
            control_bind: Some(DEFAULT_CONTROL_BIND.parse().expect("valid default control bind")),
            control_token_path: None,
        }
    }
}

impl ServerConfig {
    /// Parse from process args: `--bind ADDR`, `--control-bind ADDR`, `--library-root PATH`,
    /// `--secrets-dir PATH`, `--portal-url URL`.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut cfg = Self::default();
        let env_secrets = std::env::var("PLIPPA_SECRETS_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let env_portal = std::env::var(crate::portal::ENV_PORTAL_URL).ok();
        let mut flag_secrets: Option<PathBuf> = None;
        let mut flag_portal: Option<String> = None;
        let mut flag_control: Option<String> = None;
        let mut it = args.into_iter().peekable();
        // skip argv[0]
        let _ = it.next();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--bind" => {
                    if let Some(v) = it.next() {
                        if let Ok(addr) = v.parse() {
                            cfg.bind = addr;
                        }
                    }
                }
                "--control-bind" => {
                    if let Some(v) = it.next() {
                        flag_control = Some(v);
                    }
                }
                "--library-root" => {
                    if let Some(v) = it.next() {
                        cfg.library_root = Some(PathBuf::from(v));
                    }
                }
                "--secrets-dir" => {
                    if let Some(v) = it.next() {
                        if !v.is_empty() {
                            flag_secrets = Some(PathBuf::from(v));
                        }
                    }
                }
                "--portal-url" => {
                    if let Some(v) = it.next() {
                        flag_portal = Some(v);
                    }
                }
                _ => {}
            }
        }
        if cfg.library_root.is_none() {
            if let Ok(v) = std::env::var("PLIPPA_LIBRARY_ROOT") {
                if !v.is_empty() {
                    cfg.library_root = Some(PathBuf::from(v));
                }
            }
        }
        // An explicit flag wins over the environment; the built-in default stays when
        // neither is given.
        cfg.secrets_dir = flag_secrets.or(env_secrets).or(cfg.secrets_dir);
        cfg.portal_url = pick_portal_override(flag_portal, env_portal);
        cfg.control_bind = pick_control_bind(
            flag_control,
            std::env::var("PLIPPA_CONTROL_BIND").ok(),
        );
        // The UI reads the token from the private runtime directory it shares with the engine.
        cfg.control_token_path = Some(plippa_ipc_proto::resolve_paths().dir.join("portal.token"));
        cfg
    }
}

/// The control listener's address: a flag, else the environment, else the default. Only a
/// loopback address is accepted (anything else is ignored with a line saying so), because the
/// listener serves the whole API to whoever holds the token over plain HTTP.
fn pick_control_bind(flag: Option<String>, env: Option<String>) -> Option<SocketAddr> {
    let default = DEFAULT_CONTROL_BIND.parse().ok();
    let Some(raw) = flag.or(env).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
        return default;
    };
    match raw.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => Some(addr),
        _ => {
            eprintln!("plippa-server: ignoring control bind `{raw}`: it must be a loopback address such as {DEFAULT_CONTROL_BIND}");
            default
        }
    }
}

/// The operator's portal address: an explicit flag, else the environment, else nothing.
///
/// A blank value means "not configured" in both places, so an empty string is never
/// advertised as the portal address.
fn pick_portal_override(flag: Option<String>, env: Option<String>) -> Option<String> {
    let clean = |v: String| {
        let trimmed = v.trim().to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    flag.and_then(clean).or_else(|| env.and_then(clean))
}

/// The board this server runs on, found the way the engine finds it so the import gate, the
/// waveform analyser and playback all ask one capability source (MEDIA_01).
///
/// `--board <id>` / `--board=<id>` (else `PLIPPA_BOARD`), `--repo-root <path>` (else
/// `PLIPPA_ROOT`, else the workspace the binary was built in). With no board id, or one that
/// cannot be loaded, the desktop capabilities apply and one line says so.
pub fn load_capabilities(args: impl IntoIterator<Item = String>) -> Capabilities {
    let mut board_id: Option<String> = None;
    let mut repo_root: Option<PathBuf> = None;
    let mut it = args.into_iter().skip(1);
    while let Some(arg) = it.next() {
        if let Some(v) = arg.strip_prefix("--board=") {
            board_id = Some(v.to_string());
            continue;
        }
        if let Some(v) = arg.strip_prefix("--repo-root=") {
            repo_root = Some(PathBuf::from(v));
            continue;
        }
        match arg.as_str() {
            "--board" => board_id = it.next(),
            "--repo-root" => repo_root = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let board_id = board_id.filter(|v| !v.is_empty()).or_else(|| env("PLIPPA_BOARD"));
    let repo_root = repo_root
        .or_else(|| env("PLIPPA_ROOT").map(PathBuf::from))
        .unwrap_or_else(workspace_root_from_crate);
    capabilities_for_board(board_id.as_deref(), &repo_root)
}

/// [`load_capabilities`] with the inputs resolved.
pub fn capabilities_for_board(board_id: Option<&str>, repo_root: &Path) -> Capabilities {
    let Some(board_id) = board_id else {
        eprintln!("plippa-server: no board configured; using desktop capabilities");
        return Capabilities::desktop_mock();
    };
    match BoardProfile::load(repo_root, board_id) {
        Ok(profile) => {
            eprintln!(
                "plippa-server: loaded board `{}`",
                profile.capabilities.board_id
            );
            profile.capabilities
        }
        Err(e) => {
            eprintln!(
                "plippa-server: board `{board_id}` load failed ({e}); using desktop capabilities"
            );
            Capabilities::desktop_mock()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_dir_flag_and_default() {
        let cfg = ServerConfig::from_args(
            ["plippa-server", "--secrets-dir", "/tmp/plippa-secrets"].map(String::from),
        );
        assert_eq!(
            cfg.secrets_dir.as_deref(),
            Some(std::path::Path::new("/tmp/plippa-secrets"))
        );

        // An empty value is ignored rather than turning the feature off silently.
        let cfg = ServerConfig::from_args(["plippa-server", "--secrets-dir", ""].map(String::from));
        assert_eq!(
            cfg.secrets_dir.as_deref(),
            Some(std::path::Path::new(DEFAULT_SECRETS_DIR))
        );
    }

    #[test]
    fn the_control_listener_is_loopback_only_and_defaults_to_8786() {
        let default = Some("127.0.0.1:8786".parse().unwrap());
        assert_eq!(ServerConfig::default().control_bind, default);
        assert_eq!(pick_control_bind(None, None), default);
        assert_eq!(
            pick_control_bind(Some("127.0.0.1:9100".into()), None),
            Some("127.0.0.1:9100".parse().unwrap())
        );
        assert_eq!(
            pick_control_bind(None, Some("[::1]:9101".into())),
            Some("[::1]:9101".parse().unwrap())
        );
        // A flag beats the environment.
        assert_eq!(
            pick_control_bind(Some("127.0.0.1:9100".into()), Some("127.0.0.1:9200".into())),
            Some("127.0.0.1:9100".parse().unwrap())
        );
        // A LAN address, a wildcard or junk is refused rather than exposing the control API.
        for bad in ["0.0.0.0:8786", "192.168.1.5:8786", "[::]:8786", "nonsense", ""] {
            assert_eq!(pick_control_bind(Some(bad.into()), None), default, "{bad}");
        }
    }

    #[test]
    fn portal_url_flag_and_environment() {
        let cfg = ServerConfig::from_args(
            ["plippa-server", "--portal-url", " 192.168.1.50:9000 "].map(String::from),
        );
        assert_eq!(cfg.portal_url.as_deref(), Some("192.168.1.50:9000"));

        // A flag beats the environment; the environment is used when no flag is given.
        let flag = pick_portal_override(Some("a.lan".into()), Some("b.lan".into()));
        assert_eq!(flag.as_deref(), Some("a.lan"));
        let env_only = pick_portal_override(None, Some("b.lan".into()));
        assert_eq!(env_only.as_deref(), Some("b.lan"));
        // Blank means "not configured" rather than an empty address in the QR code.
        assert_eq!(pick_portal_override(Some("  ".into()), Some("".into())), None);
        assert_eq!(
            pick_portal_override(Some("".into()), Some("b.lan".into())),
            Some("b.lan".into())
        );
        // No flag and no environment: the address is worked out from the interfaces.
        let cfg = ServerConfig::from_args(["plippa-server"].map(String::from));
        assert!(cfg.portal_url.is_none());
    }

    fn workspace_root() -> PathBuf {
        workspace_root_from_crate()
    }

    #[test]
    fn the_server_reads_the_board_like_the_engine_does() {
        // A real board profile: plippa-D-poc2 has no AAC.
        let caps = capabilities_for_board(Some("plippa-D-poc2"), &workspace_root());
        assert_eq!(caps.board_id, "plippa-D-poc2");
        assert!(!caps.audio_formats.aac);
        assert!(caps.supports_extension("flac") && !caps.supports_extension("m4a"));

        // The flag form reaches the same profile.
        let root = workspace_root().display().to_string();
        let args = ["plippa-server", "--board=plippa-D-poc2", "--repo-root", root.as_str()]
            .map(String::from);
        assert_eq!(load_capabilities(args).board_id, "plippa-D-poc2");

        // A board with `aac = true` changes the answer, from the same loader.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("boards/prototypes/aac-board/config");
        std::fs::create_dir_all(&dir).unwrap();
        let toml = std::fs::read_to_string(
            workspace_root().join("boards/prototypes/plippa-D-poc2/config/board.toml"),
        )
        .unwrap();
        assert!(toml.contains("aac = false"));
        std::fs::write(dir.join("board.toml"), toml.replace("aac = false", "aac = true")).unwrap();
        let caps = capabilities_for_board(Some("aac-board"), tmp.path());
        assert!(caps.supports_extension("m4a"));
    }

    #[test]
    fn an_unset_or_unloadable_board_falls_back_to_desktop_capabilities() {
        let desktop = Capabilities::desktop_mock();
        assert_eq!(capabilities_for_board(None, &workspace_root()), desktop);
        assert_eq!(
            capabilities_for_board(Some("no-such-board"), &workspace_root()),
            desktop
        );
    }
}
