//! Foreground model-off runtime orchestration.
//!
//! Herdr events and the independent safety poll are invalidation hints only.
//! One reconciliation task owns normalization, deck assembly, read tracking,
//! health transitions, and publication of the latest canonical payload.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ffi::OsString,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agentdeck_core::{
    AgentEnrichment, AssemblyEnrichments, AssemblyFeeds, CapabilityBackend, CapabilityReason,
    CapabilityState, CapabilityStatus, CapacityFeed, Clock, DeckCapabilities, DeckPayload,
    FeedStatus, HerdrAgent, HerdrAgentSession, HerdrSnapshot, HostFeed, ReadTracker, SetupHint,
    activity::{ScreenReadSchedule, parse_background, parse_phase, summarize_background},
    assemble_deck_enriched, clean_title,
    headings::{
        AcceptedHeadings, HeadingKind, HeadingStore, activity_job, outcome_job, subtitle_job,
        title_job,
    },
    is_generic_title,
    tab_titles::{TabRename, TabTitleObservation, TabTitleOwnership},
    transcript::{TranscriptDigest, TranscriptKind, TranscriptOutcome, copilot_relative_path},
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::{StreamExt as _, stream, stream::FuturesUnordered};
use sha2::{Digest as _, Sha256};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::{
    adapters::headings::{
        HeadingCapability, HeadingProvider, HeadingProviderError, HeadingProviderSelection,
        HeadingSetupHint,
    },
    adapters::herdr::{
        EventEndpoint, HerdrClient, HerdrError, HerdrTarget, ProcessError, ProtocolSupport,
        SnapshotDto, VisibleLines, assess_protocol, herdr_config_dir_with, normalize_snapshot,
        resolve_event_endpoint_with, run_event_subscription,
    },
    adapters::telemetry::{
        capacity::{
            CapacityCacheStore, CapacityOutcome, CapacityRefresher, CapacitySelection,
            NativeCapacityPlatform, PathCodexBarLocator, TokioCodexBarRunner, select_capacity,
            unix_now_seconds,
        },
        host::{HostOutcome, HostSampler},
        ollama::{
            LocalModelMonitor, LocalModelOutcome, LocalModelTelemetrySelection,
            SystemClock as LocalModelSystemClock, select_local_model_telemetry,
        },
    },
    adapters::transcripts::{
        FilesystemTranscriptSource, TranscriptObservation, TranscriptRequest, TranscriptSource,
    },
    coalescer::run_reconciliation_coalescer,
    config::{
        CapacityBackend, Config, HeadingsBackend, HeadingsConfig, HerdrConfig, HostTelemetryMode,
        NamesMode,
    },
    http::{
        ActionError, AdapterHealth, AdapterName, CapabilityHealth, CapabilityName, HealthBackend,
        HealthPort, HealthReason, HealthReport, HealthState, HealthStatus, HerdrActions,
        HttpOptions, HttpServer, SafeVersion, StateHub, serve_http,
    },
    paths::{default_cache_dir, default_transcript_roots},
    persistence::tab_titles::{TabTitleStore, TabTitleStoreError},
};

#[cfg(unix)]
use crate::{paths::default_state_dir, persistence::tab_titles::TAB_TITLE_STATE_FILE};

const INVALIDATION_CAPACITY: usize = 1;
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
const FUTURE_PROTOCOL_WARNING_INTERVAL: Duration = Duration::from_secs(60);
const TRANSCRIPT_CONCURRENCY: usize = 5;
const MAX_TRANSCRIPT_AGENTS: usize = 64;
const TRANSCRIPT_PER_AGENT_TIMEOUT: Duration = Duration::from_millis(500);
const TRANSCRIPT_TOTAL_TIMEOUT: Duration = Duration::from_millis(750);
const SCREEN_CONCURRENCY: usize = 8;
const MAX_SCREEN_AGENTS: usize = 64;
const SCREEN_PER_AGENT_TIMEOUT: Duration = Duration::from_millis(700);
const SCREEN_TOTAL_TIMEOUT: Duration = Duration::from_millis(900);
const MAX_SCREEN_PARSE_BYTES: usize = 64 * 1024;
const HEADING_DISCOVERY_RETRY: Duration = Duration::from_secs(20);
const HEADING_POLICY_TICK: Duration = Duration::from_secs(1);
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);
const CAPACITY_INITIAL_DELAY: Duration = Duration::from_secs(1);
const CAPACITY_INTERVAL: Duration = Duration::from_secs(300);
const MAX_TAB_TITLE_OBSERVATIONS: usize = 64;
const MAX_TAB_TITLE_RENAMES: usize = 64;
const MAX_TAB_TITLE_LIVE_TABS: usize = 4096;

/// Run the foreground bridge until Ctrl-C.
pub async fn serve(config: &Config) -> Result<()> {
    config.validate()?;

    let options = HttpOptions::from_config(config)?;
    let listener = TcpListener::bind(options.listen).await.with_context(|| {
        format!(
            "could not bind AgentDeck HTTP listener at {}",
            options.listen
        )
    })?;
    let herdr: Arc<dyn RuntimeHerdr> = Arc::new(ProductionHerdr::new(config.herdr.clone()));
    let events: Arc<dyn RuntimeEvents> = Arc::new(ProductionEvents::from_config(&config.herdr));
    let cancellation = CancellationToken::new();
    let runtime = run_with_listener(config, listener, herdr, events, cancellation.clone());
    tokio::pin!(runtime);

    tokio::select! {
        result = &mut runtime => result,
        signal = tokio::signal::ctrl_c() => {
            let signal = signal.context("could not install or await Ctrl-C handler");
            cancellation.cancel();
            let shutdown = runtime.await;
            signal.and(shutdown)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeFailure {
    Unavailable,
    ConnectionFailed,
    Timeout,
    ProtocolMismatch,
    InvalidData,
    Internal,
}

impl RuntimeFailure {
    const fn reason(self) -> HealthReason {
        match self {
            Self::Unavailable => HealthReason::HerdrUnavailable,
            Self::ConnectionFailed => HealthReason::ConnectionFailed,
            Self::Timeout => HealthReason::Timeout,
            Self::ProtocolMismatch => HealthReason::ProtocolMismatch,
            Self::InvalidData => HealthReason::InvalidData,
            Self::Internal => HealthReason::InternalError,
        }
    }

    const fn state(self) -> HealthState {
        match self {
            Self::ProtocolMismatch | Self::InvalidData | Self::Internal => HealthState::Error,
            Self::Unavailable | Self::ConnectionFailed | Self::Timeout => HealthState::Unavailable,
        }
    }

    const fn payload_code(self) -> &'static str {
        match self {
            Self::Unavailable => "herdr_unavailable",
            Self::ConnectionFailed => "connection_failed",
            Self::Timeout => "timeout",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::InvalidData => "invalid_data",
            Self::Internal => "internal_error",
        }
    }
}

#[async_trait]
trait RuntimeHerdr: Send + Sync + 'static {
    async fn snapshot(&self) -> std::result::Result<RuntimeSnapshot, RuntimeFailure>;
    fn invalidate_diagnostics(&self) {}
    async fn read_visible(
        &self,
        _pane_id: &str,
        _lines: VisibleLines,
    ) -> std::result::Result<String, RuntimeFailure> {
        Err(RuntimeFailure::Unavailable)
    }
    async fn focus_pane(&self, pane_id: &str) -> std::result::Result<(), RuntimeFailure>;
    async fn focus_workspace(&self, workspace_id: &str) -> std::result::Result<(), RuntimeFailure>;
    async fn create_tab(&self, workspace_id: &str) -> std::result::Result<(), RuntimeFailure>;
    async fn rename_tab(
        &self,
        _tab_id: &str,
        _title: &str,
    ) -> std::result::Result<(), RuntimeFailure> {
        Err(RuntimeFailure::Unavailable)
    }
}

#[derive(Clone, Debug)]
struct RuntimeSnapshot {
    snapshot: SnapshotDto,
    client_version: String,
    schema_protocol: u32,
}

impl RuntimeSnapshot {
    #[cfg(test)]
    fn matching(snapshot: SnapshotDto) -> Self {
        Self {
            client_version: snapshot.version.clone(),
            schema_protocol: snapshot.protocol,
            snapshot,
        }
    }
}

#[derive(Clone, Debug)]
struct ClientDiagnostics {
    version: String,
    schema_protocol: u32,
}

#[async_trait]
trait RuntimeEvents: Send + Sync + 'static {
    /// Stay alive through ordinary subscription disconnects and reconnect attempts,
    /// returning only after cancellation. Any earlier return violates the runtime
    /// contract and is treated as a supervised task failure.
    async fn run(&self, invalidations: mpsc::Sender<()>, cancellation: CancellationToken);
}

struct ProductionHerdr {
    config: HerdrConfig,
    state: Mutex<ProductionHerdrState>,
}

#[derive(Default)]
struct ProductionHerdrState {
    client: Option<HerdrClient>,
    diagnostics: Option<ClientDiagnostics>,
}

impl ProductionHerdr {
    fn new(config: HerdrConfig) -> Self {
        Self {
            config,
            state: Mutex::new(ProductionHerdrState::default()),
        }
    }

    fn client(&self) -> std::result::Result<HerdrClient, RuntimeFailure> {
        let mut state = self.state.lock().map_err(|_| RuntimeFailure::Internal)?;
        if let Some(client) = state.client.as_ref() {
            return Ok(client.clone());
        }
        let discovered =
            HerdrClient::from_config(&self.config).map_err(|error| classify_herdr_error(&error))?;
        state.client = Some(discovered.clone());
        Ok(discovered)
    }

    async fn diagnostics(
        &self,
        client: &HerdrClient,
    ) -> std::result::Result<ClientDiagnostics, RuntimeFailure> {
        if let Some(diagnostics) = self.cached_diagnostics()? {
            return Ok(diagnostics);
        }

        let version = client.version().await.map_err(|error| {
            self.clear_diagnostics();
            classify_herdr_error(&error)
        })?;
        let schema = client.schema().await.map_err(|error| {
            self.clear_diagnostics();
            classify_herdr_error(&error)
        })?;
        let diagnostics = ClientDiagnostics {
            version,
            schema_protocol: schema.protocol,
        };
        let mut state = self.state.lock().map_err(|_| RuntimeFailure::Internal)?;
        state.diagnostics = Some(diagnostics.clone());
        Ok(diagnostics)
    }

    fn cached_diagnostics(&self) -> std::result::Result<Option<ClientDiagnostics>, RuntimeFailure> {
        let state = self.state.lock().map_err(|_| RuntimeFailure::Internal)?;
        Ok(state.diagnostics.clone())
    }

    fn clear_diagnostics(&self) {
        match self.state.lock() {
            Ok(mut state) => state.diagnostics = None,
            Err(poisoned) => poisoned.into_inner().diagnostics = None,
        }
    }
}

#[async_trait]
impl RuntimeHerdr for ProductionHerdr {
    async fn snapshot(&self) -> std::result::Result<RuntimeSnapshot, RuntimeFailure> {
        let client = self.client()?;
        let diagnostics = self.diagnostics(&client).await?;
        match client.snapshot().await {
            Ok(snapshot) => Ok(RuntimeSnapshot {
                snapshot,
                client_version: diagnostics.version,
                schema_protocol: diagnostics.schema_protocol,
            }),
            Err(error) => {
                self.clear_diagnostics();
                Err(classify_herdr_error(&error))
            }
        }
    }

    fn invalidate_diagnostics(&self) {
        self.clear_diagnostics();
    }

    async fn read_visible(
        &self,
        pane_id: &str,
        lines: VisibleLines,
    ) -> std::result::Result<String, RuntimeFailure> {
        self.client()?
            .read_visible(pane_id, lines)
            .await
            .map_err(|error| classify_herdr_error(&error))
    }

    async fn focus_pane(&self, pane_id: &str) -> std::result::Result<(), RuntimeFailure> {
        self.client()?
            .focus_pane(pane_id)
            .await
            .map(|_| ())
            .map_err(|error| classify_herdr_error(&error))
    }

    async fn focus_workspace(&self, workspace_id: &str) -> std::result::Result<(), RuntimeFailure> {
        self.client()?
            .focus_workspace(workspace_id)
            .await
            .map(|_| ())
            .map_err(|error| classify_herdr_error(&error))
    }

    async fn create_tab(&self, workspace_id: &str) -> std::result::Result<(), RuntimeFailure> {
        self.client()?
            .create_focused_tab(workspace_id)
            .await
            .map(|_| ())
            .map_err(|error| classify_herdr_error(&error))
    }

    async fn rename_tab(
        &self,
        tab_id: &str,
        title: &str,
    ) -> std::result::Result<(), RuntimeFailure> {
        self.client()?
            .rename_tab(tab_id, title)
            .await
            .map(|_| ())
            .map_err(|error| classify_herdr_error(&error))
    }
}

