//! Bounded Ollama `/api/ps` local-model telemetry.
//!
//! This is separate from heading generation: a failed telemetry refresh never changes
//! a heading result already accepted by the heading adapter.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt as _;
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tokio::time::timeout;
use url::Url;

use agentdeck_core::{
    CapabilityBackend, CapabilityReason, CapabilityState, CapabilityStatus, LocalModelCall,
    LocalModelSnapshot, LocalModelStatus,
};

use crate::config::{HeadingsBackend, HeadingsConfig, LocalModelTelemetryMode, ModelOverride};

use super::capability;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const CALL_HISTORY_LIMIT: usize = 128;

pub trait LocalModelClock: Send + Sync {
    fn now_seconds(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl LocalModelClock for SystemClock {
    fn now_seconds(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalModelOutcome {
    pub capability: CapabilityStatus,
    pub snapshot: Option<LocalModelSnapshot>,
}

#[derive(Clone, Debug)]
pub struct LocalModelMonitor {
    client: Client,
    endpoint: Url,
    model: String,
    context: i64,
    state: Arc<Mutex<LocalModelState>>,
}

#[derive(Debug)]
struct LocalModelState {
    reachable: bool,
    resident_bytes: Option<u64>,
    active_calls: i64,
    calls: VecDeque<LocalModelCall>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LocalModelError {
    #[error("Ollama telemetry request timed out")]
    Timeout,
    #[error("Ollama telemetry connection failed")]
    Connection,
    #[error("Ollama telemetry returned a non-success response")]
    HttpStatus,
    #[error("Ollama telemetry response exceeded the bound")]
    ResponseTooLarge,
    #[error("Ollama telemetry returned malformed JSON")]
    InvalidData,
}

impl LocalModelMonitor {
    pub fn new(endpoint: Url, model: String, context: i64) -> Result<Self, LocalModelError> {
        let client = Client::builder()
            // Loopback telemetry must not be redirected through user proxy variables.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| LocalModelError::Connection)?;
        Ok(Self {
            client,
            endpoint,
            model,
            context,
            state: Arc::new(Mutex::new(LocalModelState {
                reachable: false,
                resident_bytes: None,
                active_calls: 0,
                calls: VecDeque::with_capacity(CALL_HISTORY_LIMIT),
            })),
        })
    }

    /// Heading execution can use these two methods without giving this adapter prompt
    /// contents or endpoint credentials. The caller supplies the clock explicitly.
    pub fn begin_call(&self) {
        self.update_state(|state| {
            state.active_calls = state.active_calls.saturating_add(1);
        });
    }

    pub fn finish_call(&self, clock: &dyn LocalModelClock, ms: i64, ok: bool) {
        self.update_state(|state| {
            state.active_calls = state.active_calls.saturating_sub(1);
            state.calls.push_back(LocalModelCall {
                at: clock.now_seconds(),
                ms: ms.max(0),
                ok,
            });
            while state.calls.len() > CALL_HISTORY_LIMIT {
                let _ = state.calls.pop_front();
            }
        });
    }

    #[must_use]
    pub fn snapshot(&self) -> LocalModelSnapshot {
        self.read_state(|state| snapshot_from_state(&self.model, self.context, state))
    }

    /// Initial state for an enabled monitor. No request has occurred yet, so the
    /// capability is explicitly not refreshed rather than pretending the model is
    /// offline.
    #[must_use]
    pub fn initial_outcome(&self) -> LocalModelOutcome {
        LocalModelOutcome {
            capability: capability(
                CapabilityState::Error,
                Some(CapabilityBackend::Ollama),
                Some(CapabilityReason::NotRefreshed),
                None,
            ),
            // Reachability has not been observed yet, so do not serialize an
            // `offline` reading that looks current.
            snapshot: None,
        }
    }

    pub async fn sample(&self) -> LocalModelOutcome {
        let result = timeout(OVERALL_TIMEOUT, self.fetch_ps()).await;
        self.update_state(|state| match &result {
            Ok(Ok(models)) => {
                state.reachable = true;
                state.resident_bytes = configured_model_bytes(models, &self.model);
            }
            Ok(Err(_)) | Err(_) => {
                state.reachable = false;
                state.resident_bytes = None;
            }
        });
        let snapshot = Some(self.snapshot());
        match result {
            Ok(Ok(_)) => LocalModelOutcome {
                capability: capability(
                    CapabilityState::Available,
                    Some(CapabilityBackend::Ollama),
                    None,
                    None,
                ),
                snapshot,
            },
            Ok(Err(error)) => LocalModelOutcome {
                capability: capability(
                    CapabilityState::Error,
                    Some(CapabilityBackend::Ollama),
                    Some(reason_for(&error)),
                    None,
                ),
                snapshot,
            },
            Err(_) => LocalModelOutcome {
                capability: capability(
                    CapabilityState::Error,
                    Some(CapabilityBackend::Ollama),
                    Some(CapabilityReason::Timeout),
                    None,
                ),
                snapshot,
            },
        }
    }

    fn read_state<T>(&self, read: impl FnOnce(&LocalModelState) -> T) -> T {
        match self.state.lock() {
            Ok(state) => read(&state),
            Err(poisoned) => read(&poisoned.into_inner()),
        }
    }

    fn update_state<T>(&self, update: impl FnOnce(&mut LocalModelState) -> T) -> T {
        match self.state.lock() {
            Ok(mut state) => update(&mut state),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }

    async fn fetch_ps(&self) -> Result<Vec<OllamaModel>, LocalModelError> {
        let url = self
            .endpoint
            .join("api/ps")
            .map_err(|_| LocalModelError::Connection)?;
        let response = self
            .client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    LocalModelError::Timeout
                } else {
                    LocalModelError::Connection
                }
            })?;
        if !response.status().is_success() {
            return Err(LocalModelError::HttpStatus);
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| LocalModelError::Connection)?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(LocalModelError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let payload: PsPayload =
            serde_json::from_slice(&body).map_err(|_| LocalModelError::InvalidData)?;
        Ok(payload.models)
    }
}

fn snapshot_from_state(model: &str, context: i64, state: &LocalModelState) -> LocalModelSnapshot {
    let status = if state.active_calls > 0 {
        LocalModelStatus::Busy
    } else if !state.reachable {
        LocalModelStatus::Offline
    } else if state.resident_bytes.is_some() {
        LocalModelStatus::Ready
    } else {
        LocalModelStatus::Unloaded
    };
    LocalModelSnapshot {
        name: display_name(model),
        status,
        resident_gb: state.resident_bytes.map(decimal_gb),
        context,
        calls: state.calls.iter().cloned().collect(),
    }
}

pub enum LocalModelTelemetrySelection {
    Disabled(LocalModelOutcome),
    Active(LocalModelMonitor),
}

/// Local-model telemetry has no independent endpoint or model selection: it is only
/// active when headings have explicitly selected an Ollama model and telemetry is
/// `auto` or `on`. Thus `off` performs no HTTP probe.
pub fn select_local_model_telemetry(
    headings: &HeadingsConfig,
    telemetry: LocalModelTelemetryMode,
) -> LocalModelTelemetrySelection {
    if telemetry == LocalModelTelemetryMode::Off || !headings_use_primary_ollama(headings) {
        return LocalModelTelemetrySelection::Disabled(disabled_outcome());
    }
    let Some(model) = headings.model.clone() else {
        return LocalModelTelemetrySelection::Disabled(disabled_outcome());
    };
    let endpoint = match headings.endpoint_url() {
        Ok(endpoint) => endpoint,
        Err(_) => return LocalModelTelemetrySelection::Disabled(error_outcome()),
    };
    match LocalModelMonitor::new(endpoint, model, 4096) {
        Ok(monitor) => LocalModelTelemetrySelection::Active(monitor),
        Err(_) => LocalModelTelemetrySelection::Disabled(error_outcome()),
    }
}

fn headings_use_primary_ollama(headings: &HeadingsConfig) -> bool {
    let ollama_selected = matches!(headings.backend, HeadingsBackend::Ollama)
        || (headings.backend == HeadingsBackend::Auto && headings.model.is_some());
    ollama_selected
        && headings.model.is_some()
        && [
            &headings.title_model,
            &headings.subtitle_model,
            &headings.outcome_model,
            &headings.activity_model,
        ]
        .into_iter()
        .any(|model| matches!(model, ModelOverride::Inherit))
}

fn disabled_outcome() -> LocalModelOutcome {
    LocalModelOutcome {
        capability: capability(
            CapabilityState::Disabled,
            Some(CapabilityBackend::Ollama),
            Some(CapabilityReason::ProviderDisabled),
            None,
        ),
        snapshot: None,
    }
}

fn error_outcome() -> LocalModelOutcome {
    LocalModelOutcome {
        capability: capability(
            CapabilityState::Error,
            Some(CapabilityBackend::Ollama),
            Some(CapabilityReason::InvalidData),
            None,
        ),
        snapshot: None,
    }
}

#[derive(Debug, Deserialize)]
struct PsPayload {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaModel {
    name: Option<String>,
    model: Option<String>,
    size_vram: Option<u64>,
    size: Option<u64>,
}

fn configured_model_bytes(models: &[OllamaModel], configured: &str) -> Option<u64> {
    models
        .iter()
        .find(|model| {
            model
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(configured))
                || model
                    .model
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(configured))
        })
        .and_then(|model| model.size_vram.or(model.size))
}

fn reason_for(error: &LocalModelError) -> CapabilityReason {
    match error {
        LocalModelError::Timeout => CapabilityReason::Timeout,
        LocalModelError::Connection => CapabilityReason::ConnectionFailed,
        LocalModelError::HttpStatus => CapabilityReason::ProviderFailed,
        LocalModelError::ResponseTooLarge | LocalModelError::InvalidData => {
            CapabilityReason::InvalidData
        }
    }
}

fn display_name(model: &str) -> String {
    model
        .split(':')
        .next()
        .unwrap_or(model)
        .to_ascii_uppercase()
}

fn decimal_gb(bytes: u64) -> f64 {
    ((bytes as f64 / 1_000_000_000.0) * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};
    use tokio::{net::TcpListener, time::sleep};

    use super::*;

    async fn start(router: Router) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("test listener must bind: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("test listener must have an address: {error}"));
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .unwrap_or_else(|error| panic!("test server must run: {error}"));
        });
        Url::parse(&format!("http://{address}/"))
            .unwrap_or_else(|error| panic!("test endpoint must parse: {error}"))
    }

    struct Clock(AtomicI64);
    impl LocalModelClock for Clock {
        fn now_seconds(&self) -> i64 {
            self.0.fetch_add(1, Ordering::Relaxed)
        }
    }

    fn monitor() -> LocalModelMonitor {
        LocalModelMonitor::new(
            Url::parse("http://127.0.0.1:11434/")
                .unwrap_or_else(|error| panic!("fixture endpoint: {error}")),
            "gemma4:12b".to_owned(),
            4096,
        )
        .unwrap_or_else(|error| panic!("fixture monitor: {error}"))
    }

    #[test]
    fn configured_model_matches_name_or_model_case_insensitively_and_rounds_decimal_gb() {
        let models: Vec<OllamaModel> = serde_json::from_str(
            r#"[{"name":"other"},{"model":"GEMMA4:12B","size_vram":8050000000}]"#,
        )
        .unwrap_or_else(|error| panic!("fixture payload: {error}"));
        assert_eq!(
            configured_model_bytes(&models, "gemma4:12b"),
            Some(8_050_000_000)
        );
        assert_eq!(decimal_gb(8_050_000_000), 8.1);
    }

    #[test]
    fn calls_are_clocked_ordered_and_bounded_to_exactly_128() {
        let monitor = monitor();
        let clock = Clock(AtomicI64::new(10));
        for index in 0..130 {
            monitor.begin_call();
            monitor.finish_call(&clock, index, index % 2 == 0);
        }
        let calls = monitor.snapshot().calls;
        assert_eq!(calls.len(), 128);
        assert_eq!(calls.first().map(|call| call.at), Some(12));
        assert_eq!(calls.last().map(|call| call.at), Some(139));
    }

    #[test]
    fn busy_has_priority_then_offline_ready_and_unloaded_are_honest() {
        let monitor = monitor();
        assert_eq!(monitor.snapshot().status, LocalModelStatus::Offline);
        monitor.update_state(|state| state.reachable = true);
        assert_eq!(monitor.snapshot().status, LocalModelStatus::Unloaded);
        monitor.update_state(|state| state.resident_bytes = Some(1));
        assert_eq!(monitor.snapshot().status, LocalModelStatus::Ready);
        monitor.begin_call();
        assert_eq!(monitor.snapshot().status, LocalModelStatus::Busy);
    }

    #[test]
    fn selection_only_enables_ollama_headings_and_auto_or_on() {
        let mut headings = HeadingsConfig::default();
        assert!(matches!(
            select_local_model_telemetry(&headings, LocalModelTelemetryMode::Auto),
            LocalModelTelemetrySelection::Disabled(_)
        ));
        headings.model = Some("gemma4:12b".to_owned());
        assert!(matches!(
            select_local_model_telemetry(&headings, LocalModelTelemetryMode::Auto),
            LocalModelTelemetrySelection::Active(_)
        ));
        assert!(matches!(
            select_local_model_telemetry(&headings, LocalModelTelemetryMode::Off),
            LocalModelTelemetrySelection::Disabled(_)
        ));

        let overrides_only = HeadingsConfig {
            title_model: crate::config::ModelOverride::Tag("title-only:latest".to_owned()),
            ..HeadingsConfig::default()
        };
        assert!(matches!(
            select_local_model_telemetry(&overrides_only, LocalModelTelemetryMode::On),
            LocalModelTelemetrySelection::Disabled(_)
        ));

        let no_primary_jobs = HeadingsConfig {
            model: Some("base:latest".to_owned()),
            title_model: crate::config::ModelOverride::Off,
            subtitle_model: crate::config::ModelOverride::Off,
            outcome_model: crate::config::ModelOverride::Off,
            activity_model: crate::config::ModelOverride::Off,
            ..HeadingsConfig::default()
        };
        assert!(matches!(
            select_local_model_telemetry(&no_primary_jobs, LocalModelTelemetryMode::On),
            LocalModelTelemetrySelection::Disabled(_)
        ));
    }

    #[test]
    fn enabled_monitor_does_not_fabricate_a_reading_before_first_sample() {
        let monitor = monitor();
        let initial = monitor.initial_outcome();
        assert_eq!(initial.capability.state, CapabilityState::Error);
        assert_eq!(
            initial.capability.reason,
            Some(CapabilityReason::NotRefreshed)
        );
        assert!(initial.snapshot.is_none());
    }

    #[test]
    fn malformed_payload_is_not_treated_as_an_empty_model_list() {
        assert!(serde_json::from_slice::<PsPayload>(br#"{"models":{}}"#).is_err());
    }

    #[tokio::test]
    async fn bounded_ps_probe_classifies_http_status_and_oversized_bodies() {
        async fn unavailable() -> impl IntoResponse {
            StatusCode::SERVICE_UNAVAILABLE
        }
        let status_monitor = LocalModelMonitor::new(
            start(Router::new().route("/api/ps", get(unavailable))).await,
            "chosen:tag".to_owned(),
            4096,
        )
        .unwrap_or_else(|error| panic!("monitor: {error}"));
        assert!(matches!(
            status_monitor.fetch_ps().await,
            Err(LocalModelError::HttpStatus)
        ));

        async fn large() -> String {
            "x".repeat(MAX_RESPONSE_BYTES + 1)
        }
        let large_monitor = LocalModelMonitor::new(
            start(Router::new().route("/api/ps", get(large))).await,
            "chosen:tag".to_owned(),
            4096,
        )
        .unwrap_or_else(|error| panic!("monitor: {error}"));
        assert!(matches!(
            large_monitor.fetch_ps().await,
            Err(LocalModelError::ResponseTooLarge)
        ));
    }

    #[tokio::test]
    async fn bounded_ps_probe_times_out() {
        async fn slow() -> impl IntoResponse {
            sleep(Duration::from_secs(4)).await;
            "{}"
        }
        let monitor = LocalModelMonitor::new(
            start(Router::new().route("/api/ps", get(slow))).await,
            "chosen:tag".to_owned(),
            4096,
        )
        .unwrap_or_else(|error| panic!("monitor: {error}"));
        assert!(matches!(
            monitor.fetch_ps().await,
            Err(LocalModelError::Timeout)
        ));
    }
}
