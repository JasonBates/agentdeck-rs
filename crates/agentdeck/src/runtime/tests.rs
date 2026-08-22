use std::{
    collections::{HashMap, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use agentdeck_core::{
    CapabilityBackend, CapabilityLevel, CapabilityReason, CapabilityState, CapabilityStatus,
    CapacityFeed, CapacityProvider, ContextUsage, HostFeed, LocalModelSnapshot, LocalModelStatus,
    context::ContextOutcome,
    transcript::{TranscriptAnalysis, TranscriptDigest, TranscriptOutcome},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    adapters::{
        herdr::{AgentDto, AgentSessionDto, TabDto, WorkspaceDto},
        transcripts::TranscriptObservation,
    },
    http::{HealthPort, StatePort},
};

type ScreenResults = HashMap<String, VecDeque<std::result::Result<String, RuntimeFailure>>>;

#[derive(Clone, Copy)]
struct NoopActions;

#[async_trait]
impl HerdrActions for NoopActions {
    async fn focus_pane(&self, _pane_id: &str) -> Result<(), ActionError> {
        Ok(())
    }

    async fn focus_workspace(&self, _workspace_id: &str) -> Result<(), ActionError> {
        Ok(())
    }

    async fn create_tab(&self, _workspace_id: &str) -> Result<(), ActionError> {
        Ok(())
    }
}

fn test_http_server() -> HttpServer {
    let mut config = Config::default();
    config.server.listen = "127.0.0.1:0".to_owned();
    config.headings.backend = HeadingsBackend::None;
    let options = HttpOptions::from_config(&config)
        .unwrap_or_else(|error| panic!("test HTTP options: {error}"));
    let states = Arc::new(
        StateHub::new(&degraded_payload("not_refreshed"))
            .unwrap_or_else(|error| panic!("test state hub: {error}")),
    );
    HttpServer::build(
        options,
        states,
        Arc::new(NoopActions),
        Arc::new(RuntimeHealth::initial()),
    )
    .unwrap_or_else(|error| panic!("test HTTP server: {error}"))
}

#[test]
fn disabled_transcripts_construct_no_filesystem_source() {
    assert!(configured_transcript_source(false).is_none());
}

fn tab_title_task_waiting_for(cancellation: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        cancellation.cancelled().await;
    })
}

#[derive(Clone)]
struct FakeHerdr {
    snapshots: Arc<Mutex<VecDeque<std::result::Result<RuntimeSnapshot, RuntimeFailure>>>>,
    fallback: Arc<Mutex<std::result::Result<RuntimeSnapshot, RuntimeFailure>>>,
    snapshot_calls: Arc<AtomicUsize>,
    snapshot_delay: Arc<Mutex<Duration>>,
    diagnostic_invalidations: Arc<AtomicUsize>,
    action_result: Arc<Mutex<std::result::Result<(), RuntimeFailure>>>,
    action_calls: Arc<AtomicUsize>,
    rename_results: Arc<Mutex<VecDeque<std::result::Result<(), RuntimeFailure>>>>,
    rename_calls: Arc<Mutex<Vec<(String, String)>>>,
    rename_delay: Arc<Mutex<Duration>>,
    screen_results: Arc<Mutex<ScreenResults>>,
    screen_calls: Arc<Mutex<Vec<(String, VisibleLines)>>>,
    screen_delay: Arc<Mutex<Duration>>,
    active_screen_reads: Arc<AtomicUsize>,
    max_active_screen_reads: Arc<AtomicUsize>,
}

impl FakeHerdr {
    fn new(fallback: std::result::Result<SnapshotDto, RuntimeFailure>) -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(VecDeque::new())),
            fallback: Arc::new(Mutex::new(fallback.map(RuntimeSnapshot::matching))),
            snapshot_calls: Arc::new(AtomicUsize::new(0)),
            snapshot_delay: Arc::new(Mutex::new(Duration::ZERO)),
            diagnostic_invalidations: Arc::new(AtomicUsize::new(0)),
            action_result: Arc::new(Mutex::new(Ok(()))),
            action_calls: Arc::new(AtomicUsize::new(0)),
            rename_results: Arc::new(Mutex::new(VecDeque::new())),
            rename_calls: Arc::new(Mutex::new(Vec::new())),
            rename_delay: Arc::new(Mutex::new(Duration::ZERO)),
            screen_results: Arc::new(Mutex::new(HashMap::new())),
            screen_calls: Arc::new(Mutex::new(Vec::new())),
            screen_delay: Arc::new(Mutex::new(Duration::ZERO)),
            active_screen_reads: Arc::new(AtomicUsize::new(0)),
            max_active_screen_reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn push(&self, result: std::result::Result<SnapshotDto, RuntimeFailure>) {
        lock(&self.snapshots).push_back(result.map(RuntimeSnapshot::matching));
    }

    fn set_fallback(&self, result: std::result::Result<SnapshotDto, RuntimeFailure>) {
        *lock(&self.fallback) = result.map(RuntimeSnapshot::matching);
    }

    fn set_observed(&self, result: std::result::Result<RuntimeSnapshot, RuntimeFailure>) {
        *lock(&self.fallback) = result;
    }

    fn set_snapshot_delay(&self, delay: Duration) {
        *lock(&self.snapshot_delay) = delay;
    }

    fn fail_actions(&self, failure: RuntimeFailure) {
        *lock(&self.action_result) = Err(failure);
    }

    fn set_screen_results(
        &self,
        pane_id: &str,
        results: impl IntoIterator<Item = std::result::Result<String, RuntimeFailure>>,
    ) {
        lock(&self.screen_results).insert(pane_id.to_owned(), results.into_iter().collect());
    }

    fn set_screen_delay(&self, delay: Duration) {
        *lock(&self.screen_delay) = delay;
    }

    fn push_rename_result(&self, result: std::result::Result<(), RuntimeFailure>) {
        lock(&self.rename_results).push_back(result);
    }

    fn set_rename_delay(&self, delay: Duration) {
        *lock(&self.rename_delay) = delay;
    }
}

struct ActiveCount {
    active: Arc<AtomicUsize>,
}

impl ActiveCount {
    fn enter(active: Arc<AtomicUsize>, maximum: &AtomicUsize) -> Self {
        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(count, Ordering::SeqCst);
        Self { active }
    }
}