fn classify_herdr_error(error: &HerdrError) -> RuntimeFailure {
    match error {
        HerdrError::Process(ProcessError::NotFound { .. }) => RuntimeFailure::Unavailable,
        HerdrError::Process(ProcessError::Timeout { .. }) => RuntimeFailure::Timeout,
        HerdrError::Process(
            ProcessError::Spawn { .. }
            | ProcessError::Cancelled { .. }
            | ProcessError::Transport { .. },
        ) => RuntimeFailure::ConnectionFailed,
        HerdrError::Process(
            ProcessError::Inspect { .. }
            | ProcessError::OutputLimit { .. }
            | ProcessError::Api { .. }
            | ProcessError::Syntax { .. },
        )
        | HerdrError::MalformedJson { .. }
        | HerdrError::InvalidUtf8 { .. }
        | HerdrError::MissingResultType { .. }
        | HerdrError::UnexpectedResultType { .. }
        | HerdrError::InvalidVersion { .. } => RuntimeFailure::InvalidData,
        HerdrError::InvalidSession { .. }
        | HerdrError::InvalidSocket { .. }
        | HerdrError::ConflictingTargets
        | HerdrError::LimiterClosed { .. } => RuntimeFailure::Internal,
    }
}

struct ProductionEvents {
    endpoint: Option<EventEndpoint>,
}

impl ProductionEvents {
    fn from_config(config: &HerdrConfig) -> Self {
        let config_dir = herdr_config_dir_with(env_os, &env::temp_dir());
        let endpoint = HerdrTarget::from_config(config)
            .ok()
            .and_then(|target| resolve_event_endpoint_with(&target, env_os, &config_dir).ok());
        Self { endpoint }
    }
}

fn env_os(key: &str) -> Option<OsString> {
    env::var_os(key)
}

#[async_trait]
impl RuntimeEvents for ProductionEvents {
    async fn run(&self, invalidations: mpsc::Sender<()>, cancellation: CancellationToken) {
        let Some(endpoint) = self.endpoint.clone() else {
            cancellation.cancelled().await;
            return;
        };
        let _result = run_event_subscription(endpoint, invalidations, cancellation).await;
    }
}

#[derive(Clone)]
struct RuntimeHealth {
    inner: Arc<RwLock<HealthReport>>,
}

impl RuntimeHealth {
    #[cfg(test)]
    fn initial() -> Self {
        Self::with_all_capabilities(
            &HeadingCapability::Disabled { backend: "none" },
            &TelemetrySnapshot::disabled(),
            &tab_title_capability(
                CapabilityState::Disabled,
                Some(CapabilityReason::ProviderDisabled),
            ),
        )
    }

    #[cfg(test)]
    fn with_heading_capability(capability: &HeadingCapability) -> Self {
        Self::with_all_capabilities(
            capability,
            &TelemetrySnapshot::disabled(),
            &tab_title_capability(
                CapabilityState::Disabled,
                Some(CapabilityReason::ProviderDisabled),
            ),
        )
    }

    #[cfg(test)]
    fn with_capabilities(capability: &HeadingCapability, telemetry: &TelemetrySnapshot) -> Self {
        Self::with_all_capabilities(
            capability,
            telemetry,
            &tab_title_capability(
                CapabilityState::Disabled,
                Some(CapabilityReason::ProviderDisabled),
            ),
        )
    }

    fn with_all_capabilities(
        capability: &HeadingCapability,
        telemetry: &TelemetrySnapshot,
        tab_title_capability: &CapabilityStatus,
    ) -> Self {
        let herdr = AdapterHealth {
            state: HealthState::Unavailable,
            version: None,
            last_success_unix_seconds: None,
            reason: Some(HealthReason::NotRefreshed),
        };
        let heading_health = heading_capability_health(capability);
        let capacity = telemetry_capability_health(&telemetry.capacity.capability);
        let host = telemetry_capability_health(&telemetry.host.capability);
        let local_model = telemetry_capability_health(&telemetry.local_model.capability);
        let tab_title = telemetry_capability_health(tab_title_capability);
        let adapter = |health: &CapabilityHealth, last_success_unix_seconds| AdapterHealth {
            state: health.state,
            version: None,
            last_success_unix_seconds,
            reason: health.reason,
        };
        let now = unix_seconds();
        let mut report = HealthReport {
            runtime_version: SafeVersion::package(),
            status: HealthStatus::Degraded,
            herdr: herdr.clone(),
            capabilities: BTreeMap::from([
                (CapabilityName::Headings, heading_health.clone()),
                (CapabilityName::Capacity, capacity.clone()),
                (CapabilityName::HostTelemetry, host.clone()),
                (CapabilityName::LocalModelTelemetry, local_model.clone()),
                (CapabilityName::TabTitleSync, tab_title.clone()),
            ]),
            adapters: BTreeMap::from([
                (AdapterName::Herdr, herdr),
                (AdapterName::Headings, adapter(&heading_health, None)),
                (
                    AdapterName::Capacity,
                    adapter(&capacity, capacity_last_success(&telemetry.capacity)),
                ),
                (
                    AdapterName::HostTelemetry,
                    adapter(&host, (host.state == HealthState::Available).then_some(now)),
                ),
                (
                    AdapterName::LocalModelTelemetry,
                    adapter(
                        &local_model,
                        (local_model.state == HealthState::Available).then_some(now),
                    ),
                ),
                (AdapterName::TabTitleSync, adapter(&tab_title, None)),
            ]),
            degraded_reasons: Vec::new(),
        };
        recompute_health_status(&mut report);
        Self {
            inner: Arc::new(RwLock::new(report)),
        }
    }

    #[cfg(test)]
    fn for_config(config: &Config) -> Self {
        Self::with_all_capabilities(
            &initial_heading_capability(&config.headings),
            &TelemetrySnapshot::disabled(),
            &tab_title_capability(
                CapabilityState::Disabled,
                Some(CapabilityReason::ProviderDisabled),
            ),
        )
    }

    fn success(&self, version: Option<SafeVersion>, at: u64) {
        self.update(|report| {
            let herdr = AdapterHealth {
                state: HealthState::Available,
                version,
                last_success_unix_seconds: Some(at),
                reason: None,
            };
            report.status = HealthStatus::Ok;
            report.herdr = herdr.clone();
            report.adapters.insert(AdapterName::Herdr, herdr);
            recompute_health_status(report);
        });
    }

    fn failure(&self, failure: RuntimeFailure) {
        self.update(|report| {
            let herdr = AdapterHealth {
                state: failure.state(),
                version: report.herdr.version.clone(),
                last_success_unix_seconds: report.herdr.last_success_unix_seconds,
                reason: Some(failure.reason()),
            };
            report.status = HealthStatus::Degraded;
            report.herdr = herdr.clone();
            report.adapters.insert(AdapterName::Herdr, herdr);
            recompute_health_status(report);
        });
    }

    fn heading_capability(&self, capability: &HeadingCapability) {
        let health = heading_capability_health(capability);
        self.update(|report| {
            report
                .capabilities
                .insert(CapabilityName::Headings, health.clone());
            let last_success_unix_seconds = if health.state == HealthState::Available {
                Some(unix_seconds())
            } else {
                report
                    .adapters
                    .get(&AdapterName::Headings)
                    .and_then(|adapter| adapter.last_success_unix_seconds)
            };
            report.adapters.insert(
                AdapterName::Headings,
                AdapterHealth {
                    state: health.state,
                    version: None,
                    last_success_unix_seconds,
                    reason: health.reason,
                },
            );
            recompute_health_status(report);
        });
    }

    fn telemetry_capability(
        &self,
        capability_name: CapabilityName,
        adapter_name: AdapterName,
        capability: &CapabilityStatus,
    ) {
        self.telemetry_capability_at(capability_name, adapter_name, capability, None);
    }

    fn telemetry_capability_at(
        &self,
        capability_name: CapabilityName,
        adapter_name: AdapterName,
        capability: &CapabilityStatus,
        success_at: Option<u64>,
    ) {
        let health = telemetry_capability_health(capability);
        self.update(|report| {
            report.capabilities.insert(capability_name, health.clone());
            let last_success_unix_seconds = if health.state == HealthState::Available {
                success_at.or_else(|| Some(unix_seconds()))
            } else {
                report
                    .adapters
                    .get(&adapter_name)
                    .and_then(|adapter| adapter.last_success_unix_seconds)
            };
            report.adapters.insert(
                adapter_name,
                AdapterHealth {
                    state: health.state,
                    version: None,
                    last_success_unix_seconds,
                    reason: health.reason,
                },
            );
            recompute_health_status(report);
        });
    }

