use std::collections::BTreeMap;

use serde::Serialize;

pub trait HealthPort: Send + Sync + 'static {
    fn report(&self) -> HealthReport;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub runtime_version: SafeVersion,
    pub status: HealthStatus,
    pub herdr: AdapterHealth,
    pub capabilities: BTreeMap<CapabilityName, CapabilityHealth>,
    pub adapters: BTreeMap<AdapterName, AdapterHealth>,
    pub degraded_reasons: Vec<HealthReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    Degraded,
}

impl HealthStatus {
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "ok" => Some(Self::Ok),
            "degraded" => Some(Self::Degraded),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CapabilityName {
    Headings,
    Capacity,
    HostTelemetry,
    LocalModelTelemetry,
    TabTitleSync,
}

impl CapabilityName {
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "headings" => Some(Self::Headings),
            "capacity" => Some(Self::Capacity),
            "hostTelemetry" => Some(Self::HostTelemetry),
            "localModelTelemetry" => Some(Self::LocalModelTelemetry),
            "tabTitleSync" => Some(Self::TabTitleSync),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdapterName {
    Herdr,
    Headings,
    Capacity,
    HostTelemetry,
    LocalModelTelemetry,
    TabTitleSync,
}

impl AdapterName {
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "herdr" => Some(Self::Herdr),
            "headings" => Some(Self::Headings),
            "capacity" => Some(Self::Capacity),
            "hostTelemetry" => Some(Self::HostTelemetry),
            "localModelTelemetry" => Some(Self::LocalModelTelemetry),
            "tabTitleSync" => Some(Self::TabTitleSync),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Available,
    Unavailable,
    Missing,
    Disabled,
    Unsupported,
    Error,
}

impl HealthState {
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "available" => Some(Self::Available),
            "unavailable" => Some(Self::Unavailable),
            "missing" => Some(Self::Missing),
            "disabled" => Some(Self::Disabled),
            "unsupported" => Some(Self::Unsupported),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthBackend {
    Herdr,
    None,
    Ollama,
    Codexbar,
    Native,
    System,
}

impl HealthBackend {
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "herdr" => Some(Self::Herdr),
            "none" => Some(Self::None),
            "ollama" => Some(Self::Ollama),
            "codexbar" => Some(Self::Codexbar),
            "native" => Some(Self::Native),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthReason {
    HerdrUnavailable,
    ConnectionFailed,
    ProviderMissing,
    ModelMissing,
    ModelUnconfigured,
    ProviderDisabled,
    ProviderFailed,
    AdapterFailed,
    Timeout,
    ProtocolMismatch,
    InvalidData,
    SamplerFailed,
    StateWriteFailed,
    NotRefreshed,
    Unsupported,
    InternalError,
}

impl HealthReason {
    pub fn from_code(value: &str) -> Option<Self> {
        match value {
            "herdr_unavailable" => Some(Self::HerdrUnavailable),
            "connection_failed" => Some(Self::ConnectionFailed),
            "provider_missing" => Some(Self::ProviderMissing),
            "model_missing" => Some(Self::ModelMissing),
            "model_unconfigured" => Some(Self::ModelUnconfigured),
            "provider_disabled" => Some(Self::ProviderDisabled),
            "provider_failed" => Some(Self::ProviderFailed),
            "adapter_failed" => Some(Self::AdapterFailed),
            "timeout" => Some(Self::Timeout),
            "protocol_mismatch" => Some(Self::ProtocolMismatch),
            "invalid_data" => Some(Self::InvalidData),
            "sampler_failed" => Some(Self::SamplerFailed),
            "state_write_failed" => Some(Self::StateWriteFailed),
            "not_refreshed" => Some(Self::NotRefreshed),
            "unsupported" => Some(Self::Unsupported),
            "internal_error" => Some(Self::InternalError),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SafeVersion(String);

impl SafeVersion {
    /// Accept version-shaped public metadata, not generic identifier tokens.
    /// Requiring a dot prevents opaque API keys and 32-character hex values
    /// from being mistaken for a version.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let without_v = value.strip_prefix('v').unwrap_or(&value);
        let suffix_at = without_v.find(['-', '+']).unwrap_or(without_v.len());
        let (core, suffix) = without_v.split_at(suffix_at);
        let parts = core.split('.').collect::<Vec<_>>();
        let valid = (3..=32).contains(&value.len())
            && parts.len() >= 2
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            && (suffix.is_empty()
                || (suffix.len() > 1
                    && suffix
                        .as_bytes()
                        .get(1)
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && suffix.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-')
                    })));
        valid.then_some(Self(value))
    }

    pub fn package() -> Self {
        // Cargo package versions satisfy `new`; keep the invariant local and
        // use a non-secret fallback if build metadata is ever malformed.
        match Self::new(env!("CARGO_PKG_VERSION")) {
            Some(version) => version,
            None => Self("0.0.0".to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterHealth {
    pub state: HealthState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<SafeVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_unix_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<HealthReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityHealth {
    pub state: HealthState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<HealthBackend>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<HealthReason>,
}

#[derive(Clone)]
pub struct StaticHealth(pub HealthReport);

impl HealthPort for StaticHealth {
    fn report(&self) -> HealthReport {
        self.0.clone()
    }
}
