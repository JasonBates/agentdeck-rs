use std::{
    fmt, fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::{Host, Url};

use crate::cli::ServeArgs;
use agentdeck_core::headings::HeadingKind;

pub const REDACTED_TOKEN: &str = "<redacted>";
pub const MIN_REMOTE_AUTH_TOKEN_BYTES: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub herdr: HerdrConfig,
    pub transcripts: TranscriptsConfig,
    pub headings: HeadingsConfig,
    pub capacity: CapacityConfig,
    pub telemetry: TelemetryConfig,
    pub tab_titles: TabTitlesConfig,
    pub security: SecurityConfig,
}

impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => Ok(config),
                Err(error) => {
                    let location = safe_toml_location(&contents, error.span());
                    Err(anyhow!(
                        "invalid AgentDeck config at {}{location}",
                        path.display()
                    ))
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error)
                .with_context(|| format!("could not read AgentDeck config at {}", path.display())),
        }
    }

    /// Apply compatibility environment variables using an injected lookup so
    /// precedence is deterministic in tests.
    pub fn apply_environment(&mut self, mut get: impl FnMut(&str) -> Option<String>) -> Result<()> {
        if let Some(port) = get("AGENTDECK_PORT") {
            self.set_port(parse_port("AGENTDECK_PORT", &port)?);
        }
        if let Some(interval) = get("AGENTDECK_INTERVAL") {
            self.server.reconcile_interval =
                parse_legacy_interval("AGENTDECK_INTERVAL", &interval)?;
        }
        if let Some(model) = get("AGENTDECK_MODEL") {
            self.set_model(model);
        }
        if let Some(model) = get("AGENTDECK_TITLE_MODEL") {
            self.headings.title_model = ModelOverride::parse_legacy(model)?;
        }
        if let Some(names) = get("AGENTDECK_NAMES") {
            self.headings.names = NamesMode::from_str(&names, true)
                .map_err(|error| anyhow!("invalid AGENTDECK_NAMES: {error}"))?;
        }
        if let Some(value) = get("AGENTDECK_TAB_TITLES") {
            self.tab_titles.enabled = !value.eq_ignore_ascii_case("off");
        }
        if let Some(path) = get("AGENTDECK_PUBLIC") {
            self.server.public_dir = Some(path);
        }
        if let Some(host) = get("AGENTDECK_PUBLIC_HOST") {
            self.server.public_host = Some(host);
        }
        Ok(())
    }

    pub fn apply_serve_args(&mut self, args: &ServeArgs) -> Result<()> {
        if let Some(port) = args.port {
            self.set_port(port);
        }
        if let Some(interval) = args.interval {
            self.server.reconcile_interval = checked_seconds("--interval", interval)?;
        }
        if let Some(model) = &args.model {
            self.set_model(model.clone());
        }
        if let Some(model) = &args.title_model {
            self.headings.title_model = ModelOverride::parse_legacy(model.clone())?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let listen = self.server.listen.parse::<SocketAddr>().with_context(|| {
            format!(
                "server.listen is not a socket address: {}",
                self.server.listen
            )
        })?;

        validate_duration("server.reconcile_interval", &self.server.reconcile_interval)?;
        validate_base_path(&self.server.base_path)?;
        validate_public_host(self.server.public_host.as_deref())?;
        for origin in &self.security.allowed_origins {
            validate_origin(origin)?;
        }
        self.herdr.validate()?;
        self.headings.validate()?;
        validate_token(
            self.security.auth_token.as_deref(),
            listen.ip().is_loopback(),
        )?;
        Ok(())
    }

    /// Render effective configuration for diagnostics without exposing credentials.
    pub fn redacted_toml(&self) -> Result<String> {
        let mut redacted = self.clone();
        if redacted.security.auth_token.is_some() {
            redacted.security.auth_token = Some(REDACTED_TOKEN.to_owned());
        }
        toml::to_string_pretty(&redacted).context("could not serialize AgentDeck config")
    }

    fn set_port(&mut self, port: u16) {
        let ip = self
            .server
            .listen
            .parse::<SocketAddr>()
            .map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |addr| addr.ip());
        self.server.listen = SocketAddr::new(ip, port).to_string();
    }

    fn set_model(&mut self, model: String) {
        if model.eq_ignore_ascii_case("off") || model.eq_ignore_ascii_case("none") {
            self.headings.backend = HeadingsBackend::None;
            self.headings.model = None;
            self.headings.title_model = ModelOverride::Inherit;
            self.headings.subtitle_model = ModelOverride::Inherit;
            self.headings.outcome_model = ModelOverride::Inherit;
            self.headings.activity_model = ModelOverride::Inherit;
        } else {
            self.headings.backend = HeadingsBackend::Ollama;
            self.headings.model = Some(model);
        }
    }
}

