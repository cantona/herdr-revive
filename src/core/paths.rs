use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

pub const DEFAULT_SESSION: &str = "default";

/// Everything herdr-core consults to place its client socket, in the order
/// it consults it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketInputs {
    pub session: String,
    pub hc_socket: Option<PathBuf>,
    pub config_socket: Option<PathBuf>,
    pub runtime_dir: Option<PathBuf>,
    pub run_user_dir: Option<PathBuf>,
    pub state_home: PathBuf,
}

impl SocketInputs {
    pub fn from_env() -> Result<Self> {
        let session = session_name(std::env::var("HC_SESSION").ok().as_deref())?;
        let home = home_dir()?;
        let config_home = non_empty("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let config_socket =
            core_config_socket(&config_home.join("herdr-core").join("config.toml"))?;
        let run_user = PathBuf::from(format!("/run/user/{}", nix::unistd::getuid().as_raw()));
        Ok(Self {
            session,
            hc_socket: non_empty("HC_SOCKET"),
            config_socket,
            runtime_dir: non_empty("XDG_RUNTIME_DIR"),
            run_user_dir: run_user.is_dir().then_some(run_user),
            state_home: non_empty("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state")),
        })
    }
}

fn non_empty(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn session_name(raw: Option<&str>) -> Result<String> {
    let name = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SESSION);
    ensure!(
        name != "."
            && name != ".."
            && !name
                .chars()
                .any(|c| c.is_control() || r#"/\:*?"<>|"#.contains(c)),
        "HC_SESSION cannot be a folder name"
    );
    Ok(name.to_string())
}

pub fn resolve_socket(inputs: &SocketInputs) -> PathBuf {
    if let Some(socket) = &inputs.hc_socket {
        return socket.clone();
    }
    if let Some(socket) = &inputs.config_socket {
        return socket.clone();
    }
    let file = format!("{}.sock", inputs.session);
    if let Some(run) = &inputs.runtime_dir {
        return run.join("herdr-core").join(file);
    }
    if let Some(run) = &inputs.run_user_dir {
        return run.join("herdr-core").join(file);
    }
    inputs
        .state_home
        .join("herdr-core")
        .join(&inputs.session)
        .join(file)
}

fn core_config_socket(path: &Path) -> Result<Option<PathBuf>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("cannot read herdr-core config.toml"),
    };
    let value: toml::Value = toml::from_str(&text).context("invalid herdr-core config.toml")?;
    Ok(value
        .get("socket")
        .and_then(toml::Value::as_str)
        .map(PathBuf::from))
}

/// Whether herdr-core's configured `shell` starts a login shell
/// (`shell = ["bash", "-l"]`), which reads a login file, not ~/.bashrc; None
/// when no `shell` is set and core's own default applies. An unreadable or
/// invalid config counts as unset: socket resolution reports it.
pub fn core_shell_is_login(home: &Path) -> Option<bool> {
    let config_home = non_empty("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(config_home.join("herdr-core").join("config.toml")).ok()?;
    let value = toml::from_str::<toml::Value>(&text).ok()?;
    let words: Vec<String> = match value.get("shell") {
        Some(toml::Value::Array(items)) => items
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(toml::Value::String(line)) => line.split_whitespace().map(str::to_string).collect(),
        _ => return None,
    };
    Some(words.iter().skip(1).any(|word| {
        word == "--login"
            || (word.starts_with('-') && !word.starts_with("--") && word[1..].contains('l'))
    }))
}

pub fn home_dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is required")?);
    ensure!(home.is_absolute(), "HOME must be absolute");
    Ok(home)
}

pub fn config_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    tool_dir(explicit, "XDG_CONFIG_HOME", ".config")
}

pub fn state_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    tool_dir(explicit, "XDG_STATE_HOME", ".local/state")
}

fn tool_dir(explicit: Option<PathBuf>, var: &str, fallback: &str) -> Result<PathBuf> {
    tool_dir_from(explicit, non_empty(var), home_dir, var, fallback)
}

fn tool_dir_from(
    explicit: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: impl FnOnce() -> Result<PathBuf>,
    var: &str,
    fallback: &str,
) -> Result<PathBuf> {
    let path = match (explicit, xdg) {
        (Some(path), _) => path,
        (None, Some(base)) => base.join(crate::model::TOOL),
        (None, None) => home()?.join(fallback).join(crate::model::TOOL),
    };
    ensure!(
        path.is_absolute(),
        "{var} and explicit directories must be absolute"
    );
    Ok(path)
}

