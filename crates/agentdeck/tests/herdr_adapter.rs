use std::{
    ffi::OsString,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agentdeck::adapters::herdr::{
    AgentDto, CommandLimits, CommandOutput, CommandSpec, HerdrClient, HerdrError, HerdrTarget,
    ProcessError, ProcessRunner, SnapshotDto, TabDto, VisibleLines, WorkspaceDto, WorktreeDto,
};
use agentdeck::config::HerdrConfig;
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;

const PROTOCOL_19: &str = include_str!("../../../Tests/fixtures/herdr/protocol-19-snapshot.json");
const PROTOCOL_20: &str = include_str!("../../../Tests/fixtures/herdr/protocol-20-snapshot.json");

type RunnerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommandOutput, ProcessError>> + Send + 'a>>;

#[derive(Default)]
struct RecordingRunner {
    calls: Mutex<Vec<CommandSpec>>,
}

impl RecordingRunner {
    fn calls(&self) -> Vec<CommandSpec> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ProcessRunner for RecordingRunner {
    fn run(&self, spec: CommandSpec, permit: OwnedSemaphorePermit) -> RunnerFuture<'_> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(spec.clone());
        Box::pin(async move {
            let _permit = permit;
            Ok(success_output(&spec.label))
        })
    }
}

struct StaticRunner {
    stdout: Vec<u8>,
}

impl ProcessRunner for StaticRunner {
    fn run(&self, _spec: CommandSpec, permit: OwnedSemaphorePermit) -> RunnerFuture<'_> {
        let stdout = self.stdout.clone();
        Box::pin(async move {
            let _permit = permit;
            Ok(CommandOutput {
                stdout,
                stderr: Vec::new(),
            })
        })
    }
}

struct ConcurrencyRunner {
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl ConcurrencyRunner {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl ProcessRunner for ConcurrencyRunner {
    fn run(&self, spec: CommandSpec, permit: OwnedSemaphorePermit) -> RunnerFuture<'_> {
        Box::pin(async move {
            let _permit = permit;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(35)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(success_output(&spec.label))
        })
    }
}

fn success_output(label: &str) -> CommandOutput {
    let stdout = match label {
        "herdr --version" => b"herdr 0.8.2\n".to_vec(),
        "herdr api schema --json" => {
            br#"{"protocol":20,"schema_version":1,"future":true}"#.to_vec()
        }
        "herdr api snapshot" => PROTOCOL_20.as_bytes().to_vec(),
        "herdr agent focus" => envelope("agent_info"),
        "herdr workspace focus" => envelope("workspace_info"),
        "herdr tab create" => envelope("tab_created"),
        "herdr tab rename" => envelope("tab_info"),
        "herdr agent read" => b"visible output".to_vec(),
        other => panic!("unexpected test command label {other:?}"),
    };
    CommandOutput {
        stdout,
        stderr: Vec::new(),
    }
}

fn envelope(kind: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "fake",
        "result": {"type": kind, "preserved": true}
    }))
    .unwrap_or_else(|error| panic!("could not build test envelope: {error}"))
}

fn snapshot_from_fixture(fixture: &str) -> SnapshotDto {
    let value: Value = serde_json::from_str(fixture)
        .unwrap_or_else(|error| panic!("invalid test fixture: {error}"));
    serde_json::from_value(value["result"]["snapshot"].clone())
        .unwrap_or_else(|error| panic!("fixture snapshot did not decode: {error}"))
}

fn strings(values: &[OsString]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

fn call<'a>(calls: &'a [CommandSpec], label: &str) -> &'a CommandSpec {
    calls
        .iter()
        .find(|call| call.label == label)
        .unwrap_or_else(|| panic!("missing recorded command {label}"))
}