fn safe_toml_location(contents: &str, span: Option<std::ops::Range<usize>>) -> String {
    let Some(offset) = span.map(|span| span.start) else {
        return String::new();
    };
    let Some(prefix) = contents.get(..offset) else {
        return String::new();
    };
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.len(), |(_, tail)| tail.len())
        + 1;
    format!(" (line {line}, column {column})")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub base_path: String,
    pub reconcile_interval: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_host: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9798".to_owned(),
            base_path: "/".to_owned(),
            reconcile_interval: "1s".to_owned(),
            public_dir: None,
            public_host: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerdrConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriptsConfig {
    /// Preserve the existing AgentDeck behavior by default, while allowing a
    /// privacy-conscious installation to disable every local transcript read.
    pub enabled: bool,
}

impl Default for TranscriptsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl HerdrConfig {
    fn validate(&self) -> Result<()> {
        if self.session.is_some() && self.socket.is_some() {
            bail!("herdr.session and herdr.socket are mutually exclusive");
        }
        if let Some(session) = &self.session {
            validate_herdr_session_name(session).map_err(|message| anyhow!(message))?;
        }
        if let Some(socket) = &self.socket {
            validate_herdr_socket_name(socket).map_err(|message| anyhow!(message))?;
        }
        Ok(())
    }
}

/// Validate the public Herdr named-session grammar once for configuration and
/// command routing. Herdr treats the limit as bytes rather than characters.
pub fn validate_herdr_session_name(session: &str) -> std::result::Result<(), &'static str> {
    let valid = !matches!(session, "." | "..")
        && (1..=64).contains(&session.len())
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(
            "herdr.session must be 1-64 ASCII letters, digits, '.', '_' or '-', excluding '.' and '..'",
        )
    }
}

