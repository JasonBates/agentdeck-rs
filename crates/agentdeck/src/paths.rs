use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::BaseDirs;

use crate::adapters::transcripts::TranscriptRoots;

/// Resolve AgentDeck's configuration path using platform semantics. Environment
/// access stays at this executable boundary; the helpers below inject it in tests.
pub fn default_config_file() -> Result<PathBuf> {
    let base = BaseDirs::new().context("could not resolve platform user directories")?;

    #[cfg(windows)]
    let path = windows_config_file(base.config_dir(), |key| env::var_os(key));

    #[cfg(not(windows))]
    let path = unix_config_file(base.home_dir(), |key| env::var_os(key));

    Ok(path)
}

/// Resolve the directory for versioned AgentDeck state.
pub fn default_state_dir() -> Result<PathBuf> {
    let base = BaseDirs::new().context("could not resolve platform user directories")?;

    #[cfg(windows)]
    let path = windows_state_dir(base.data_local_dir(), |key| env::var_os(key));

    #[cfg(not(windows))]
    let path = unix_state_dir(base.home_dir(), |key| env::var_os(key));

    Ok(path)
}

/// Resolve the directory for disposable AgentDeck caches. This intentionally
/// remains separate from durable state so callers can safely recreate it.
pub fn default_cache_dir() -> Result<PathBuf> {
    let base = BaseDirs::new().context("could not resolve platform user directories")?;

    #[cfg(windows)]
    let path = windows_cache_dir(base.data_local_dir(), |key| env::var_os(key));

    #[cfg(not(windows))]
    let path = unix_cache_dir(base.home_dir(), |key| env::var_os(key));

    Ok(path)
}

/// Resolve the local transcript roots used by the supported filesystem adapters.
///
/// The platform directory provider owns home/profile discovery. Keeping these
/// application-specific descendants here prevents runtime orchestration from
/// inspecting environment variables or spelling private absolute paths.
pub fn default_transcript_roots() -> Result<TranscriptRoots> {
    let base = BaseDirs::new().context("could not resolve platform user directories")?;

    #[cfg(windows)]
    let roots = transcript_roots(base.home_dir(), |key| env::var_os(key));

    #[cfg(not(windows))]
    let roots = transcript_roots(base.home_dir(), |key| env::var_os(key));

    Ok(roots)
}

#[must_use]
pub fn unix_transcript_roots(home: &Path) -> TranscriptRoots {
    transcript_roots(home, |_| None)
}

#[must_use]
pub fn windows_transcript_roots(profile: &Path) -> TranscriptRoots {
    transcript_roots(profile, |_| None)
}

