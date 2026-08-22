use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use crate::config::{HerdrConfig, validate_herdr_session_name, validate_herdr_socket_name};

use super::{HerdrError, ProcessError};

const SOCKET_ENV: &str = "HERDR_SOCKET_PATH";
const SESSION_ENV: &str = "HERDR_SESSION";
const BINARY_ENV: &str = "HERDR_BIN_PATH";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HerdrTarget {
    #[default]
    Auto,
    Session(String),
    Socket(PathBuf),
}

impl HerdrTarget {
    pub fn from_config(config: &HerdrConfig) -> Result<Self, HerdrError> {
        match (&config.session, &config.socket) {
            (Some(_), Some(_)) => Err(HerdrError::ConflictingTargets),
            (Some(session), None) => Self::session(session.clone()),
            (None, Some(socket)) => Self::socket(socket),
            (None, None) => Ok(Self::Auto),
        }
    }

    pub fn session(session: impl Into<String>) -> Result<Self, HerdrError> {
        let session = session.into();
        validate_herdr_session_name(&session).map_err(|message| HerdrError::InvalidSession {
            session: session.clone(),
            message,
        })?;
        Ok(Self::Session(session))
    }

    pub fn socket(socket: impl Into<PathBuf>) -> Result<Self, HerdrError> {
        let socket = socket.into();
        let Some(text) = socket.to_str() else {
            return Err(HerdrError::InvalidSocket {
                socket,
                message: "herdr.socket must be valid UTF-8",
            });
        };
        validate_herdr_socket_name(text).map_err(|message| HerdrError::InvalidSocket {
            socket: socket.clone(),
            message,
        })?;
        Ok(Self::Socket(socket))
    }

    pub(crate) fn route(&self, command_args: impl IntoIterator<Item = OsString>) -> RoutedCommand {
        let mut args = Vec::new();
        let mut env_set = Vec::new();
        let mut env_remove = Vec::new();
        match self {
            Self::Auto => {}
            Self::Session(session) => {
                args.push(OsString::from("--session"));
                args.push(OsString::from(session));
                env_remove.push(OsString::from(SOCKET_ENV));
                env_remove.push(OsString::from(SESSION_ENV));
            }
            Self::Socket(socket) => {
                env_set.push((OsString::from(SOCKET_ENV), socket.as_os_str().to_owned()));
                env_remove.push(OsString::from(SESSION_ENV));
            }
        }
        args.extend(command_args);
        RoutedCommand {
            args,
            env_set,
            env_remove,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RoutedCommand {
    pub args: Vec<OsString>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
}

pub fn resolve_herdr_binary() -> Result<PathBuf, ProcessError> {
    resolve_herdr_binary_with(None, |key| env::var_os(key), default_platform_candidates())
}

/// Injectable binary discovery for deterministic tests and future explicit
/// binary configuration. `platform_candidates` replaces only the fixed
/// `/opt`/`/usr` fallbacks; PATH, HOME and LOCALAPPDATA remain injected through
/// `get_env`. The returned path is canonical and therefore absolute.
pub fn resolve_herdr_binary_with(
    explicit: Option<&Path>,
    mut get_env: impl FnMut(&str) -> Option<OsString>,
    platform_candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<PathBuf, ProcessError> {
    if let Some(explicit) = explicit {
        return executable_path(explicit);
    }
    if let Some(injected) = get_env(BINARY_ENV) {
        return executable_path(Path::new(&injected));
    }

    let executable_name = if cfg!(windows) { "herdr.exe" } else { "herdr" };
    let mut candidates = Vec::new();
    if let Some(path) = get_env("PATH") {
        candidates.extend(env::split_paths(&path).map(|dir| dir.join(executable_name)));
    }
    if let Some(home) = get_env("HOME") {
        candidates.push(PathBuf::from(home).join(".local/bin").join(executable_name));
    }
    if cfg!(windows) {
        if let Some(local_app_data) = get_env("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local_app_data)
                    .join("Programs/Herdr/bin")
                    .join(executable_name),
            );
        }
    }
    candidates.extend(platform_candidates);

    let mut seen = HashSet::new();
    let mut last = PathBuf::from(executable_name);
    for candidate in candidates {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        last = candidate.clone();
        if let Ok(path) = executable_path(&candidate) {
            return Ok(path);
        }
    }
    Err(ProcessError::NotFound { candidate: last })
}

#[cfg(windows)]
fn default_platform_candidates() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(windows))]
fn default_platform_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin/herdr"),
        PathBuf::from("/usr/local/bin/herdr"),
        PathBuf::from("/usr/bin/herdr"),
    ]
}

fn executable_path(candidate: &Path) -> Result<PathBuf, ProcessError> {
    let metadata = fs::metadata(candidate).map_err(|_| ProcessError::NotFound {
        candidate: candidate.to_path_buf(),
    })?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(ProcessError::NotFound {
            candidate: candidate.to_path_buf(),
        });
    }
    fs::canonicalize(candidate).map_err(|source| ProcessError::Inspect {
        command: format!("herdr executable {}", candidate.display()),
        source,
    })
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::HerdrTarget;

    fn strings(values: &[OsString]) -> Vec<String> {
        values
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn automatic_target_preserves_inherited_routing() {
        let routed = HerdrTarget::Auto.route([OsString::from("api")]);
        assert_eq!(strings(&routed.args), ["api"]);
        assert!(routed.env_set.is_empty());
        assert!(routed.env_remove.is_empty());
    }

    #[test]
    fn explicit_session_prefixes_even_default_and_removes_inherited_routing() {
        for name in ["default", "work.session_2"] {
            let target = HerdrTarget::session(name)
                .unwrap_or_else(|error| panic!("valid session rejected: {error}"));
            let routed = target.route([OsString::from("api"), OsString::from("snapshot")]);
            assert_eq!(
                strings(&routed.args),
                ["--session", name, "api", "snapshot"]
            );
            assert!(routed.env_set.is_empty());
            assert_eq!(
                strings(&routed.env_remove),
                ["HERDR_SOCKET_PATH", "HERDR_SESSION"]
            );
        }
    }

    #[test]
    fn explicit_socket_sets_socket_and_removes_only_session() {
        let routed = HerdrTarget::socket("/tmp/a socket.sock")
            .unwrap_or_else(|error| panic!("valid socket rejected: {error}"))
            .route([OsString::from("api")]);
        assert_eq!(strings(&routed.args), ["api"]);
        assert_eq!(routed.env_set.len(), 1);
        assert_eq!(routed.env_set[0].0, "HERDR_SOCKET_PATH");
        assert_eq!(routed.env_set[0].1, "/tmp/a socket.sock");
        assert_eq!(strings(&routed.env_remove), ["HERDR_SESSION"]);
    }

    #[test]
    fn session_validation_reuses_the_config_grammar() {
        for invalid in ["", ".", "..", "has space", "slash/name", "ü"] {
            assert!(
                HerdrTarget::session(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(HerdrTarget::session("a".repeat(65)).is_err());
        assert!(HerdrTarget::session("A.valid-1").is_ok());
    }

    #[test]
    fn socket_validation_reuses_the_config_grammar() {
        for invalid in ["", " ", " leading", "trailing ", "line\nbreak"] {
            assert!(
                HerdrTarget::socket(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(HerdrTarget::socket("socket name ü").is_ok());
    }
}