impl Drop for ActiveCount {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_heading_shared(
    shared: &RwLock<HeadingSharedState>,
) -> std::sync::RwLockReadGuard<'_, HeadingSharedState> {
    match shared.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[async_trait]
impl RuntimeHerdr for FakeHerdr {
    async fn snapshot(&self) -> std::result::Result<RuntimeSnapshot, RuntimeFailure> {
        self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
        let delay = *lock(&self.snapshot_delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let queued = lock(&self.snapshots).pop_front();
        queued.unwrap_or_else(|| lock(&self.fallback).clone())
    }

    fn invalidate_diagnostics(&self) {
        self.diagnostic_invalidations.fetch_add(1, Ordering::SeqCst);
    }

    async fn read_visible(
        &self,
        pane_id: &str,
        lines: VisibleLines,
    ) -> std::result::Result<String, RuntimeFailure> {
        lock(&self.screen_calls).push((pane_id.to_owned(), lines));
        let _active = ActiveCount::enter(
            self.active_screen_reads.clone(),
            &self.max_active_screen_reads,
        );
        let delay = *lock(&self.screen_delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        lock(&self.screen_results)
            .get_mut(pane_id)
            .and_then(VecDeque::pop_front)
            .unwrap_or(Err(RuntimeFailure::Unavailable))
    }

    async fn focus_pane(&self, _pane_id: &str) -> std::result::Result<(), RuntimeFailure> {
        self.action_calls.fetch_add(1, Ordering::SeqCst);
        *lock(&self.action_result)
    }

    async fn focus_workspace(
        &self,
        _workspace_id: &str,
    ) -> std::result::Result<(), RuntimeFailure> {
        self.action_calls.fetch_add(1, Ordering::SeqCst);
        *lock(&self.action_result)
    }

    async fn create_tab(&self, _workspace_id: &str) -> std::result::Result<(), RuntimeFailure> {
        self.action_calls.fetch_add(1, Ordering::SeqCst);
        *lock(&self.action_result)
    }

    async fn rename_tab(
        &self,
        tab_id: &str,
        title: &str,
    ) -> std::result::Result<(), RuntimeFailure> {
        lock(&self.rename_calls).push((tab_id.to_owned(), title.to_owned()));
        let delay = *lock(&self.rename_delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        lock(&self.rename_results).pop_front().unwrap_or(Ok(()))
    }
}

#[derive(Clone)]
struct FakeTranscriptSource {
    observations: Arc<Mutex<HashMap<String, VecDeque<TranscriptObservation>>>>,
    calls: Arc<Mutex<Vec<TranscriptRequest>>>,
    delay: Arc<Mutex<Duration>>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

impl FakeTranscriptSource {
    fn new() -> Self {
        Self {
            observations: Arc::new(Mutex::new(HashMap::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
            delay: Arc::new(Mutex::new(Duration::ZERO)),
            active: Arc::new(AtomicUsize::new(0)),
            maximum: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn set(&self, cwd: &str, observations: impl IntoIterator<Item = TranscriptObservation>) {
        lock(&self.observations).insert(cwd.to_owned(), observations.into_iter().collect());
    }

    fn set_delay(&self, delay: Duration) {
        *lock(&self.delay) = delay;
    }
}

#[async_trait]
impl TranscriptSource for FakeTranscriptSource {
    async fn observe(&self, request: TranscriptRequest) -> TranscriptObservation {
        lock(&self.calls).push(request.clone());
        let _active = ActiveCount::enter(self.active.clone(), &self.maximum);
        let delay = *lock(&self.delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        lock(&self.observations)
            .get_mut(&request.cwd)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(TranscriptObservation::unavailable)
    }
}

type HeadingResult = Result<Option<String>, HeadingProviderError>;

#[derive(Clone)]
struct FakeHeadingProvider {
    calls: Arc<Mutex<Vec<(HeadingKind, String)>>>,
    responses: Arc<Mutex<VecDeque<HeadingResult>>>,
    delay: Arc<Mutex<Duration>>,
    pending: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

impl FakeHeadingProvider {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(VecDeque::new())),
            delay: Arc::new(Mutex::new(Duration::ZERO)),
            pending: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicUsize::new(0)),
            maximum: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn set_delay(&self, delay: Duration) {
        *lock(&self.delay) = delay;
    }

    fn set_pending(&self, pending: bool) {
        self.pending.store(pending, Ordering::SeqCst);
    }

    fn push(&self, response: HeadingResult) {
        lock(&self.responses).push_back(response);
    }

    fn count(&self, kind: HeadingKind) -> usize {
        lock(&self.calls)
            .iter()
            .filter(|(called, _)| *called == kind)
            .count()
    }
}

#[async_trait]
impl HeadingProvider for FakeHeadingProvider {
    async fn generate(
        &self,
        job: &agentdeck_core::headings::HeadingJob,
        _current_title: Option<&str>,
    ) -> HeadingResult {
        lock(&self.calls).push((job.kind, job.prompt.clone()));
        let _active = ActiveCount::enter(self.active.clone(), &self.maximum);
        if self.pending.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let delay = *lock(&self.delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if let Some(response) = lock(&self.responses).pop_front() {
            return response;
        }
        Ok(Some(
            match job.kind {
                HeadingKind::Title => "Build portable heading runtime",
                HeadingKind::Subtitle => "Test latest heading observations",
                HeadingKind::Outcome => "The portable heading runtime is connected and tested",
                HeadingKind::Activity => "Running heading worker tests",
            }
            .to_owned(),
        ))
    }
}

struct PanickingHeadingProvider;

#[async_trait]
impl HeadingProvider for PanickingHeadingProvider {
    async fn generate(
        &self,
        _job: &agentdeck_core::headings::HeadingJob,
        _current_title: Option<&str>,
    ) -> std::result::Result<Option<String>, HeadingProviderError> {
        panic!("synthetic heading provider panic");
    }
}

struct FakeHeadingDiscovery {
    capability: Arc<Mutex<HeadingCapability>>,
    provider: FakeHeadingProvider,
    calls: Arc<AtomicUsize>,
}

impl FakeHeadingDiscovery {
    fn new(capability: HeadingCapability, provider: FakeHeadingProvider) -> Self {
        Self {
            capability: Arc::new(Mutex::new(capability)),
            provider,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl RuntimeHeadingDiscovery for FakeHeadingDiscovery {
    async fn discover(&self) -> HeadingProviderSelection {
        self.calls.fetch_add(1, Ordering::SeqCst);
        HeadingProviderSelection {
            capability: lock(&self.capability).clone(),
            provider: Box::new(self.provider.clone()),
        }
    }
}

#[derive(Clone)]
struct FakeTelemetrySource {
    initial: TelemetrySnapshot,
    capacity: Arc<Mutex<VecDeque<Option<CapacityOutcome>>>>,
    host: Arc<Mutex<VecDeque<Option<HostOutcome>>>>,
    local_model: Arc<Mutex<VecDeque<Option<LocalModelOutcome>>>>,
    capacity_calls: Arc<AtomicUsize>,
    host_calls: Arc<AtomicUsize>,
    local_calls: Arc<AtomicUsize>,
    capacity_pending: Arc<AtomicBool>,
    local_pending: Arc<AtomicBool>,
    capacity_dropped: Arc<AtomicBool>,
    local_dropped: Arc<AtomicBool>,
}

impl FakeTelemetrySource {
    fn new(initial: TelemetrySnapshot) -> Self {
        Self {
            initial,
            capacity: Arc::new(Mutex::new(VecDeque::new())),
            host: Arc::new(Mutex::new(VecDeque::new())),
            local_model: Arc::new(Mutex::new(VecDeque::new())),
            capacity_calls: Arc::new(AtomicUsize::new(0)),
            host_calls: Arc::new(AtomicUsize::new(0)),
            local_calls: Arc::new(AtomicUsize::new(0)),
            capacity_pending: Arc::new(AtomicBool::new(false)),
            local_pending: Arc::new(AtomicBool::new(false)),
            capacity_dropped: Arc::new(AtomicBool::new(false)),
            local_dropped: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl RuntimeTelemetrySource for FakeTelemetrySource {
    fn initial(&self) -> TelemetrySnapshot {
        self.initial.clone()
    }

    async fn refresh_capacity(&self) -> Option<CapacityOutcome> {
        self.capacity_calls.fetch_add(1, Ordering::SeqCst);
        if self.capacity_pending.load(Ordering::SeqCst) {
            let _drop = DropFlag(self.capacity_dropped.clone());
            std::future::pending::<()>().await;
        }
        lock(&self.capacity).pop_front().flatten()
    }

    fn sample_host(&self) -> Option<HostOutcome> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.host).pop_front().flatten()
    }

    async fn sample_local_model(&self) -> Option<LocalModelOutcome> {
        self.local_calls.fetch_add(1, Ordering::SeqCst);
        if self.local_pending.load(Ordering::SeqCst) {
            let _drop = DropFlag(self.local_dropped.clone());
            std::future::pending::<()>().await;
        }
        lock(&self.local_model).pop_front().flatten()
    }
}

fn telemetry_capability(
    state: CapabilityState,
    backend: CapabilityBackend,
    reason: Option<CapabilityReason>,
) -> CapabilityStatus {
    CapabilityStatus {
        state,
        backend: Some(backend),
        level: None,
        reason,
        setup_hint: None,
    }
}

fn capacity_outcome(state: CapabilityState, reason: Option<CapabilityReason>) -> CapacityOutcome {
    CapacityOutcome {
        capability: telemetry_capability(state, CapabilityBackend::Codexbar, reason),
        feed: CapacityFeed {
            ok: state == CapabilityState::Available,
            reason: reason.map(|reason| format!("{reason:?}").to_ascii_lowercase()),
            providers: vec![CapacityProvider {
                name: "Claude".to_owned(),
                percent_used: Some(25.0),
                label: "25% used".to_owned(),
                windows: Vec::new(),
                note: None,
            }],
        },
        provider_collected_at: BTreeMap::from([("Claude".to_owned(), 10)]),
        collected_at: (state == CapabilityState::Available).then_some(10),
    }
}

fn host_outcome(load: f64) -> HostOutcome {
    HostOutcome {
        capability: CapabilityStatus {
            level: Some(CapabilityLevel::Basic),
            ..telemetry_capability(CapabilityState::Available, CapabilityBackend::System, None)
        },
        feed: HostFeed {
            ok: true,
            load1: load,
            load5: load,
            cores: 8,
            system: None,
        },
        basic: None,
    }
}

fn local_outcome(state: CapabilityState, reason: Option<CapabilityReason>) -> LocalModelOutcome {
    LocalModelOutcome {
        capability: telemetry_capability(state, CapabilityBackend::Ollama, reason),
        snapshot: Some(LocalModelSnapshot {
            name: "FIXTURE".to_owned(),
            status: if state == CapabilityState::Available {
                LocalModelStatus::Ready
            } else {
                LocalModelStatus::Offline
            },
            resident_gb: (state == CapabilityState::Available).then_some(2.0),
            context: 4096,
            calls: Vec::new(),
        }),
    }
}

struct TestEvents {
    trigger: Arc<Notify>,
    disconnected: bool,
}

impl TestEvents {
    fn connected() -> (Self, Arc<Notify>) {
        let trigger = Arc::new(Notify::new());
        (
            Self {
                trigger: trigger.clone(),
                disconnected: false,
            },
            trigger,
        )
    }

    fn disconnected() -> Self {
        Self {
            trigger: Arc::new(Notify::new()),
            disconnected: true,
        }
    }
}

#[async_trait]
impl RuntimeEvents for TestEvents {
    async fn run(&self, invalidations: mpsc::Sender<()>, cancellation: CancellationToken) {
        if self.disconnected {
            cancellation.cancelled().await;
            return;
        }
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return,
                () = self.trigger.notified() => {
                    let _result = invalidations.try_send(());
                }
            }
        }
    }
}

struct TestRuntime {
    address: SocketAddr,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl TestRuntime {
    async fn stop(self) {
        self.cancellation.cancel();
        self.task
            .await
            .unwrap_or_else(|error| panic!("runtime join: {error}"))
            .unwrap_or_else(|error| panic!("runtime stop: {error}"));
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }
}

async fn spawn_runtime(
    herdr: Arc<dyn RuntimeHerdr>,
    events: Arc<TestEvents>,
    poll: Duration,
) -> TestRuntime {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap_or_else(|error| panic!("bind test listener: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("test listener address: {error}"));
    let mut config = Config::default();
    config.server.listen = address.to_string();
    config.server.reconcile_interval = humantime::format_duration(poll).to_string();
    config.headings.backend = HeadingsBackend::None;
    config.capacity.backend = CapacityBackend::Off;
    config.telemetry.host = HostTelemetryMode::Off;
    config.telemetry.local_model = crate::config::LocalModelTelemetryMode::Off;
    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let task = tokio::spawn(async move {
        run_with_listener(&config, listener, herdr, events, task_cancel).await
    });
    let runtime = TestRuntime {
        address,
        cancellation,
        task,
    };
    wait_for_server(&runtime).await;
    runtime
}

async fn wait_for_server(runtime: &TestRuntime) {
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client.get(runtime.url("/api/health")).send().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("runtime listener did not become ready");
}

async fn get_json(runtime: &TestRuntime, path: &str) -> Value {
    reqwest::get(runtime.url(path))
        .await
        .unwrap_or_else(|error| panic!("GET {path}: {error}"))
        .json()
        .await
        .unwrap_or_else(|error| panic!("decode {path}: {error}"))
}

async fn wait_for(runtime: &TestRuntime, path: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..150 {
        let value = get_json(runtime, path).await;
        if predicate(&value) {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached for {path}");
}

fn snapshot(label: &str) -> SnapshotDto {
    SnapshotDto {
        version: "0.8.2".to_owned(),
        protocol: 20,
        agents: Vec::new(),
        workspaces: vec![WorkspaceDto {
            workspace_id: format!("w-{label}"),
            label: label.to_owned(),
            number: 1,
            agent_status: "idle".to_owned(),
            focused: true,
            pane_count: 0,
            tab_count: 0,
            active_tab_id: String::new(),
            worktree: None,
        }],
        tabs: Vec::new(),
        focused_pane_id: None,
        focused_tab_id: None,
        focused_workspace_id: Some(format!("w-{label}")),
        panes: Vec::new(),
        layouts: Vec::new(),
    }
}

fn snapshot_with_agents(label: &str, agents: Vec<AgentDto>) -> SnapshotDto {
    let mut snapshot = snapshot(label);
    snapshot.workspaces[0].pane_count = agents.len();
    snapshot.agents = agents;
    snapshot
}

fn agent(index: usize, kind: &str, status: &str, cwd: &str) -> AgentDto {
    AgentDto {
        terminal_id: format!("terminal-{index}"),
        agent: Some(kind.to_owned()),
        agent_session: Some(AgentSessionDto {
            source: "herdr".to_owned(),
            agent: kind.to_owned(),
            kind: "session".to_owned(),
            value: format!("session-{index}"),
        }),
        agent_status: status.to_owned(),
        cwd: Some(cwd.to_owned()),
        focused: false,
        pane_id: format!("w1:p{index}"),
        tab_id: "t1".to_owned(),
        workspace_id: format!("w-{label}", label = "agents"),
        terminal_title_stripped: Some(format!("Agent {index}")),
        state_change_seq: Some(u64::try_from(index).unwrap_or(u64::MAX)),
        revision: u64::try_from(index).unwrap_or(u64::MAX),
    }
}

fn copilot_agent(index: usize, status: &str, cwd: &str) -> AgentDto {
    let mut agent = agent(index, "copilot", status, cwd);
    agent.agent_session = Some(AgentSessionDto {
        source: "herdr".to_owned(),
        agent: "copilot".to_owned(),
        kind: "id".to_owned(),
        value: format!("copilot-session-{index}"),
    });
    agent
}

fn single_tab_snapshot(label: &str) -> SnapshotDto {
    let mut snapshot = snapshot_with_agents("agents", vec![agent(1, "claude", "working", "/cwd")]);
    snapshot.workspaces[0].tab_count = 1;
    snapshot.workspaces[0].active_tab_id = "t1".to_owned();
    snapshot.tabs = vec![TabDto {
        tab_id: "t1".to_owned(),
        workspace_id: "w-agents".to_owned(),
        label: label.to_owned(),
        number: 1,
        agent_status: "working".to_owned(),
        focused: true,
        pane_count: 1,
    }];
    snapshot
}

fn single_tab_batch(snapshot: &SnapshotDto, model_title: &str) -> TabTitleBatch {
    let normalized = normalize_snapshot(snapshot)
        .unwrap_or_else(|error| panic!("single-tab fixture must normalize: {error}"));
    let agent = normalized
        .agents
        .first()
        .unwrap_or_else(|| panic!("single-tab fixture has an agent"));
    TabTitleBatch {
        generation: 1,
        ready: true,
        candidates: vec![TabTitleCandidate {
            observation: TabTitleObservation {
                tab_id: "t1".to_owned(),
                current_label: snapshot.tabs[0].label.clone(),
                model_title: Some(model_title.to_owned()),
                agent_count: 1,
            },
            identity: Some(TabTitlePaneIdentity {
                pane_id: agent.pane_id.clone(),
                screen: ScreenIdentity::from_agent(agent),
            }),
        }],
        live_tab_ids: Some(vec!["t1".to_owned()]),
    }
}

#[cfg(unix)]
fn temporary_tab_title_store() -> (tempfile::TempDir, TabTitleStore) {
    use std::os::unix::fs::PermissionsExt as _;

    let directory =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tab-title temp dir: {error}"));
    let root = directory
        .path()
        .canonicalize()
        .unwrap_or_else(|error| panic!("canonical tab-title temp dir: {error}"));
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("private tab-title temp dir: {error}"));
    (
        directory,
        TabTitleStore::new(root.join(TAB_TITLE_STATE_FILE)),
    )
}

fn ready_observation(reply_key: &str, written_at: i64, used: i64) -> TranscriptObservation {
    TranscriptObservation {
        analysis: TranscriptOutcome::Ready(Box::new(TranscriptAnalysis {
            opening: None,
            digest: Some(TranscriptDigest {
                opening: String::new(),
                requests: String::new(),
                recent: String::new(),
                last_prompt: "private prompt".to_owned(),
                last_prompt_key: Some("prompt-key".to_owned()),
                last_reply: "private reply".to_owned(),
                last_reply_key: Some(reply_key.to_owned()),
                written_at,
            }),
            malformed_lines: 0,
            decoded_records: 2,
        })),
        context: ContextOutcome::Ready(ContextUsage {
            used,
            limit: 200_000,
            percent: used.saturating_mul(100) / 200_000,
            model: Some("fixture-model".to_owned()),
        }),
        written_at: Some(written_at),
    }
}

fn malformed_observation() -> TranscriptObservation {
    TranscriptObservation {
        analysis: TranscriptOutcome::Malformed,
        context: ContextOutcome::Malformed,
        written_at: None,
    }
}

fn heading_observation(
    pane_id: &str,
    cwd: &str,
    prompt_key: &str,
    reply_key: Option<&str>,
    screen: Option<&str>,
) -> HeadingObservation {
    let digest = TranscriptDigest {
        opening: format!("opening-{prompt_key}"),
        requests: format!("requests-{prompt_key}"),
        recent: format!("recent-{prompt_key}"),
        last_prompt: format!("prompt-{prompt_key}"),
        last_prompt_key: Some(prompt_key.to_owned()),
        last_reply: reply_key.map_or_else(String::new, |key| format!("reply-{key}")),
        last_reply_key: reply_key.map(ToOwned::to_owned),
        written_at: 1,
    };
    HeadingObservation {
        panes: HashMap::from([(
            pane_id.to_owned(),
            HeadingPaneObservation {
                identity: ScreenIdentity {
                    kind: "claude".to_owned(),
                    cwd: cwd.to_owned(),
                    session: None,
                },
                digest: Some(digest),
                screen: screen.map(|value| HeadingScreen::new(value.to_owned())),
            },
        )]),
    }
}

async fn wait_for_heading_calls(provider: &FakeHeadingProvider, minimum: usize) {
    for _ in 0..200 {
        if lock(&provider.calls).len() >= minimum {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("heading provider did not reach {minimum} calls");
}

async fn wait_for_rename_calls(herdr: &FakeHerdr, minimum: usize) {
    for _ in 0..200 {
        if lock(&herdr.rename_calls).len() >= minimum {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("rename worker did not reach {minimum} calls");
}

struct TestHeadingWorker {
    runtime: RuntimeHeadings,
    health: RuntimeHealth,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
    _invalidations: mpsc::Receiver<()>,
}

impl TestHeadingWorker {
    fn spawn(config: &HeadingsConfig, discovery: Arc<dyn RuntimeHeadingDiscovery>) -> Self {
        let (runtime, observations, shared) = RuntimeHeadings::new(config);
        let health = RuntimeHealth::with_heading_capability(&initial_heading_capability(config));
        let (invalidations, receiver) = mpsc::channel(INVALIDATION_CAPACITY);
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let task_health = health.clone();
        let task = tokio::spawn(async move {
            run_heading_worker(
                discovery,
                observations,
                shared,
                task_health,
                invalidations,
                task_cancel,
            )
            .await;
        });
        Self {
            runtime,
            health,
            cancellation,
            task,
            _invalidations: receiver,
        }
    }

    async fn stop(self) {
        self.cancellation.cancel();
        self.task
            .await
            .unwrap_or_else(|error| panic!("heading worker join: {error}"));
    }
}

fn owner_for(
    herdr: Arc<dyn RuntimeHerdr>,
    transcripts: Option<Arc<dyn TranscriptSource>>,
) -> (StateOwner, Arc<StateHub>, RuntimeHealth) {
    let states = Arc::new(
        StateHub::new(&degraded_payload("not_refreshed"))
            .unwrap_or_else(|error| panic!("state hub: {error}")),
    );
    let health = RuntimeHealth::initial();
    let headings_config = HeadingsConfig {
        backend: HeadingsBackend::None,
        ..HeadingsConfig::default()
    };
    let (headings, _observations, _shared) = RuntimeHeadings::new(&headings_config);
    (
        StateOwner {
            herdr,
            states: states.clone(),
            health: health.clone(),
            tracker: Arc::new(Mutex::new(ReadTracker::new())),
            future_protocol_warning: Arc::new(Mutex::new(FutureProtocolWarning::default())),
            transcripts,
            screens: Arc::new(Mutex::new(ScreenState::default())),
            started_at: Instant::now(),
            headings,
            tab_titles: RuntimeTabTitles::inactive(),
        },
        states,
        health,
    )
}

fn current_json(states: &StateHub) -> Value {
    serde_json::from_slice(&states.current())
        .unwrap_or_else(|error| panic!("current payload JSON: {error}"))
}

#[test]
fn normal_runtime_always_reports_ui_safe_optional_capabilities() {
    let config = Config::default();
    let capabilities = runtime_capabilities(&initial_heading_capability(&config.headings));
    assert_eq!(capabilities.headings.state, CapabilityState::Missing);
    assert_eq!(
        capabilities.headings.reason,
        Some(CapabilityReason::ModelUnconfigured)
    );
    let docs_path = capabilities
        .headings
        .setup_hint
        .as_ref()
        .map(|hint| hint.docs_path.as_str());
    assert_eq!(docs_path, Some("docs/setup.html#contextual-card-headings"));

    let generated: Config = toml::from_str(crate::config_init::MINIMAL_CONFIG_TOML)
        .unwrap_or_else(|error| panic!("generated config must parse: {error}"));
    assert_eq!(generated.headings.backend, HeadingsBackend::Auto);
    let capabilities = runtime_capabilities(&initial_heading_capability(&generated.headings));
    assert_eq!(capabilities.headings.state, CapabilityState::Missing);
    assert_eq!(capabilities.capacity.state, CapabilityState::Disabled);
    assert_eq!(capabilities.host_telemetry.state, CapabilityState::Disabled);
    assert_eq!(
        capabilities.local_model_telemetry.state,
        CapabilityState::Disabled
    );
    assert_eq!(capabilities.tab_title_sync.state, CapabilityState::Disabled);
}

#[test]
fn tab_title_selection_is_fail_closed_without_path_work_when_disabled_or_unsupported() {
    let disabled = crate::config::TabTitlesConfig { enabled: false };
    let calls = Arc::new(AtomicUsize::new(0));
    let disabled_calls = calls.clone();
    let binding = tab_title_binding_with(&disabled, true, move || {
        disabled_calls.fetch_add(1, Ordering::SeqCst);
        Err(())
    });
    assert!(matches!(binding, TabTitleBinding::Disabled));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let enabled = crate::config::TabTitlesConfig { enabled: true };
    let unsupported_calls = calls.clone();
    let binding = tab_title_binding_with(&enabled, false, move || {
        unsupported_calls.fetch_add(1, Ordering::SeqCst);
        Err(())
    });
    assert!(matches!(binding, TabTitleBinding::Unsupported));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn tab_title_binding_does_not_load_or_create_state_until_the_worker_runs() {
    let config = crate::config::TabTitlesConfig { enabled: true };
    let binding = tab_title_binding_with(&config, true, || {
        Ok(PathBuf::from("/fixture/state/tab-titles.json"))
    });
    let (runtime, input) = RuntimeTabTitles::from_binding(binding);
    assert_eq!(runtime.capability.status().state, CapabilityState::Error);
    assert!(runtime.observations.is_some());
    assert!(matches!(input, TabTitleTaskInput::Active { .. }));
}

#[test]
fn heading_tab_title_batch_rejects_duplicate_tabs_and_never_invents_a_model_title() {
    let config = HeadingsConfig {
        backend: HeadingsBackend::None,
        ..HeadingsConfig::default()
    };
    let (headings, _, _) = RuntimeHeadings::new(&config);
    let mut raw = single_tab_snapshot("1");
    let snapshot = normalize_snapshot(&raw)
        .unwrap_or_else(|error| panic!("single tab snapshot must normalize: {error}"));
    let batch = headings.tab_title_batch(&snapshot);
    assert_eq!(batch.candidates.len(), 1);
    assert_eq!(batch.candidates[0].observation.agent_count, 1);
    assert_eq!(batch.candidates[0].observation.model_title, None);
    assert_eq!(batch.live_tab_ids, Some(vec!["t1".to_owned()]));

    raw.tabs.push(raw.tabs[0].clone());
    let snapshot = normalize_snapshot(&raw)
        .unwrap_or_else(|error| panic!("duplicate tab fixture must normalize: {error}"));
    let batch = headings.tab_title_batch(&snapshot);
    assert!(batch.candidates.is_empty());
    assert_eq!(batch.live_tab_ids, Some(vec!["t1".to_owned()]));
}

#[cfg(unix)]
#[tokio::test]
async fn tab_title_rename_uses_a_fresh_matching_snapshot_and_persists_only_after_success() {
    let (_directory, store) = temporary_tab_title_store();
    let raw = single_tab_snapshot("");
    let fake = Arc::new(FakeHerdr::new(Ok(raw.clone())));
    let batch = single_tab_batch(&raw, "Generated title");
    let (_sender, mut observations) = watch::channel(batch.clone());
    let cancellation = CancellationToken::new();
    let mut ownership = TabTitleOwnership::default();

    let result = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut observations,
        &mut ownership,
        &batch,
        &cancellation,
    )
    .await;

    assert!(matches!(result, Ok(TabTitleBatchResult::Completed)));
    assert_eq!(
        lock(&fake.rename_calls).as_slice(),
        [("t1".to_owned(), "Generated title".to_owned())]
    );
    assert_eq!(
        store
            .load()
            .unwrap_or_else(|error| panic!("saved ownership: {error}"))
            .managed()
            .get("t1")
            .map(String::as_str),
        Some("Generated title")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tab_title_fresh_mismatch_or_manual_label_never_renames_and_releases_ownership() {
    let (_directory, store) = temporary_tab_title_store();
    let raw = single_tab_snapshot("Manual title");
    let fake = Arc::new(FakeHerdr::new(Ok(raw.clone())));
    let mut batch = single_tab_batch(&raw, "Generated title");
    batch.candidates[0].observation.current_label = "Manual title".to_owned();
    let (_sender, mut observations) = watch::channel(batch.clone());
    let cancellation = CancellationToken::new();
    let mut ownership = TabTitleOwnership::from_managed(BTreeMap::from([(
        "t1".to_owned(),
        "Generated title".to_owned(),
    )]));

    let result = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut observations,
        &mut ownership,
        &batch,
        &cancellation,
    )
    .await;
    assert!(matches!(result, Ok(TabTitleBatchResult::Completed)));
    assert!(lock(&fake.rename_calls).is_empty());
    assert!(ownership.managed().is_empty());
    assert!(!ownership.is_dirty());

    let raw = single_tab_snapshot("");
    let batch = single_tab_batch(&raw, "Generated title");
    let mut reused_pane = raw.clone();
    reused_pane.agents[0].pane_id = "w1:p-reused".to_owned();
    fake.set_fallback(Ok(reused_pane));
    let (_sender, mut observations) = watch::channel(batch.clone());
    let mut ownership = TabTitleOwnership::default();
    let result = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut observations,
        &mut ownership,
        &batch,
        &cancellation,
    )
    .await;
    assert!(matches!(result, Ok(TabTitleBatchResult::Completed)));
    assert!(lock(&fake.rename_calls).is_empty());

    let mismatch = RuntimeSnapshot {
        snapshot: single_tab_snapshot(""),
        client_version: "different-client".to_owned(),
        schema_protocol: 20,
    };
    fake.set_observed(Ok(mismatch));
    let raw = single_tab_snapshot("");
    let batch = single_tab_batch(&raw, "Generated title");
    let (_sender, mut observations) = watch::channel(batch.clone());
    let mut ownership = TabTitleOwnership::default();
    let result = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut observations,
        &mut ownership,
        &batch,
        &cancellation,
    )
    .await;
    assert!(matches!(
        result,
        Err(TabTitleWorkerError::Runtime(
            RuntimeFailure::ProtocolMismatch
        ))
    ));
    assert!(lock(&fake.rename_calls).is_empty());
    assert_eq!(fake.diagnostic_invalidations.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn tab_title_worker_retries_only_after_a_later_observation_and_recovers_capability() {
    let (_directory, store) = temporary_tab_title_store();
    let raw = single_tab_snapshot("");
    let fake = Arc::new(FakeHerdr::new(Ok(raw.clone())));
    fake.push_rename_result(Err(RuntimeFailure::Unavailable));
    fake.push_rename_result(Ok(()));
    let (sender, receiver) = watch::channel(TabTitleBatch::default());
    let capability = RuntimeTabTitleCapability::new(tab_title_capability(
        CapabilityState::Error,
        Some(CapabilityReason::NotRefreshed),
    ));
    let health = RuntimeHealth::initial();
    let (invalidations, _receiver) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(run_tab_title_worker(
        store.clone(),
        fake.clone(),
        receiver,
        capability.clone(),
        health.clone(),
        invalidations,
        cancellation.clone(),
    ));

    sender.send_replace(single_tab_batch(&raw, "Generated title"));
    wait_for_rename_calls(&fake, 1).await;
    assert_eq!(capability.status().state, CapabilityState::Error);
    assert!(matches!(store.load(), Err(TabTitleStoreError::Missing)));

    let mut retry = single_tab_batch(&raw, "Generated title");
    retry.generation = 2;
    sender.send_replace(retry);
    wait_for_rename_calls(&fake, 2).await;
    cancellation.cancel();
    assert!(matches!(
        task.await
            .unwrap_or_else(|error| panic!("tab-title worker join: {error}")),
        TabTitleWorkerExit::Cancelled
    ));
    assert_eq!(capability.status().state, CapabilityState::Available);
    assert_eq!(health.report().status, HealthStatus::Degraded);
    assert_eq!(
        store
            .load()
            .unwrap_or_else(|error| panic!("saved retry ownership: {error}"))
            .managed()
            .get("t1")
            .map(String::as_str),
        Some("Generated title")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tab_title_worker_cancels_a_slow_rename_without_holding_reconciliation_resources() {
    let (_directory, store) = temporary_tab_title_store();
    let raw = single_tab_snapshot("");
    let fake = Arc::new(FakeHerdr::new(Ok(raw.clone())));
    fake.set_rename_delay(Duration::from_secs(30));
    let (sender, receiver) = watch::channel(TabTitleBatch::default());
    let capability = RuntimeTabTitleCapability::new(tab_title_capability(
        CapabilityState::Error,
        Some(CapabilityReason::NotRefreshed),
    ));
    let (invalidations, _receiver) = mpsc::channel(1);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(run_tab_title_worker(
        store,
        fake.clone(),
        receiver,
        capability,
        RuntimeHealth::initial(),
        invalidations,
        cancellation.clone(),
    ));

    sender.send_replace(single_tab_batch(&raw, "Generated title"));
    wait_for_rename_calls(&fake, 1).await;
    cancellation.cancel();
    let exit = tokio::time::timeout(Duration::from_millis(100), task)
        .await
        .unwrap_or_else(|_| panic!("slow title rename ignored cancellation"))
        .unwrap_or_else(|error| panic!("tab-title worker join: {error}"));
    assert!(matches!(exit, TabTitleWorkerExit::Cancelled));
}

#[cfg(unix)]
#[tokio::test]
async fn tab_title_newer_batch_during_fresh_snapshot_makes_the_old_intent_obsolete() {
    let (_directory, store) = temporary_tab_title_store();
    let raw = single_tab_snapshot("");
    let fake = Arc::new(FakeHerdr::new(Ok(raw.clone())));
    fake.set_snapshot_delay(Duration::from_millis(30));
    let batch = single_tab_batch(&raw, "Old title");
    let (sender, mut observations) = watch::channel(batch.clone());
    let cancellation = CancellationToken::new();
    let mut ownership = TabTitleOwnership::default();
    let send_newest = async {
        while fake.snapshot_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let mut newest = single_tab_batch(&raw, "New title");
        newest.generation = 2;
        sender.send_replace(newest);
    };
    let process_old = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut observations,
        &mut ownership,
        &batch,
        &cancellation,
    );
    let (result, ()) = tokio::join!(process_old, send_newest);

    assert!(matches!(result, Ok(TabTitleBatchResult::Obsolete)));
    assert!(lock(&fake.rename_calls).is_empty());
    assert!(ownership.managed().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn tab_title_already_superseded_batch_cannot_release_prune_or_recover_ownership() {
    let (_directory, store) = temporary_tab_title_store();
    let fake = Arc::new(FakeHerdr::new(Ok(single_tab_snapshot(""))));
    let cancellation = CancellationToken::new();

    let manual_raw = single_tab_snapshot("Manual label");
    let manual_batch = single_tab_batch(&manual_raw, "Generated title");
    let mut manual_ownership = TabTitleOwnership::from_managed(BTreeMap::from([(
        "t1".to_owned(),
        "Generated title".to_owned(),
    )]));
    let (manual_sender, mut manual_observations) = watch::channel(manual_batch.clone());
    let mut newest = manual_batch.clone();
    newest.generation = 2;
    manual_sender.send_replace(newest);
    let manual = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut manual_observations,
        &mut manual_ownership,
        &manual_batch,
        &cancellation,
    )
    .await;
    assert!(matches!(manual, Ok(TabTitleBatchResult::Obsolete)));
    assert_eq!(
        manual_ownership.managed().get("t1").map(String::as_str),
        Some("Generated title")
    );
    assert!(!manual_ownership.is_dirty());

    let prune_raw = single_tab_snapshot("");
    let prune_batch = single_tab_batch(&prune_raw, "Generated title");
    let mut prune_ownership = TabTitleOwnership::from_managed(BTreeMap::from([(
        "dead-tab".to_owned(),
        "Old title".to_owned(),
    )]));
    let (prune_sender, mut prune_observations) = watch::channel(prune_batch.clone());
    let mut newest = prune_batch.clone();
    newest.generation = 2;
    prune_sender.send_replace(newest);
    let prune = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut prune_observations,
        &mut prune_ownership,
        &prune_batch,
        &cancellation,
    )
    .await;
    assert!(matches!(prune, Ok(TabTitleBatchResult::Obsolete)));
    assert_eq!(
        prune_ownership
            .managed()
            .get("dead-tab")
            .map(String::as_str),
        Some("Old title")
    );
    assert!(!prune_ownership.is_dirty());

    let recovered_raw = single_tab_snapshot("Recovered title");
    let recovered_batch = single_tab_batch(&recovered_raw, "Recovered title");
    let mut recovered_ownership = TabTitleOwnership::default();
    let (recovered_sender, mut recovered_observations) = watch::channel(recovered_batch.clone());
    let mut newest = recovered_batch.clone();
    newest.generation = 2;
    recovered_sender.send_replace(newest);
    let recovered = process_tab_title_batch(
        &store,
        fake.as_ref(),
        &mut recovered_observations,
        &mut recovered_ownership,
        &recovered_batch,
        &cancellation,
    )
    .await;
    assert!(matches!(recovered, Ok(TabTitleBatchResult::Obsolete)));
    assert!(recovered_ownership.managed().is_empty());
    assert!(!recovered_ownership.is_dirty());

    assert!(matches!(store.load(), Err(TabTitleStoreError::Missing)));
    assert_eq!(fake.snapshot_calls.load(Ordering::SeqCst), 0);
    assert!(lock(&fake.rename_calls).is_empty());
}

#[test]
fn typed_telemetry_feeds_and_all_capabilities_are_merged_without_fabrication() {
    let mut snapshot = TelemetrySnapshot::disabled();
    snapshot.capacity = capacity_outcome(CapabilityState::Available, None);
    snapshot.host = host_outcome(1.5);
    snapshot.local_model = local_outcome(CapabilityState::Available, None);
    let capabilities = runtime_capabilities_with_telemetry(
        &HeadingCapability::Disabled { backend: "none" },
        &snapshot,
    );
    let feeds = telemetry_feeds(None, &snapshot, capabilities.clone());

    assert!(feeds.capacity.ok);
    assert_eq!(feeds.host.load1, 1.5);
    assert!(feeds.host.system.is_none());
    assert_eq!(
        feeds.local_model.as_ref().map(|model| model.status),
        Some(LocalModelStatus::Ready)
    );
    assert_eq!(capabilities.capacity.state, CapabilityState::Available);
    assert_eq!(
        capabilities.host_telemetry.level,
        Some(CapabilityLevel::Basic)
    );
    assert_eq!(
        capabilities.local_model_telemetry.state,
        CapabilityState::Available
    );
    assert_eq!(capabilities.tab_title_sync.state, CapabilityState::Disabled);
}

#[tokio::test]
async fn production_basic_host_starts_with_an_honest_delta_warmup() {
    let mut config = Config::default();
    config.headings.backend = HeadingsBackend::None;
    config.capacity.backend = CapacityBackend::Off;
    config.telemetry.host = HostTelemetryMode::Basic;
    config.telemetry.local_model = crate::config::LocalModelTelemetryMode::Off;
    let source = ProductionTelemetrySource::new(&config).await;
    let initial = source.initial();
    let basic = initial
        .host
        .basic
        .as_ref()
        .unwrap_or_else(|| panic!("basic host snapshot must be present"));

    assert_eq!(initial.host.capability.state, CapabilityState::Available);
    assert_eq!(initial.host.capability.level, Some(CapabilityLevel::Basic));
    assert!(basic.cpu_busy.is_none());
    assert!(initial.host.feed.system.is_none());
    assert!(source.sample_host().is_some());
}

#[test]
fn optional_health_only_degrades_for_error_and_recovers_with_last_success() {
    let mut snapshot = TelemetrySnapshot::disabled();
    snapshot.capacity.capability = telemetry_capability(
        CapabilityState::Missing,
        CapabilityBackend::Codexbar,
        Some(CapabilityReason::ProviderMissing),
    );
    snapshot.host.capability = telemetry_capability(
        CapabilityState::Unsupported,
        CapabilityBackend::Native,
        Some(CapabilityReason::Unsupported),
    );
    let health = RuntimeHealth::with_capabilities(
        &HeadingCapability::Disabled { backend: "none" },
        &snapshot,
    );
    health.success(SafeVersion::new("0.8.2"), 10);
    assert_eq!(health.report().status, HealthStatus::Ok);

    let failed = capacity_outcome(
        CapabilityState::Error,
        Some(CapabilityReason::ProviderFailed),
    );
    health.telemetry_capability(
        CapabilityName::Capacity,
        AdapterName::Capacity,
        &failed.capability,
    );
    let report = health.report();
    assert_eq!(report.status, HealthStatus::Degraded);
    assert_eq!(report.degraded_reasons, vec![HealthReason::ProviderFailed]);

    let recovered = capacity_outcome(CapabilityState::Available, None);
    health.telemetry_capability(
        CapabilityName::Capacity,
        AdapterName::Capacity,
        &recovered.capability,
    );
    let report = health.report();
    assert_eq!(report.status, HealthStatus::Ok);
    assert!(
        report.adapters[&AdapterName::Capacity]
            .last_success_unix_seconds
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn telemetry_schedules_capacity_at_one_second_and_host_local_every_five() {
    let source = Arc::new(FakeTelemetrySource::new(TelemetrySnapshot::disabled()));
    lock(&source.capacity).push_back(Some(capacity_outcome(CapabilityState::Available, None)));
    lock(&source.host).push_back(Some(host_outcome(2.0)));
    lock(&source.local_model).push_back(Some(local_outcome(CapabilityState::Available, None)));
    let telemetry = RuntimeTelemetry::new(source.initial());
    let health = RuntimeHealth::with_capabilities(
        &HeadingCapability::Disabled { backend: "none" },
        &source.initial(),
    );
    let (invalidations, mut receiver) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(run_telemetry_worker(
        source.clone(),
        telemetry.clone(),
        health,
        invalidations,
        cancellation.clone(),
    ));

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(999)).await;
    tokio::task::yield_now().await;
    assert_eq!(source.capacity_calls.load(Ordering::SeqCst), 0);
    assert_eq!(source.host_calls.load(Ordering::SeqCst), 0);
    assert_eq!(source.local_calls.load(Ordering::SeqCst), 0);

    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(source.capacity_calls.load(Ordering::SeqCst), 1);
    assert!(receiver.try_recv().is_ok());

    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert_eq!(source.host_calls.load(Ordering::SeqCst), 1);
    assert_eq!(source.local_calls.load(Ordering::SeqCst), 1);
    assert_eq!(telemetry.snapshot().host.feed.load1, 2.0);

    tokio::time::advance(Duration::from_secs(296)).await;
    tokio::task::yield_now().await;
    assert_eq!(source.capacity_calls.load(Ordering::SeqCst), 2);

    cancellation.cancel();
    task.await
        .unwrap_or_else(|error| panic!("telemetry worker join: {error}"));
}

#[tokio::test(start_paused = true)]
async fn telemetry_publishes_only_latest_changes_and_local_errors_recover() {
    let source = Arc::new(FakeTelemetrySource::new(TelemetrySnapshot::disabled()));
    lock(&source.host).extend([
        Some(host_outcome(1.0)),
        Some(host_outcome(1.0)),
        Some(host_outcome(2.0)),
    ]);
    lock(&source.local_model).extend([
        Some(local_outcome(
            CapabilityState::Error,
            Some(CapabilityReason::Timeout),
        )),
        Some(local_outcome(CapabilityState::Available, None)),
    ]);
    let telemetry = RuntimeTelemetry::new(source.initial());
    let health = RuntimeHealth::with_capabilities(
        &HeadingCapability::Disabled { backend: "none" },
        &source.initial(),
    );
    health.success(SafeVersion::new("0.8.2"), 1);
    let (invalidations, mut receiver) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(run_telemetry_worker(
        source,
        telemetry.clone(),
        health.clone(),
        invalidations,
        cancellation.clone(),
    ));

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(health.report().status, HealthStatus::Degraded);
    while receiver.try_recv().is_ok() {}

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(health.report().status, HealthStatus::Ok);
    assert_eq!(telemetry.snapshot().host.feed.load1, 1.0);
    while receiver.try_recv().is_ok() {}

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(telemetry.snapshot().host.feed.load1, 2.0);
    assert!(receiver.try_recv().is_ok());

    cancellation.cancel();
    task.await
        .unwrap_or_else(|error| panic!("telemetry worker join: {error}"));
}

#[tokio::test(start_paused = true)]
async fn stalled_local_sample_does_not_block_other_schedules_and_cancels_cleanly() {
    let source = Arc::new(FakeTelemetrySource::new(TelemetrySnapshot::disabled()));
    source.local_pending.store(true, Ordering::SeqCst);
    lock(&source.host).push_back(Some(host_outcome(3.0)));
    let telemetry = RuntimeTelemetry::new(source.initial());
    let health = RuntimeHealth::initial();
    let (invalidations, _receiver) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(run_telemetry_worker(
        source.clone(),
        telemetry.clone(),
        health,
        invalidations,
        cancellation.clone(),
    ));

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert_eq!(source.local_calls.load(Ordering::SeqCst), 1);
    assert_eq!(source.host_calls.load(Ordering::SeqCst), 1);
    assert_eq!(telemetry.snapshot().host.feed.load1, 3.0);

    cancellation.cancel();
    task.await
        .unwrap_or_else(|error| panic!("telemetry worker join: {error}"));
    assert!(source.local_dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn heading_call_lease_tracks_busy_ring_and_cancellation_without_content() {
    let monitor = LocalModelMonitor::new(
        url::Url::parse("http://127.0.0.1:11434/")
            .unwrap_or_else(|error| panic!("fixture endpoint: {error}")),
        "fixture-model:latest".to_owned(),
        4096,
    )
    .unwrap_or_else(|error| panic!("fixture monitor: {error}"));
    let telemetry = RuntimeTelemetry::new(TelemetrySnapshot {
        local_model: monitor.initial_outcome(),
        ..TelemetrySnapshot::disabled()
    });
    let (invalidations, _receiver) = mpsc::channel(1);
    let reporter = HeadingCallReporter {
        monitor: Some(monitor.clone()),
        telemetry: telemetry.clone(),
        invalidations,
    };

    let lease = reporter.begin();
    assert_eq!(monitor.snapshot().status, LocalModelStatus::Busy);
    drop(lease);
    assert_eq!(monitor.snapshot().status, LocalModelStatus::Offline);
    assert_eq!(
        monitor.snapshot().calls.last().map(|call| call.ok),
        Some(false)
    );

    for index in 0..130 {
        let lease = reporter.begin();
        lease.finish(index % 2 == 0);
    }
    assert_eq!(monitor.snapshot().calls.len(), 128);

    let provider = FakeHeadingProvider::new();
    provider.set_delay(Duration::from_secs(30));
    let wrapped = Arc::new(TelemetryHeadingProvider {
        inner: Box::new(provider),
        calls: reporter,
    });
    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let task = tokio::spawn(async move {
        generate_heading(
            wrapped.as_ref(),
            &agentdeck_core::headings::HeadingJob {
                kind: HeadingKind::Title,
                prompt: "private prompt must not enter telemetry".to_owned(),
            },
            None,
            &task_cancel,
        )
        .await
    });
    for _ in 0..100 {
        if monitor.snapshot().status == LocalModelStatus::Busy {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(monitor.snapshot().status, LocalModelStatus::Busy);
    cancellation.cancel();
    let generation = task
        .await
        .unwrap_or_else(|error| panic!("heading generation join: {error}"));
    assert!(matches!(generation, HeadingGeneration::Cancelled));
    assert_eq!(monitor.snapshot().status, LocalModelStatus::Offline);
    let encoded = serde_json::to_string(&telemetry.snapshot().local_model.snapshot)
        .unwrap_or_else(|error| panic!("local telemetry JSON: {error}"));
    assert!(!encoded.contains("private prompt"));

    let (invalidations, _receiver) = mpsc::channel(1);
    let panicking = Arc::new(TelemetryHeadingProvider {
        inner: Box::new(PanickingHeadingProvider),
        calls: HeadingCallReporter {
            monitor: Some(monitor.clone()),
            telemetry,
            invalidations,
        },
    });
    let panic_task = tokio::spawn(async move {
        panicking
            .generate(
                &agentdeck_core::headings::HeadingJob {
                    kind: HeadingKind::Title,
                    prompt: "fixture".to_owned(),
                },
                None,
            )
            .await
    });
    assert!(panic_task.await.is_err_and(|error| error.is_panic()));
    assert_eq!(monitor.snapshot().status, LocalModelStatus::Offline);
    assert_eq!(
        monitor.snapshot().calls.last().map(|call| call.ok),
        Some(false)
    );
}

#[test]
fn every_heading_discovery_outcome_maps_to_closed_payload_and_health_codes() {
    let hint = HeadingSetupHint {
        message: "Install the configured local provider.".to_owned(),
        action_label: "Learn more".to_owned(),
        docs_path: "docs/setup.html#contextual-card-headings".to_owned(),
    };
    let cases = [
        (
            HeadingCapability::Available { backend: "ollama" },
            CapabilityState::Available,
            HealthState::Available,
            None,
            None,
        ),
        (
            HeadingCapability::Disabled { backend: "none" },
            CapabilityState::Disabled,
            HealthState::Disabled,
            Some(CapabilityReason::ProviderDisabled),
            Some(HealthReason::ProviderDisabled),
        ),
        (
            HeadingCapability::MissingProvider {
                backend: "ollama",
                setup_hint: hint.clone(),
            },
            CapabilityState::Missing,
            HealthState::Missing,
            Some(CapabilityReason::ProviderMissing),
            Some(HealthReason::ProviderMissing),
        ),
        (
            HeadingCapability::MissingModel {
                backend: "ollama",
                model: "fixture-model".to_owned(),
                setup_hint: hint.clone(),
            },
            CapabilityState::Missing,
            HealthState::Missing,
            Some(CapabilityReason::ModelMissing),
            Some(HealthReason::ModelMissing),
        ),
        (
            HeadingCapability::Unconfigured {
                backend: "none",
                setup_hint: hint,
            },
            CapabilityState::Missing,
            HealthState::Missing,
            Some(CapabilityReason::ModelUnconfigured),
            Some(HealthReason::ModelUnconfigured),
        ),
        (
            HeadingCapability::Error {
                backend: "ollama",
                reason: "discovery-failed",
            },
            CapabilityState::Error,
            HealthState::Error,
            Some(CapabilityReason::ProviderFailed),
            Some(HealthReason::ProviderFailed),
        ),
    ];

    for (capability, payload_state, health_state, reason, health_reason) in cases {
        let payload = heading_capability_status(&capability);
        let health = heading_capability_health(&capability);
        assert_eq!(payload.state, payload_state);
        assert_eq!(payload.reason, reason);
        assert_eq!(health.state, health_state);
        assert_eq!(health.reason, health_reason);
        assert_eq!(
            payload.setup_hint.is_some(),
            matches!(
                capability,
                HeadingCapability::MissingProvider { .. }
                    | HeadingCapability::MissingModel { .. }
                    | HeadingCapability::Unconfigured { .. }
            )
        );
    }
}

#[test]
fn validated_copilot_enters_heading_inputs_while_unknown_stays_excluded() {
    let normalized = normalize_snapshot(&snapshot_with_agents(
        "agents",
        vec![
            agent(1, "copilot", "idle", "/copilot"),
            agent(2, "future-agent", "idle", "/future"),
        ],
    ))
    .unwrap_or_else(|error| panic!("unsupported heading snapshot: {error}"));
    let observations = HashMap::from([
        (
            "w1:p1".to_owned(),
            ready_observation("private-copilot-reply", 1, 1),
        ),
        (
            "w1:p2".to_owned(),
            ready_observation("private-future-reply", 1, 1),
        ),
    ]);
    let config = HeadingsConfig::default();
    let (runtime, mut receiver, _shared) = RuntimeHeadings::new(&config);
    runtime.submit(&normalized, &observations, &HashMap::new());
    let latest = receiver.borrow_and_update().clone();
    assert!(latest.panes["w1:p1"].digest.is_some());
    assert!(latest.panes["w1:p2"].digest.is_none());
}

#[tokio::test]
async fn configured_heading_worker_generates_from_a_validated_copilot_digest() {
    let provider = FakeHeadingProvider::new();
    let discovery = Arc::new(FakeHeadingDiscovery::new(
        HeadingCapability::Available { backend: "ollama" },
        provider.clone(),
    ));
    let config = HeadingsConfig {
        backend: HeadingsBackend::Ollama,
        model: Some("fixture-model".to_owned()),
        ..HeadingsConfig::default()
    };
    let worker = TestHeadingWorker::spawn(&config, discovery.clone());
    let normalized = normalize_snapshot(&snapshot_with_agents(
        "agents",
        vec![copilot_agent(1, "idle", "/copilot")],
    ))
    .unwrap_or_else(|error| panic!("Copilot heading snapshot: {error}"));
    let observations = HashMap::from([(
        "w1:p1".to_owned(),
        ready_observation("copilot-private-reply", 1, 1),
    )]);

    for _ in 0..100 {
        if discovery.calls.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(discovery.calls.load(Ordering::SeqCst), 1);
    worker
        .runtime
        .submit(&normalized, &observations, &HashMap::new());
    wait_for_heading_calls(&provider, 3).await;

    assert_eq!(provider.count(HeadingKind::Title), 1);
    assert_eq!(provider.count(HeadingKind::Subtitle), 1);
    assert_eq!(provider.count(HeadingKind::Outcome), 1);
    worker.stop().await;
}

#[tokio::test]
async fn disabled_and_missing_capabilities_never_generate() {
    for capability in [
        HeadingCapability::Disabled { backend: "none" },
        HeadingCapability::MissingProvider {
            backend: "ollama",
            setup_hint: HeadingSetupHint {
                message: "Install the local provider.".to_owned(),
                action_label: "Learn more".to_owned(),
                docs_path: "docs/setup.html#contextual-card-headings".to_owned(),
            },
        },
    ] {
        let provider = FakeHeadingProvider::new();
        let discovery = Arc::new(FakeHeadingDiscovery::new(capability, provider.clone()));
        let config = HeadingsConfig {
            backend: HeadingsBackend::None,
            ..HeadingsConfig::default()
        };
        let worker = TestHeadingWorker::spawn(&config, discovery.clone());
        worker
            .runtime
            .observations
            .send_replace(heading_observation(
                "w1:p1",
                "/cwd",
                "private-prompt-key",
                Some("private-reply-key"),
                Some("private screen content"),
            ));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(lock(&provider.calls).is_empty());
        assert_eq!(discovery.calls.load(Ordering::SeqCst), 1);
        worker.stop().await;
    }
}

#[tokio::test]
async fn production_none_and_auto_without_model_make_zero_provider_connections() {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap_or_else(|error| panic!("heading probe listener: {error}"));
    let endpoint = format!(
        "http://{}",
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("heading probe address: {error}"))
    );
    for config in [
        HeadingsConfig {
            backend: HeadingsBackend::None,
            endpoint: endpoint.clone(),
            model: Some("must-not-be-probed".to_owned()),
            ..HeadingsConfig::default()
        },
        HeadingsConfig {
            endpoint: endpoint.clone(),
            ..HeadingsConfig::default()
        },
    ] {
        let selection = tokio::time::timeout(
            Duration::from_millis(50),
            HeadingProviderSelection::discover(&config),
        )
        .await
        .unwrap_or_else(|_| panic!("inert heading discovery attempted provider I/O"));
        assert!(matches!(
            selection.capability,
            HeadingCapability::Disabled { .. } | HeadingCapability::Unconfigured { .. }
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err(),
        "inert heading modes opened a provider connection"
    );
}

#[tokio::test]
async fn heading_store_cadence_is_used_and_accepted_values_overlay_cards() {
    let provider = FakeHeadingProvider::new();
    let (_sender, receiver) = watch::channel(HeadingObservation::default());
    let mut worker = HeadingWorkerState::new();
    let normalized = normalize_snapshot(&snapshot_with_agents(
        "agents",
        vec![agent(1, "claude", "idle", "/cwd")],
    ))
    .unwrap_or_else(|error| panic!("heading snapshot: {error}"));

    for index in 1..=6 {
        let reply_key = match index {
            1 => Some("reply-1"),
            6 => Some("reply-6"),
            _ => None,
        };
        let mut observation = heading_observation(
            "w1:p1",
            "/cwd",
            &format!("prompt-{index}"),
            reply_key,
            (index == 1).then_some("running exact heading cadence tests"),
        );
        if let Some(pane) = observation.panes.get_mut("w1:p1") {
            pane.identity = ScreenIdentity::from_agent(&normalized.agents[0]);
        } else {
            panic!("heading observation pane missing");
        }
        let result = process_heading_batch(
            &provider,
            &receiver,
            &mut worker,
            &observation,
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, HeadingBatchResult::Completed(_)));
    }
    assert_eq!(provider.count(HeadingKind::Title), 4);
    assert_eq!(provider.count(HeadingKind::Subtitle), 6);
    assert_eq!(provider.count(HeadingKind::Outcome), 2);
    assert_eq!(provider.count(HeadingKind::Activity), 1);

    let config = HeadingsConfig {
        names: NamesMode::All,
        ..HeadingsConfig::default()
    };
    let (runtime, _observations, shared) = RuntimeHeadings::new(&config);
    let health = RuntimeHealth::for_config(&Config::default());
    let (invalidations, _receiver) = mpsc::channel(1);
    publish_heading_results(
        &shared,
        &health,
        &invalidations,
        &worker,
        HeadingBatchReport {
            attempted: true,
            failure: None,
        },
    );
    let mut enrichments = AssemblyEnrichments::default();
    runtime.overlay(&normalized, &mut enrichments);
    let accepted = &enrichments.by_pane["w1:p1"];
    assert_eq!(
        accepted.model_title.as_deref(),
        Some("Build portable heading runtime")
    );
    assert_eq!(
        accepted.focus.as_deref(),
        Some("Test latest heading observations")
    );
    assert_eq!(
        accepted.state.as_deref(),
        Some("The portable heading runtime is connected and tested")
    );
    assert_eq!(
        accepted.activity.as_deref(),
        Some("Running heading worker tests")
    );
}

#[tokio::test]
async fn heading_failure_retains_last_good_then_success_restores_available() {
    let provider = FakeHeadingProvider::new();
    let (_sender, receiver) = watch::channel(HeadingObservation::default());
    let mut worker = HeadingWorkerState::new();
    let cancellation = CancellationToken::new();
    let first = heading_observation("w1:p1", "/cwd", "prompt-1", Some("reply-1"), None);
    let result =
        process_heading_batch(&provider, &receiver, &mut worker, &first, &cancellation).await;
    assert!(matches!(result, HeadingBatchResult::Completed(_)));
    let key = worker.identities["w1:p1"].1.clone();
    let first_outcome = worker
        .store
        .accepted(&key)
        .and_then(|value| value.outcome.clone());

    provider.push(Err(HeadingProviderError::RequestTimeout));
    let failed = heading_observation("w1:p1", "/cwd", "prompt-1", Some("reply-2"), None);
    let report = match process_heading_batch(
        &provider,
        &receiver,
        &mut worker,
        &failed,
        &cancellation,
    )
    .await
    {
        HeadingBatchResult::Completed(report) => report,
        _ => panic!("failure batch must complete"),
    };
    assert_eq!(report.failure, Some(HeadingAttemptFailure::Timeout));
    assert_eq!(
        worker
            .store
            .accepted(&key)
            .and_then(|value| value.outcome.clone()),
        first_outcome
    );

    let config = HeadingsConfig {
        model: Some("fixture-model".to_owned()),
        ..HeadingsConfig::default()
    };
    let (_runtime, _observations, shared) = RuntimeHeadings::new(&config);
    let health =
        RuntimeHealth::with_heading_capability(&HeadingCapability::Available { backend: "ollama" });
    let (invalidations, _receiver) = mpsc::channel(1);
    publish_heading_results(&shared, &health, &invalidations, &worker, report);
    assert_eq!(
        heading_capability_status(&lock_heading_shared(&shared).capability).reason,
        Some(CapabilityReason::Timeout)
    );

    let recovered = heading_observation("w1:p1", "/cwd", "prompt-1", Some("reply-3"), None);
    let report =
        match process_heading_batch(&provider, &receiver, &mut worker, &recovered, &cancellation)
            .await
        {
            HeadingBatchResult::Completed(report) => report,
            _ => panic!("recovery batch must complete"),
        };
    publish_heading_results(&shared, &health, &invalidations, &worker, report);
    assert!(matches!(
        lock_heading_shared(&shared).capability,
        HeadingCapability::Available { .. }
    ));
    assert_eq!(
        health.report().capabilities[&CapabilityName::Headings].state,
        HealthState::Available
    );
}

#[tokio::test]
async fn slow_worker_coalesces_to_latest_without_stalling_reconcile_or_actions() {
    let provider = FakeHeadingProvider::new();
    provider.set_delay(Duration::from_millis(30));
    let discovery = Arc::new(FakeHeadingDiscovery::new(
        HeadingCapability::Available { backend: "ollama" },
        provider.clone(),
    ));
    let config = HeadingsConfig {
        model: Some("fixture-model".to_owned()),
        ..HeadingsConfig::default()
    };
    let worker = TestHeadingWorker::spawn(&config, discovery);
    worker
        .runtime
        .observations
        .send_replace(heading_observation(
            "w1:p1",
            "/cwd",
            "old-observation",
            None,
            None,
        ));
    wait_for_heading_calls(&provider, 1).await;

    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents(
        "agents",
        vec![agent(1, "claude", "idle", "/cwd")],
    ))));
    let states = Arc::new(
        StateHub::new(&degraded_payload("not_refreshed"))
            .unwrap_or_else(|error| panic!("state hub: {error}")),
    );
    let owner = StateOwner {
        herdr: fake.clone(),
        states,
        health: worker.health.clone(),
        tracker: Arc::new(Mutex::new(ReadTracker::new())),
        future_protocol_warning: Arc::new(Mutex::new(FutureProtocolWarning::default())),
        transcripts: None,
        screens: Arc::new(Mutex::new(ScreenState::default())),
        started_at: Instant::now(),
        headings: worker.runtime.clone(),
        tab_titles: RuntimeTabTitles::inactive(),
    };
    tokio::time::timeout(Duration::from_millis(20), owner.reconcile())
        .await
        .unwrap_or_else(|_| panic!("provider work stalled reconciliation"));

    let (invalidations, _receiver) = mpsc::channel(1);
    let actions = RuntimeActions {
        herdr: fake,
        invalidations,
        health: worker.health.clone(),
    };
    tokio::time::timeout(Duration::from_millis(20), actions.focus_pane("w1:p1"))
        .await
        .unwrap_or_else(|_| panic!("provider work stalled actions"))
        .unwrap_or_else(|error| panic!("action failed: {error:?}"));

    worker
        .runtime
        .observations
        .send_replace(heading_observation(
            "w1:p1",
            "/cwd",
            "obsolete-middle",
            None,
            None,
        ));
    worker
        .runtime
        .observations
        .send_replace(heading_observation(
            "w1:p1",
            "/cwd",
            "latest-observation",
            None,
            None,
        ));

    wait_for_heading_calls(&provider, 3).await;
    {
        let calls = lock(&provider.calls);
        assert!(
            calls
                .iter()
                .any(|(_, prompt)| prompt.contains("old-observation"))
        );
        assert!(
            !calls
                .iter()
                .any(|(_, prompt)| prompt.contains("obsolete-middle"))
        );
        assert!(
            calls
                .iter()
                .any(|(_, prompt)| prompt.contains("latest-observation"))
        );
    }
    assert_eq!(provider.maximum.load(Ordering::SeqCst), 1);
    worker.stop().await;
}

#[tokio::test]
async fn obsolete_slow_heading_attempt_preserves_progress_and_stops_retry_storm() {
    let provider = FakeHeadingProvider::new();
    provider.set_delay(Duration::from_millis(30));
    let initial = heading_observation("w1:p1", "/cwd", "stable-prompt", None, Some("screen-a"));
    let (sender, mut receiver) = watch::channel(initial.clone());
    let mut worker = HeadingWorkerState::new();
    let cancellation = CancellationToken::new();

    let latest = heading_observation("w1:p1", "/cwd", "stable-prompt", None, Some("screen-b"));
    let update_sender = sender.clone();
    let update = tokio::spawn({
        let latest = latest.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            update_sender.send_replace(latest);
        }
    });
    let first =
        process_heading_batch(&provider, &receiver, &mut worker, &initial, &cancellation).await;
    update
        .await
        .unwrap_or_else(|error| panic!("observation update must join: {error}"));
    assert!(matches!(first, HeadingBatchResult::Obsolete(_)));

    let accepted_after_obsolete = worker
        .store
        .accepted("w1:p1#1")
        .unwrap_or_else(|| panic!("obsolete attempt state must be retained"));
    assert_eq!(
        accepted_after_obsolete.title.as_deref(),
        Some("Build portable heading runtime")
    );

    receiver.borrow_and_update();
    let second =
        process_heading_batch(&provider, &receiver, &mut worker, &latest, &cancellation).await;
    assert!(matches!(second, HeadingBatchResult::Completed(_)));
    assert_eq!(lock(&provider.calls).len(), 3);
    assert_eq!(provider.count(HeadingKind::Title), 1);
    assert_eq!(provider.count(HeadingKind::Subtitle), 1);
    assert_eq!(provider.count(HeadingKind::Activity), 1);
    assert_eq!(provider.maximum.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pane_identity_change_and_death_prune_accepted_headings_without_migration() {
    let provider = FakeHeadingProvider::new();
    let (_sender, receiver) = watch::channel(HeadingObservation::default());
    let mut worker = HeadingWorkerState::new();
    let cancellation = CancellationToken::new();
    let first = heading_observation("w1:p1", "/old", "prompt-1", None, None);
    let result =
        process_heading_batch(&provider, &receiver, &mut worker, &first, &cancellation).await;
    assert!(matches!(result, HeadingBatchResult::Completed(_)));
    assert_eq!(worker.store.len(), 1);

    let replaced = HeadingObservation {
        panes: HashMap::from([(
            "w1:p1".to_owned(),
            HeadingPaneObservation {
                identity: ScreenIdentity {
                    kind: "claude".to_owned(),
                    cwd: "/new".to_owned(),
                    session: None,
                },
                digest: None,
                screen: None,
            },
        )]),
    };
    let result =
        process_heading_batch(&provider, &receiver, &mut worker, &replaced, &cancellation).await;
    assert!(matches!(result, HeadingBatchResult::Completed(_)));
    assert!(worker.store.is_empty());

    let empty = HeadingObservation::default();
    let result =
        process_heading_batch(&provider, &receiver, &mut worker, &empty, &cancellation).await;
    assert!(matches!(result, HeadingBatchResult::Completed(_)));
    assert!(worker.identities.is_empty());
    assert!(worker.store.is_empty());
}

#[tokio::test]
async fn cancelling_heading_worker_drops_in_flight_generation_and_releases_lane() {
    let provider = FakeHeadingProvider::new();
    provider.set_pending(true);
    let discovery = Arc::new(FakeHeadingDiscovery::new(
        HeadingCapability::Available { backend: "ollama" },
        provider.clone(),
    ));
    let config = HeadingsConfig {
        model: Some("fixture-model".to_owned()),
        ..HeadingsConfig::default()
    };
    let worker = TestHeadingWorker::spawn(&config, discovery);
    worker
        .runtime
        .observations
        .send_replace(heading_observation("w1:p1", "/cwd", "prompt-1", None, None));
    wait_for_heading_calls(&provider, 1).await;
    assert_eq!(provider.active.load(Ordering::SeqCst), 1);
    worker.stop().await;
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_failures_expose_only_closed_codes_not_private_inputs_or_errors() {
    let secret = "private-prompt-screen-token-0123456789abcdef";
    let provider = FakeHeadingProvider::new();
    provider.push(Err(HeadingProviderError::Transport));
    let (_sender, receiver) = watch::channel(HeadingObservation::default());
    let mut worker = HeadingWorkerState::new();
    let observation = heading_observation("w1:p1", "/cwd", secret, None, Some(secret));
    let report = match process_heading_batch(
        &provider,
        &receiver,
        &mut worker,
        &observation,
        &CancellationToken::new(),
    )
    .await
    {
        HeadingBatchResult::Completed(report) => report,
        _ => panic!("provider failure batch must complete"),
    };
    let config = HeadingsConfig {
        model: Some("fixture-model".to_owned()),
        ..HeadingsConfig::default()
    };
    let (runtime, _observations, shared) = RuntimeHeadings::new(&config);
    let health = RuntimeHealth::for_config(&Config::default());
    let (invalidations, _receiver) = mpsc::channel(1);
    publish_heading_results(&shared, &health, &invalidations, &worker, report);
    let encoded = format!(
        "{} {}",
        serde_json::to_string(&runtime.capabilities())
            .unwrap_or_else(|error| panic!("capabilities JSON: {error}")),
        serde_json::to_string(&health.report())
            .unwrap_or_else(|error| panic!("health JSON: {error}"))
    );
    assert!(!encoded.contains(secret));
    assert!(encoded.contains("provider_failed"));
    assert!(!encoded.contains("Ollama transport"));
}

#[tokio::test]
async fn startup_is_degraded_when_herdr_is_absent_and_health_uses_closed_codes() {
    let fake = Arc::new(FakeHerdr::new(Err(RuntimeFailure::Unavailable)));
    let runtime = spawn_runtime(
        fake,
        Arc::new(TestEvents::disconnected()),
        Duration::from_secs(30),
    )
    .await;

    let state = get_json(&runtime, "/api/snapshot").await;
    assert!(!state["herdr"]["ok"].as_bool().unwrap_or(true));
    let health = wait_for(&runtime, "/api/health", |value| {
        value["herdr"]["reason"] == "herdr_unavailable"
    })
    .await;
    assert_eq!(health["status"], "degraded");
    assert_eq!(health["herdr"]["state"], "unavailable");
    runtime.stop().await;
}

#[tokio::test]
async fn immediate_reconcile_publishes_authoritative_model_off_state() {
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("ready"))));
    let runtime = spawn_runtime(
        fake.clone(),
        Arc::new(TestEvents::disconnected()),
        Duration::from_secs(30),
    )
    .await;

    let state = wait_for(&runtime, "/api/snapshot", |value| {
        value["herdr"]["ok"] == true
    })
    .await;
    assert_eq!(state["workspaces"][0]["label"], "ready");
    assert_eq!(state["capacity"]["reason"], "disabled");
    assert!(state.get("localModel").is_none());
    assert_eq!(state["capabilities"]["headings"]["state"], "disabled");
    assert_eq!(state["capabilities"]["capacity"]["state"], "disabled");
    assert_eq!(fake.snapshot_calls.load(Ordering::SeqCst), 1);
    runtime.stop().await;
}

#[tokio::test]
async fn byte_identical_reconciliation_is_not_republished() {
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("same"))));
    let states = Arc::new(
        StateHub::new(&degraded_payload("not_refreshed"))
            .unwrap_or_else(|error| panic!("state hub: {error}")),
    );
    let health = RuntimeHealth::initial();
    let headings_config = HeadingsConfig {
        backend: HeadingsBackend::None,
        ..HeadingsConfig::default()
    };
    let (headings, _observations, _shared) = RuntimeHeadings::new(&headings_config);
    let owner = StateOwner {
        herdr: fake,
        states: states.clone(),
        health,
        tracker: Arc::new(Mutex::new(ReadTracker::new())),
        future_protocol_warning: Arc::new(Mutex::new(FutureProtocolWarning::default())),
        transcripts: None,
        screens: Arc::new(Mutex::new(ScreenState::default())),
        started_at: Instant::now(),
        headings,
        tab_titles: RuntimeTabTitles::inactive(),
    };
    let mut receiver = states.subscribe();
    owner.reconcile().await;
    receiver
        .changed()
        .await
        .unwrap_or_else(|error| panic!("first publish: {error}"));
    receiver.borrow_and_update();
    owner.reconcile().await;
    assert!(matches!(receiver.has_changed(), Ok(false)));
}

#[tokio::test]
async fn disconnected_events_do_not_stop_polling_and_herdr_recovers() {
    let fake = Arc::new(FakeHerdr::new(Err(RuntimeFailure::ConnectionFailed)));
    let runtime = spawn_runtime(
        fake.clone(),
        Arc::new(TestEvents::disconnected()),
        Duration::from_millis(40),
    )
    .await;
    wait_for(&runtime, "/api/health", |value| {
        value["herdr"]["reason"] == "connection_failed"
    })
    .await;

    fake.set_fallback(Ok(snapshot("recovered")));
    let state = wait_for(&runtime, "/api/snapshot", |value| {
        value["workspaces"][0]["label"] == "recovered"
    })
    .await;
    assert_eq!(state["herdr"]["ok"], true);
    let health = get_json(&runtime, "/api/health").await;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["herdr"]["version"], "0.8.2");
    runtime.stop().await;
}

#[tokio::test]
async fn event_and_poll_race_cannot_miss_final_state_across_restart() {
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("final"))));
    fake.push(Ok(snapshot("first")));
    fake.push(Err(RuntimeFailure::ConnectionFailed));
    let (events, trigger) = TestEvents::connected();
    let runtime = spawn_runtime(fake, Arc::new(events), Duration::from_millis(45)).await;
    wait_for(&runtime, "/api/snapshot", |value| {
        value["workspaces"][0]["label"] == "first"
    })
    .await;

    trigger.notify_one();
    let state = wait_for(&runtime, "/api/snapshot", |value| {
        value["workspaces"][0]["label"] == "final"
    })
    .await;
    assert_eq!(state["herdr"]["ok"], true);
    runtime.stop().await;
}

#[tokio::test]
async fn successful_actions_invalidate_and_failures_map_to_503() {
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("before"))));
    let runtime = spawn_runtime(
        fake.clone(),
        Arc::new(TestEvents::disconnected()),
        Duration::from_secs(30),
    )
    .await;
    wait_for(&runtime, "/api/snapshot", |value| {
        value["herdr"]["ok"] == true
    })
    .await;
    fake.set_fallback(Ok(snapshot("after")));

    let client = reqwest::Client::new();
    let response = client
        .post(runtime.url("/api/focus"))
        .header("content-type", "application/json")
        .body(r#"{"paneId":"w1:p1"}"#)
        .send()
        .await
        .unwrap_or_else(|error| panic!("successful action: {error}"));
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    wait_for(&runtime, "/api/snapshot", |value| {
        value["workspaces"][0]["label"] == "after"
    })
    .await;

    fake.fail_actions(RuntimeFailure::ConnectionFailed);
    let response = client
        .post(runtime.url("/api/workspace"))
        .header("content-type", "application/json")
        .body(r#"{"workspaceId":"w1"}"#)
        .send()
        .await
        .unwrap_or_else(|error| panic!("failed action: {error}"));
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .json::<Value>()
            .await
            .unwrap_or_else(|error| panic!("action error JSON: {error}")),
        json!({"ok": false, "error": "herdr_unavailable"})
    );
    assert!(fake.action_calls.load(Ordering::SeqCst) >= 2);
    let calls_after_success = fake.snapshot_calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        fake.snapshot_calls.load(Ordering::SeqCst),
        calls_after_success,
        "a failed action must not enqueue an invalidation"
    );
    runtime.stop().await;
}

#[tokio::test]
async fn protocol_failure_degrades_then_recovers_and_shutdown_releases_listener() {
    let mut old = snapshot("old");
    old.protocol = 18;
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("new"))));
    fake.push(Ok(old));
    let runtime = spawn_runtime(
        fake,
        Arc::new(TestEvents::disconnected()),
        Duration::from_millis(40),
    )
    .await;
    wait_for(&runtime, "/api/health", |value| {
        value["herdr"]["reason"] == "protocol_mismatch"
    })
    .await;
    wait_for(&runtime, "/api/snapshot", |value| {
        value["workspaces"][0]["label"] == "new"
    })
    .await;
    let address = runtime.address;
    runtime.stop().await;
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn schema_snapshot_or_client_version_mismatch_degrades_health_safely() {
    let schema_fake = Arc::new(FakeHerdr::new(Ok(snapshot("unused"))));
    let mut future = snapshot("schema-mismatch");
    future.protocol = 21;
    schema_fake.set_observed(Ok(RuntimeSnapshot {
        client_version: future.version.clone(),
        schema_protocol: 20,
        snapshot: future,
    }));
    let runtime = spawn_runtime(
        schema_fake.clone(),
        Arc::new(TestEvents::disconnected()),
        Duration::from_secs(30),
    )
    .await;
    let health = wait_for(&runtime, "/api/health", |value| {
        value["herdr"]["reason"] == "protocol_mismatch"
    })
    .await;
    assert_eq!(health["status"], "degraded");
    assert_eq!(
        schema_fake.diagnostic_invalidations.load(Ordering::SeqCst),
        1
    );
    runtime.stop().await;

    let version_fake = Arc::new(FakeHerdr::new(Ok(snapshot("unused"))));
    let secret = "0123456789abcdef0123456789abcdef";
    version_fake.set_observed(Ok(RuntimeSnapshot {
        snapshot: snapshot("version-mismatch"),
        client_version: secret.to_owned(),
        schema_protocol: 20,
    }));
    let runtime = spawn_runtime(
        version_fake,
        Arc::new(TestEvents::disconnected()),
        Duration::from_secs(30),
    )
    .await;
    let health = wait_for(&runtime, "/api/health", |value| {
        value["herdr"]["reason"] == "protocol_mismatch"
    })
    .await;
    assert!(!health.to_string().contains(secret));
    runtime.stop().await;
}

#[tokio::test]
async fn matching_future_protocol_is_usable_after_required_subset_decodes() {
    let mut future = snapshot("future");
    future.protocol = 21;
    let fake = Arc::new(FakeHerdr::new(Ok(future)));
    let runtime = spawn_runtime(
        fake,
        Arc::new(TestEvents::disconnected()),
        Duration::from_secs(30),
    )
    .await;

    let state = wait_for(&runtime, "/api/snapshot", |value| {
        value["workspaces"][0]["label"] == "future"
    })
    .await;
    assert_eq!(state["herdr"]["ok"], true);
    assert_eq!(get_json(&runtime, "/api/health").await["status"], "ok");
    runtime.stop().await;
}

#[tokio::test]
async fn transcript_ready_missing_malformed_and_unsupported_agents_are_mapped_without_content() {
    let agents = vec![
        agent(1, "claude", "idle", "/claude"),
        agent(2, "pi", "idle", "/pi"),
        agent(3, "codex", "idle", "/codex"),
        agent(4, "copilot", "idle", "/copilot"),
        agent(5, "future-agent", "idle", "/future"),
    ];
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents("agents", agents))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set("/claude", [ready_observation("reply-1", 100, 50_000)]);
    transcripts.set("/pi", [TranscriptObservation::not_yet_created()]);
    transcripts.set("/codex", [malformed_observation()]);
    transcripts.set(
        "/copilot",
        [ready_observation("must-not-be-read", 100, 199_000)],
    );
    transcripts.set(
        "/future",
        [ready_observation("must-not-be-read", 100, 199_000)],
    );
    let (owner, states, _) = owner_for(fake, Some(transcripts.clone()));

    owner.reconcile().await;
    let payload = current_json(&states);
    let calls = lock(&transcripts.calls);

    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .iter()
            .any(|request| request.kind == TranscriptKind::Claude)
    );
    assert!(
        calls
            .iter()
            .any(|request| request.kind == TranscriptKind::Pi)
    );
    assert!(
        calls
            .iter()
            .any(|request| request.kind == TranscriptKind::Codex)
    );
    assert_eq!(payload["agents"][0]["context"]["used"], 50_000);
    assert!(payload["agents"][1].get("context").is_none());
    assert!(payload["agents"][2].get("context").is_none());
    assert!(payload["agents"][3].get("context").is_none());
    assert!(payload["agents"][4].get("context").is_none());
    let encoded = payload.to_string();
    assert!(!encoded.contains("private prompt"));
    assert!(!encoded.contains("private reply"));
    assert!(!encoded.contains("reply-1"));
    assert!(!encoded.contains("must-not-be-read"));
}

#[tokio::test]
async fn validated_copilot_transcript_maps_context_reply_age_and_unread() {
    let written_at = i64::try_from(unix_seconds())
        .unwrap_or(i64::MAX)
        .saturating_sub(65);
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents(
        "agents",
        vec![copilot_agent(1, "idle", "/copilot")],
    ))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set(
        "/copilot",
        [
            ready_observation("copilot-reply-1", written_at, 50_000),
            ready_observation("copilot-reply-2", written_at + 1, 90_000),
        ],
    );
    let (owner, states, health) = owner_for(fake, Some(transcripts.clone()));

    owner.reconcile().await;
    let first = current_json(&states);
    assert_eq!(first["agents"][0]["context"]["used"], 50_000);
    assert_eq!(first["agents"][0]["unread"], false);
    let reply_age = first["agents"][0]["repliedAgo"]
        .as_i64()
        .unwrap_or_else(|| panic!("Copilot reply age must be present"));
    assert!((60..=90).contains(&reply_age));

    owner.reconcile().await;
    let second = current_json(&states);
    assert_eq!(second["agents"][0]["context"]["used"], 90_000);
    assert_eq!(second["agents"][0]["unread"], true);
    assert_eq!(second["agents"][0]["repliedAgo"], 0);
    let calls = lock(&transcripts.calls);
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|request| {
        request.kind == TranscriptKind::Copilot
            && request.session.as_ref().is_some_and(|session| {
                session.agent == "copilot"
                    && session.kind == "id"
                    && session.value == "copilot-session-1"
            })
    }));
    drop(calls);

    let payload = second.to_string();
    let diagnostics = format!("{:?}", health.report());
    for private in ["private prompt", "private reply", "copilot-reply"] {
        assert!(!payload.contains(private));
        assert!(!diagnostics.contains(private));
    }
}

#[tokio::test]
async fn missing_and_malformed_copilot_transcripts_do_not_fabricate_enrichment() {
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents(
        "agents",
        vec![
            copilot_agent(1, "idle", "/copilot-missing"),
            copilot_agent(2, "idle", "/copilot-malformed"),
        ],
    ))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set(
        "/copilot-missing",
        [TranscriptObservation::not_yet_created()],
    );
    transcripts.set("/copilot-malformed", [malformed_observation()]);
    let (owner, states, _) = owner_for(fake, Some(transcripts.clone()));

    owner.reconcile().await;
    let payload = current_json(&states);
    assert_eq!(lock(&transcripts.calls).len(), 2);
    for agent in payload["agents"]
        .as_array()
        .unwrap_or_else(|| panic!("agents must be an array"))
    {
        assert!(agent.get("context").is_none());
        assert!(agent.get("repliedAgo").is_none());
        assert_eq!(agent["unread"], false);
    }
}

#[tokio::test]
async fn invalid_foreign_path_and_unknown_sessions_never_request_transcripts() {
    let mut missing = copilot_agent(1, "idle", "/missing-session");
    missing.agent_session = None;
    let mut path = copilot_agent(2, "idle", "/path-session");
    path.agent_session = Some(AgentSessionDto {
        source: "herdr".to_owned(),
        agent: "copilot".to_owned(),
        kind: "path".to_owned(),
        value: "/private/copilot/events.jsonl".to_owned(),
    });
    let mut foreign = copilot_agent(3, "idle", "/foreign-session");
    foreign.agent_session = Some(AgentSessionDto {
        source: "herdr".to_owned(),
        agent: "claude".to_owned(),
        kind: "id".to_owned(),
        value: "foreign-session".to_owned(),
    });
    let mut traversal = copilot_agent(4, "idle", "/traversal-session");
    traversal.agent_session = Some(AgentSessionDto {
        source: "herdr".to_owned(),
        agent: "copilot".to_owned(),
        kind: "id".to_owned(),
        value: "../escape".to_owned(),
    });
    let unknown = agent(5, "future-agent", "idle", "/unknown");
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents(
        "agents",
        vec![missing, path, foreign, traversal, unknown],
    ))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    let (owner, states, _) = owner_for(fake, Some(transcripts.clone()));

    owner.reconcile().await;
    let payload = current_json(&states);
    assert!(lock(&transcripts.calls).is_empty());
    assert_eq!(payload["agents"].as_array().map(Vec::len), Some(5));
    for agent in payload["agents"]
        .as_array()
        .unwrap_or_else(|| panic!("agents must be an array"))
    {
        assert!(agent.get("context").is_none());
        assert!(agent.get("repliedAgo").is_none());
        assert_eq!(agent["unread"], false);
    }
}

#[tokio::test]
async fn copilot_identity_loss_and_pane_reuse_clear_prior_read_state() {
    let written_at = i64::try_from(unix_seconds())
        .unwrap_or(i64::MAX)
        .saturating_sub(65);
    let valid = copilot_agent(1, "idle", "/copilot");
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents(
        "agents",
        vec![valid.clone()],
    ))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set(
        "/copilot",
        [
            ready_observation("reply-1", written_at, 50_000),
            ready_observation("reply-2", written_at + 1, 60_000),
            ready_observation("reply-2", written_at + 1, 70_000),
        ],
    );
    let (owner, states, _) = owner_for(fake.clone(), Some(transcripts.clone()));

    owner.reconcile().await;
    owner.reconcile().await;
    let unread = current_json(&states);
    assert_eq!(unread["agents"][0]["unread"], true);
    assert_eq!(lock(&transcripts.calls).len(), 2);

    let mut invalid = valid.clone();
    invalid.agent_session = Some(AgentSessionDto {
        source: "herdr".to_owned(),
        agent: "copilot".to_owned(),
        kind: "id".to_owned(),
        value: "../foreign".to_owned(),
    });
    fake.set_fallback(Ok(snapshot_with_agents("agents", vec![invalid])));
    owner.reconcile().await;
    let invalid_payload = current_json(&states);
    assert_eq!(lock(&transcripts.calls).len(), 2);
    assert!(invalid_payload["agents"][0].get("context").is_none());
    assert!(invalid_payload["agents"][0].get("repliedAgo").is_none());
    assert_eq!(invalid_payload["agents"][0]["unread"], false);

    let mut reused = valid;
    reused.agent_session = Some(AgentSessionDto {
        source: "herdr".to_owned(),
        agent: "copilot".to_owned(),
        kind: "id".to_owned(),
        value: "copilot-session-reused".to_owned(),
    });
    fake.set_fallback(Ok(snapshot_with_agents("agents", vec![reused])));
    owner.reconcile().await;
    let reused_payload = current_json(&states);
    assert_eq!(lock(&transcripts.calls).len(), 3);
    assert_eq!(reused_payload["agents"][0]["context"]["used"], 70_000);
    assert_eq!(reused_payload["agents"][0]["unread"], false);
}

#[tokio::test]
async fn transcript_reply_changes_drive_unread_and_identity_changes_reset_it() {
    let first_agent = agent(1, "claude", "idle", "/old-path");
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents(
        "agents",
        vec![first_agent],
    ))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set("/old-path", [ready_observation("reply-old", 100, 10_000)]);
    transcripts.set("/new-path", [ready_observation("reply-new", 200, 90_000)]);
    let (owner, states, _) = owner_for(fake.clone(), Some(transcripts.clone()));

    owner.reconcile().await;
    let first = current_json(&states);
    assert_eq!(first["agents"][0]["unread"], false);
    assert_eq!(first["agents"][0]["context"]["used"], 10_000);

    let second_agent = agent(1, "claude", "idle", "/new-path");
    fake.set_fallback(Ok(snapshot_with_agents("agents", vec![second_agent])));
    owner.reconcile().await;
    let second = current_json(&states);
    assert_eq!(second["agents"][0]["unread"], false);
    assert_eq!(second["agents"][0]["context"]["used"], 90_000);
    assert_eq!(lock(&transcripts.calls).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn transcript_reads_have_five_way_concurrency_agent_cap_and_total_deadline() {
    let agents = (1..=65)
        .map(|index| copilot_agent(index, "idle", &format!("/cwd-{index}")))
        .collect::<Vec<_>>();
    let normalized = normalize_snapshot(&snapshot_with_agents("agents", agents))
        .unwrap_or_else(|error| panic!("fixture snapshot must normalize: {error}"));
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("unused"))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set_delay(Duration::from_secs(10));
    let (owner, _, _) = owner_for(fake, Some(transcripts.clone()));

    let observations = owner.read_transcripts(&normalized).await;

    assert!(observations.is_empty());
    assert_eq!(lock(&transcripts.calls).len(), 10);
    assert_eq!(transcripts.maximum.load(Ordering::SeqCst), 5);
    assert_eq!(transcripts.active.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn transcript_agent_cap_is_exact_when_reads_complete() {
    let agents = (1..=65)
        .map(|index| copilot_agent(index, "idle", &format!("/cwd-{index}")))
        .collect::<Vec<_>>();
    let normalized = normalize_snapshot(&snapshot_with_agents("agents", agents))
        .unwrap_or_else(|error| panic!("fixture snapshot must normalize: {error}"));
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("unused"))));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set_delay(Duration::from_millis(10));
    let (owner, _, _) = owner_for(fake, Some(transcripts.clone()));

    let observations = owner.read_transcripts(&normalized).await;

    assert_eq!(observations.len(), 64);
    assert_eq!(lock(&transcripts.calls).len(), 64);
    assert_eq!(transcripts.maximum.load(Ordering::SeqCst), 5);
}

#[tokio::test(start_paused = true)]
async fn screen_phase_background_cadence_failure_cache_identity_and_prune_are_bounded() {
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("unused"))));
    fake.set_screen_results(
        "w1:p1",
        [
            Ok("✻ Thinking… (2s · 123 tokens)\n2 shells".to_owned()),
            Err(RuntimeFailure::Timeout),
        ],
    );
    let (owner, _, _) = owner_for(fake.clone(), None);
    let working = normalize_snapshot(&snapshot_with_agents(
        "agents",
        vec![agent(1, "claude", "working", "/old")],
    ))
    .unwrap_or_else(|error| panic!("fixture snapshot must normalize: {error}"));