#[test]
fn protocol_19_fixture_decodes_nullable_and_unknown_agent_data() {
    let snapshot = snapshot_from_fixture(PROTOCOL_19);
    assert_eq!(snapshot.protocol, 19);
    assert_eq!(snapshot.focused_pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(snapshot.focused_tab_id.as_deref(), Some("w1:t1"));
    assert_eq!(snapshot.focused_workspace_id.as_deref(), Some("w1"));
    assert_eq!(snapshot.agents[1].agent.as_deref(), Some("future-agent"));
    assert_eq!(snapshot.agents[1].cwd, None);
    assert_eq!(snapshot.agents[1].agent_session, None);
}

#[test]
fn protocol_20_fixture_decodes_copilot_nulls_and_omitted_focus_fields() {
    let snapshot = snapshot_from_fixture(PROTOCOL_20);
    assert_eq!(snapshot.protocol, 20);
    assert_eq!(snapshot.focused_pane_id, None);
    assert_eq!(snapshot.focused_tab_id, None);
    assert_eq!(snapshot.focused_workspace_id, None);
    assert_eq!(snapshot.agents[0].agent.as_deref(), Some("copilot"));
    assert_eq!(snapshot.agents[1].agent, None);
    assert_eq!(snapshot.agents[1].cwd, None);
}

#[test]
fn snapshot_and_nested_session_reject_missing_required_fields() {
    let missing_agents = json!({
        "version": "0.8.2", "protocol": 20,
        "workspaces": [], "tabs": [], "panes": [], "layouts": []
    });
    assert!(serde_json::from_value::<SnapshotDto>(missing_agents).is_err());

    let session_missing_kind = json!({
        "terminal_id": "term-1",
        "agent": "claude",
        "agent_session": {"source": "hook", "agent": "claude", "value": "one"},
        "agent_status": "idle", "focused": false,
        "pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1", "revision": 1
    });
    assert!(serde_json::from_value::<AgentDto>(session_missing_kind).is_err());

    let workspace_missing_active_tab = json!({
        "workspace_id": "w1", "label": "one", "number": 1,
        "agent_status": "idle", "focused": false, "pane_count": 1, "tab_count": 1
    });
    assert!(serde_json::from_value::<WorkspaceDto>(workspace_missing_active_tab).is_err());

    let tab_missing_pane_count = json!({
        "tab_id": "w1:t1", "workspace_id": "w1", "label": "one", "number": 1,
        "agent_status": "idle", "focused": false
    });
    assert!(serde_json::from_value::<TabDto>(tab_missing_pane_count).is_err());

    let worktree_missing_checkout = json!({
        "repo_key": "repo:one", "repo_name": "one", "repo_root": "/repo",
        "is_linked_worktree": false
    });
    assert!(serde_json::from_value::<WorktreeDto>(worktree_missing_checkout).is_err());
}

#[test]
fn configured_session_and_socket_conflict_before_command_execution() {
    let config = HerdrConfig {
        session: Some("default".to_owned()),
        socket: Some("explicit.sock".to_owned()),
    };
    assert!(matches!(
        HerdrTarget::from_config(&config),
        Err(HerdrError::ConflictingTargets)
    ));
}

#[test]
fn target_construction_rejects_invalid_configured_socket_without_config_prevalidation() {
    for socket in ["", " ", " leading", "trailing ", "line\nbreak"] {
        let config = HerdrConfig {
            session: None,
            socket: Some(socket.to_owned()),
        };
        assert!(
            matches!(
                HerdrTarget::from_config(&config),
                Err(HerdrError::InvalidSocket { .. })
            ),
            "accepted invalid socket {socket:?}"
        );
    }
}

#[tokio::test]
async fn client_uses_exact_argv_routing_limits_and_preserves_argument_boundaries() {
    let runner = Arc::new(RecordingRunner::default());
    let client = HerdrClient::with_runner(
        PathBuf::from("/absolute/herdr"),
        HerdrTarget::session("named.session")
            .unwrap_or_else(|error| panic!("valid target rejected: {error}")),
        runner.clone(),
    );

    assert_eq!(
        client
            .version()
            .await
            .unwrap_or_else(|error| panic!("version: {error}")),
        "0.8.2"
    );
    assert_eq!(
        client
            .schema()
            .await
            .unwrap_or_else(|error| panic!("schema: {error}"))
            .protocol,
        20
    );
    assert_eq!(
        client
            .snapshot()
            .await
            .unwrap_or_else(|error| panic!("snapshot: {error}"))
            .protocol,
        20
    );
    let pane = "--pane; $(never) ü";
    let workspace = "--workspace leading ü; $()";
    let title = "A title ü; $(never) --leading";
    assert_eq!(
        client
            .focus_pane(pane)
            .await
            .unwrap_or_else(|error| panic!("focus pane: {error}"))
            .result
            .kind,
        "agent_info"
    );
    assert_eq!(
        client
            .focus_workspace(workspace)
            .await
            .unwrap_or_else(|error| panic!("focus workspace: {error}"))
            .result
            .kind,
        "workspace_info"
    );
    assert_eq!(
        client
            .create_focused_tab(workspace)
            .await
            .unwrap_or_else(|error| panic!("create tab: {error}"))
            .result
            .kind,
        "tab_created"
    );
    let renamed = client
        .rename_tab("w1:t1", title)
        .await
        .unwrap_or_else(|error| panic!("rename tab: {error}"));
    assert_eq!(renamed.result.kind, "tab_info");
    assert_eq!(
        renamed.result.fields.get("preserved"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        client
            .read_visible(pane, VisibleLines::Phase40)
            .await
            .unwrap_or_else(|error| panic!("read: {error}")),
        "visible output"
    );

    let calls = runner.calls();
    let version = call(&calls, "herdr --version");
    assert_eq!(strings(&version.args), ["--version"]);
    assert_eq!(version.limits, limits(2, 64 * 1024, 64 * 1024));
    assert!(version.env_remove.is_empty());
    let schema = call(&calls, "herdr api schema --json");
    assert_eq!(strings(&schema.args), ["api", "schema", "--json"]);
    assert_eq!(schema.limits, limits(5, 2 * 1024 * 1024, 256 * 1024));
    let snapshot = call(&calls, "herdr api snapshot");
    assert_eq!(
        strings(&snapshot.args),
        ["--session", "named.session", "api", "snapshot"]
    );
    assert_eq!(snapshot.limits, limits(12, 4 * 1024 * 1024, 256 * 1024));
    assert_eq!(
        strings(&snapshot.env_remove),
        ["HERDR_SOCKET_PATH", "HERDR_SESSION"]
    );
    assert!(snapshot.env_set.is_empty());

    assert_eq!(
        strings(&call(&calls, "herdr agent focus").args),
        ["--session", "named.session", "agent", "focus", pane]
    );
    assert_eq!(
        strings(&call(&calls, "herdr workspace focus").args),
        [
            "--session",
            "named.session",
            "workspace",
            "focus",
            workspace
        ]
    );
    assert_eq!(
        strings(&call(&calls, "herdr tab create").args),
        [
            "--session",
            "named.session",
            "tab",
            "create",
            "--workspace",
            workspace,
            "--focus"
        ]
    );
    assert_eq!(
        strings(&call(&calls, "herdr tab rename").args),
        [
            "--session",
            "named.session",
            "tab",
            "rename",
            "w1:t1",
            title
        ]
    );
    let read = call(&calls, "herdr agent read");
    assert_eq!(
        strings(&read.args),
        [
            "--session",
            "named.session",
            "agent",
            "read",
            pane,
            "--source",
            "visible",
            "--lines",
            "40",
            "--format",
            "text"
        ]
    );
    assert_eq!(read.limits, limits(12, 512 * 1024, 256 * 1024));
}

#[tokio::test]
async fn socket_target_sets_socket_and_removes_inherited_session() {
    let runner = Arc::new(RecordingRunner::default());
    let client = HerdrClient::with_runner(
        PathBuf::from("/absolute/herdr"),
        HerdrTarget::socket(PathBuf::from("socket name ü"))
            .unwrap_or_else(|error| panic!("valid socket rejected: {error}")),
        runner.clone(),
    );
    client
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot: {error}"));
    let calls = runner.calls();
    let snapshot = call(&calls, "herdr api snapshot");
    assert_eq!(strings(&snapshot.args), ["api", "snapshot"]);
    assert_eq!(
        snapshot.env_set,
        [(
            OsString::from("HERDR_SOCKET_PATH"),
            OsString::from("socket name ü")
        )]
    );
    assert_eq!(strings(&snapshot.env_remove), ["HERDR_SESSION"]);
}

#[tokio::test]
async fn auto_target_does_not_override_inherited_routing() {
    let runner = Arc::new(RecordingRunner::default());
    let client = HerdrClient::with_runner(
        PathBuf::from("/absolute/herdr"),
        HerdrTarget::Auto,
        runner.clone(),
    );
    client
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("snapshot: {error}"));
    let calls = runner.calls();
    let snapshot = call(&calls, "herdr api snapshot");
    assert_eq!(strings(&snapshot.args), ["api", "snapshot"]);
    assert!(snapshot.env_set.is_empty());
    assert!(snapshot.env_remove.is_empty());
}

#[tokio::test]
async fn malformed_missing_and_wrong_result_types_are_distinct() {
    let malformed = client_with_static(b"{bad".to_vec());
    assert!(matches!(
        malformed.focus_pane("w1:p1").await,
        Err(HerdrError::MalformedJson { .. })
    ));
    let missing = client_with_static(br#"{"id":"x","result":{}}"#.to_vec());
    assert!(matches!(
        missing.focus_pane("w1:p1").await,
        Err(HerdrError::MissingResultType { .. })
    ));
    let wrong = client_with_static(br#"{"id":"x","result":{"type":"workspace_info"}}"#.to_vec());
    assert!(
        matches!(wrong.focus_pane("w1:p1").await, Err(HerdrError::UnexpectedResultType { actual, .. }) if actual == "workspace_info")
    );
}

#[tokio::test]
async fn zero_exit_api_error_and_invalid_read_utf8_are_typed() {
    let api =
        client_with_static(br#"{"id":"x","error":{"code":"nope","message":"failed"}}"#.to_vec());
    assert!(
        matches!(api.focus_pane("w1:p1").await, Err(HerdrError::Process(ProcessError::Api { status: 0, code, .. })) if code == "nope")
    );
    let invalid = client_with_static(vec![0xff, 0xfe]);
    assert!(matches!(
        invalid
            .read_visible("w1:p1", VisibleLines::Background16)
            .await,
        Err(HerdrError::InvalidUtf8 { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_calls_are_serialized() {
    let runner = Arc::new(ConcurrencyRunner::new());
    let client = HerdrClient::with_runner(
        PathBuf::from("/absolute/herdr"),
        HerdrTarget::Auto,
        runner.clone(),
    );
    let mut joins = Vec::new();
    for _ in 0..6 {
        let client = client.clone();
        joins.push(tokio::spawn(async move { client.snapshot().await }));
    }
    join_all(joins).await;
    assert_eq!(runner.peak(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutations_are_limited_to_four() {
    let runner = Arc::new(ConcurrencyRunner::new());
    let client = HerdrClient::with_runner(
        PathBuf::from("/absolute/herdr"),
        HerdrTarget::Auto,
        runner.clone(),
    );
    let mut joins = Vec::new();
    for index in 0..12 {
        let client = client.clone();
        joins.push(tokio::spawn(async move {
            client.focus_pane(&format!("w1:p{index}")).await
        }));
    }
    join_all(joins).await;
    assert_eq!(runner.peak(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_are_limited_to_eight() {
    let runner = Arc::new(ConcurrencyRunner::new());
    let client = HerdrClient::with_runner(
        PathBuf::from("/absolute/herdr"),
        HerdrTarget::Auto,
        runner.clone(),
    );
    let mut joins = Vec::new();
    for index in 0..20 {
        let client = client.clone();
        joins.push(tokio::spawn(async move {
            client
                .read_visible(&format!("w1:p{index}"), VisibleLines::Background16)
                .await
        }));
    }
    join_all(joins).await;
    assert_eq!(runner.peak(), 8);
}

async fn join_all<T: Send + 'static>(joins: Vec<tokio::task::JoinHandle<Result<T, HerdrError>>>) {
    for join in joins {
        let result = join
            .await
            .unwrap_or_else(|error| panic!("test task failed: {error}"));
        result.unwrap_or_else(|error| panic!("client call failed: {error}"));
    }
}

fn client_with_static(stdout: Vec<u8>) -> HerdrClient {
    HerdrClient::with_runner(
        PathBuf::from("/absolute/herdr"),
        HerdrTarget::Auto,
        Arc::new(StaticRunner { stdout }),
    )
}

fn limits(timeout: u64, stdout_bytes: usize, stderr_bytes: usize) -> CommandLimits {
    CommandLimits {
        timeout: Duration::from_secs(timeout),
        stdout_bytes,
        stderr_bytes,
    }
}