    fn update(&self, update: impl FnOnce(&mut HealthReport)) {
        match self.inner.write() {
            Ok(mut report) => update(&mut report),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }
}

fn capacity_last_success(outcome: &CapacityOutcome) -> Option<u64> {
    outcome
        .collected_at
        .into_iter()
        .chain(outcome.provider_collected_at.values().copied())
        .filter_map(|value| u64::try_from(value).ok())
        .max()
}

fn recompute_health_status(report: &mut HealthReport) {
    let mut reasons = Vec::new();
    if report.herdr.state != HealthState::Available {
        if let Some(reason) = report.herdr.reason {
            reasons.push(reason);
        }
    }
    for name in [
        CapabilityName::Headings,
        CapabilityName::Capacity,
        CapabilityName::HostTelemetry,
        CapabilityName::LocalModelTelemetry,
    ] {
        if let Some(capability) = report.capabilities.get(&name) {
            if capability.state == HealthState::Error {
                if let Some(reason) = capability.reason {
                    if !reasons.contains(&reason) {
                        reasons.push(reason);
                    }
                }
            }
        }
    }
    report.status = if reasons.is_empty() {
        HealthStatus::Ok
    } else {
        HealthStatus::Degraded
    };
    report.degraded_reasons = reasons;
}

impl HealthPort for RuntimeHealth {
    fn report(&self) -> HealthReport {
        match self.inner.read() {
            Ok(report) => report.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[derive(Clone)]
struct RuntimeActions {
    herdr: Arc<dyn RuntimeHerdr>,
    invalidations: mpsc::Sender<()>,
    health: RuntimeHealth,
}

impl RuntimeActions {
    fn complete(&self, result: std::result::Result<(), RuntimeFailure>) -> Result<(), ActionError> {
        match result {
            Ok(()) => {
                let _result = self.invalidations.try_send(());
                Ok(())
            }
            Err(failure) => {
                self.health.failure(failure);
                Err(ActionError::HerdrUnavailable)
            }
        }
    }
}

#[async_trait]
impl HerdrActions for RuntimeActions {
    async fn focus_pane(&self, pane_id: &str) -> Result<(), ActionError> {
        self.complete(self.herdr.focus_pane(pane_id).await)
    }

    async fn focus_workspace(&self, workspace_id: &str) -> Result<(), ActionError> {
        self.complete(self.herdr.focus_workspace(workspace_id).await)
    }

    async fn create_tab(&self, workspace_id: &str) -> Result<(), ActionError> {
        self.complete(self.herdr.create_tab(workspace_id).await)
    }
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_seconds(&self) -> i64 {
        i64::try_from(unix_seconds()).unwrap_or(i64::MAX)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Clone, Debug, PartialEq)]
struct TelemetrySnapshot {
    capacity: CapacityOutcome,
    host: HostOutcome,
    local_model: LocalModelOutcome,
}

impl TelemetrySnapshot {
    fn disabled() -> Self {
        let disabled = |backend| CapabilityStatus {
            state: CapabilityState::Disabled,
            backend: Some(backend),
            level: None,
            reason: Some(CapabilityReason::ProviderDisabled),
            setup_hint: None,
        };
        Self {
            capacity: CapacityOutcome {
                capability: disabled(CapabilityBackend::Codexbar),
                feed: CapacityFeed {
                    ok: false,
                    reason: Some("disabled".to_owned()),
                    providers: Vec::new(),
                },
                provider_collected_at: BTreeMap::new(),
                collected_at: None,
            },
            host: HostOutcome {
                capability: disabled(CapabilityBackend::System),
                feed: empty_host_feed(),
                basic: None,
            },
            local_model: LocalModelOutcome {
                capability: disabled(CapabilityBackend::Ollama),
                snapshot: None,
            },
        }
    }
}

#[derive(Clone)]
struct RuntimeTelemetry {
    inner: Arc<RwLock<TelemetrySnapshot>>,
}

impl RuntimeTelemetry {
    fn new(snapshot: TelemetrySnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
        }
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self::new(TelemetrySnapshot::disabled())
    }

    fn snapshot(&self) -> TelemetrySnapshot {
        match self.inner.read() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn update_capacity(&self, outcome: CapacityOutcome) -> bool {
        self.update(|snapshot| {
            if snapshot.capacity == outcome {
                false
            } else {
                snapshot.capacity = outcome;
                true
            }
        })
    }

    fn update_host(&self, outcome: HostOutcome) -> bool {
        self.update(|snapshot| {
            if snapshot.host == outcome {
                false
            } else {
                snapshot.host = outcome;
                true
            }
        })
    }

    fn update_local_model(&self, outcome: LocalModelOutcome) -> bool {
        self.update(|snapshot| {
            if snapshot.local_model == outcome {
                false
            } else {
                snapshot.local_model = outcome;
                true
            }
        })
    }

    fn refresh_local_snapshot(&self, monitor: &LocalModelMonitor) -> bool {
        self.update(|snapshot| {
            let next = Some(monitor.snapshot());
            if snapshot.local_model.snapshot == next {
                false
            } else {
                snapshot.local_model.snapshot = next;
                true
            }
        })
    }

    fn update(&self, update: impl FnOnce(&mut TelemetrySnapshot) -> bool) -> bool {
        match self.inner.write() {
            Ok(mut snapshot) => update(&mut snapshot),
            Err(poisoned) => update(&mut poisoned.into_inner()),
        }
    }
}

/// The tab-title capability is deliberately separate from headings and telemetry:
/// title synchronization is optional, mutating, and must never make reconciliation
/// wait for persistence or a second Herdr snapshot.
#[derive(Clone)]
struct RuntimeTabTitleCapability {
    inner: Arc<RwLock<CapabilityStatus>>,
}

impl RuntimeTabTitleCapability {
    fn new(status: CapabilityStatus) -> Self {
        Self {
            inner: Arc::new(RwLock::new(status)),
        }
    }

    fn status(&self) -> CapabilityStatus {
        match self.inner.read() {
            Ok(status) => status.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn update(&self, status: CapabilityStatus) -> bool {
        match self.inner.write() {
            Ok(mut current) => {
                if *current == status {
                    false
                } else {
                    *current = status;
                    true
                }
            }
            Err(poisoned) => {
                let mut current = poisoned.into_inner();
                if *current == status {
                    false
                } else {
                    *current = status;
                    true
                }
            }
        }
    }
}

fn tab_title_capability(
    state: CapabilityState,
    reason: Option<CapabilityReason>,
) -> CapabilityStatus {
    CapabilityStatus {
        state,
        backend: Some(CapabilityBackend::Herdr),
        level: None,
        reason,
        setup_hint: None,
    }
}

enum TabTitleBinding {
    Disabled,
    Unsupported,
    Error,
    Active(TabTitleStore),
}

impl TabTitleBinding {
    fn capability(&self) -> CapabilityStatus {
        match self {
            Self::Disabled => tab_title_capability(
                CapabilityState::Disabled,
                Some(CapabilityReason::ProviderDisabled),
            ),
            Self::Unsupported => tab_title_capability(
                CapabilityState::Unsupported,
                Some(CapabilityReason::Unsupported),
            ),
            Self::Error | Self::Active(_) => {
                tab_title_capability(CapabilityState::Error, Some(CapabilityReason::NotRefreshed))
            }
        }
    }
}

/// Configuration/platform selection has an injected resolver so disabled and
/// unsupported tests can prove that no state-path lookup occurs.
fn tab_title_binding_with(
    config: &crate::config::TabTitlesConfig,
    unix_persistence_supported: bool,
    resolve_path: impl FnOnce() -> std::result::Result<PathBuf, ()>,
) -> TabTitleBinding {
    if !config.enabled {
        return TabTitleBinding::Disabled;
    }
    if !unix_persistence_supported {
        return TabTitleBinding::Unsupported;
    }
    let Ok(path) = resolve_path() else {
        return TabTitleBinding::Error;
    };
    TabTitleBinding::Active(TabTitleStore::new(path))
}

fn production_tab_title_binding(config: &crate::config::TabTitlesConfig) -> TabTitleBinding {
    #[cfg(unix)]
    {
        tab_title_binding_with(config, true, || {
            Ok(default_state_dir()
                .map_err(|_| ())?
                .join(TAB_TITLE_STATE_FILE))
        })
    }
    #[cfg(not(unix))]
    {
        tab_title_binding_with(config, false, || Err(()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabTitleCandidate {
    observation: TabTitleObservation,
    identity: Option<TabTitlePaneIdentity>,
}

/// The worker carries both the immutable pane key and the stable screen
/// identity. A reused tab must not receive a stale title merely because its new
/// pane happens to share a cwd or session spelling with a prior observation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TabTitlePaneIdentity {
    pane_id: String,
    screen: ScreenIdentity,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TabTitleBatch {
    generation: u64,
    ready: bool,
    candidates: Vec<TabTitleCandidate>,
    live_tab_ids: Option<Vec<String>>,
}

#[derive(Clone)]
struct RuntimeTabTitles {
    capability: RuntimeTabTitleCapability,
    observations: Option<watch::Sender<TabTitleBatch>>,
    generation: Arc<AtomicU64>,
}

enum TabTitleTaskInput {
    Inactive,
    Active {
        store: TabTitleStore,
        observations: watch::Receiver<TabTitleBatch>,
    },
}

impl RuntimeTabTitles {
    #[cfg(test)]
    fn inactive() -> Self {
        Self {
            capability: RuntimeTabTitleCapability::new(tab_title_capability(
                CapabilityState::Disabled,
                Some(CapabilityReason::ProviderDisabled),
            )),
            observations: None,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn from_binding(binding: TabTitleBinding) -> (Self, TabTitleTaskInput) {
        let capability = RuntimeTabTitleCapability::new(binding.capability());
        match binding {
            TabTitleBinding::Active(store) => {
                let (sender, receiver) = watch::channel(TabTitleBatch::default());
                (
                    Self {
                        capability,
                        observations: Some(sender),
                        generation: Arc::new(AtomicU64::new(0)),
                    },
                    TabTitleTaskInput::Active {
                        store,
                        observations: receiver,
                    },
                )
            }
            TabTitleBinding::Disabled | TabTitleBinding::Unsupported | TabTitleBinding::Error => (
                Self {
                    capability,
                    observations: None,
                    generation: Arc::new(AtomicU64::new(0)),
                },
                TabTitleTaskInput::Inactive,
            ),
        }
    }

    fn submit(&self, mut batch: TabTitleBatch) {
        let Some(observations) = &self.observations else {
            return;
        };
        batch.generation = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        observations.send_replace(batch);
    }

    fn is_active(&self) -> bool {
        self.observations.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabTitleIntent {
    rename: TabRename,
    identity: TabTitlePaneIdentity,
}

enum TabTitleWorkerExit {
    Cancelled,
    ObservationClosed,
}

async fn run_tab_title_task(
    input: TabTitleTaskInput,
    herdr: Arc<dyn RuntimeHerdr>,
    capability: RuntimeTabTitleCapability,
    health: RuntimeHealth,
    invalidations: mpsc::Sender<()>,
    cancellation: CancellationToken,
) {
    match input {
        TabTitleTaskInput::Inactive => cancellation.cancelled().await,
        TabTitleTaskInput::Active {
            store,
            observations,
        } => {
            let _exit = run_tab_title_worker(
                store,
                herdr,
                observations,
                capability,
                health,
                invalidations,
                cancellation,
            )
            .await;
        }
    }
}

async fn run_tab_title_worker(
    store: TabTitleStore,
    herdr: Arc<dyn RuntimeHerdr>,
    mut observations: watch::Receiver<TabTitleBatch>,
    capability: RuntimeTabTitleCapability,
    health: RuntimeHealth,
    invalidations: mpsc::Sender<()>,
    cancellation: CancellationToken,
) -> TabTitleWorkerExit {
    let mut ownership = None;
    let mut pending_save = false;
    let mut processed_batch = None;
    let mut retry = interval_at(Instant::now(), TELEMETRY_INTERVAL);
    retry.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        if ownership.is_none() {
            match store.load() {
                Ok(loaded) => {
                    ownership = Some(loaded);
                    update_tab_title_capability(
                        &capability,
                        &health,
                        &invalidations,
                        tab_title_capability(CapabilityState::Available, None),
                    );
                }
                Err(TabTitleStoreError::Missing) => {
                    ownership = Some(TabTitleOwnership::default());
                    update_tab_title_capability(
                        &capability,
                        &health,
                        &invalidations,
                        tab_title_capability(CapabilityState::Available, None),
                    );
                }
                Err(error) => {
                    update_tab_title_capability(
                        &capability,
                        &health,
                        &invalidations,
                        tab_title_persistence_error(&error),
                    );
                }
            }
        }

        tokio::select! {
            biased;
            () = cancellation.cancelled() => return TabTitleWorkerExit::Cancelled,
            changed = observations.changed() => {
                if changed.is_err() {
                    return TabTitleWorkerExit::ObservationClosed;
                }
            }
            _ = retry.tick() => {}
        }

        let Some(ownership) = ownership.as_mut() else {
            continue;
        };
        if pending_save {
            match store.save(ownership) {
                Ok(()) => {
                    pending_save = false;
                    update_tab_title_capability(
                        &capability,
                        &health,
                        &invalidations,
                        tab_title_capability(CapabilityState::Available, None),
                    );
                }
                Err(error) => update_tab_title_capability(
                    &capability,
                    &health,
                    &invalidations,
                    tab_title_persistence_error(&error),
                ),
            }
            continue;
        }
        let batch = observations.borrow_and_update().clone();
        if !batch.ready {
            continue;
        }
        if processed_batch.as_ref() == Some(&batch) {
            continue;
        }
        match process_tab_title_batch(
            &store,
            herdr.as_ref(),
            &mut observations,
            ownership,
            &batch,
            &cancellation,
        )
        .await
        {
            Ok(TabTitleBatchResult::Completed) => {
                processed_batch = Some(batch);
                update_tab_title_capability(
                    &capability,
                    &health,
                    &invalidations,
                    tab_title_capability(CapabilityState::Available, None),
                );
            }
            Ok(TabTitleBatchResult::Obsolete) => {
                processed_batch = Some(batch);
            }
            Err(TabTitleWorkerError::Persistence(error)) => {
                processed_batch = Some(batch);
                pending_save = true;
                update_tab_title_capability(
                    &capability,
                    &health,
                    &invalidations,
                    tab_title_persistence_error(&error),
                );
            }
            Err(TabTitleWorkerError::Runtime(error)) => {
                processed_batch = Some(batch);
                update_tab_title_capability(
                    &capability,
                    &health,
                    &invalidations,
                    tab_title_runtime_error(error),
                );
            }
        }
    }
}

enum TabTitleBatchResult {
    Completed,
    Obsolete,
}

enum TabTitleWorkerError {
    Persistence(TabTitleStoreError),
    Runtime(RuntimeFailure),
}

async fn process_tab_title_batch(
    store: &TabTitleStore,
    herdr: &dyn RuntimeHerdr,
    observations: &mut watch::Receiver<TabTitleBatch>,
    ownership: &mut TabTitleOwnership,
    batch: &TabTitleBatch,
    cancellation: &CancellationToken,
) -> std::result::Result<TabTitleBatchResult, TabTitleWorkerError> {
    // Planning can release/prune ownership or recover a matching generated title.
    // Do it on a clone only after proving this is still the latest watch value, so
    // an already-superseded batch cannot mutate in-memory or durable ownership.
    if tab_title_batch_is_obsolete(observations, cancellation) {
        return Ok(TabTitleBatchResult::Obsolete);
    }
    let mut planned_ownership = ownership.clone();
    let intents = tab_title_intents(&mut planned_ownership, batch);
    // `plan_with_live_tabs` is synchronous, but a sender can run on another
    // runtime thread. Recheck immediately before the synchronous durable write.
    if tab_title_batch_is_obsolete(observations, cancellation) {
        return Ok(TabTitleBatchResult::Obsolete);
    }
    if planned_ownership.is_dirty() {
        if let Err(error) = store.save(&mut planned_ownership) {
            // Keep the changed clone dirty so the worker's pending-save retry
            // persists the exact planned state rather than silently discarding it.
            *ownership = planned_ownership;
            return Err(TabTitleWorkerError::Persistence(error));
        }
        *ownership = planned_ownership;
    }

    for intent in intents {
        if tab_title_batch_is_obsolete(observations, cancellation) {
            return Ok(TabTitleBatchResult::Obsolete);
        }
        let snapshot = fresh_tab_title_snapshot(herdr, cancellation)
            .await
            .map_err(TabTitleWorkerError::Runtime)?;
        // A newer reconciliation may have arrived while the fresh snapshot was
        // in flight. Never apply an older intent after that hand-off; the watch
        // channel retains exactly the newest batch for the next worker turn.
        if tab_title_batch_is_obsolete(observations, cancellation) {
            return Ok(TabTitleBatchResult::Obsolete);
        }
        if !tab_title_intent_matches(&snapshot, &intent) {
            continue;
        }
        // Herdr has no compare-and-set rename command. The authoritative snapshot
        // above closes every earlier race; a user change after this check wins the
        // unavoidable final command race and is observed on the next batch.
        let renamed = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(TabTitleBatchResult::Obsolete),
            result = herdr.rename_tab(&intent.rename.tab_id, &intent.rename.title) => result,
        };
        if let Err(error) = renamed {
            return Err(TabTitleWorkerError::Runtime(error));
        }
        // The rename already occurred. Persisting its ownership is required even
        // if a newer observation raced just after the last check; otherwise that
        // real mutation could be mistaken for a manual label on the next batch.
        ownership.rename_succeeded(&intent.rename);
        store
            .save(ownership)
            .map_err(TabTitleWorkerError::Persistence)?;
    }
    Ok(TabTitleBatchResult::Completed)
}

fn tab_title_batch_is_obsolete(
    observations: &watch::Receiver<TabTitleBatch>,
    cancellation: &CancellationToken,
) -> bool {
    cancellation.is_cancelled() || observations.has_changed().unwrap_or(true)
}

fn tab_title_intents(
    ownership: &mut TabTitleOwnership,
    batch: &TabTitleBatch,
) -> Vec<TabTitleIntent> {
    let candidates = batch
        .candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .identity
                .as_ref()
                .map(|identity| (candidate.observation.tab_id.as_str(), identity.clone()))
        })
        .collect::<HashMap<_, _>>();
    ownership
        .plan_with_live_tabs(
            &batch
                .candidates
                .iter()
                .map(|candidate| candidate.observation.clone())
                .collect::<Vec<_>>(),
            batch.live_tab_ids.as_deref(),
        )
        .into_iter()
        .take(MAX_TAB_TITLE_RENAMES)
        .filter_map(|rename| {
            candidates
                .get(rename.tab_id.as_str())
                .cloned()
                .map(|identity| TabTitleIntent { rename, identity })
        })
        .collect()
}

async fn fresh_tab_title_snapshot(
    herdr: &dyn RuntimeHerdr,
    cancellation: &CancellationToken,
) -> std::result::Result<HerdrSnapshot, RuntimeFailure> {
    let observed = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RuntimeFailure::Unavailable),
        observed = herdr.snapshot() => observed,
    }?;
    let raw = observed.snapshot;
    if observed.schema_protocol != raw.protocol || observed.client_version != raw.version {
        herdr.invalidate_diagnostics();
        return Err(RuntimeFailure::ProtocolMismatch);
    }
    if !assess_protocol(raw.protocol).is_usable() {
        return Err(RuntimeFailure::ProtocolMismatch);
    }
    normalize_snapshot(&raw).map_err(|_| RuntimeFailure::InvalidData)
}

fn tab_title_intent_matches(snapshot: &HerdrSnapshot, intent: &TabTitleIntent) -> bool {
    let mut tabs = snapshot
        .tabs
        .iter()
        .filter(|tab| tab.tab_id == intent.rename.tab_id);
    let Some(tab) = tabs.next() else {
        return false;
    };
    if tabs.next().is_some()
        || tab.label.as_deref().unwrap_or_default() != intent.rename.expected_current_label
    {
        return false;
    }
    let mut agents = snapshot
        .agents
        .iter()
        .filter(|agent| agent.tab_id == intent.rename.tab_id);
    let Some(agent) = agents.next() else {
        return false;
    };
    agents.next().is_none()
        && agent.pane_id == intent.identity.pane_id
        && ScreenIdentity::from_agent(agent) == intent.identity.screen
}

fn tab_title_persistence_error(_error: &TabTitleStoreError) -> CapabilityStatus {
    tab_title_capability(
        CapabilityState::Error,
        Some(CapabilityReason::StateWriteFailed),
    )
}

fn tab_title_runtime_error(error: RuntimeFailure) -> CapabilityStatus {
    let reason = match error {
        RuntimeFailure::Timeout => CapabilityReason::Timeout,
        RuntimeFailure::ProtocolMismatch | RuntimeFailure::InvalidData => {
            CapabilityReason::InvalidData
        }
        RuntimeFailure::Unavailable
        | RuntimeFailure::ConnectionFailed
        | RuntimeFailure::Internal => CapabilityReason::ProviderFailed,
    };
    tab_title_capability(CapabilityState::Error, Some(reason))
}

fn update_tab_title_capability(
    capability: &RuntimeTabTitleCapability,
    health: &RuntimeHealth,
    invalidations: &mpsc::Sender<()>,
    status: CapabilityStatus,
) {
    health.telemetry_capability(
        CapabilityName::TabTitleSync,
        AdapterName::TabTitleSync,
        &status,
    );
    signal_if_changed(capability.update(status), invalidations);
}

#[async_trait]
trait RuntimeTelemetrySource: Send + Sync + 'static {
    fn initial(&self) -> TelemetrySnapshot;
    fn heading_monitor(&self) -> Option<LocalModelMonitor> {
        None
    }
    async fn refresh_capacity(&self) -> Option<CapacityOutcome>;
    fn sample_host(&self) -> Option<HostOutcome>;
    async fn sample_local_model(&self) -> Option<LocalModelOutcome>;
}

struct ProductionTelemetrySource {
    initial: TelemetrySnapshot,
    capacity: Option<Arc<CapacityRefresher<TokioCodexBarRunner>>>,
    host: Mutex<HostSampler>,
    host_mode: HostTelemetryMode,
    local_model: Option<LocalModelMonitor>,
}

impl ProductionTelemetrySource {
    async fn new(config: &Config) -> Self {
        let (capacity, capacity_refresher) = production_capacity(config).await;
        let mut host_sampler = HostSampler::new();
        let host = host_sampler.sample(config.telemetry.host);
        let (local_model, local_monitor) =
            match select_local_model_telemetry(&config.headings, config.telemetry.local_model) {
                LocalModelTelemetrySelection::Disabled(outcome) => (outcome, None),
                LocalModelTelemetrySelection::Active(monitor) => {
                    (monitor.initial_outcome(), Some(monitor))
                }
            };
        Self {
            initial: TelemetrySnapshot {
                capacity,
                host,
                local_model,
            },
            capacity: capacity_refresher,
            host: Mutex::new(host_sampler),
            host_mode: config.telemetry.host,
            local_model: local_monitor,
        }
    }
}

#[async_trait]
impl RuntimeTelemetrySource for ProductionTelemetrySource {
    fn initial(&self) -> TelemetrySnapshot {
        self.initial.clone()
    }

    fn heading_monitor(&self) -> Option<LocalModelMonitor> {
        self.local_model.clone()
    }

    async fn refresh_capacity(&self) -> Option<CapacityOutcome> {
        let refresher = self.capacity.as_ref()?;
        Some(refresher.refresh_if_due(unix_now_seconds()).await)
    }

    fn sample_host(&self) -> Option<HostOutcome> {
        if matches!(
            self.host_mode,
            HostTelemetryMode::Off | HostTelemetryMode::Detailed
        ) {
            return None;
        }
        Some(match self.host.lock() {
            Ok(mut sampler) => sampler.sample(self.host_mode),
            Err(poisoned) => poisoned.into_inner().sample(self.host_mode),
        })
    }

    async fn sample_local_model(&self) -> Option<LocalModelOutcome> {
        let monitor = self.local_model.as_ref()?;
        Some(monitor.sample().await)
    }
}

async fn production_capacity(
    config: &Config,
) -> (
    CapacityOutcome,
    Option<Arc<CapacityRefresher<TokioCodexBarRunner>>>,
) {
    if config.capacity.backend == CapacityBackend::Off {
        return (TelemetrySnapshot::disabled().capacity, None);
    }
    let cache_path = match default_cache_dir() {
        Ok(directory) => directory.join("capacity-v2.json"),
        Err(_) => {
            return (
                capacity_error_outcome(CapabilityReason::StateWriteFailed),
                None,
            );
        }
    };
    match select_capacity(
        &config.capacity,
        &NativeCapacityPlatform,
        &PathCodexBarLocator,
        TokioCodexBarRunner,
        CapacityCacheStore::new(cache_path),
        unix_now_seconds(),
    ) {
        CapacitySelection::Inactive(outcome) => (outcome, None),
        CapacitySelection::Active(refresher) => {
            let refresher = Arc::new(refresher);
            let initial = refresher.current().await;
            (initial, Some(refresher))
        }
    }
}

fn capacity_error_outcome(reason: CapabilityReason) -> CapacityOutcome {
    CapacityOutcome {
        capability: CapabilityStatus {
            state: CapabilityState::Error,
            backend: Some(CapabilityBackend::Codexbar),
            level: None,
            reason: Some(reason),
            setup_hint: None,
        },
        feed: CapacityFeed {
            ok: false,
            reason: Some(
                match reason {
                    CapabilityReason::StateWriteFailed => "state_write_failed",
                    _ => "adapter_failed",
                }
                .to_owned(),
            ),
            providers: Vec::new(),
        },
        provider_collected_at: BTreeMap::new(),
        collected_at: None,
    }
}

fn empty_host_feed() -> HostFeed {
    HostFeed {
        ok: false,
        load1: 0.0,
        load5: 0.0,
        cores: 0,
        system: None,
    }
}

enum TelemetryJob {
    Capacity(Option<CapacityOutcome>),
    LocalModel(Option<LocalModelOutcome>),
}

type TelemetryJobFuture = Pin<Box<dyn Future<Output = TelemetryJob> + Send>>;

async fn run_telemetry_worker(
    source: Arc<dyn RuntimeTelemetrySource>,
    telemetry: RuntimeTelemetry,
    health: RuntimeHealth,
    invalidations: mpsc::Sender<()>,
    cancellation: CancellationToken,
) {
    let mut host_timer = interval_at(Instant::now() + TELEMETRY_INTERVAL, TELEMETRY_INTERVAL);
    host_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut local_timer = interval_at(Instant::now() + TELEMETRY_INTERVAL, TELEMETRY_INTERVAL);
    local_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut capacity_timer =
        interval_at(Instant::now() + CAPACITY_INITIAL_DELAY, CAPACITY_INTERVAL);
    capacity_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut jobs = FuturesUnordered::<TelemetryJobFuture>::new();
    let mut capacity_running = false;
    let mut local_running = false;

    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return,
            Some(job) = jobs.next(), if !jobs.is_empty() => {
                match job {
                    TelemetryJob::Capacity(outcome) => {
                        capacity_running = false;
                        if let Some(outcome) = outcome {
                            health.telemetry_capability_at(
                                CapabilityName::Capacity,
                                AdapterName::Capacity,
                                &outcome.capability,
                                capacity_last_success(&outcome),
                            );
                            signal_if_changed(
                                telemetry.update_capacity(outcome),
                                &invalidations,
                            );
                        }
                    }
                    TelemetryJob::LocalModel(outcome) => {
                        local_running = false;
                        if let Some(outcome) = outcome {
                            health.telemetry_capability(
                                CapabilityName::LocalModelTelemetry,
                                AdapterName::LocalModelTelemetry,
                                &outcome.capability,
                            );
                            signal_if_changed(
                                telemetry.update_local_model(outcome),
                                &invalidations,
                            );
                        }
                    }
                }
            }
            _ = capacity_timer.tick(), if !capacity_running => {
                capacity_running = true;
                let source = source.clone();
                jobs.push(Box::pin(async move {
                    TelemetryJob::Capacity(source.refresh_capacity().await)
                }));
            }
            _ = host_timer.tick() => {
                if let Some(outcome) = source.sample_host() {
                    health.telemetry_capability(
                        CapabilityName::HostTelemetry,
                        AdapterName::HostTelemetry,
                        &outcome.capability,
                    );
                    signal_if_changed(telemetry.update_host(outcome), &invalidations);
                }
            }
            _ = local_timer.tick(), if !local_running => {
                local_running = true;
                let source = source.clone();
                jobs.push(Box::pin(async move {
                    TelemetryJob::LocalModel(source.sample_local_model().await)
                }));
            }
        }
    }
}

fn signal_if_changed(changed: bool, invalidations: &mpsc::Sender<()>) {
    if changed {
        let _result = invalidations.try_send(());
    }
}

#[derive(Clone)]
struct HeadingCallReporter {
    monitor: Option<LocalModelMonitor>,
    telemetry: RuntimeTelemetry,
    invalidations: mpsc::Sender<()>,
}

impl HeadingCallReporter {
    fn begin(&self) -> HeadingCallLease {
        if let Some(monitor) = &self.monitor {
            monitor.begin_call();
            signal_if_changed(
                self.telemetry.refresh_local_snapshot(monitor),
                &self.invalidations,
            );
        }
        HeadingCallLease {
            reporter: self.clone(),
            started_at: Instant::now(),
            finished: false,
        }
    }

    fn finish(&self, started_at: Instant, ok: bool) {
        let Some(monitor) = &self.monitor else {
            return;
        };
        let elapsed = i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX);
        monitor.finish_call(&LocalModelSystemClock, elapsed, ok);
        signal_if_changed(
            self.telemetry.refresh_local_snapshot(monitor),
            &self.invalidations,
        );
    }
}

struct HeadingCallLease {
    reporter: HeadingCallReporter,
    started_at: Instant,
    finished: bool,
}

impl HeadingCallLease {
    fn finish(mut self, ok: bool) {
        self.reporter.finish(self.started_at, ok);
        self.finished = true;
    }
}

impl Drop for HeadingCallLease {
    fn drop(&mut self) {
        if !self.finished {
            self.reporter.finish(self.started_at, false);
        }
    }
}

struct TelemetryHeadingProvider {
    inner: Box<dyn HeadingProvider>,
    calls: HeadingCallReporter,
}

#[async_trait]
impl HeadingProvider for TelemetryHeadingProvider {
    async fn generate(
        &self,
        job: &agentdeck_core::headings::HeadingJob,
        current_title: Option<&str>,
    ) -> std::result::Result<Option<String>, HeadingProviderError> {
        let lease = self.calls.begin();
        let result = self.inner.generate(job, current_title).await;
        lease.finish(result.is_ok());
        result
    }
}

#[derive(Clone)]
struct StateOwner {
    herdr: Arc<dyn RuntimeHerdr>,
    states: Arc<StateHub>,
    health: RuntimeHealth,
    tracker: Arc<Mutex<ReadTracker>>,
    future_protocol_warning: Arc<Mutex<FutureProtocolWarning>>,
    transcripts: Option<Arc<dyn TranscriptSource>>,
    screens: Arc<Mutex<ScreenState>>,
    started_at: Instant,
    headings: RuntimeHeadings,
    tab_titles: RuntimeTabTitles,
}

#[derive(Default)]
struct FutureProtocolWarning {
    last_warning: Option<Instant>,
}

impl FutureProtocolWarning {
    fn should_warn(&mut self, now: Instant) -> bool {
        let allowed = self.last_warning.is_none_or(|last| {
            now.saturating_duration_since(last) >= FUTURE_PROTOCOL_WARNING_INTERVAL
        });
        if allowed {
            self.last_warning = Some(now);
        }
        allowed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScreenIdentity {
    kind: String,
    cwd: String,
    session: Option<HerdrAgentSession>,
}

impl ScreenIdentity {
    fn from_agent(agent: &HerdrAgent) -> Self {
        Self {
            kind: agent.kind.clone(),
            cwd: agent.cwd.clone(),
            session: agent.session.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScreenObservation {
    phase: Option<agentdeck_core::Phase>,
    background: Option<String>,
    heading_screen: Option<HeadingScreen>,
}

#[derive(Default)]
struct ScreenState {
    schedule: ScreenReadSchedule,
    observations: HashMap<String, (ScreenIdentity, ScreenObservation)>,
}

#[derive(Clone)]
struct ScreenRequest {
    pane_id: String,
    identity: ScreenIdentity,
    working: bool,
    lines: VisibleLines,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScreenReadOutput {
    enrichments: AssemblyEnrichments,
    headings: HashMap<String, HeadingScreen>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HeadingScreen {
    key: String,
    content: String,
}

impl HeadingScreen {
    fn new(content: String) -> Self {
        let key = format!("{:x}", Sha256::digest(content.as_bytes()));
        Self { key, content }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HeadingPaneObservation {
    identity: ScreenIdentity,
    digest: Option<TranscriptDigest>,
    screen: Option<HeadingScreen>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HeadingObservation {
    panes: HashMap<String, HeadingPaneObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AcceptedPaneHeadings {
    identity: ScreenIdentity,
    values: AcceptedHeadings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HeadingSharedState {
    capability: HeadingCapability,
    accepted: HashMap<String, AcceptedPaneHeadings>,
}

#[derive(Clone)]
struct RuntimeHeadings {
    observations: watch::Sender<HeadingObservation>,
    shared: Arc<RwLock<HeadingSharedState>>,
    names: NamesMode,
    telemetry: RuntimeTelemetry,
    tab_titles: RuntimeTabTitleCapability,
}

impl RuntimeHeadings {
    #[cfg(test)]
    fn new(
        config: &HeadingsConfig,
    ) -> (
        Self,
        watch::Receiver<HeadingObservation>,
        Arc<RwLock<HeadingSharedState>>,
    ) {
        Self::new_with_telemetry(
            config,
            RuntimeTelemetry::disabled(),
            RuntimeTabTitleCapability::new(tab_title_capability(
                CapabilityState::Disabled,
                Some(CapabilityReason::ProviderDisabled),
            )),
        )
    }

    fn new_with_telemetry(
        config: &HeadingsConfig,
        telemetry: RuntimeTelemetry,
        tab_titles: RuntimeTabTitleCapability,
    ) -> (
        Self,
        watch::Receiver<HeadingObservation>,
        Arc<RwLock<HeadingSharedState>>,
    ) {
        let (observations, receiver) = watch::channel(HeadingObservation::default());
        let shared = Arc::new(RwLock::new(HeadingSharedState {
            capability: initial_heading_capability(config),
            accepted: HashMap::new(),
        }));
        (
            Self {
                observations,
                shared: shared.clone(),
                names: config.names,
                telemetry,
                tab_titles,
            },
            receiver,
            shared,
        )
    }

    fn submit(
        &self,
        snapshot: &HerdrSnapshot,
        transcripts: &HashMap<String, TranscriptObservation>,
        screens: &HashMap<String, HeadingScreen>,
    ) {
        let panes = snapshot
            .agents
            .iter()
            .map(|agent| {
                let digest = if transcript_kind(&agent.kind).supports_generated_headings() {
                    transcripts.get(&agent.pane_id).and_then(|observation| {
                        let TranscriptOutcome::Ready(analysis) = &observation.analysis else {
                            return None;
                        };
                        analysis.digest.clone()
                    })
                } else {
                    None
                };
                (
                    agent.pane_id.clone(),
                    HeadingPaneObservation {
                        identity: ScreenIdentity::from_agent(agent),
                        digest,
                        screen: screens.get(&agent.pane_id).cloned(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let observation = HeadingObservation { panes };
        self.observations.send_if_modified(|current| {
            if current == &observation {
                false
            } else {
                *current = observation;
                true
            }
        });
    }

    fn overlay(&self, snapshot: &HerdrSnapshot, enrichments: &mut AssemblyEnrichments) {
        let shared = match self.shared.read() {
            Ok(shared) => shared,
            Err(poisoned) => poisoned.into_inner(),
        };
        for agent in &snapshot.agents {
            let Some(accepted) = shared.accepted.get(&agent.pane_id) else {
                continue;
            };
            if accepted.identity != ScreenIdentity::from_agent(agent) {
                continue;
            }
            let enrichment = enrichments
                .by_pane
                .entry(agent.pane_id.clone())
                .or_default();
            if self.names == NamesMode::All || agent_has_generic_title(agent) {
                enrichment.model_title = accepted.values.title.clone();
            }
            enrichment.focus = accepted.values.subtitle.clone();
            enrichment.state = accepted.values.outcome.clone();
            enrichment.activity = accepted.values.activity.clone();
        }
    }

    /// Snapshot-only title inputs for the independent mutation worker. Every
    /// candidate is tied to a unique known tab and exactly one current agent;
    /// raw model text stays in-process and never reaches health or logging.
    fn tab_title_batch(&self, snapshot: &HerdrSnapshot) -> TabTitleBatch {
        let shared = match self.shared.read() {
            Ok(shared) => shared,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut tab_counts = HashMap::<&str, usize>::new();
        for tab in &snapshot.tabs {
            *tab_counts.entry(&tab.tab_id).or_default() += 1;
        }
        let known = tab_counts
            .iter()
            .filter_map(|(tab_id, count)| (*count == 1).then_some(*tab_id))
            .collect::<HashSet<_>>();
        let mut agents = HashMap::<&str, Vec<&HerdrAgent>>::new();
        for agent in &snapshot.agents {
            if known.contains(agent.tab_id.as_str()) {
                agents.entry(&agent.tab_id).or_default().push(agent);
            }
        }

        let live_tab_ids = (tab_counts.len() <= MAX_TAB_TITLE_LIVE_TABS).then(|| {
            tab_counts
                .keys()
                .map(|tab_id| (*tab_id).to_owned())
                .collect::<Vec<_>>()
        });
        let mut candidates = Vec::new();
        for tab in &snapshot.tabs {
            if candidates.len() == MAX_TAB_TITLE_OBSERVATIONS {
                break;
            }
            let count = tab_counts
                .get(tab.tab_id.as_str())
                .copied()
                .unwrap_or_default();
            if count != 1 {
                continue;
            }
            let tab_agents = agents
                .get(tab.tab_id.as_str())
                .map_or(&[][..], |agents| agents.as_slice());
            let single = (tab_agents.len() == 1).then(|| tab_agents[0]);
            let identity = single.map(|agent| TabTitlePaneIdentity {
                pane_id: agent.pane_id.clone(),
                screen: ScreenIdentity::from_agent(agent),
            });
            let model_title = single.and_then(|agent| {
                let accepted = shared.accepted.get(&agent.pane_id)?;
                (accepted.identity == ScreenIdentity::from_agent(agent))
                    .then(|| accepted.values.title.clone())
                    .flatten()
            });
            candidates.push(TabTitleCandidate {
                observation: TabTitleObservation {
                    tab_id: tab.tab_id.clone(),
                    current_label: tab.label.clone().unwrap_or_default(),
                    model_title,
                    agent_count: tab_agents.len(),
                },
                identity,
            });
        }
        TabTitleBatch {
            generation: 0,
            ready: true,
            candidates,
            live_tab_ids,
        }
    }

    fn capabilities(&self) -> DeckCapabilities {
        let capability = match self.shared.read() {
            Ok(shared) => shared.capability.clone(),
            Err(poisoned) => poisoned.into_inner().capability.clone(),
        };
        runtime_capabilities_with_all_telemetry(
            &capability,
            &self.telemetry.snapshot(),
            &self.tab_titles.status(),
        )
    }

    fn feeds(&self, herdr_detail: Option<String>) -> AssemblyFeeds {
        telemetry_feeds(
            herdr_detail,
            &self.telemetry.snapshot(),
            self.capabilities(),
        )
    }
}

fn agent_has_generic_title(agent: &HerdrAgent) -> bool {
    let title = clean_title(agent.terminal_title_stripped.as_deref());
    let cwd_name = agent
        .cwd
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or(&agent.cwd);
    is_generic_title(&title, cwd_name)
}

#[async_trait]
trait RuntimeHeadingDiscovery: Send + Sync + 'static {
    async fn discover(&self) -> HeadingProviderSelection;
}

struct ProductionHeadingDiscovery {
    config: HeadingsConfig,
    calls: HeadingCallReporter,
}

#[async_trait]
impl RuntimeHeadingDiscovery for ProductionHeadingDiscovery {
    async fn discover(&self) -> HeadingProviderSelection {
        let selection = HeadingProviderSelection::discover(&self.config).await;
        HeadingProviderSelection {
            capability: selection.capability,
            provider: Box::new(TelemetryHeadingProvider {
                inner: selection.provider,
                calls: self.calls.clone(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeadingAttemptFailure {
    ProviderFailed,
    Timeout,
}

impl HeadingAttemptFailure {
    const fn capability(self) -> HeadingCapability {
        HeadingCapability::Error {
            backend: "ollama",
            reason: match self {
                Self::ProviderFailed => "provider-failed",
                Self::Timeout => "timeout",
            },
        }
    }
}

fn classify_heading_error(error: &HeadingProviderError) -> HeadingAttemptFailure {
    match error {
        HeadingProviderError::AcquireTimeout | HeadingProviderError::RequestTimeout => {
            HeadingAttemptFailure::Timeout
        }
        HeadingProviderError::ConnectionRefused
        | HeadingProviderError::Transport
        | HeadingProviderError::HttpStatus(_)
        | HeadingProviderError::ResponseBodyTooLarge
        | HeadingProviderError::MalformedResponse
        | HeadingProviderError::EmptyContent
        | HeadingProviderError::ThinkingOnly
        | HeadingProviderError::ContentTooLong
        | HeadingProviderError::TooLong
        | HeadingProviderError::Quality(_) => HeadingAttemptFailure::ProviderFailed,
    }
}

struct HeadingWorkerState {
    store: HeadingStore,
    identities: HashMap<String, (ScreenIdentity, String)>,
    next_identity: u64,
}

impl HeadingWorkerState {
    fn new() -> Self {
        Self {
            store: HeadingStore::default(),
            identities: HashMap::new(),
            next_identity: 0,
        }
    }

    fn synchronize(&mut self, observation: &HeadingObservation) -> HashMap<String, String> {
        self.identities
            .retain(|pane_id, _| observation.panes.contains_key(pane_id));
        for (pane_id, pane) in &observation.panes {
            let changed = self
                .identities
                .get(pane_id)
                .is_none_or(|(identity, _)| identity != &pane.identity);
            if changed {
                self.next_identity = self.next_identity.saturating_add(1);
                self.identities.insert(
                    pane_id.clone(),
                    (
                        pane.identity.clone(),
                        format!("{pane_id}#{}", self.next_identity),
                    ),
                );
            }
        }
        let keys = self
            .identities
            .values()
            .map(|(_, key)| key.clone())
            .collect::<HashSet<_>>();
        self.store.retain(&keys);
        self.identities
            .iter()
            .map(|(pane_id, (_, key))| (pane_id.clone(), key.clone()))
            .collect()
    }
}

enum HeadingWorkerExit {
    Cancelled,
    ObservationClosed,
}

#[derive(Clone, Copy, Default)]
struct HeadingBatchReport {
    attempted: bool,
    failure: Option<HeadingAttemptFailure>,
}

enum HeadingBatchResult {
    Completed(HeadingBatchReport),
    Obsolete(HeadingBatchReport),
    Cancelled,
    ObservationClosed,
}

enum HeadingGeneration {
    Completed(Result<Option<String>, HeadingProviderError>),
    Cancelled,
}

async fn run_heading_worker(
    discovery: Arc<dyn RuntimeHeadingDiscovery>,
    mut observations: watch::Receiver<HeadingObservation>,
    shared: Arc<RwLock<HeadingSharedState>>,
    health: RuntimeHealth,
    invalidations: mpsc::Sender<()>,
    cancellation: CancellationToken,
) {
    loop {
        let selection = tokio::select! {
            biased;
            () = cancellation.cancelled() => return,
            selection = discovery.discover() => selection,
        };
        let HeadingProviderSelection {
            capability,
            provider,
        } = selection;
        update_heading_capability(&shared, &health, &invalidations, capability.clone());
        if matches!(capability, HeadingCapability::Available { .. }) {
            let exit = run_available_heading_worker(
                provider,
                &mut observations,
                &shared,
                &health,
                &invalidations,
                &cancellation,
            )
            .await;
            match exit {
                HeadingWorkerExit::Cancelled | HeadingWorkerExit::ObservationClosed => return,
            }
        }
        if matches!(
            capability,
            HeadingCapability::Disabled { .. } | HeadingCapability::Unconfigured { .. }
        ) {
            loop {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return,
                    result = observations.changed() => {
                        if result.is_err() {
                            return;
                        }
                        observations.borrow_and_update();
                    }
                }
            }
        }
        let retry = tokio::time::sleep(HEADING_DISCOVERY_RETRY);
        tokio::pin!(retry);
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return,
                () = &mut retry => break,
                result = observations.changed() => {
                    if result.is_err() {
                        return;
                    }
                    observations.borrow_and_update();
                }
            }
        }
    }
}

async fn run_available_heading_worker(
    provider: Box<dyn HeadingProvider>,
    observations: &mut watch::Receiver<HeadingObservation>,
    shared: &Arc<RwLock<HeadingSharedState>>,
    health: &RuntimeHealth,
    invalidations: &mpsc::Sender<()>,
    cancellation: &CancellationToken,
) -> HeadingWorkerExit {
    let mut worker = HeadingWorkerState::new();
    let mut timer = interval_at(Instant::now(), HEADING_POLICY_TICK);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return HeadingWorkerExit::Cancelled,
            result = observations.changed() => {
                if result.is_err() {
                    return HeadingWorkerExit::ObservationClosed;
                }
            }
            _ = timer.tick() => {}
        }
        let observation = observations.borrow_and_update().clone();
        match process_heading_batch(
            provider.as_ref(),
            observations,
            &mut worker,
            &observation,
            cancellation,
        )
        .await
        {
            HeadingBatchResult::Completed(report) => {
                publish_heading_results(shared, health, invalidations, &worker, report);
            }
            HeadingBatchResult::Obsolete(report) => {
                update_heading_attempt_health(shared, health, invalidations, &report);
            }
            HeadingBatchResult::Cancelled => return HeadingWorkerExit::Cancelled,
            HeadingBatchResult::ObservationClosed => {
                return HeadingWorkerExit::ObservationClosed;
            }
        }
    }
}

async fn process_heading_batch(
    provider: &dyn HeadingProvider,
    observations: &watch::Receiver<HeadingObservation>,
    worker: &mut HeadingWorkerState,
    observation: &HeadingObservation,
    cancellation: &CancellationToken,
) -> HeadingBatchResult {
    let keys = worker.synchronize(observation);
    let mut proposed = worker.store.clone();
    let now_ms = monotonic_millis();
    let mut report = HeadingBatchReport::default();
    let mut pane_ids = observation.panes.keys().cloned().collect::<Vec<_>>();
    pane_ids.sort();

    for pane_id in pane_ids {
        let Some(pane) = observation.panes.get(&pane_id) else {
            continue;
        };
        let Some(key) = keys.get(&pane_id) else {
            continue;
        };
        if let Some(digest) = &pane.digest {
            let plan = proposed.plan_transcript(key, digest, now_ms);
            let current_title = proposed
                .accepted(key)
                .and_then(|accepted| accepted.title.clone());
            let mut title = None;
            let mut subtitle = None;
            let mut outcome = None;
            if plan.outcome {
                match generate_heading(
                    provider,
                    &outcome_job(digest),
                    current_title.as_deref(),
                    cancellation,
                )
                .await
                {
                    HeadingGeneration::Completed(result) => {
                        report.attempted = true;
                        match result {
                            Ok(value) => outcome = value,
                            Err(error) => report.failure = Some(classify_heading_error(&error)),
                        }
                    }
                    HeadingGeneration::Cancelled => return HeadingBatchResult::Cancelled,
                }
                proposed.complete_outcome(key, &plan, outcome.clone());
                if let Some(result) = heading_observation_state(observations, &report) {
                    worker.store = proposed;
                    return result;
                }
            }
            if plan.title {
                match generate_heading(
                    provider,
                    &title_job(digest),
                    current_title.as_deref(),
                    cancellation,
                )
                .await
                {
                    HeadingGeneration::Completed(result) => {
                        report.attempted = true;
                        match result {
                            Ok(value) => title = value,
                            Err(error) => report.failure = Some(classify_heading_error(&error)),
                        }
                    }
                    HeadingGeneration::Cancelled => return HeadingBatchResult::Cancelled,
                }
                proposed.complete_title(key, &plan, title.clone());
                if let Some(result) = heading_observation_state(observations, &report) {
                    worker.store = proposed;
                    return result;
                }
            }
            if plan.subtitle {
                let title_for_subtitle = title.as_deref().or(current_title.as_deref());
                match generate_heading(
                    provider,
                    &subtitle_job(digest, title_for_subtitle),
                    title_for_subtitle,
                    cancellation,
                )
                .await
                {
                    HeadingGeneration::Completed(result) => {
                        report.attempted = true;
                        match result {
                            Ok(value) => subtitle = value,
                            Err(error) => report.failure = Some(classify_heading_error(&error)),
                        }
                    }
                    HeadingGeneration::Cancelled => return HeadingBatchResult::Cancelled,
                }
                proposed.complete_subtitle(key, &plan, subtitle.clone());
                if let Some(result) = heading_observation_state(observations, &report) {
                    worker.store = proposed;
                    return result;
                }
            }
            proposed.complete_transcript(key, &plan, title, subtitle, outcome);
        }
        if let Some(screen) = &pane.screen {
            if proposed.plan_activity(key, &screen.key, now_ms) {
                let activity = match generate_heading(
                    provider,
                    &activity_job(&screen.content),
                    None,
                    cancellation,
                )
                .await
                {
                    HeadingGeneration::Completed(result) => {
                        report.attempted = true;
                        match result {
                            Ok(value) => value,
                            Err(error) => {
                                report.failure = Some(classify_heading_error(&error));
                                None
                            }
                        }
                    }
                    HeadingGeneration::Cancelled => return HeadingBatchResult::Cancelled,
                };
                proposed.complete_activity(key, activity.clone());
                if let Some(result) = heading_observation_state(observations, &report) {
                    worker.store = proposed;
                    return result;
                }
                proposed.complete_activity(key, activity);
            }
        }
    }
    worker.store = proposed;
    HeadingBatchResult::Completed(report)
}

fn heading_observation_state(
    observations: &watch::Receiver<HeadingObservation>,
    report: &HeadingBatchReport,
) -> Option<HeadingBatchResult> {
    match observations.has_changed() {
        Ok(true) => Some(HeadingBatchResult::Obsolete(*report)),
        Ok(false) => None,
        Err(_) => Some(HeadingBatchResult::ObservationClosed),
    }
}

async fn generate_heading(
    provider: &dyn HeadingProvider,
    job: &agentdeck_core::headings::HeadingJob,
    current_title: Option<&str>,
    cancellation: &CancellationToken,
) -> HeadingGeneration {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => HeadingGeneration::Cancelled,
        result = provider.generate(job, current_title) => HeadingGeneration::Completed(result),
    }
}

fn monotonic_millis() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn publish_heading_results(
    shared: &Arc<RwLock<HeadingSharedState>>,
    health: &RuntimeHealth,
    invalidations: &mpsc::Sender<()>,
    worker: &HeadingWorkerState,
    report: HeadingBatchReport,
) {
    let accepted = worker
        .identities
        .iter()
        .filter_map(|(pane_id, (identity, key))| {
            worker.store.accepted(key).map(|values| {
                (
                    pane_id.clone(),
                    AcceptedPaneHeadings {
                        identity: identity.clone(),
                        values: values.clone(),
                    },
                )
            })
        })
        .collect::<HashMap<_, _>>();
    let changed = update_heading_shared(shared, |state| {
        if state.accepted == accepted {
            false
        } else {
            state.accepted = accepted;
            true
        }
    });
    if changed {
        let _result = invalidations.try_send(());
    }
    update_heading_attempt_health(shared, health, invalidations, &report);
}

fn update_heading_attempt_health(
    shared: &Arc<RwLock<HeadingSharedState>>,
    health: &RuntimeHealth,
    invalidations: &mpsc::Sender<()>,
    report: &HeadingBatchReport,
) {
    if let Some(failure) = report.failure {
        update_heading_capability(shared, health, invalidations, failure.capability());
    } else if report.attempted {
        update_heading_capability(
            shared,
            health,
            invalidations,
            HeadingCapability::Available { backend: "ollama" },
        );
    }
}

fn update_heading_capability(
    shared: &Arc<RwLock<HeadingSharedState>>,
    health: &RuntimeHealth,
    invalidations: &mpsc::Sender<()>,
    capability: HeadingCapability,
) {
    let changed = update_heading_shared(shared, |state| {
        if state.capability == capability {
            false
        } else {
            state.capability = capability.clone();
            true
        }
    });
    health.heading_capability(&capability);
    if changed {
        let _result = invalidations.try_send(());
    }
}

fn update_heading_shared(
    shared: &Arc<RwLock<HeadingSharedState>>,
    update: impl FnOnce(&mut HeadingSharedState) -> bool,
) -> bool {
    match shared.write() {
        Ok(mut state) => update(&mut state),
        Err(poisoned) => update(&mut poisoned.into_inner()),
    }
}

impl StateOwner {
    async fn reconcile(&self) {
        match self.fetch_payload().await {
            Ok((payload, version)) => match self.states.publish(&payload) {
                Ok(_) => self.health.success(version, unix_seconds()),
                Err(_) => {
                    self.health.failure(RuntimeFailure::Internal);
                    let _result = self.states.publish(&degraded_payload_from_feeds(
                        RuntimeFailure::Internal.payload_code(),
                        self.headings.feeds(None),
                    ));
                }
            },
            Err(failure) => {
                self.health.failure(failure);
                let _result = self.states.publish(&degraded_payload_from_feeds(
                    failure.payload_code(),
                    self.headings.feeds(None),
                ));
            }
        }
    }

    async fn fetch_payload(
        &self,
    ) -> std::result::Result<(DeckPayload, Option<SafeVersion>), RuntimeFailure> {
        let observed = self.herdr.snapshot().await?;
        let raw = observed.snapshot;
        if observed.schema_protocol != raw.protocol || observed.client_version != raw.version {
            self.herdr.invalidate_diagnostics();
            return Err(RuntimeFailure::ProtocolMismatch);
        }
        let support = assess_protocol(raw.protocol);
        if !support.is_usable() {
            return Err(RuntimeFailure::ProtocolMismatch);
        }
        self.warn_for_future_protocol(support);
        let mut snapshot = normalize_snapshot(&raw).map_err(|_| RuntimeFailure::InvalidData)?;
        let version = SafeVersion::new(raw.version);
        let detail = version
            .as_ref()
            .map(|version| format!("Herdr {}", version.as_str()));
        let (transcript_observations, screen_output) = tokio::join!(
            self.read_transcripts(&snapshot),
            self.read_screens(&snapshot)
        );
        let mut enrichments = screen_output.enrichments;
        apply_transcripts(&mut snapshot, &transcript_observations, &mut enrichments);
        self.headings
            .submit(&snapshot, &transcript_observations, &screen_output.headings);
        self.headings.overlay(&snapshot, &mut enrichments);
        if self.tab_titles.is_active() {
            self.tab_titles
                .submit(self.headings.tab_title_batch(&snapshot));
        }
        let mut tracker = self.tracker.lock().map_err(|_| RuntimeFailure::Internal)?;
        let feeds = self.headings.feeds(detail);
        let payload =
            assemble_deck_enriched(&snapshot, &mut tracker, &SystemClock, feeds, &enrichments);
        Ok((payload, version))
    }

    async fn read_transcripts(
        &self,
        snapshot: &HerdrSnapshot,
    ) -> HashMap<String, TranscriptObservation> {
        let Some(source) = self.transcripts.clone() else {
            return HashMap::new();
        };
        let requests = snapshot
            .agents
            .iter()
            .filter_map(|agent| {
                let kind = transcript_kind(&agent.kind);
                supports_transcript_enrichment(kind, agent.session.as_ref()).then(|| {
                    (
                        agent.pane_id.clone(),
                        TranscriptRequest {
                            kind,
                            session: agent.session.clone(),
                            cwd: agent.cwd.clone(),
                        },
                    )
                })
            })
            .take(MAX_TRANSCRIPT_AGENTS)
            .collect::<Vec<_>>();
        let reads = stream::iter(requests.into_iter().map(|(pane_id, request)| {
            let source = source.clone();
            async move {
                timeout(TRANSCRIPT_PER_AGENT_TIMEOUT, source.observe(request))
                    .await
                    .ok()
                    .map(|observation| (pane_id, observation))
            }
        }))
        .buffer_unordered(TRANSCRIPT_CONCURRENCY);
        let mut reads = Box::pin(reads);
        let deadline = Instant::now() + TRANSCRIPT_TOTAL_TIMEOUT;
        let mut observations = HashMap::new();
        loop {
            match timeout_at(deadline, reads.next()).await {
                Ok(Some(Some((pane_id, observation)))) => {
                    observations.insert(pane_id, observation);
                }
                Ok(Some(None)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        observations
    }

    async fn read_screens(&self, snapshot: &HerdrSnapshot) -> ScreenReadOutput {
        let now_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let requests = self.prepare_screen_reads(snapshot, now_ms);
        let reads = stream::iter(requests.into_iter().map(|request| {
            let herdr = self.herdr.clone();
            async move {
                let result = timeout(
                    SCREEN_PER_AGENT_TIMEOUT,
                    herdr.read_visible(&request.pane_id, request.lines),
                )
                .await
                .ok()
                .and_then(std::result::Result::ok)
                .filter(|screen| screen.len() <= MAX_SCREEN_PARSE_BYTES);
                (request, result)
            }
        }))
        .buffer_unordered(SCREEN_CONCURRENCY);
        let mut reads = Box::pin(reads);
        let deadline = Instant::now() + SCREEN_TOTAL_TIMEOUT;
        let mut completed = Vec::new();
        while let Ok(Some(result)) = timeout_at(deadline, reads.next()).await {
            completed.push(result);
        }
        self.apply_screen_reads(snapshot, completed)
    }

    fn prepare_screen_reads(&self, snapshot: &HerdrSnapshot, now_ms: u64) -> Vec<ScreenRequest> {
        let live = snapshot
            .agents
            .iter()
            .map(|agent| agent.pane_id.clone())
            .collect::<HashSet<_>>();
        let mut state = match self.screens.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.schedule.retain(&live);
        state
            .observations
            .retain(|pane_id, _| live.contains(pane_id));

        let mut requests = Vec::new();
        for agent in snapshot.agents.iter().take(MAX_SCREEN_AGENTS) {
            let identity = ScreenIdentity::from_agent(agent);
            if state
                .observations
                .get(&agent.pane_id)
                .is_none_or(|(cached, _)| cached != &identity)
            {
                state.observations.insert(
                    agent.pane_id.clone(),
                    (identity.clone(), ScreenObservation::default()),
                );
            }
            let working = agent.agent_status == "working";
            if !working {
                if let Some((_, observation)) = state.observations.get_mut(&agent.pane_id) {
                    observation.phase = None;
                }
            }
            if state
                .schedule
                .admit(&agent.pane_id, &agent.kind, working, now_ms)
            {
                requests.push(ScreenRequest {
                    pane_id: agent.pane_id.clone(),
                    identity,
                    working,
                    lines: if working {
                        VisibleLines::Phase40
                    } else {
                        VisibleLines::Background16
                    },
                });
            }
        }
        requests
    }

    fn apply_screen_reads(
        &self,
        snapshot: &HerdrSnapshot,
        completed: Vec<(ScreenRequest, Option<String>)>,
    ) -> ScreenReadOutput {
        let mut state = match self.screens.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (request, screen) in completed {
            let Some(screen) = screen else {
                continue;
            };
            let Some((identity, observation)) = state.observations.get_mut(&request.pane_id) else {
                continue;
            };
            if identity != &request.identity {
                continue;
            }
            if request.working {
                observation.phase = parse_phase(&screen);
            }
            observation.background = summarize_background(&parse_background(&screen));
            observation.heading_screen = Some(HeadingScreen::new(screen));
        }

        let mut enrichments = AssemblyEnrichments::default();
        let mut headings = HashMap::new();
        for agent in &snapshot.agents {
            let identity = ScreenIdentity::from_agent(agent);
            let Some((cached_identity, observation)) = state.observations.get(&agent.pane_id)
            else {
                continue;
            };
            if cached_identity != &identity {
                continue;
            }
            if observation.phase.is_some() || observation.background.is_some() {
                enrichments.by_pane.insert(
                    agent.pane_id.clone(),
                    AgentEnrichment {
                        phase: observation.phase.clone(),
                        background: observation.background.clone(),
                        ..AgentEnrichment::default()
                    },
                );
            }
            if let Some(screen) = observation.heading_screen.clone() {
                headings.insert(agent.pane_id.clone(), screen);
            }
        }
        ScreenReadOutput {
            enrichments,
            headings,
        }
    }

    fn warn_for_future_protocol(&self, support: ProtocolSupport) {
        let ProtocolSupport::FutureUntested { protocol } = support else {
            return;
        };
        let should_warn = match self.future_protocol_warning.lock() {
            Ok(mut state) => state.should_warn(Instant::now()),
            Err(poisoned) => poisoned.into_inner().should_warn(Instant::now()),
        };
        if should_warn {
            eprintln!(
                "warning: Herdr protocol {protocol} is untested; the required snapshot subset decoded"
            );
        }
    }
}

fn transcript_kind(kind: &str) -> TranscriptKind {
    if kind.eq_ignore_ascii_case("claude") {
        TranscriptKind::Claude
    } else if kind.eq_ignore_ascii_case("pi") {
        TranscriptKind::Pi
    } else if kind.eq_ignore_ascii_case("codex") {
        TranscriptKind::Codex
    } else if kind.eq_ignore_ascii_case("copilot") {
        TranscriptKind::Copilot
    } else {
        TranscriptKind::Unknown
    }
}

fn supports_transcript_enrichment(
    kind: TranscriptKind,
    session: Option<&HerdrAgentSession>,
) -> bool {
    kind.supports_enrichment()
        && (kind != TranscriptKind::Copilot || session.and_then(copilot_relative_path).is_some())
}

fn apply_transcripts(
    snapshot: &mut HerdrSnapshot,
    observations: &HashMap<String, TranscriptObservation>,
    enrichments: &mut AssemblyEnrichments,
) {
    for agent in &mut snapshot.agents {
        let Some(observation) = observations.get(&agent.pane_id) else {
            continue;
        };
        agent.reply_key = observation.reply_key().map(ToOwned::to_owned);
        agent.transcript_written_at = observation.written_at;
        if let Some(context) = observation.context_usage() {
            enrichments
                .by_pane
                .entry(agent.pane_id.clone())
                .or_default()
                .context = Some(context.clone());
        }
    }
}

fn production_transcript_source() -> Option<Arc<dyn TranscriptSource>> {
    let roots = default_transcript_roots().ok()?;
    let source = FilesystemTranscriptSource::new(roots).ok()?;
    Some(Arc::new(source))
}

fn configured_transcript_source(enabled: bool) -> Option<Arc<dyn TranscriptSource>> {
    enabled.then(production_transcript_source).flatten()
}

#[cfg(test)]
fn model_off_feeds(herdr_detail: Option<String>) -> AssemblyFeeds {
    let telemetry = TelemetrySnapshot::disabled();
    telemetry_feeds(
        herdr_detail,
        &telemetry,
        runtime_capabilities(&HeadingCapability::Disabled { backend: "none" }),
    )
}

fn telemetry_feeds(
    herdr_detail: Option<String>,
    telemetry: &TelemetrySnapshot,
    capabilities: DeckCapabilities,
) -> AssemblyFeeds {
    AssemblyFeeds {
        herdr_detail,
        capacity: telemetry.capacity.feed.clone(),
        host: telemetry.host.feed.clone(),
        local_model: telemetry.local_model.snapshot.clone(),
        capabilities: Some(capabilities),
    }
}

fn initial_heading_capability(config: &HeadingsConfig) -> HeadingCapability {
    if config.backend == HeadingsBackend::None {
        return HeadingCapability::Disabled { backend: "none" };
    }
    let configured = [
        HeadingKind::Title,
        HeadingKind::Subtitle,
        HeadingKind::Outcome,
        HeadingKind::Activity,
    ]
    .into_iter()
    .any(|kind| config.model_for(kind).is_some());
    if !configured {
        return HeadingCapability::Unconfigured {
            backend: "none",
            setup_hint: HeadingSetupHint {
                message:
                    "Configure an installed Ollama model to generate contextual card headings."
                        .to_owned(),
                action_label: "Learn more".to_owned(),
                docs_path: "docs/setup.html#contextual-card-headings".to_owned(),
            },
        };
    }
    HeadingCapability::Error {
        backend: "ollama",
        reason: "not-refreshed",
    }
}

fn capability_backend(backend: &str) -> CapabilityBackend {
    if backend == "ollama" {
        CapabilityBackend::Ollama
    } else {
        CapabilityBackend::None
    }
}

fn health_backend(backend: &str) -> HealthBackend {
    if backend == "ollama" {
        HealthBackend::Ollama
    } else {
        HealthBackend::None
    }
}

fn setup_hint(hint: &HeadingSetupHint) -> SetupHint {
    SetupHint {
        message: hint.message.clone(),
        action_label: hint.action_label.clone(),
        docs_path: hint.docs_path.clone(),
        command: None,
    }
}

fn heading_capability_status(capability: &HeadingCapability) -> CapabilityStatus {
    match capability {
        HeadingCapability::Available { backend } => CapabilityStatus {
            state: CapabilityState::Available,
            backend: Some(capability_backend(backend)),
            level: None,
            reason: None,
            setup_hint: None,
        },
        HeadingCapability::Disabled { backend } => CapabilityStatus {
            state: CapabilityState::Disabled,
            backend: Some(capability_backend(backend)),
            level: None,
            reason: Some(CapabilityReason::ProviderDisabled),
            setup_hint: None,
        },
        HeadingCapability::MissingProvider {
            backend,
            setup_hint: hint,
        } => CapabilityStatus {
            state: CapabilityState::Missing,
            backend: Some(capability_backend(backend)),
            level: None,
            reason: Some(CapabilityReason::ProviderMissing),
            setup_hint: Some(setup_hint(hint)),
        },
        HeadingCapability::MissingModel {
            backend,
            setup_hint: hint,
            ..
        } => CapabilityStatus {
            state: CapabilityState::Missing,
            backend: Some(capability_backend(backend)),
            level: None,
            reason: Some(CapabilityReason::ModelMissing),
            setup_hint: Some(setup_hint(hint)),
        },
        HeadingCapability::Unconfigured {
            backend,
            setup_hint: hint,
        } => CapabilityStatus {
            state: CapabilityState::Missing,
            backend: Some(capability_backend(backend)),
            level: None,
            reason: Some(CapabilityReason::ModelUnconfigured),
            setup_hint: Some(setup_hint(hint)),
        },
        HeadingCapability::Error { backend, reason } => CapabilityStatus {
            state: CapabilityState::Error,
            backend: Some(capability_backend(backend)),
            level: None,
            reason: Some(match *reason {
                "timeout" => CapabilityReason::Timeout,
                "invalid-endpoint" => CapabilityReason::InvalidData,
                "not-refreshed" => CapabilityReason::NotRefreshed,
                _ => CapabilityReason::ProviderFailed,
            }),
            setup_hint: None,
        },
    }
}

fn heading_capability_health(capability: &HeadingCapability) -> CapabilityHealth {
    match capability {
        HeadingCapability::Available { backend } => CapabilityHealth {
            state: HealthState::Available,
            backend: Some(health_backend(backend)),
            reason: None,
        },
        HeadingCapability::Disabled { backend } => CapabilityHealth {
            state: HealthState::Disabled,
            backend: Some(health_backend(backend)),
            reason: Some(HealthReason::ProviderDisabled),
        },
        HeadingCapability::MissingProvider { backend, .. } => CapabilityHealth {
            state: HealthState::Missing,
            backend: Some(health_backend(backend)),
            reason: Some(HealthReason::ProviderMissing),
        },
        HeadingCapability::MissingModel { backend, .. } => CapabilityHealth {
            state: HealthState::Missing,
            backend: Some(health_backend(backend)),
            reason: Some(HealthReason::ModelMissing),
        },
        HeadingCapability::Unconfigured { backend, .. } => CapabilityHealth {
            state: HealthState::Missing,
            backend: Some(health_backend(backend)),
            reason: Some(HealthReason::ModelUnconfigured),
        },
        HeadingCapability::Error { backend, reason } => CapabilityHealth {
            state: HealthState::Error,
            backend: Some(health_backend(backend)),
            reason: Some(match *reason {
                "timeout" => HealthReason::Timeout,
                "invalid-endpoint" => HealthReason::InvalidData,
                "not-refreshed" => HealthReason::NotRefreshed,
                _ => HealthReason::ProviderFailed,
            }),
        },
    }
}

fn telemetry_capability_health(capability: &CapabilityStatus) -> CapabilityHealth {
    CapabilityHealth {
        state: match capability.state {
            CapabilityState::Available => HealthState::Available,
            CapabilityState::Missing => HealthState::Missing,
            CapabilityState::Disabled => HealthState::Disabled,
            CapabilityState::Unsupported => HealthState::Unsupported,
            CapabilityState::Error => HealthState::Error,
        },
        backend: capability.backend.map(telemetry_health_backend),
        reason: capability.reason.map(telemetry_health_reason),
    }
}

const fn telemetry_health_backend(backend: CapabilityBackend) -> HealthBackend {
    match backend {
        CapabilityBackend::Herdr => HealthBackend::Herdr,
        CapabilityBackend::None => HealthBackend::None,
        CapabilityBackend::Ollama => HealthBackend::Ollama,
        CapabilityBackend::Codexbar => HealthBackend::Codexbar,
        CapabilityBackend::Native => HealthBackend::Native,
        CapabilityBackend::System => HealthBackend::System,
    }
}

const fn telemetry_health_reason(reason: CapabilityReason) -> HealthReason {
    match reason {
        CapabilityReason::ProviderMissing => HealthReason::ProviderMissing,
        CapabilityReason::ModelMissing => HealthReason::ModelMissing,
        CapabilityReason::ModelUnconfigured => HealthReason::ModelUnconfigured,
        CapabilityReason::ProviderDisabled => HealthReason::ProviderDisabled,
        CapabilityReason::ProviderFailed => HealthReason::ProviderFailed,
        CapabilityReason::ConnectionFailed => HealthReason::ConnectionFailed,
        CapabilityReason::Timeout => HealthReason::Timeout,
        CapabilityReason::InvalidData => HealthReason::InvalidData,
        CapabilityReason::Unsupported => HealthReason::Unsupported,
        CapabilityReason::SamplerFailed => HealthReason::SamplerFailed,
        CapabilityReason::StateWriteFailed => HealthReason::StateWriteFailed,
        CapabilityReason::NotRefreshed => HealthReason::NotRefreshed,
    }
}

#[cfg(test)]
fn runtime_capabilities(headings: &HeadingCapability) -> DeckCapabilities {
    runtime_capabilities_with_all_telemetry(
        headings,
        &TelemetrySnapshot::disabled(),
        &tab_title_capability(
            CapabilityState::Disabled,
            Some(CapabilityReason::ProviderDisabled),
        ),
    )
}

#[cfg(test)]
fn runtime_capabilities_with_telemetry(
    headings: &HeadingCapability,
    telemetry: &TelemetrySnapshot,
) -> DeckCapabilities {
    runtime_capabilities_with_all_telemetry(
        headings,
        telemetry,
        &tab_title_capability(
            CapabilityState::Disabled,
            Some(CapabilityReason::ProviderDisabled),
        ),
    )
}

fn runtime_capabilities_with_all_telemetry(
    headings: &HeadingCapability,
    telemetry: &TelemetrySnapshot,
    tab_titles: &CapabilityStatus,
) -> DeckCapabilities {
    DeckCapabilities {
        headings: heading_capability_status(headings),
        capacity: telemetry.capacity.capability.clone(),
        host_telemetry: telemetry.host.capability.clone(),
        local_model_telemetry: telemetry.local_model.capability.clone(),
        tab_title_sync: tab_titles.clone(),
    }
}

#[cfg(test)]
fn degraded_payload(reason: &str) -> DeckPayload {
    let feeds = model_off_feeds(None);
    DeckPayload {
        herdr: FeedStatus {
            ok: false,
            detail: Some(reason.to_owned()),
        },
        workspaces: Vec::new(),
        agents: Vec::new(),
        capacity: feeds.capacity,
        host: feeds.host,
        local_model: None,
        capabilities: None,
    }
}

fn degraded_payload_from_feeds(reason: &str, feeds: AssemblyFeeds) -> DeckPayload {
    DeckPayload {
        herdr: FeedStatus {
            ok: false,
            detail: Some(reason.to_owned()),
        },
        workspaces: Vec::new(),
        agents: Vec::new(),
        capacity: feeds.capacity,
        host: feeds.host,
        local_model: feeds.local_model,
        capabilities: feeds.capabilities,
    }
}

async fn run_with_listener(
    config: &Config,
    listener: TcpListener,
    herdr: Arc<dyn RuntimeHerdr>,
    events: Arc<dyn RuntimeEvents>,
    cancellation: CancellationToken,
) -> Result<()> {
    let interval = humantime::parse_duration(&config.server.reconcile_interval)
        .context("validated reconcile interval could not be parsed")?;
    let mut options = HttpOptions::from_config(config)?;
    options.listen = listener
        .local_addr()
        .context("could not inspect HTTP listener")?;

    let telemetry_source: Arc<dyn RuntimeTelemetrySource> =
        Arc::new(ProductionTelemetrySource::new(config).await);
    let telemetry = RuntimeTelemetry::new(telemetry_source.initial());
    let (tab_titles, tab_title_input) =
        RuntimeTabTitles::from_binding(production_tab_title_binding(&config.tab_titles));
    let (headings, heading_observations, heading_shared) = RuntimeHeadings::new_with_telemetry(
        &config.headings,
        telemetry.clone(),
        tab_titles.capability.clone(),
    );
    let transcripts = configured_transcript_source(config.transcripts.enabled);
    let states = Arc::new(StateHub::new(&degraded_payload_from_feeds(
        "not_refreshed",
        headings.feeds(None),
    ))?);
    let health = RuntimeHealth::with_all_capabilities(
        &initial_heading_capability(&config.headings),
        &telemetry.snapshot(),
        &tab_titles.capability.status(),
    );
    let (invalidations, receiver) = mpsc::channel(INVALIDATION_CAPACITY);
    let actions: Arc<dyn HerdrActions> = Arc::new(RuntimeActions {
        herdr: Arc::clone(&herdr),
        invalidations: invalidations.clone(),
        health: health.clone(),
    });
    let http = HttpServer::build(options, states.clone(), actions, Arc::new(health.clone()))?;

    let server_cancel = cancellation.clone();
    let router = http.router();
    let server_task =
        tokio::spawn(async move { serve_http(listener, router, server_cancel).await });

    let event_cancel = cancellation.clone();
    let event_invalidations = invalidations.clone();
    let event_task = tokio::spawn(async move {
        events.run(event_invalidations, event_cancel).await;
    });

    let poll_cancel = cancellation.clone();
    let poll_invalidations = invalidations.clone();
    let poll_task = tokio::spawn(async move {
        run_poll(interval, poll_invalidations, poll_cancel).await;
    });

    let heading_cancel = cancellation.clone();
    let heading_invalidations = invalidations.clone();
    let heading_health = health.clone();
    let heading_discovery: Arc<dyn RuntimeHeadingDiscovery> =
        Arc::new(ProductionHeadingDiscovery {
            config: config.headings.clone(),
            calls: HeadingCallReporter {
                monitor: telemetry_source.heading_monitor(),
                telemetry: telemetry.clone(),
                invalidations: invalidations.clone(),
            },
        });
    let heading_task = tokio::spawn(async move {
        run_heading_worker(
            heading_discovery,
            heading_observations,
            heading_shared,
            heading_health,
            heading_invalidations,
            heading_cancel,
        )
        .await;
    });

    let telemetry_cancel = cancellation.clone();
    let telemetry_invalidations = invalidations.clone();
    let telemetry_health = health.clone();
    let telemetry_task = tokio::spawn(async move {
        run_telemetry_worker(
            telemetry_source,
            telemetry,
            telemetry_health,
            telemetry_invalidations,
            telemetry_cancel,
        )
        .await;
    });

    let tab_title_cancel = cancellation.clone();
    let tab_title_invalidations = invalidations.clone();
    let tab_title_health = health.clone();
    let tab_title_herdr = herdr.clone();
    let tab_title_capability = tab_titles.capability.clone();
    let tab_title_task = tokio::spawn(async move {
        run_tab_title_task(
            tab_title_input,
            tab_title_herdr,
            tab_title_capability,
            tab_title_health,
            tab_title_invalidations,
            tab_title_cancel,
        )
        .await;
    });

    let owner_cancel = cancellation.clone();
    let owner_task = tokio::spawn(async move {
        let owner = StateOwner {
            herdr,
            states,
            health,
            tracker: Arc::new(Mutex::new(ReadTracker::new())),
            future_protocol_warning: Arc::new(Mutex::new(FutureProtocolWarning::default())),
            transcripts,
            screens: Arc::new(Mutex::new(ScreenState::default())),
            started_at: Instant::now(),
            headings,
            tab_titles,
        };
        run_reconciliation_coalescer(receiver, owner_cancel, || {
            let owner = owner.clone();
            async move { owner.reconcile().await }
        })
        .await;
    });

    let _result = invalidations.try_send(());
    drop(invalidations);

    supervise(
        RuntimeTasks {
            server: server_task,
            owner: owner_task,
            events: event_task,
            poll: poll_task,
            headings: heading_task,
            telemetry: telemetry_task,
            tab_titles: tab_title_task,
        },
        cancellation,
        http,
    )
    .await
}

async fn run_poll(
    every: Duration,
    invalidations: mpsc::Sender<()>,
    cancellation: CancellationToken,
) {
    let mut timer = interval_at(Instant::now() + every, every);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return,
            _ = timer.tick() => match invalidations.try_send(()) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                Err(mpsc::error::TrySendError::Closed(())) => return,
            }
        }
    }
}

struct RuntimeTasks {
    server: JoinHandle<std::io::Result<()>>,
    owner: JoinHandle<()>,
    events: JoinHandle<()>,
    poll: JoinHandle<()>,
    headings: JoinHandle<()>,
    telemetry: JoinHandle<()>,
    tab_titles: JoinHandle<()>,
}

async fn supervise(
    tasks: RuntimeTasks,
    cancellation: CancellationToken,
    http: HttpServer,
) -> Result<()> {
    let RuntimeTasks {
        mut server,
        mut owner,
        mut events,
        mut poll,
        mut headings,
        mut telemetry,
        mut tab_titles,
    } = tasks;
    enum Exit {
        Cancelled,
        Server(std::result::Result<std::io::Result<()>, tokio::task::JoinError>),
        Owner(std::result::Result<(), tokio::task::JoinError>),
        Events(std::result::Result<(), tokio::task::JoinError>),
        Poll(std::result::Result<(), tokio::task::JoinError>),
        Headings(std::result::Result<(), tokio::task::JoinError>),
        Telemetry(std::result::Result<(), tokio::task::JoinError>),
        TabTitles(std::result::Result<(), tokio::task::JoinError>),
    }

    let exit = tokio::select! {
        biased;
        () = cancellation.cancelled() => Exit::Cancelled,
        result = &mut server => Exit::Server(result),
        result = &mut owner => Exit::Owner(result),
        result = &mut events => Exit::Events(result),
        result = &mut poll => Exit::Poll(result),
        result = &mut headings => Exit::Headings(result),
        result = &mut telemetry => Exit::Telemetry(result),
        result = &mut tab_titles => Exit::TabTitles(result),
    };
    cancellation.cancel();
    http.shutdown();

    let mut failures = Vec::new();
    let (
        server_consumed,
        owner_consumed,
        events_consumed,
        poll_consumed,
        headings_consumed,
        telemetry_consumed,
        tab_titles_consumed,
    ) = match exit {
        Exit::Cancelled => (false, false, false, false, false, false, false),
        Exit::Server(result) => {
            if let Some(failure) = classify_server_exit(result, true) {
                failures.push(failure);
            }
            (true, false, false, false, false, false, false)
        }
        Exit::Owner(result) => {
            if let Some(failure) = classify_unit_exit("state owner", result, true) {
                failures.push(failure);
            }
            (false, true, false, false, false, false, false)
        }
        Exit::Events(result) => {
            if let Some(failure) = classify_unit_exit("event subscription", result, true) {
                failures.push(failure);
            }
            (false, false, true, false, false, false, false)
        }
        Exit::Poll(result) => {
            if let Some(failure) = classify_unit_exit("poll", result, true) {
                failures.push(failure);
            }
            (false, false, false, true, false, false, false)
        }
        Exit::Headings(result) => {
            if let Some(failure) = classify_unit_exit("heading worker", result, true) {
                failures.push(failure);
            }
            (false, false, false, false, true, false, false)
        }
        Exit::Telemetry(result) => {
            if let Some(failure) = classify_unit_exit("telemetry", result, true) {
                failures.push(failure);
            }
            (false, false, false, false, false, true, false)
        }
        Exit::TabTitles(result) => {
            if let Some(failure) = classify_unit_exit("tab-title worker", result, true) {
                failures.push(failure);
            }
            (false, false, false, false, false, false, true)
        }
    };

    let deadline = Instant::now() + SHUTDOWN_DRAIN;
    let server_drain = async move {
        if server_consumed {
            drop(server);
            None
        } else {
            drain_server(server, deadline).await
        }
    };
    let owner_drain = async move {
        if owner_consumed {
            drop(owner);
            None
        } else {
            drain_unit("state owner", owner, deadline).await
        }
    };
    let event_drain = async move {
        if events_consumed {
            drop(events);
            None
        } else {
            drain_unit("event subscription", events, deadline).await
        }
    };
    let poll_drain = async move {
        if poll_consumed {
            drop(poll);
            None
        } else {
            drain_unit("poll", poll, deadline).await
        }
    };
    let heading_drain = async move {
        if headings_consumed {
            drop(headings);
            None
        } else {
            drain_unit("heading worker", headings, deadline).await
        }
    };
    let telemetry_drain = async move {
        if telemetry_consumed {
            drop(telemetry);
            None
        } else {
            drain_unit("telemetry", telemetry, deadline).await
        }
    };
    let tab_title_drain = async move {
        if tab_titles_consumed {
            drop(tab_titles);
            None
        } else {
            drain_unit("tab-title worker", tab_titles, deadline).await
        }
    };
    let (
        server_failure,
        owner_failure,
        event_failure,
        poll_failure,
        heading_failure,
        telemetry_failure,
        tab_title_failure,
    ) = tokio::join!(
        server_drain,
        owner_drain,
        event_drain,
        poll_drain,
        heading_drain,
        telemetry_drain,
        tab_title_drain
    );
    failures.extend(
        [
            server_failure,
            owner_failure,
            event_failure,
            poll_failure,
            heading_failure,
            telemetry_failure,
            tab_title_failure,
        ]
        .into_iter()
        .flatten(),
    );

    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!("runtime shutdown failed: {}", failures.join(", ")))
    }
}

fn classify_server_exit(
    result: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
    early: bool,
) -> Option<String> {
    match result {
        Ok(Ok(())) if early => Some("HTTP server stopped before shutdown".to_owned()),
        Ok(Ok(())) => None,
        Ok(Err(_)) | Err(_) => Some("HTTP server task failed".to_owned()),
    }
}

fn classify_unit_exit(
    name: &'static str,
    result: std::result::Result<(), tokio::task::JoinError>,
    early: bool,
) -> Option<String> {
    match result {
        Ok(()) if early => Some(format!("{name} stopped before shutdown")),
        Ok(()) => None,
        Err(_) => Some(format!("{name} task failed")),
    }
}

async fn drain_server(
    mut task: JoinHandle<std::io::Result<()>>,
    deadline: Instant,
) -> Option<String> {
    match timeout_at(deadline, &mut task).await {
        Ok(result) => classify_server_exit(result, false),
        Err(_) => {
            task.abort();
            let _result = task.await;
            Some("HTTP server task did not stop before the shutdown deadline".to_owned())
        }
    }
}

async fn drain_unit(
    name: &'static str,
    mut task: JoinHandle<()>,
    deadline: Instant,
) -> Option<String> {
    match timeout_at(deadline, &mut task).await {
        Ok(result) => classify_unit_exit(name, result, false),
        Err(_) => {
            task.abort();
            let _result = task.await;
            Some(format!(
                "{name} task did not stop before the shutdown deadline"
            ))
        }
    }
}

#[cfg(test)]
mod tests;