    let first = owner.read_screens(&working).await;
    let first = first
        .enrichments
        .by_pane
        .get("w1:p1")
        .unwrap_or_else(|| panic!("first screen enrichment must exist"));
    assert_eq!(
        first.phase.as_ref().map(|phase| phase.verb.as_str()),
        Some("Thinking")
    );
    assert_eq!(first.background.as_deref(), Some("2 shells"));

    let cached = owner.read_screens(&working).await;
    assert_eq!(cached, owner.apply_screen_reads(&working, Vec::new()));
    assert_eq!(lock(&fake.screen_calls).len(), 1);

    tokio::time::advance(Duration::from_secs(1)).await;
    let failed = owner.read_screens(&working).await;
    assert_eq!(
        failed, cached,
        "failed reads must retain the last good screen"
    );
    assert_eq!(lock(&fake.screen_calls).len(), 2);

    let idle = normalize_snapshot(&snapshot_with_agents(
        "agents",
        vec![agent(1, "claude", "idle", "/old")],
    ))
    .unwrap_or_else(|error| panic!("idle fixture must normalize: {error}"));
    let idle_enrichment = owner.read_screens(&idle).await;
    let idle_card = idle_enrichment
        .enrichments
        .by_pane
        .get("w1:p1")
        .unwrap_or_else(|| panic!("cached background must remain"));
    assert!(idle_card.phase.is_none());
    assert_eq!(idle_card.background.as_deref(), Some("2 shells"));