/// Validate the configured local socket spelling once for configuration and
/// command routing. Herdr itself maps this name to the platform transport.
pub fn validate_herdr_socket_name(socket: &str) -> std::result::Result<(), &'static str> {
    if socket.trim().is_empty() || socket.trim() != socket || socket.chars().any(char::is_control) {
        Err("herdr.socket must be a trimmed, nonblank local socket name")
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum HeadingsBackend {
    #[default]
    Auto,
    None,
    Ollama,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NamesMode {
    All,
    #[default]
    Fallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadingsConfig {
    pub backend: HeadingsBackend,
    /// Base URL of the explicitly configured local Ollama service.
    pub endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub title_model: ModelOverride,
    pub subtitle_model: ModelOverride,
    pub outcome_model: ModelOverride,
    pub activity_model: ModelOverride,
    pub names: NamesMode,
}

impl Default for HeadingsConfig {
    fn default() -> Self {
        Self {
            backend: HeadingsBackend::Auto,
            endpoint: "http://127.0.0.1:11434".to_owned(),
            model: None,
            title_model: ModelOverride::Inherit,
            subtitle_model: ModelOverride::Inherit,
            outcome_model: ModelOverride::Inherit,
            activity_model: ModelOverride::Inherit,
            names: NamesMode::Fallback,
        }
    }
}

impl HeadingsConfig {
    fn validate(&self) -> Result<()> {
        validate_ollama_endpoint(&self.endpoint)?;
        validate_optional_model_tag("headings.model", self.model.as_deref())?;
        self.title_model.validate("headings.title_model")?;
        self.subtitle_model.validate("headings.subtitle_model")?;
        self.outcome_model.validate("headings.outcome_model")?;
        self.activity_model.validate("headings.activity_model")?;
        if self.backend == HeadingsBackend::Ollama && self.model.is_none() {
            bail!("headings.backend='ollama' requires a nonblank headings.model");
        }
        Ok(())
    }

    #[must_use]
    pub fn model_for(&self, kind: HeadingKind) -> Option<&str> {
        match match kind {
            HeadingKind::Title => &self.title_model,
            HeadingKind::Subtitle => &self.subtitle_model,
            HeadingKind::Outcome => &self.outcome_model,
            HeadingKind::Activity => &self.activity_model,
        } {
            ModelOverride::Inherit => self.model.as_deref(),
            ModelOverride::Off => None,
            ModelOverride::Tag(tag) => Some(tag),
        }
    }

    pub(crate) fn endpoint_url(&self) -> Result<Url> {
        validate_ollama_endpoint(&self.endpoint)?;
        Url::parse(&self.endpoint).context("validated headings.endpoint must parse")
    }
}

/// A per-job model choice. Tags stay opaque to AgentDeck: Ollama owns their grammar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ModelOverride {
    #[default]
    Inherit,
    Off,
    Tag(String),
}

impl Serialize for ModelOverride {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match self {
            Self::Inherit => "inherit",
            Self::Off => "off",
            Self::Tag(tag) => tag,
        })
    }
}

impl<'de> Deserialize<'de> for ModelOverride {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "inherit" => Self::Inherit,
            "off" => Self::Off,
            _ => Self::Tag(value),
        })
    }
}

impl ModelOverride {
    pub fn parse_legacy(value: String) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "inherit" => Ok(Self::Inherit),
            "off" | "none" => Ok(Self::Off),
            _ => {
                validate_model_tag("AGENTDECK_TITLE_MODEL/--title-model", &value)?;
                Ok(Self::Tag(value))
            }
        }
    }

    fn validate(&self, source: &str) -> Result<()> {
        if let Self::Tag(tag) = self {
            validate_model_tag(source, tag)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CapacityBackend {
    #[default]
    Auto,
    Off,
    Codexbar,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityConfig {
    pub backend: CapacityBackend,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum HostTelemetryMode {
    #[default]
    Auto,
    Off,
    Basic,
    Detailed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LocalModelTelemetryMode {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    pub host: HostTelemetryMode,
    pub local_model: LocalModelTelemetryMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TabTitlesConfig {
    pub enabled: bool,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub allowed_origins: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

impl fmt::Debug for SecurityConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecurityConfig")
            .field("allowed_origins", &self.allowed_origins)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| REDACTED_TOKEN),
            )
            .finish()
    }
}

fn parse_port(source: &str, value: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .with_context(|| format!("{source} must be a port from 0 to 65535"))
}

fn parse_legacy_interval(source: &str, value: &str) -> Result<String> {
    let seconds = value
        .parse::<f64>()
        .with_context(|| format!("{source} must be a number of seconds"))?;
    checked_seconds(source, seconds)
}

fn checked_seconds(source: &str, seconds: f64) -> Result<String> {
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("{source} must be a positive finite number of seconds");
    }
    Ok(format!("{seconds}s"))
}

fn validate_duration(source: &str, value: &str) -> Result<Duration> {
    let duration = humantime::parse_duration(value)
        .with_context(|| format!("{source} must be a duration such as '1s' or '500ms'"))?;
    if duration.is_zero() {
        bail!("{source} must be greater than zero");
    }
    Ok(duration)
}

fn validate_optional_model_tag(source: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        validate_model_tag(source, value)?;
    }
    Ok(())
}

fn validate_model_tag(source: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        bail!("{source} must be trimmed, nonblank, and contain no control characters");
    }
    Ok(())
}

