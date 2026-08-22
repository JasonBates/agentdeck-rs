//! Secure, explicit first-run configuration creation.
//!
//! This module deliberately does not inspect optional providers or start the
//! bridge. `--stdout` is side-effect free; the file path mode creates only the
//! target's parent directory and an atomically replaced private config file.

use std::{
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use tempfile::NamedTempFile;

/// The minimal first-run configuration. Herdr uses its own default routing and
/// local-model enrichment remains visibly recommended without selecting a tag:
/// `auto` with no model is an inert, unconfigured capability, so initialization
/// cannot probe, download, or load a model. Optional capacity, telemetry, and
/// title-synchronization tables are absent.
pub const MINIMAL_CONFIG_TOML: &str = r#"# AgentDeck configuration
# Herdr is the only hard runtime dependency. A local model is recommended for
# contextual titles, current-step subtitles, and outcome summaries.

[server]
listen = "127.0.0.1:9798"
base_path = "/"
reconcile_interval = "1s"

[herdr]

# Supported local transcripts are read automatically for context and reply state.
# Set this false for Herdr-only cards with no transcript-file access.
[transcripts]
enabled = true

[headings]
backend = "auto"
# endpoint = "http://127.0.0.1:11434"
# model = "your-installed-model-tag"

[security]
allowed_origins = []
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigInitOptions {
    pub path: PathBuf,
    pub force: bool,
    pub stdout: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigInitOutcome {
    Printed { contents: &'static str },
    Written { path: PathBuf },
}

/// Print or atomically write the minimal config. An existing target is never
/// replaced unless `force` is explicitly set; the no-overwrite path also uses
/// an atomic no-clobber persist to close the check/write race.
pub fn initialize_config(options: &ConfigInitOptions) -> Result<ConfigInitOutcome> {
    if options.stdout {
        return Ok(ConfigInitOutcome::Printed {
            contents: MINIMAL_CONFIG_TOML,
        });
    }

    let parent = options
        .path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("configuration path must have a parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "could not create configuration directory {}",
            parent.display()
        )
    })?;
    make_private_directory(parent)?;

    if !options.force && options.path.exists() {
        bail!(
            "configuration already exists at {}; use --force to replace it",
            options.path.display()
        );
    }

    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create temporary config in {}", parent.display()))?;
    make_private_file(temporary.as_file())?;
    temporary
        .write_all(MINIMAL_CONFIG_TOML.as_bytes())
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .context("could not write temporary AgentDeck config")?;

    if options.force {
        temporary.persist(&options.path).map_err(|error| {
            anyhow::anyhow!(
                "could not atomically replace configuration {}: {}",
                options.path.display(),
                error.error
            )
        })?;
    } else {
        temporary
            .persist_noclobber(&options.path)
            .map_err(|error| {
                if error.error.kind() == std::io::ErrorKind::AlreadyExists {
                    anyhow::anyhow!(
                        "configuration already exists at {}; use --force to replace it",
                        options.path.display()
                    )
                } else {
                    anyhow::anyhow!(
                        "could not atomically create configuration {}: {}",
                        options.path.display(),
                        error.error
                    )
                }
            })?;
    }
    make_private_path(&options.path)?;
    sync_parent(parent)?;

    Ok(ConfigInitOutcome::Written {
        path: options.path.clone(),
    })
}

fn make_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "could not make configuration directory private: {}",
                path.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn make_private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("could not make temporary configuration private")?;
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

fn make_private_path(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not make configuration private: {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn sync_parent(parent: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| {
                format!(
                    "could not sync configuration directory {}",
                    parent.display()
                )
            })?;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{ConfigInitOptions, ConfigInitOutcome, MINIMAL_CONFIG_TOML, initialize_config};
    use crate::config::{Config, HeadingsBackend};

    #[test]
    fn stdout_is_side_effect_free_and_parses_to_a_safe_config() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("temp directory: {error}"));
        let path = directory.path().join("config.toml");
        let outcome = initialize_config(&ConfigInitOptions {
            path: path.clone(),
            force: false,
            stdout: true,
        })
        .unwrap_or_else(|error| panic!("config stdout: {error}"));

        assert_eq!(
            outcome,
            ConfigInitOutcome::Printed {
                contents: MINIMAL_CONFIG_TOML
            }
        );
        assert!(!path.exists());
        let config: Config = toml::from_str(MINIMAL_CONFIG_TOML)
            .unwrap_or_else(|error| panic!("minimal config parses: {error}"));
        assert_eq!(config.headings.backend, HeadingsBackend::Auto);
        assert!(config.headings.model.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn create_refuses_overwrite_unless_forced() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("temp directory: {error}"));
        let path = directory.path().join("private/config.toml");
        let options = ConfigInitOptions {
            path: path.clone(),
            force: false,
            stdout: false,
        };
        assert_eq!(
            initialize_config(&options).unwrap_or_else(|error| panic!("create config: {error}")),
            ConfigInitOutcome::Written { path: path.clone() }
        );
        fs::write(&path, "[headings]\nbackend = 'auto'\n")
            .unwrap_or_else(|error| panic!("overwrite fixture: {error}"));
        assert!(initialize_config(&options).is_err());

        initialize_config(&ConfigInitOptions {
            force: true,
            ..options
        })
        .unwrap_or_else(|error| panic!("forced config write: {error}"));
        assert_eq!(
            fs::read_to_string(path).unwrap_or_else(|error| panic!("read config: {error}")),
            MINIMAL_CONFIG_TOML
        );
    }

    #[cfg(unix)]
    #[test]
    fn created_config_and_parent_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("temp directory: {error}"));
        let path = directory.path().join("private/config.toml");
        initialize_config(&ConfigInitOptions {
            path: path.clone(),
            force: false,
            stdout: false,
        })
        .unwrap_or_else(|error| panic!("create private config: {error}"));

        assert_eq!(
            fs::metadata(&path)
                .unwrap_or_else(|error| panic!("config metadata: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap_or_else(|| panic!("config parent")))
                .unwrap_or_else(|error| panic!("parent metadata: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