    let replaced = normalize_snapshot(&snapshot_with_agents(
        "agents",
        vec![agent(1, "claude", "idle", "/new")],
    ))
    .unwrap_or_else(|error| panic!("replacement fixture must normalize: {error}"));
    assert!(
        owner
            .read_screens(&replaced)
            .await
            .enrichments
            .by_pane
            .is_empty()
    );

    let empty = normalize_snapshot(&snapshot_with_agents("agents", Vec::new()))
        .unwrap_or_else(|error| panic!("empty fixture must normalize: {error}"));
    assert!(
        owner
            .read_screens(&empty)
            .await
            .enrichments
            .by_pane
            .is_empty()
    );
    let screens = lock(&owner.screens);
    assert!(screens.observations.is_empty());
    assert!(screens.schedule.is_empty());
}

#[tokio::test(start_paused = true)]
async fn screen_reads_have_eight_way_concurrency_exact_agent_cap_and_parse_bound() {
    let agents = (1..=65)
        .map(|index| agent(index, "claude", "working", &format!("/cwd-{index}")))
        .collect::<Vec<_>>();
    let normalized = normalize_snapshot(&snapshot_with_agents("agents", agents))
        .unwrap_or_else(|error| panic!("fixture snapshot must normalize: {error}"));
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot("unused"))));
    fake.set_screen_delay(Duration::from_millis(10));
    for index in 1..=64 {
        fake.set_screen_results(&format!("w1:p{index}"), [Ok("✻ Working… (1s)".to_owned())]);
    }
    let (owner, _, _) = owner_for(fake.clone(), None);

    let enriched = owner.read_screens(&normalized).await;

    assert_eq!(lock(&fake.screen_calls).len(), 64);
    assert_eq!(fake.max_active_screen_reads.load(Ordering::SeqCst), 8);
    assert_eq!(enriched.enrichments.by_pane.len(), 64);

    tokio::time::advance(Duration::from_secs(1)).await;
    fake.set_screen_results("w1:p1", [Ok("x".repeat(MAX_SCREEN_PARSE_BYTES + 1))]);
    let _ = owner.read_screens(&normalized).await;
    let cached = owner.apply_screen_reads(&normalized, Vec::new());
    assert_eq!(
        cached.enrichments.by_pane["w1:p1"]
            .phase
            .as_ref()
            .map(|phase| phase.verb.as_str()),
        Some("Working"),
        "oversized injected screen text must be ignored and retain the last good value"
    );
}