fn validate_ollama_endpoint(value: &str) -> Result<()> {
    let parsed = Url::parse(value).map_err(|_| anyhow!("headings.endpoint is not a valid URL"))?;
    let canonical = parsed.origin().ascii_serialization();
    let loopback = match parsed.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host().is_none()
        || !loopback
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || value != canonical
    {
        bail!(
            "headings.endpoint must be an exact canonical loopback http/https origin without credentials, path, query, or fragment"
        );
    }
    Ok(())
}

fn validate_base_path(value: &str) -> Result<()> {
    let valid = value == "/"
        || value.strip_prefix('/').is_some_and(|relative| {
            !relative.is_empty()
                && relative.split('/').all(|segment| {
                    segment
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                        && segment.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                        })
                        && !matches!(segment, "." | "..")
                })
        });
    if !valid {
        bail!(
            "server.base_path must be '/' or slash-separated literal ASCII segments beginning with a letter or digit and containing only letters, digits, '.', '_' or '-'"
        );
    }
    Ok(())
}

fn validate_public_host(value: Option<&str>) -> Result<()> {
    let Some(host) = value else {
        return Ok(());
    };
    let labels_valid = (1..=253).contains(&host.len())
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        });
    if !labels_valid {
        bail!(
            "server.public_host must be a canonical lowercase DNS hostname without a scheme, port, path, wildcard, or credentials"
        );
    }
    Ok(())
}

fn validate_origin(value: &str) -> Result<()> {
    let parsed = Url::parse(value)
        .map_err(|_| anyhow!("security.allowed_origins entry is not a valid URL"))?;
    let scheme_ok = matches!(parsed.scheme(), "http" | "https");
    let credentials_absent = parsed.username().is_empty() && parsed.password().is_none();
    let exact_origin = parsed.origin().ascii_serialization();
    if !scheme_ok
        || !credentials_absent
        || parsed.host().is_none()
        || value.contains('*')
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || value != exact_origin
    {
        bail!(
            "security.allowed_origins entries must be exact canonical http/https origins without credentials, wildcard, path, query, or fragment"
        );
    }
    Ok(())
}