/// The store session for a core socket: a namespace of its own, so TUI and app
/// snapshots never share a directory, and 64 hex digits as every session must be.
pub fn session_identity(socket: &Path) -> Result<(PathBuf, String)> {
    ensure!(socket.is_absolute(), "core socket path must be absolute");
    let name = socket.file_name().context("socket has no filename")?;
    let parent = socket
        .parent()
        .context("socket has no parent")?
        .canonicalize()
        .context("cannot resolve core socket directory")?;
    let normalized = parent.join(name);
    let text = normalized
        .to_str()
        .context("non-UTF-8 socket paths are unsupported")?;
    Ok((
        normalized.clone(),
        crate::store::digest(format!("cantona.herdr-revive\0core-session-v1\0{text}").as_bytes()),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_login_shell_is_read_from_the_core_config() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/herdr-core");
        std::fs::create_dir_all(&config).unwrap();
        // Only meaningful when XDG_CONFIG_HOME is not ambient in the run.
        if std::env::var_os("XDG_CONFIG_HOME").is_some() {
            return;
        }
        assert_eq!(core_shell_is_login(home.path()), None);
        for (shell, login) in [
            (r#"shell = ["bash", "-l"]"#, true),
            (r#"shell = ["bash", "--login"]"#, true),
            (r#"shell = ["bash", "-il"]"#, true),
            (r#"shell = "bash -l""#, true),
            (r#"shell = ["bash"]"#, false),
            (r#"shell = ["/bin/l-bash", "-i"]"#, false),
        ] {
            std::fs::write(config.join("config.toml"), shell).unwrap();
            assert_eq!(core_shell_is_login(home.path()), Some(login), "{shell}");
        }
        std::fs::write(config.join("config.toml"), "socket = \"/x\"").unwrap();
        assert_eq!(core_shell_is_login(home.path()), None);
    }

    use super::*;
    use std::path::PathBuf;

    fn inputs() -> SocketInputs {
        SocketInputs {
            session: "default".into(),
            hc_socket: None,
            config_socket: None,
            runtime_dir: None,
            run_user_dir: None,
            state_home: PathBuf::from("/home/u/.local/state"),
        }
    }

    #[test]
    fn socket_rules_apply_in_core_order() {
        let mut i = inputs();
        assert_eq!(
            resolve_socket(&i),
            PathBuf::from("/home/u/.local/state/herdr-core/default/default.sock")
        );
        i.run_user_dir = Some("/run/user/1000".into());
        assert_eq!(
            resolve_socket(&i),
            PathBuf::from("/run/user/1000/herdr-core/default.sock")
        );
        i.runtime_dir = Some("/tmp/xdg".into());
        assert_eq!(
            resolve_socket(&i),
            PathBuf::from("/tmp/xdg/herdr-core/default.sock")
        );
        i.config_socket = Some("/srv/core.sock".into());
        assert_eq!(resolve_socket(&i), PathBuf::from("/srv/core.sock"));
        i.hc_socket = Some("/x/hc.sock".into());
        assert_eq!(resolve_socket(&i), PathBuf::from("/x/hc.sock"));
        i.session = "smoke".into();
        i.hc_socket = None;
        i.config_socket = None;
        assert_eq!(
            resolve_socket(&i),
            PathBuf::from("/tmp/xdg/herdr-core/smoke.sock")
        );
    }

    #[test]
    fn session_names_follow_core_rules() {
        assert_eq!(session_name(None).unwrap(), "default");
        assert_eq!(session_name(Some("  ")).unwrap(), "default");
        assert_eq!(session_name(Some("smoke ")).unwrap(), "smoke");
        assert_eq!(session_name(Some("MyTest")).unwrap(), "MyTest");
        for bad in ["a/b", r"a\b", "a:b", "..", ".", "a\tb"] {
            assert!(session_name(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn config_socket_is_read_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(core_config_socket(&path).unwrap(), None);
        std::fs::write(&path, "notes = false\nsocket = \"/srv/hc.sock\"\n").unwrap();
        assert_eq!(
            core_config_socket(&path).unwrap(),
            Some(PathBuf::from("/srv/hc.sock"))
        );
        std::fs::write(&path, "notes = false\n").unwrap();
        assert_eq!(core_config_socket(&path).unwrap(), None);
        std::fs::write(&path, "socket = [\n").unwrap();
        assert!(core_config_socket(&path).is_err());
    }

    #[test]
    fn explicit_dirs_are_used_as_given_and_defaults_get_the_tool_name() {
        let explicit = config_dir(Some("/etc/revive".into())).unwrap();
        assert_eq!(explicit, PathBuf::from("/etc/revive"));
        assert!(config_dir(Some("relative".into())).is_err());
        let default = config_dir(None).unwrap();
        assert!(default.is_absolute());
        assert_eq!(
            default.file_name().and_then(|n| n.to_str()),
            Some("herdr-revive")
        );
        let state = state_dir(None).unwrap();
        assert_eq!(
            state.file_name().and_then(|n| n.to_str()),
            Some("herdr-revive")
        );
    }

    #[test]
    fn an_xdg_directory_needs_no_home() {
        let no_home = || -> Result<PathBuf> { anyhow::bail!("HOME is required") };
        assert_eq!(
            tool_dir_from(
                None,
                Some("/x/state".into()),
                no_home,
                "XDG_STATE_HOME",
                ".local/state"
            )
            .unwrap(),
            PathBuf::from("/x/state/herdr-revive")
        );
        let error = tool_dir_from(None, None, no_home, "XDG_STATE_HOME", ".local/state")
            .unwrap_err()
            .to_string();
        assert_eq!(error, "HOME is required");
        assert_eq!(
            tool_dir_from(
                None,
                None,
                || Ok("/home/u".into()),
                "XDG_STATE_HOME",
                ".local/state"
            )
            .unwrap(),
            PathBuf::from("/home/u/.local/state/herdr-revive")
        );
        assert!(tool_dir_from(None, Some("rel".into()), no_home, "XDG_STATE_HOME", ".x").is_err());
    }

    #[test]
    fn core_sessions_never_collide_with_tui_sessions() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("default.sock");
        let (normalized, core) = session_identity(&socket).unwrap();
        let (_, tui) = crate::store::session_identity(&socket).unwrap();
        assert_eq!(core.len(), 64);
        assert!(core.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(core, tui);
        assert!(normalized.is_absolute());
        assert!(session_identity(std::path::Path::new("relative.sock")).is_err());
    }
}