#[tokio::test]
async fn cached_screen_enrichment_does_not_republish_identical_payload_bytes() {
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents(
        "agents",
        vec![agent(1, "claude", "working", "/cwd")],
    ))));
    fake.set_screen_results("w1:p1", [Ok("✻ Working… (1s)".to_owned())]);
    let (owner, states, _) = owner_for(fake.clone(), None);
    let mut receiver = states.subscribe();

    owner.reconcile().await;
    receiver
        .changed()
        .await
        .unwrap_or_else(|error| panic!("first publish: {error}"));
    receiver.borrow_and_update();
    owner.reconcile().await;

    assert!(matches!(receiver.has_changed(), Ok(false)));
    assert_eq!(lock(&fake.screen_calls).len(), 1);
}

#[tokio::test]
async fn cancelling_reconciliation_drops_every_runtime_enrichment_future() {
    let agents = (1..=5)
        .map(|index| copilot_agent(index, "working", &format!("/cwd-{index}")))
        .collect::<Vec<_>>();
    let fake = Arc::new(FakeHerdr::new(Ok(snapshot_with_agents("agents", agents))));
    fake.set_screen_delay(Duration::from_secs(60));
    let transcripts = Arc::new(FakeTranscriptSource::new());
    transcripts.set_delay(Duration::from_secs(60));
    let (owner, _, health) = owner_for(fake.clone(), Some(transcripts.clone()));
    let task = tokio::spawn(async move {
        owner.reconcile().await;
    });

    for _ in 0..100 {
        if transcripts.active.load(Ordering::SeqCst) == 5
            && fake.active_screen_reads.load(Ordering::SeqCst) == 5
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(transcripts.active.load(Ordering::SeqCst), 5);
    assert_eq!(fake.active_screen_reads.load(Ordering::SeqCst), 5);

    let (invalidations, _receiver) = mpsc::channel(1);
    let actions = RuntimeActions {
        herdr: fake.clone(),
        invalidations,
        health,
    };
    tokio::time::timeout(Duration::from_millis(20), actions.focus_pane("w1:p1"))
        .await
        .unwrap_or_else(|_| panic!("slow Copilot observations stalled Herdr actions"))
        .unwrap_or_else(|error| panic!("action failed: {error:?}"));

    task.abort();
    let joined = task.await;
    assert!(joined.is_err_and(|error| error.is_cancelled()));
    assert_eq!(transcripts.active.load(Ordering::SeqCst), 0);
    assert_eq!(fake.active_screen_reads.load(Ordering::SeqCst), 0);
}

#[test]
fn future_protocol_warnings_are_rate_limited_by_monotonic_time() {
    let start = Instant::now();
    let mut warning = FutureProtocolWarning::default();

    assert!(warning.should_warn(start));
    assert!(!warning.should_warn(start + Duration::from_secs(59)));
    assert!(warning.should_warn(start + Duration::from_secs(60)));
}

#[test]
fn raw_herdr_errors_are_classified_without_entering_health() {
    let secret = "prompt-secret-session-token-0123456789";
    let failure = classify_herdr_error(&HerdrError::Process(ProcessError::Api {
        status: 1,
        id: Some(secret.to_owned()),
        code: secret.to_owned(),
        message: secret.to_owned(),
    }));
    let health = RuntimeHealth::initial();
    health.failure(failure);
    let encoded = serde_json::to_string(&health.report())
        .unwrap_or_else(|error| panic!("health JSON: {error}"));
    assert!(!encoded.contains(secret));
    assert!(encoded.contains("invalid_data"));
}

struct CancellationHerdr {
    snapshot_dropped: Arc<AtomicBool>,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl RuntimeHerdr for CancellationHerdr {
    async fn snapshot(&self) -> std::result::Result<RuntimeSnapshot, RuntimeFailure> {
        let _drop_flag = DropFlag(self.snapshot_dropped.clone());
        std::future::pending().await
    }

    async fn focus_pane(&self, _pane_id: &str) -> std::result::Result<(), RuntimeFailure> {
        Ok(())
    }

    async fn focus_workspace(
        &self,
        _workspace_id: &str,
    ) -> std::result::Result<(), RuntimeFailure> {
        Ok(())
    }

    async fn create_tab(&self, _workspace_id: &str) -> std::result::Result<(), RuntimeFailure> {
        Ok(())
    }
}

#[tokio::test]
async fn shutdown_cancels_in_flight_snapshot_and_joins_the_owner() {
    let dropped = Arc::new(AtomicBool::new(false));
    let herdr: Arc<dyn RuntimeHerdr> = Arc::new(CancellationHerdr {
        snapshot_dropped: dropped.clone(),
    });
    let runtime = spawn_runtime(
        herdr,
        Arc::new(TestEvents::disconnected()),
        Duration::from_secs(30),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let initial = get_json(&runtime, "/api/snapshot").await;
    assert_eq!(initial["herdr"]["ok"], false);
    assert!(initial["workspaces"].as_array().is_some_and(Vec::is_empty));
    tokio::time::timeout(Duration::from_secs(1), runtime.stop())
        .await
        .unwrap_or_else(|_| panic!("runtime shutdown did not complete"));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn supervisor_collects_simultaneous_cancel_failure_and_panic_safely() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let owner_cancel = cancellation.clone();
    let poll_cancel = cancellation.clone();
    let secret = "runtime-io-secret-0123456789abcdef";
    let server: JoinHandle<std::io::Result<()>> =
        tokio::spawn(async move { Err(std::io::Error::other(secret)) });
    let owner = tokio::spawn(async move {
        owner_cancel.cancelled().await;
    });
    let events = tokio::spawn(async move {
        panic!("synthetic event panic");
    });
    let poll = tokio::spawn(async move {
        poll_cancel.cancelled().await;
    });
    let headings = tokio::spawn(async move {
        panic!("synthetic heading panic");
    });
    let telemetry = tokio::spawn(async move {});
    let tab_titles = tokio::spawn(async move {
        panic!("synthetic tab-title panic");
    });

    let error = supervise(
        RuntimeTasks {
            server,
            owner,
            events,
            poll,
            headings,
            telemetry,
            tab_titles,
        },
        cancellation,
        test_http_server(),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("concurrent task failures must fail supervision"));
    let message = error.to_string();

    assert!(message.contains("HTTP server task failed"));
    assert!(message.contains("event subscription task failed"));
    assert!(message.contains("heading worker task failed"));
    assert!(message.contains("tab-title worker task failed"));
    assert!(!message.contains(secret));
}

#[tokio::test]
async fn heading_worker_normal_exit_is_supervised_as_an_early_failure() {
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let owner_cancel = cancellation.clone();
    let event_cancel = cancellation.clone();
    let poll_cancel = cancellation.clone();
    let server: JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
        server_cancel.cancelled().await;
        Ok(())
    });
    let owner = tokio::spawn(async move {
        owner_cancel.cancelled().await;
    });
    let events = tokio::spawn(async move {
        event_cancel.cancelled().await;
    });
    let poll = tokio::spawn(async move {
        poll_cancel.cancelled().await;
    });
    let headings = tokio::spawn(async move {});
    let telemetry_cancel = cancellation.clone();
    let telemetry = tokio::spawn(async move {
        telemetry_cancel.cancelled().await;
    });

    let error = supervise(
        RuntimeTasks {
            server,
            owner,
            events,
            poll,
            headings,
            telemetry,
            tab_titles: tab_title_task_waiting_for(cancellation.clone()),
        },
        cancellation.clone(),
        test_http_server(),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("early heading worker exit must fail supervision"));

    assert!(cancellation.is_cancelled());
    assert!(
        error
            .to_string()
            .contains("heading worker stopped before shutdown")
    );
}

#[tokio::test]
async fn tab_title_worker_normal_exit_is_supervised_as_an_early_failure() {
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let owner_cancel = cancellation.clone();
    let event_cancel = cancellation.clone();
    let poll_cancel = cancellation.clone();
    let heading_cancel = cancellation.clone();
    let telemetry_cancel = cancellation.clone();
    let server = tokio::spawn(async move {
        server_cancel.cancelled().await;
        Ok(())
    });
    let owner = tokio::spawn(async move { owner_cancel.cancelled().await });
    let events = tokio::spawn(async move { event_cancel.cancelled().await });
    let poll = tokio::spawn(async move { poll_cancel.cancelled().await });
    let headings = tokio::spawn(async move { heading_cancel.cancelled().await });
    let telemetry = tokio::spawn(async move { telemetry_cancel.cancelled().await });
    let tab_titles = tokio::spawn(async move {});

    let error = supervise(
        RuntimeTasks {
            server,
            owner,
            events,
            poll,
            headings,
            telemetry,
            tab_titles,
        },
        cancellation.clone(),
        test_http_server(),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("early tab-title worker exit must fail supervision"));

    assert!(cancellation.is_cancelled());
    assert!(
        error
            .to_string()
            .contains("tab-title worker stopped before shutdown")
    );
}

#[tokio::test]
async fn telemetry_normal_exit_is_supervised_and_every_peer_is_reaped() {
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let owner_cancel = cancellation.clone();
    let event_cancel = cancellation.clone();
    let poll_cancel = cancellation.clone();
    let heading_cancel = cancellation.clone();
    let server = tokio::spawn(async move {
        server_cancel.cancelled().await;
        Ok(())
    });
    let owner = tokio::spawn(async move { owner_cancel.cancelled().await });
    let events = tokio::spawn(async move { event_cancel.cancelled().await });
    let poll = tokio::spawn(async move { poll_cancel.cancelled().await });
    let headings = tokio::spawn(async move { heading_cancel.cancelled().await });
    let telemetry = tokio::spawn(async move {});

    let error = supervise(
        RuntimeTasks {
            server,
            owner,
            events,
            poll,
            headings,
            telemetry,
            tab_titles: tab_title_task_waiting_for(cancellation.clone()),
        },
        cancellation.clone(),
        test_http_server(),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("early telemetry exit must fail supervision"));

    assert!(cancellation.is_cancelled());
    assert!(
        error
            .to_string()
            .contains("telemetry stopped before shutdown")
    );
}

#[tokio::test]
async fn telemetry_panic_is_supervised_without_exposing_panic_content() {
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let owner_cancel = cancellation.clone();
    let event_cancel = cancellation.clone();
    let poll_cancel = cancellation.clone();
    let heading_cancel = cancellation.clone();
    let server = tokio::spawn(async move {
        server_cancel.cancelled().await;
        Ok(())
    });
    let owner = tokio::spawn(async move { owner_cancel.cancelled().await });
    let events = tokio::spawn(async move { event_cancel.cancelled().await });
    let poll = tokio::spawn(async move { poll_cancel.cancelled().await });
    let headings = tokio::spawn(async move { heading_cancel.cancelled().await });
    let telemetry = tokio::spawn(async move {
        panic!("private-telemetry-token-0123456789");
    });

    let error = supervise(
        RuntimeTasks {
            server,
            owner,
            events,
            poll,
            headings,
            telemetry,
            tab_titles: tab_title_task_waiting_for(cancellation.clone()),
        },
        cancellation,
        test_http_server(),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("telemetry panic must fail supervision"));
    let message = error.to_string();
    assert!(message.contains("telemetry task failed"));
    assert!(!message.contains("private-telemetry-token"));
}

#[tokio::test]
async fn event_only_panic_triggers_shutdown_and_reaps_every_other_task() {
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let owner_cancel = cancellation.clone();
    let poll_cancel = cancellation.clone();
    let heading_cancel = cancellation.clone();
    let server_dropped = Arc::new(AtomicBool::new(false));
    let owner_dropped = Arc::new(AtomicBool::new(false));
    let events_dropped = Arc::new(AtomicBool::new(false));
    let poll_dropped = Arc::new(AtomicBool::new(false));
    let server_flag = server_dropped.clone();
    let owner_flag = owner_dropped.clone();
    let events_flag = events_dropped.clone();
    let poll_flag = poll_dropped.clone();
    let server: JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
        let _flag = DropFlag(server_flag);
        server_cancel.cancelled().await;
        Ok(())
    });
    let owner = tokio::spawn(async move {
        let _flag = DropFlag(owner_flag);
        owner_cancel.cancelled().await;
    });
    let events = tokio::spawn(async move {
        let _flag = DropFlag(events_flag);
        panic!("synthetic event-only panic");
    });
    let poll = tokio::spawn(async move {
        let _flag = DropFlag(poll_flag);
        poll_cancel.cancelled().await;
    });
    let headings = tokio::spawn(async move {
        heading_cancel.cancelled().await;
    });
    let telemetry_cancel = cancellation.clone();
    let telemetry = tokio::spawn(async move {
        telemetry_cancel.cancelled().await;
    });

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        supervise(
            RuntimeTasks {
                server,
                owner,
                events,
                poll,
                headings,
                telemetry,
                tab_titles: tab_title_task_waiting_for(cancellation.clone()),
            },
            cancellation.clone(),
            test_http_server(),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("event task panic did not trigger bounded shutdown"))
    .err()
    .unwrap_or_else(|| panic!("event task panic must fail supervision"));

    assert!(cancellation.is_cancelled());
    assert!(error.to_string().contains("event subscription task failed"));
    assert!(server_dropped.load(Ordering::SeqCst));
    assert!(owner_dropped.load(Ordering::SeqCst));
    assert!(events_dropped.load(Ordering::SeqCst));
    assert!(poll_dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn supervisor_aborts_and_joins_a_task_that_misses_the_shared_deadline() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let server_cancel = cancellation.clone();
    let event_cancel = cancellation.clone();
    let poll_cancel = cancellation.clone();
    let heading_cancel = cancellation.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let owner_dropped = dropped.clone();
    let server: JoinHandle<std::io::Result<()>> = tokio::spawn(async move {
        server_cancel.cancelled().await;
        Ok(())
    });
    let owner = tokio::spawn(async move {
        let _flag = DropFlag(owner_dropped);
        std::future::pending::<()>().await;
    });
    let events = tokio::spawn(async move {
        event_cancel.cancelled().await;
    });
    let poll = tokio::spawn(async move {
        poll_cancel.cancelled().await;
    });
    let headings = tokio::spawn(async move {
        heading_cancel.cancelled().await;
    });
    let telemetry_cancel = cancellation.clone();
    let telemetry = tokio::spawn(async move {
        telemetry_cancel.cancelled().await;
    });

    let error = supervise(
        RuntimeTasks {
            server,
            owner,
            events,
            poll,
            headings,
            telemetry,
            tab_titles: tab_title_task_waiting_for(cancellation.clone()),
        },
        cancellation,
        test_http_server(),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("stuck task must fail supervision"));

    assert!(
        error
            .to_string()
            .contains("state owner task did not stop before the shutdown deadline")
    );
    assert!(dropped.load(Ordering::SeqCst));
}