fn validate_token(token: Option<&str>, loopback: bool) -> Result<()> {
    let Some(token) = token else {
        if loopback {
            return Ok(());
        }
        bail!("a non-loopback server.listen requires security.auth_token");
    };

    if token.trim().is_empty() || token.trim() != token || token.chars().any(char::is_control) {
        bail!("security.auth_token must be trimmed, nonblank, and contain no control characters");
    }
    if !loopback && token.len() < MIN_REMOTE_AUTH_TOKEN_BYTES {
        bail!(
            "security.auth_token for a non-loopback listener must be at least {MIN_REMOTE_AUTH_TOKEN_BYTES} bytes; use a randomly generated token"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::{
        Config, HeadingsBackend, HostTelemetryMode, MIN_REMOTE_AUTH_TOKEN_BYTES, ModelOverride,
        NamesMode, REDACTED_TOKEN,
    };
    use crate::cli::ServeArgs;

    #[test]
    fn defaults_match_the_migration_contract() {
        let config = Config::default();

        assert_eq!(config.server.listen, "127.0.0.1:9798");
        assert_eq!(config.server.reconcile_interval, "1s");
        assert!(config.transcripts.enabled);
        assert_eq!(config.headings.backend, HeadingsBackend::Auto);
        assert_eq!(config.telemetry.host, HostTelemetryMode::Auto);
        assert!(!config.tab_titles.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn transcript_file_access_can_be_disabled_explicitly() {
        let config: Config = toml::from_str("[transcripts]\nenabled = false\n")
            .unwrap_or_else(|error| panic!("transcript config must parse: {error}"));
        assert!(!config.transcripts.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_parse_errors_never_include_source_or_tokens() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must be available: {error}"));
        let path = directory.path().join("config.toml");
        let secret = "0123456789abcdef0123456789abcdef";
        fs::write(
            &path,
            format!("[security]\nauth_token = {secret}\ninvalid = [\n"),
        )
        .unwrap_or_else(|error| panic!("fixture config must be written: {error}"));

        let error = match Config::read(&path) {
            Ok(_) => panic!("invalid TOML must fail"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("invalid AgentDeck config at"));
        assert!(error.contains("line "));
        assert!(error.contains("column "));
        assert!(!error.contains(secret));
        assert!(!error.contains("auth_token"));
        assert!(!error.contains("invalid ="));
    }

    #[test]
    fn non_utf8_config_errors_never_include_valid_prefixes_or_tokens() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory must be available: {error}"));
        let path = directory.path().join("config.toml");
        let secret = "fedcba9876543210fedcba9876543210";
        let mut contents = format!("[security]\nauth_token = '{secret}'\n").into_bytes();
        contents.extend_from_slice(&[0xff, 0xfe, b'\n']);
        fs::write(&path, contents)
            .unwrap_or_else(|error| panic!("fixture config must be written: {error}"));

        let error = match Config::read(&path) {
            Ok(_) => panic!("non-UTF-8 TOML must fail"),
            Err(error) => format!("{error:#}"),
        };

        assert!(error.contains("could not read AgentDeck config at"));
        assert!(!error.contains(secret));
        assert!(!error.contains("auth_token"));
        assert!(!error.contains("[security]"));
    }

    #[test]
    fn precedence_is_cli_then_environment_then_file() {
        let mut config: Config = toml::from_str(
            r#"
                [server]
                listen = "127.0.0.1:8000"
                reconcile_interval = "9s"

                [headings]
                backend = "ollama"
                model = "file-model"
            "#,
        )
        .unwrap_or_else(|error| panic!("test config must parse: {error}"));
        let env = BTreeMap::from([
            ("AGENTDECK_PORT", "8100"),
            ("AGENTDECK_INTERVAL", "2.5"),
            ("AGENTDECK_MODEL", "env-model"),
            ("AGENTDECK_NAMES", "all"),
        ]);
        config
            .apply_environment(|key| env.get(key).map(ToString::to_string))
            .unwrap_or_else(|error| panic!("environment must apply: {error}"));
        config
            .apply_serve_args(&ServeArgs {
                port: Some(8200),
                interval: Some(0.5),
                model: Some("org/arbitrary-model:q4_K_M".to_owned()),
                title_model: None,
            })
            .unwrap_or_else(|error| panic!("CLI must apply: {error}"));

        assert_eq!(config.server.listen, "127.0.0.1:8200");
        assert_eq!(config.server.reconcile_interval, "0.5s");
        assert_eq!(
            config.headings.model.as_deref(),
            Some("org/arbitrary-model:q4_K_M")
        );
        assert_eq!(config.headings.names, NamesMode::All);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn model_off_disables_headings_without_a_probe() {
        let mut config = Config::default();
        config
            .apply_environment(|key| (key == "AGENTDECK_MODEL").then(|| "off".to_owned()))
            .unwrap_or_else(|error| panic!("environment must apply: {error}"));

        assert_eq!(config.headings.backend, HeadingsBackend::None);
        assert_eq!(config.headings.model, None);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn ollama_requires_a_nonblank_model() {
        for model in [None, Some(""), Some("   ")] {
            let mut config = Config::default();
            config.headings.backend = HeadingsBackend::Ollama;
            config.headings.model = model.map(ToOwned::to_owned);
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn headings_support_opaque_model_tags_and_per_job_choices() {
        let config: Config = toml::from_str(
            r#"
                [headings]
                backend = "ollama"
                endpoint = "http://127.0.0.1:11434"
                model = "registry.example/small model:q4_K_M"
                title_model = "inherit"
                subtitle_model = "off"
                outcome_model = "another/arbitrary-tag:latest"
                activity_model = "inherit"
            "#,
        )
        .unwrap_or_else(|error| panic!("config must parse: {error}"));

        assert_eq!(config.headings.title_model, ModelOverride::Inherit);
        assert_eq!(config.headings.subtitle_model, ModelOverride::Off);
        assert_eq!(
            config
                .headings
                .model_for(agentdeck_core::headings::HeadingKind::Title),
            Some("registry.example/small model:q4_K_M")
        );
        assert_eq!(
            config
                .headings
                .model_for(agentdeck_core::headings::HeadingKind::Subtitle),
            None
        );
        assert_eq!(
            config
                .headings
                .model_for(agentdeck_core::headings::HeadingKind::Outcome),
            Some("another/arbitrary-tag:latest")
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn heading_endpoint_must_be_a_canonical_origin_without_credentials() {
        let mut config = Config::default();
        for endpoint in [
            "http://127.0.0.1:11434/",
            "http://user@127.0.0.1:11434",
            "http://127.0.0.1:11434/api",
            "http://127.0.0.1:11434?probe=1",
            "ftp://127.0.0.1:11434",
            "http://192.168.0.2:11434",
            "https://ollama.example.test",
        ] {
            config.headings.endpoint = endpoint.to_owned();
            assert!(config.validate().is_err(), "accepted {endpoint:?}");
        }
    }

    #[test]
    fn unknown_toml_fields_and_enum_spellings_are_rejected_recursively() {
        assert!(toml::from_str::<Config>("mystery = true").is_err());
        assert!(toml::from_str::<Config>("[server]\nmystery = true").is_err());
        assert!(toml::from_str::<Config>("[headings]\nbackend = 'automatic'").is_err());
        assert!(toml::from_str::<Config>("[telemetry]\nhost = 'maximum'").is_err());
    }

    #[test]
    fn every_reconcile_interval_is_validated() {
        let mut config = Config::default();
        config.server.reconcile_interval = "500ms".to_owned();
        assert!(config.validate().is_ok());

        for invalid in ["", "1", "zero", "0s"] {
            config.server.reconcile_interval = invalid.to_owned();
            assert!(config.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn base_path_requires_a_safe_normalized_absolute_path() {
        let mut config = Config::default();
        for valid in ["/", "/deck", "/deck/v1"] {
            config.server.base_path = valid.to_owned();
            assert!(config.validate().is_ok(), "rejected {valid:?}");
        }
        for invalid in [
            "deck",
            "/deck/",
            "//deck",
            "/deck//v1",
            "/./deck",
            "/deck/..",
            "/%2e%2e",
            "/deck?q=1",
            "/deck#fragment",
            "/deck\\admin",
            "/{deck}",
            "/:deck",
            "/*deck",
            "/déck",
            "/-deck",
            "/deck\u{0}",
        ] {
            config.server.base_path = invalid.to_owned();
            assert!(config.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn public_host_is_a_literal_canonical_dns_name() {
        let mut config = Config::default();
        for valid in [
            "deck.example.test",
            "mac-studio.tail123.ts.net",
            "127.0.0.1",
        ] {
            config.server.public_host = Some(valid.to_owned());
            assert!(config.validate().is_ok(), "rejected {valid:?}");
        }
        for invalid in [
            "https://deck.example.test",
            "Deck.example.test",
            "deck.example.test:443",
            "*.example.test",
            "deck..example.test",
            "-deck.example.test",
            "deck.example.test/other",
        ] {
            config.server.public_host = Some(invalid.to_owned());
            assert!(config.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn allowed_origins_are_exact_canonical_http_or_https_origins() {
        let mut config = Config::default();
        for valid in [
            "https://deck.example.test",
            "http://127.0.0.1:3000",
            "https://[::1]:8443",
        ] {
            config.security.allowed_origins = vec![valid.to_owned()];
            assert!(config.validate().is_ok(), "rejected {valid:?}");
        }
        for invalid in [
            "*",
            "ftp://deck.example.test",
            "https://*.example.test",
            "https://deck.example.test/",
            "https://deck.example.test/path",
            "https://deck.example.test?q=1",
            "https://deck.example.test#fragment",
            "https://user@deck.example.test",
        ] {
            config.security.allowed_origins = vec![invalid.to_owned()];
            assert!(config.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn semantic_url_validation_never_echoes_rejected_values_or_secrets() {
        let secret = "semantic-url-secret-0123456789abcdef";
        let mut config = Config::default();
        for endpoint in [
            format!("http://user:{secret}@127.0.0.1:11434"),
            format!("http://127.0.0.1:11434/?token={secret}"),
            format!("not-a-url-{secret}"),
        ] {
            config.headings.endpoint = endpoint.clone();
            let error = match config.validate() {
                Ok(()) => panic!("secret-bearing endpoint must be rejected"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("headings.endpoint"));
            assert!(!error.contains(secret));
            assert!(!error.contains(&endpoint));
        }

        config.headings.endpoint = "http://127.0.0.1:11434".to_owned();
        for origin in [
            format!("https://user:{secret}@deck.example.test"),
            format!("https://deck.example.test/?token={secret}"),
            format!("not-an-origin-{secret}"),
        ] {
            config.security.allowed_origins = vec![origin.clone()];
            let error = match config.validate() {
                Ok(()) => panic!("secret-bearing origin must be rejected"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("security.allowed_origins"));
            assert!(!error.contains(secret));
            assert!(!error.contains(&origin));
        }
    }

    #[test]
    fn herdr_session_and_socket_are_validated() {
        let mut config = Config::default();
        for valid in ["default", "work.session_2", "A-1"] {
            config.herdr.session = Some(valid.to_owned());
            config.herdr.socket = None;
            assert!(config.validate().is_ok(), "rejected {valid:?}");
        }
        for invalid in ["", ".", "..", "has space", "slash/name", "ü"] {
            config.herdr.session = Some(invalid.to_owned());
            assert!(config.validate().is_err(), "accepted {invalid:?}");
        }
        config.herdr.session = Some("a".repeat(65));
        assert!(config.validate().is_err());
        config.herdr.session = Some("default".to_owned());
        config.herdr.socket = Some("/tmp/herdr.sock".to_owned());
        assert!(config.validate().is_err());
        config.herdr.session = None;
        config.herdr.socket = Some("  ".to_owned());
        assert!(config.validate().is_err());
    }

    #[test]
    fn remote_bind_requires_a_strong_trimmed_token() {
        for token in [None, Some(""), Some("   "), Some("short-token")] {
            let mut config = Config::default();
            config.server.listen = "0.0.0.0:9798".to_owned();
            config.security.auth_token = token.map(ToOwned::to_owned);
            assert!(config.validate().is_err());
        }

        let mut config = Config::default();
        config.server.listen = "0.0.0.0:9798".to_owned();
        config.security.auth_token = Some("x".repeat(MIN_REMOTE_AUTH_TOKEN_BYTES));
        assert!(config.validate().is_ok());
        config.security.auth_token = Some(format!(" {}", "x".repeat(MIN_REMOTE_AUTH_TOKEN_BYTES)));
        assert!(config.validate().is_err());
    }

    #[test]
    fn loopback_only_configuration_does_not_require_a_token() {
        let config = Config::default();
        assert!(config.security.auth_token.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn debug_and_printable_toml_redact_the_token() {
        let secret = "this-is-a-private-token-that-must-not-leak";
        let mut config = Config::default();
        config.security.auth_token = Some(secret.to_owned());

        let debug = format!("{config:?}");
        let printable = config
            .redacted_toml()
            .unwrap_or_else(|error| panic!("redacted config must serialize: {error}"));
        assert!(!debug.contains(secret));
        assert!(!printable.contains(secret));
        assert!(debug.contains(REDACTED_TOKEN));
        assert!(printable.contains(REDACTED_TOKEN));
    }
}