fn transcript_roots(
    home: &Path,
    mut get_env: impl FnMut(&str) -> Option<OsString>,
) -> TranscriptRoots {
    let copilot_home = get_env("COPILOT_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".copilot"));
    TranscriptRoots {
        claude_projects_root: home.join(".claude").join("projects"),
        codex_sessions_root: home.join(".codex").join("sessions"),
        copilot_session_state_root: copilot_home.join("session-state"),
    }
}

/// XDG requires an absolute `XDG_CONFIG_HOME`; empty or relative values are
/// ignored and fall back to the compatibility path under the supplied home.
pub fn unix_config_file(home: &Path, mut get_env: impl FnMut(&str) -> Option<OsString>) -> PathBuf {
    let root = get_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    root.join("agentdeck").join("config.toml")
}

/// XDG state follows the same absolute-path rule as XDG config.
pub fn unix_state_dir(home: &Path, mut get_env: impl FnMut(&str) -> Option<OsString>) -> PathBuf {
    let root = get_env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"));
    root.join("agentdeck")
}

/// XDG cache follows the same absolute-path rule as XDG config and state.
pub fn unix_cache_dir(home: &Path, mut get_env: impl FnMut(&str) -> Option<OsString>) -> PathBuf {
    let root = get_env("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".cache"));
    root.join("agentdeck")
}

/// Windows uses `%APPDATA%`; `directories` supplies the same roaming config
/// location as a fallback when the environment is unavailable.
pub fn windows_config_file(
    fallback_appdata: &Path,
    mut get_env: impl FnMut(&str) -> Option<OsString>,
) -> PathBuf {
    let root = get_env("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_appdata.to_path_buf());
    root.join("agentdeck").join("config.toml")
}

/// Windows state is local to the machine rather than roaming with `%APPDATA%`.
pub fn windows_state_dir(
    fallback_local_appdata: &Path,
    mut get_env: impl FnMut(&str) -> Option<OsString>,
) -> PathBuf {
    let root = get_env("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_local_appdata.to_path_buf());
    root.join("agentdeck").join("state")
}

/// Windows cache is local to the machine, alongside versioned state.
pub fn windows_cache_dir(
    fallback_local_appdata: &Path,
    mut get_env: impl FnMut(&str) -> Option<OsString>,
) -> PathBuf {
    let root = get_env("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_local_appdata.to_path_buf());
    root.join("agentdeck").join("cache")
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    #[cfg(unix)]
    use std::collections::BTreeMap;

    use super::{
        windows_cache_dir, windows_config_file, windows_state_dir, windows_transcript_roots,
    };

    #[cfg(unix)]
    use super::{unix_cache_dir, unix_config_file, unix_state_dir, unix_transcript_roots};

    #[cfg(unix)]
    #[test]
    fn unix_honors_absolute_xdg_config_home() {
        let env = BTreeMap::from([("XDG_CONFIG_HOME", OsString::from("/config-root"))]);
        let path = unix_config_file(Path::new("/home/fixture"), |key| env.get(key).cloned());

        assert_eq!(path, Path::new("/config-root/agentdeck/config.toml"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_ignores_relative_or_missing_xdg_config_home() {
        for xdg in [None, Some(OsString::from("relative/config"))] {
            let path = unix_config_file(Path::new("/home/fixture"), |key| {
                (key == "XDG_CONFIG_HOME").then(|| xdg.clone()).flatten()
            });
            assert_eq!(
                path,
                Path::new("/home/fixture/.config/agentdeck/config.toml")
            );
        }
    }

    #[test]
    fn windows_honors_appdata_then_falls_back_to_platform_directory() {
        let from_env = windows_config_file(Path::new("fallback"), |key| {
            (key == "APPDATA").then(|| OsString::from("roaming"))
        });
        let fallback = windows_config_file(Path::new("fallback"), |_| None);

        assert_eq!(from_env, Path::new("roaming/agentdeck/config.toml"));
        assert_eq!(fallback, Path::new("fallback/agentdeck/config.toml"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_state_paths_honor_xdg_state() {
        let unix = unix_state_dir(Path::new("/home/fixture"), |key| {
            (key == "XDG_STATE_HOME").then(|| OsString::from("/state-root"))
        });
        let unix_fallback = unix_state_dir(Path::new("/home/fixture"), |_| None);

        assert_eq!(unix, Path::new("/state-root/agentdeck"));
        assert_eq!(
            unix_fallback,
            Path::new("/home/fixture/.local/state/agentdeck")
        );
    }

    #[test]
    fn windows_state_paths_use_local_appdata() {
        let windows = windows_state_dir(Path::new("fallback"), |key| {
            (key == "LOCALAPPDATA").then(|| OsString::from("local"))
        });
        assert_eq!(windows, Path::new("local/agentdeck/state"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_cache_paths_honor_xdg_cache() {
        let unix = unix_cache_dir(Path::new("/home/fixture"), |key| {
            (key == "XDG_CACHE_HOME").then(|| OsString::from("/cache-root"))
        });
        let unix_fallback = unix_cache_dir(Path::new("/home/fixture"), |_| None);

        assert_eq!(unix, Path::new("/cache-root/agentdeck"));
        assert_eq!(unix_fallback, Path::new("/home/fixture/.cache/agentdeck"));
    }

    #[test]
    fn windows_cache_paths_use_local_appdata() {
        let windows = windows_cache_dir(Path::new("fallback"), |key| {
            (key == "LOCALAPPDATA").then(|| OsString::from("local"))
        });
        assert_eq!(windows, Path::new("local/agentdeck/cache"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_transcript_roots_follow_home() {
        let unix = unix_transcript_roots(Path::new("/home/fixture"));
        assert_eq!(
            unix.claude_projects_root,
            Path::new("/home/fixture/.claude/projects")
        );
        assert_eq!(
            unix.codex_sessions_root,
            Path::new("/home/fixture/.codex/sessions")
        );
        assert_eq!(
            unix.copilot_session_state_root,
            Path::new("/home/fixture/.copilot/session-state")
        );
    }

    #[test]
    fn windows_transcript_roots_follow_profile() {
        let windows = windows_transcript_roots(Path::new("profile"));
        assert_eq!(
            windows.claude_projects_root,
            Path::new("profile/.claude/projects")
        );
        assert_eq!(
            windows.codex_sessions_root,
            Path::new("profile/.codex/sessions")
        );
        assert_eq!(
            windows.copilot_session_state_root,
            Path::new("profile/.copilot/session-state")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_transcript_roots_honor_only_absolute_copilot_home() {
        let overridden = super::transcript_roots(Path::new("/home/fixture"), |key| {
            (key == "COPILOT_HOME").then(|| OsString::from("/copilot-state"))
        });
        let relative = super::transcript_roots(Path::new("/home/fixture"), |key| {
            (key == "COPILOT_HOME").then(|| OsString::from("relative"))
        });
        assert_eq!(
            overridden.copilot_session_state_root,
            Path::new("/copilot-state/session-state")
        );
        assert_eq!(
            relative.copilot_session_state_root,
            Path::new("/home/fixture/.copilot/session-state")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_transcript_roots_honor_only_absolute_copilot_home() {
        let overridden = super::transcript_roots(Path::new(r"C:\Users\fixture"), |key| {
            (key == "COPILOT_HOME").then(|| OsString::from(r"C:\copilot-state"))
        });
        let relative = super::transcript_roots(Path::new(r"C:\Users\fixture"), |key| {
            (key == "COPILOT_HOME").then(|| OsString::from("relative"))
        });
        assert_eq!(
            overridden.copilot_session_state_root,
            Path::new(r"C:\copilot-state\session-state")
        );
        assert_eq!(
            relative.copilot_session_state_root,
            Path::new(r"C:\Users\fixture\.copilot\session-state")
        );
    }
}
