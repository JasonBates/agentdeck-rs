//! Read-only local dependency and configuration diagnostics.
//!
//! Doctor never installs providers, loads models, authenticates, writes state,
//! or connects to the AgentDeck HTTP service. Its only probes are bounded local
//! Herdr CLI calls, a loopback Ollama metadata request when a model is explicitly
//! configured, an ephemeral bind feasibility check, and filesystem metadata.

use std::{
    env, fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use directories::BaseDirs;
use futures_util::StreamExt as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    adapters::{
        herdr::{
            HerdrClient, HerdrError, HerdrTarget, ProcessError, assess_protocol,
            herdr_config_dir_with, resolve_event_endpoint_with,
        },
        telemetry::capacity::{CodexBarLocator, PathCodexBarLocator},
    },
    config::{CapacityBackend, Config, HeadingsBackend},
    paths::{default_cache_dir, default_config_file, default_state_dir},
};

pub const DOCTOR_SCHEMA_VERSION: u32 = 1;
const OLLAMA_TIMEOUT: Duration = Duration::from_secs(3);
const OLLAMA_MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Available,
    Missing,
    Disabled,
    Unsupported,
    Unavailable,
    Invalid,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorCheck {
    pub status: DoctorStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl DoctorCheck {
    const fn new(status: DoctorStatus) -> Self {
        Self {
            status,
            detail: None,
        }
    }

    fn with_detail(status: DoctorStatus, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: Some(detail.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorConfig {
    pub path: String,
    pub exists: bool,
    #[serde(flatten)]
    pub check: DoctorCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorHerdr {
    pub executable: DoctorCheck,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_protocol: Option<u32>,
    pub compatibility: DoctorCheck,
    pub events: DoctorCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorOllama {
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_available: Option<bool>,
    #[serde(flatten)]
    pub check: DoctorCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorCodexBar {
    pub configured: bool,
    pub supported: bool,
    #[serde(flatten)]
    pub check: DoctorCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorPath {
    pub path: String,
    #[serde(flatten)]
    pub check: DoctorCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorPaths {
    pub state: DoctorPath,
    pub cache: DoctorPath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorPlatform {
    pub os: String,
    pub architecture: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub platform: DoctorPlatform,
    pub config: DoctorConfig,
    pub herdr: DoctorHerdr,
    pub bind: DoctorCheck,
    pub ollama: DoctorOllama,
    pub codexbar: DoctorCodexBar,
    pub paths: DoctorPaths,
    pub warnings: Vec<String>,
}

/// Collect a complete, redacted diagnostic report. `config_path` is the explicit
/// CLI path when present; otherwise platform configuration semantics are used.
pub async fn inspect(config_path: Option<&Path>) -> Result<DoctorReport> {
    let path = match config_path {
        Some(path) => path.to_path_buf(),
        None => default_config_file()?,
    };
    let exists = path.exists();
    let (config, config_check) = load_effective_config(&path);
    let paths = DoctorPaths {
        state: inspect_directory(default_state_dir()?),
        cache: inspect_directory(default_cache_dir()?),
    };
    let (herdr, bind, ollama, codexbar) = match config.as_ref() {
        Some(config) => (
            inspect_herdr(config).await,
            inspect_bind(config),
            inspect_ollama(config).await,
            inspect_codexbar(config),
        ),
        None => (
            skipped_herdr(),
            DoctorCheck::with_detail(DoctorStatus::Skipped, "configuration is invalid"),
            DoctorOllama {
                configured: false,
                model_available: None,
                check: DoctorCheck::with_detail(DoctorStatus::Skipped, "configuration is invalid"),
            },
            DoctorCodexBar {
                configured: false,
                supported: cfg!(target_os = "macos"),
                check: DoctorCheck::with_detail(DoctorStatus::Skipped, "configuration is invalid"),
            },
        ),
    };

    let warnings = {
        #[cfg(windows)]
        {
            vec![
                "Windows Herdr event IPC is beta until verified against native Herdr 0.8.2 / protocol 20; older preview builds are unsupported."
                    .to_owned(),
            ]
        }
        #[cfg(not(windows))]
        {
            Vec::new()
        }
    };

    Ok(DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        platform: DoctorPlatform {
            os: env::consts::OS.to_owned(),
            architecture: env::consts::ARCH.to_owned(),
        },
        config: DoctorConfig {
            path: redact_path(&path),
            exists,
            check: config_check,
        },
        herdr,
        bind,
        ollama,
        codexbar,
        paths,
        warnings,
    })
}

/// Render the human form from the same structured report used by `--json`.
pub fn render_human(report: &DoctorReport) -> String {
    let mut output = format!(
        "AgentDeck doctor (schema {})\nplatform: {}/{}\nconfig: {} ({})\nherdr executable: {}\n",
        report.schema_version,
        report.platform.os,
        report.platform.architecture,
        report.config.path,
        status_text(report.config.check.status),
        status_text(report.herdr.executable.status),
    );
    if let Some(version) = &report.herdr.version {
        output.push_str(&format!("herdr version: {version}\n"));
    }
    if let Some(protocol) = report
        .herdr
        .snapshot_protocol
        .or(report.herdr.schema_protocol)
    {
        output.push_str(&format!(
            "herdr protocol: {protocol} ({})\n",
            status_text(report.herdr.compatibility.status)
        ));
    }
    output.push_str(&format!(
        "herdr events: {}\nbind: {}\nollama: {}\ncodexbar: {}\nstate: {} ({})\ncache: {} ({})\n",
        status_text(report.herdr.events.status),
        status_text(report.bind.status),
        status_text(report.ollama.check.status),
        status_text(report.codexbar.check.status),
        report.paths.state.path,
        status_text(report.paths.state.check.status),
        report.paths.cache.path,
        status_text(report.paths.cache.check.status),
    ));
    for warning in &report.warnings {
        output.push_str(&format!("warning: {warning}\n"));
    }
    output
}

fn load_effective_config(path: &Path) -> (Option<Config>, DoctorCheck) {
    let result = (|| -> Result<Config> {
        let mut config = Config::read(path)?;
        config.apply_environment(|key| env::var(key).ok())?;
        config.validate()?;
        Ok(config)
    })();
    match result {
        Ok(config) => {
            let status = if path.exists() {
                DoctorStatus::Available
            } else {
                DoctorStatus::Missing
            };
            (Some(config), DoctorCheck::new(status))
        }
        Err(_) => (
            None,
            DoctorCheck::with_detail(
                DoctorStatus::Invalid,
                "configuration could not be parsed or validated",
            ),
        ),
    }
}

async fn inspect_herdr(config: &Config) -> DoctorHerdr {
    let target = match HerdrTarget::from_config(&config.herdr) {
        Ok(target) => target,
        Err(_) => return skipped_herdr(),
    };
    let events = inspect_event_marker(&target);
    let client = match HerdrClient::from_config(&config.herdr) {
        Ok(client) => client,
        Err(error) => {
            return DoctorHerdr {
                executable: check_herdr_error(&error),
                version: None,
                schema_protocol: None,
                snapshot_protocol: None,
                compatibility: DoctorCheck::with_detail(
                    DoctorStatus::Skipped,
                    "Herdr is unavailable",
                ),
                events,
            };
        }
    };
    let executable =
        DoctorCheck::with_detail(DoctorStatus::Available, redact_path(client.binary()));
    let version = client.version().await.ok();
    let schema_protocol = client.schema().await.ok().map(|schema| schema.protocol);
    let snapshot_protocol = client
        .snapshot()
        .await
        .ok()
        .map(|snapshot| snapshot.protocol);
    let compatibility = snapshot_protocol.or(schema_protocol).map_or_else(
        || DoctorCheck::with_detail(DoctorStatus::Unavailable, "no compatible protocol response"),
        |protocol| {
            let support = assess_protocol(protocol);
            let status = if support.is_usable() {
                DoctorStatus::Available
            } else {
                DoctorStatus::Invalid
            };
            DoctorCheck::with_detail(
                status,
                support.diagnostic(version.as_deref().unwrap_or("unknown")),
            )
        },
    );
    DoctorHerdr {
        executable,
        version,
        schema_protocol,
        snapshot_protocol,
        compatibility,
        events,
    }
}

fn inspect_event_marker(target: &HerdrTarget) -> DoctorCheck {
    let config_dir = herdr_config_dir_with(|key| env::var_os(key), &env::temp_dir());
    let endpoint = match resolve_event_endpoint_with(target, |key| env::var_os(key), &config_dir) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            return DoctorCheck::with_detail(DoctorStatus::Invalid, "event endpoint is invalid");
        }
    };
    match fs::metadata(endpoint.marker()) {
        Ok(metadata) if is_event_marker(&metadata) => {
            DoctorCheck::with_detail(DoctorStatus::Available, "local event endpoint is present")
        }
        Ok(_) => DoctorCheck::with_detail(
            DoctorStatus::Invalid,
            "event endpoint is not a local socket marker",
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            DoctorCheck::with_detail(DoctorStatus::Missing, "local event endpoint is not present")
        }
        Err(_) => DoctorCheck::with_detail(
            DoctorStatus::Unavailable,
            "local event endpoint could not be inspected",
        ),
    }
}

fn is_event_marker(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        metadata.file_type().is_socket() || metadata.is_file()
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn check_herdr_error(error: &HerdrError) -> DoctorCheck {
    match error {
        HerdrError::Process(ProcessError::NotFound { .. }) => {
            DoctorCheck::with_detail(DoctorStatus::Missing, "Herdr executable was not found")
        }
        _ => DoctorCheck::with_detail(
            DoctorStatus::Unavailable,
            "Herdr executable could not be inspected",
        ),
    }
}

fn skipped_herdr() -> DoctorHerdr {
    DoctorHerdr {
        executable: DoctorCheck::with_detail(DoctorStatus::Skipped, "configuration is invalid"),
        version: None,
        schema_protocol: None,
        snapshot_protocol: None,
        compatibility: DoctorCheck::with_detail(DoctorStatus::Skipped, "configuration is invalid"),
        events: DoctorCheck::with_detail(DoctorStatus::Skipped, "configuration is invalid"),
    }
}

fn inspect_bind(config: &Config) -> DoctorCheck {
    let address = match config.server.listen.parse::<SocketAddr>() {
        Ok(address) => address,
        Err(_) => {
            return DoctorCheck::with_detail(DoctorStatus::Invalid, "server.listen is invalid");
        }
    };
    match TcpListener::bind(address) {
        Ok(listener) => {
            drop(listener);
            DoctorCheck::with_detail(
                DoctorStatus::Available,
                "configured listen address can be bound",
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => DoctorCheck::with_detail(
            DoctorStatus::Unavailable,
            "configured listen address is already in use",
        ),
        Err(_) => DoctorCheck::with_detail(
            DoctorStatus::Unavailable,
            "configured listen address cannot be bound",
        ),
    }
}

async fn inspect_ollama(config: &Config) -> DoctorOllama {
    let configured = matches!(config.headings.backend, HeadingsBackend::Ollama)
        || (config.headings.backend == HeadingsBackend::Auto && config.headings.model.is_some());
    let Some(model) = config.headings.model.as_deref() else {
        return DoctorOllama {
            configured,
            model_available: None,
            check: DoctorCheck::new(if config.headings.backend == HeadingsBackend::None {
                DoctorStatus::Disabled
            } else {
                DoctorStatus::Skipped
            }),
        };
    };
    if !configured {
        return DoctorOllama {
            configured: false,
            model_available: None,
            check: DoctorCheck::new(DoctorStatus::Skipped),
        };
    }
    let endpoint = match config.headings.endpoint_url() {
        Ok(endpoint) => endpoint,
        Err(_) => {
            return DoctorOllama {
                configured: true,
                model_available: None,
                check: DoctorCheck::with_detail(
                    DoctorStatus::Invalid,
                    "configured Ollama endpoint is invalid",
                ),
            };
        }
    };
    match ollama_has_model(endpoint, model).await {
        Ok(true) => DoctorOllama {
            configured: true,
            model_available: Some(true),
            check: DoctorCheck::new(DoctorStatus::Available),
        },
        Ok(false) => DoctorOllama {
            configured: true,
            model_available: Some(false),
            check: DoctorCheck::with_detail(
                DoctorStatus::Missing,
                "configured model is not installed",
            ),
        },
        Err(()) => DoctorOllama {
            configured: true,
            model_available: None,
            check: DoctorCheck::with_detail(
                DoctorStatus::Unavailable,
                "Ollama did not return bounded local metadata",
            ),
        },
    }
}

async fn ollama_has_model(endpoint: url::Url, model: &str) -> Result<bool, ()> {
    tokio::time::timeout(
        OLLAMA_TIMEOUT,
        ollama_has_model_within_timeout(endpoint, model),
    )
    .await
    .map_err(|_| ())?
}

async fn ollama_has_model_within_timeout(endpoint: url::Url, model: &str) -> Result<bool, ()> {
    let url = endpoint.join("api/tags").map_err(|_| ())?;
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ())?;
    let response = client.get(url).send().await.map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if bytes.len().saturating_add(chunk.len()) > OLLAMA_MAX_RESPONSE_BYTES {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    let tags: OllamaTags = serde_json::from_slice(&bytes).map_err(|_| ())?;
    Ok(tags.models.into_iter().any(|candidate| {
        candidate.name.as_deref() == Some(model) || candidate.model.as_deref() == Some(model)
    }))
}

#[derive(Debug, Deserialize)]
struct OllamaTags {
    models: Vec<OllamaTag>,
}

#[derive(Debug, Deserialize)]
struct OllamaTag {
    name: Option<String>,
    model: Option<String>,
}

fn inspect_codexbar(config: &Config) -> DoctorCodexBar {
    let supported = cfg!(target_os = "macos");
    if config.capacity.backend == CapacityBackend::Off {
        return DoctorCodexBar {
            configured: false,
            supported,
            check: DoctorCheck::new(DoctorStatus::Disabled),
        };
    }
    if !supported {
        return DoctorCodexBar {
            configured: config.capacity.backend == CapacityBackend::Codexbar,
            supported: false,
            check: DoctorCheck::with_detail(
                DoctorStatus::Unsupported,
                "CodexBar integration is supported only on macOS",
            ),
        };
    }
    let available = PathCodexBarLocator.locate().is_some();
    DoctorCodexBar {
        configured: config.capacity.backend == CapacityBackend::Codexbar,
        supported: true,
        check: if available {
            DoctorCheck::with_detail(DoctorStatus::Available, "CodexBar executable is present")
        } else {
            DoctorCheck::with_detail(DoctorStatus::Missing, "CodexBar executable was not found")
        },
    }
}

fn inspect_directory(path: PathBuf) -> DoctorPath {
    let check = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => DoctorCheck::new(DoctorStatus::Available),
        Ok(_) => DoctorCheck::with_detail(DoctorStatus::Invalid, "path is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            DoctorCheck::with_detail(DoctorStatus::Missing, "not created yet")
        }
        Err(_) => {
            DoctorCheck::with_detail(DoctorStatus::Unavailable, "path could not be inspected")
        }
    };
    DoctorPath {
        path: redact_path(&path),
        check,
    }
}

fn redact_path(path: &Path) -> String {
    let home = BaseDirs::new().map(|directories| directories.home_dir().to_path_buf());
    redact_path_with_home(path, home.as_deref())
}

fn redact_path_with_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(relative) = path.strip_prefix(home) {
            return if relative.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display())
            };
        }
    }
    path.display().to_string()
}

const fn status_text(status: DoctorStatus) -> &'static str {
    match status {
        DoctorStatus::Available => "available",
        DoctorStatus::Missing => "missing",
        DoctorStatus::Disabled => "disabled",
        DoctorStatus::Unsupported => "unsupported",
        DoctorStatus::Unavailable => "unavailable",
        DoctorStatus::Invalid => "invalid",
        DoctorStatus::Skipped => "skipped",
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, path::Path};

    use super::{DoctorCheck, DoctorStatus, inspect_bind, redact_path_with_home, render_human};
    use crate::config::Config;

    #[test]
    fn path_redaction_removes_the_home_directory() {
        assert_eq!(
            redact_path_with_home(
                Path::new("/home/tester/.config/agentdeck/config.toml"),
                Some(Path::new("/home/tester"))
            ),
            "~/.config/agentdeck/config.toml"
        );
        assert_eq!(
            redact_path_with_home(
                Path::new("/tmp/config.toml"),
                Some(Path::new("/home/tester"))
            ),
            "/tmp/config.toml"
        );
    }

    #[test]
    fn bind_check_identifies_an_occupied_address_without_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("ephemeral listener: {error}"));
        let mut config = Config::default();
        config.server.listen = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("listener address: {error}"))
            .to_string();
        assert_eq!(inspect_bind(&config).status, DoctorStatus::Unavailable);
    }

    #[test]
    fn check_serialization_omits_absent_detail() {
        let encoded = serde_json::to_string(&DoctorCheck::new(DoctorStatus::Available))
            .unwrap_or_else(|error| panic!("serialize check: {error}"));
        assert_eq!(encoded, r#"{"status":"available"}"#);
    }

    #[test]
    fn human_renderer_uses_structured_statuses() {
        let report = super::DoctorReport {
            schema_version: 1,
            platform: super::DoctorPlatform {
                os: "test".into(),
                architecture: "test".into(),
            },
            config: super::DoctorConfig {
                path: "~/.config/agentdeck/config.toml".into(),
                exists: false,
                check: DoctorCheck::new(DoctorStatus::Missing),
            },
            herdr: super::skipped_herdr(),
            bind: DoctorCheck::new(DoctorStatus::Skipped),
            ollama: super::DoctorOllama {
                configured: false,
                model_available: None,
                check: DoctorCheck::new(DoctorStatus::Disabled),
            },
            codexbar: super::DoctorCodexBar {
                configured: false,
                supported: false,
                check: DoctorCheck::new(DoctorStatus::Unsupported),
            },
            paths: super::DoctorPaths {
                state: super::DoctorPath {
                    path: "~/.local/state/agentdeck".into(),
                    check: DoctorCheck::new(DoctorStatus::Missing),
                },
                cache: super::DoctorPath {
                    path: "~/.cache/agentdeck".into(),
                    check: DoctorCheck::new(DoctorStatus::Missing),
                },
            },
            warnings: vec!["fixture warning".into()],
        };
        let rendered = render_human(&report);
        assert!(rendered.contains("config: ~/.config/agentdeck/config.toml (missing)"));
        assert!(rendered.contains("warning: fixture warning"));
    }
}
