#![cfg(feature = "test-helper")]

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agentdeck::adapters::herdr::{
    CommandLimits, CommandOutput, CommandSpec, HerdrClient, HerdrError, HerdrTarget, OutputStream,
    ProcessError, ProcessRunner, TokioProcessRunner, VisibleLines, resolve_herdr_binary_with,
};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const CAP: usize = 64 * 1024;

type RunnerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommandOutput, ProcessError>> + Send + 'a>>;

struct ScenarioRunner {
    scenario: &'static str,
}

impl ProcessRunner for ScenarioRunner {
    fn run(&self, mut spec: CommandSpec, permit: OwnedSemaphorePermit) -> RunnerFuture<'_> {
        spec.env_set.push((
            OsString::from("AGENTDECK_FAKE_SCENARIO"),
            OsString::from(self.scenario),
        ));
        Box::pin(async move { TokioProcessRunner.run(spec, permit).await })
    }
}

#[cfg(unix)]
struct SequencedRunner {
    calls: AtomicUsize,
    hanging_calls: usize,
    records: Vec<PathBuf>,
}

#[cfg(unix)]
impl ProcessRunner for SequencedRunner {
    fn run(&self, mut spec: CommandSpec, permit: OwnedSemaphorePermit) -> RunnerFuture<'_> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.hanging_calls {
            spec.env_set.push((
                OsString::from("AGENTDECK_FAKE_SCENARIO"),
                OsString::from("silent_timeout"),
            ));
        }
        if let Some(record) = self.records.get(call) {
            spec.env_set.push((
                OsString::from("AGENTDECK_FAKE_RECORD"),
                record.as_os_str().to_owned(),
            ));
        }
        Box::pin(async move { TokioProcessRunner.run(spec, permit).await })
    }
}

#[tokio::test]
async fn successful_process_preserves_warning_stderr_and_accepts_empty_stdout() {
    let warning = run_scenario("success_warning", limits(2, CAP, CAP))
        .await
        .unwrap_or_else(|error| panic!("warning success failed: {error}"));
    assert_eq!(warning.stdout, b"valid stdout");
    assert_eq!(warning.stderr, b"synthetic warning");

    let empty = run_scenario("empty_success", limits(2, CAP, CAP))
        .await
        .unwrap_or_else(|error| panic!("empty success failed: {error}"));
    assert!(empty.stdout.is_empty());
    assert!(empty.stderr.is_empty());
}

#[tokio::test]
async fn timeouts_before_and_after_partial_output_terminate_and_reap() {
    // Keep this real-process timeout comfortably above cold-start scheduling under
    // the fully parallel workspace suite. Exact production deadline values are
    // covered by the command-spec matrix; this case is specifically proving that
    // an already-started child is killed and reaped on timeout.
    let process_timeout = Duration::from_secs(3);
    for scenario in ["silent_timeout", "partial_timeout"] {
        let record = temp_record(scenario);
        let mut spec = scenario_spec(
            scenario,
            CommandLimits {
                timeout: process_timeout,
                stdout_bytes: CAP,
                stderr_bytes: CAP,
            },
        );
        add_record(&mut spec, &record);
        let task = tokio::spawn(async move {
            TokioProcessRunner
                .run(spec, standalone_permit())
                .await
                .error_or_panic()
        });
        #[cfg(unix)]
        let started = read_record(&record).await;
        #[cfg(not(unix))]
        let _ = read_record(&record).await;
        #[cfg(unix)]
        assert!(
            pid_alive(record_pid(&started)),
            "fake {scenario} process exited before the timeout"
        );
        let error = task
            .await
            .unwrap_or_else(|error| panic!("timeout scenario task failed: {error}"));
        assert!(
            matches!(error, ProcessError::Timeout { timeout, .. } if timeout == process_timeout),
            "wrong timeout classification for {scenario}: {error}"
        );
        #[cfg(unix)]
        assert!(
            !pid_alive(record_pid(&started)),
            "fake {scenario} process was not reaped"
        );
        remove_file(&record);
    }
}

#[tokio::test]
async fn raw_agent_read_success_and_json_api_error_use_distinct_paths() {
    let raw_client = HerdrClient::with_runner(
        fake_binary(),
        HerdrTarget::Auto,
        Arc::new(TokioProcessRunner),
    );
    assert_eq!(
        raw_client
            .read_visible("w1:p1", VisibleLines::Background16)
            .await
            .unwrap_or_else(|error| panic!("raw read failed: {error}")),
        "visible output"
    );

    let api_client = HerdrClient::with_runner(
        fake_binary(),
        HerdrTarget::Auto,
        Arc::new(ScenarioRunner {
            scenario: "api_error",
        }),
    );
    assert!(
        matches!(api_client.read_visible("w1:p1", VisibleLines::Phase40).await,
            Err(HerdrError::Process(ProcessError::Api {
                status: 1,
                id: Some(id),
                code,
                message,
            })) if id == "fake" && code == "pane_not_found" && message == "gone")
    );
}

#[tokio::test]
async fn exact_stream_caps_are_accepted_and_one_byte_over_is_rejected_and_reaped() {
    for (scenario, expected_stream) in [
        ("stdout_exact", OutputStream::Stdout),
        ("stderr_exact", OutputStream::Stderr),
    ] {
        let output = run_scenario(scenario, limits(2, CAP, CAP))
            .await
            .unwrap_or_else(|error| panic!("exact cap {scenario} failed: {error}"));
        match expected_stream {
            OutputStream::Stdout => {
                assert_eq!(output.stdout.len(), CAP);
                assert!(output.stderr.is_empty());
            }
            OutputStream::Stderr => {
                assert_eq!(output.stderr.len(), CAP);
                assert!(output.stdout.is_empty());
            }
        }
    }

    for (scenario, expected_stream) in [
        ("stdout_over", OutputStream::Stdout),
        ("stderr_over", OutputStream::Stderr),
    ] {
        let record = temp_record(scenario);
        let mut spec = scenario_spec(scenario, limits(2, CAP, CAP));
        add_record(&mut spec, &record);
        let error = TokioProcessRunner
            .run(spec, standalone_permit())
            .await
            .error_or_panic();
        assert!(
            matches!(error, ProcessError::OutputLimit { stream, limit: CAP, .. } if stream == expected_stream),
            "wrong one-byte-over classification for {scenario}: {error}"
        );
        assert_recorded_process_reaped(&record).await;
    }
}

#[tokio::test]
async fn live_children_are_killed_and_reaped_on_either_stream_cap() {
    for (scenario, expected_stream) in [
        ("stdout_cap", OutputStream::Stdout),
        ("stderr_cap", OutputStream::Stderr),
    ] {
        let record = temp_record(scenario);
        let mut spec = scenario_spec(scenario, limits(2, CAP, CAP));
        add_record(&mut spec, &record);
        let error = TokioProcessRunner
            .run(spec, standalone_permit())
            .await
            .error_or_panic();
        assert!(
            matches!(error, ProcessError::OutputLimit { stream, limit: CAP, .. } if stream == expected_stream),
            "wrong live-child cap classification for {scenario}: {error}"
        );
        assert_recorded_process_reaped(&record).await;
    }
}

#[tokio::test]
async fn real_runner_drains_saturated_streams_and_classifies_exit_kinds() {
    let duplex = run_scenario("duplex", limits(3, 512 * 1024, 512 * 1024))
        .await
        .unwrap_or_else(|error| panic!("duplex fake failed: {error}"));
    assert_eq!(duplex.stdout.len(), 384 * 1024);
    assert_eq!(duplex.stderr.len(), 384 * 1024);

    let api = run_scenario("api_error", limits(2, CAP, CAP))
        .await
        .error_or_panic();
    assert!(
        matches!(api, ProcessError::Api { status: 1, id: Some(id), code, message }
            if id == "fake" && code == "pane_not_found" && message == "gone")
    );
    let syntax = run_scenario("syntax", limits(2, CAP, CAP))
        .await
        .error_or_panic();
    assert!(matches!(syntax, ProcessError::Syntax { message } if message == "usage: herdr fake"));
    let transport = run_scenario("transport", limits(2, CAP, CAP))
        .await
        .error_or_panic();
    assert!(
        matches!(transport, ProcessError::Transport { status: Some(1), message }
            if message == "connection refused")
    );
}

#[tokio::test]
async fn real_tokio_process_preserves_exact_argv_and_target_environment_boundaries() {
    let tab_id = "--tab;$(never) ü";
    let title = "A title ü; $(never) --leading & |";
    for case in [
        RouteCase {
            kind: "session",
            value: "named.session",
            expected_args: json!(["--session", "named.session", "tab", "rename", tab_id, title]),
            expected_socket: Value::Null,
            expected_session: Value::Null,
        },
        RouteCase {
            kind: "socket",
            value: "explicit socket ü",
            expected_args: json!(["tab", "rename", tab_id, title]),
            expected_socket: Value::String("explicit socket ü".to_owned()),
            expected_session: Value::Null,
        },
        RouteCase {
            kind: "auto",
            value: "-",
            expected_args: json!(["tab", "rename", tab_id, title]),
            expected_socket: Value::String("inherited socket".to_owned()),
            expected_session: Value::String("inherited.session".to_owned()),
        },
    ] {
        let record = temp_record(case.kind);
        let output = tokio::process::Command::new(fake_binary())
            .args([
                OsStr::new("--driver"),
                OsStr::new(case.kind),
                OsStr::new(case.value),
                record.as_os_str(),
                OsStr::new("rename"),
                OsStr::new(tab_id),
                OsStr::new(title),
            ])
            .env("HERDR_SOCKET_PATH", "inherited socket")
            .env("HERDR_SESSION", "inherited.session")
            .output()
            .await
            .unwrap_or_else(|error| panic!("could not run routing driver: {error}"));
        assert!(
            output.status.success(),
            "routing driver {} failed: {}",
            case.kind,
            String::from_utf8_lossy(&output.stderr)
        );
        let recorded = read_record(&record).await;
        assert_eq!(recorded["args"], case.expected_args, "{} args", case.kind);
        assert_eq!(
            recorded["socket"], case.expected_socket,
            "{} socket",
            case.kind
        );
        assert_eq!(
            recorded["session"], case.expected_session,
            "{} session",
            case.kind
        );
        remove_file(&record);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_snapshot_holds_admission_until_cleanup_before_replacement() {
    for iteration in 0..20 {
        let first_record = temp_record(&format!("snapshot-cancel-{iteration}-first"));
        let second_record = temp_record(&format!("snapshot-cancel-{iteration}-second"));
        let runner = Arc::new(SequencedRunner {
            calls: AtomicUsize::new(0),
            hanging_calls: 1,
            records: vec![first_record.clone(), second_record.clone()],
        });
        let client = HerdrClient::with_runner(fake_binary(), HerdrTarget::Auto, runner);

        let first_client = client.clone();
        let first = tokio::spawn(async move { first_client.snapshot().await });
        let first_pid = record_pid(&read_record(&first_record).await);
        assert!(
            pid_alive(first_pid),
            "first fake exited before cancellation"
        );

        abort_and_join(first).await;
        let second_client = client.clone();
        let second = tokio::spawn(async move { second_client.snapshot().await });
        let _second_pid = record_pid(&read_record(&second_record).await);
        assert!(
            !pid_alive(first_pid),
            "replacement spawned before cancelled snapshot PID {first_pid} was reaped"
        );
        let snapshot = second
            .await
            .unwrap_or_else(|error| panic!("replacement snapshot task failed: {error}"))
            .unwrap_or_else(|error| panic!("replacement snapshot failed: {error}"));
        assert_eq!(snapshot.protocol, 20);

        remove_file(&first_record);
        remove_file(&second_record);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_never_exceeds_mutation_or_read_process_lanes() {
    for (lane, limit) in [(PooledLane::Mutation, 4), (PooledLane::Read, 8)] {
        assert_cancelled_pool_stays_bounded(lane, limit).await;
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum PooledLane {
    Mutation,
    Read,
}

#[cfg(unix)]
async fn assert_cancelled_pool_stays_bounded(lane: PooledLane, limit: usize) {
    for iteration in 0..10 {
        let records = (0..limit * 2)
            .map(|call| temp_record(&format!("{lane:?}-cancel-{iteration}-{call}")))
            .collect::<Vec<_>>();
        let runner = Arc::new(SequencedRunner {
            calls: AtomicUsize::new(0),
            hanging_calls: limit * 2,
            records: records.clone(),
        });
        let client = HerdrClient::with_runner(fake_binary(), HerdrTarget::Auto, runner);

        let first = (0..limit)
            .map(|call| spawn_pooled_call(client.clone(), lane, call))
            .collect::<Vec<_>>();
        let mut first_pids = Vec::with_capacity(limit);
        for record in &records[..limit] {
            first_pids.push(record_pid(&read_record(record).await));
        }
        assert_eq!(
            first_pids.iter().filter(|pid| pid_alive(**pid)).count(),
            limit,
            "{lane:?} did not fill its {limit}-process lane before cancellation"
        );

        abort_all(first).await;

        let replacement = (0..limit)
            .map(|call| spawn_pooled_call(client.clone(), lane, limit + call))
            .collect::<Vec<_>>();
        let mut replacement_pids = Vec::with_capacity(limit);
        let mut sampled_peak = limit;
        for record in &records[limit..] {
            replacement_pids.push(record_pid(&read_record(record).await));
            let alive = first_pids
                .iter()
                .chain(&replacement_pids)
                .filter(|pid| pid_alive(**pid))
                .count();
            sampled_peak = sampled_peak.max(alive);
            assert!(
                alive <= limit,
                "{lane:?} admitted {alive} live children with a configured limit of {limit}"
            );
        }
        assert_eq!(
            sampled_peak, limit,
            "{lane:?} did not exercise the full lane"
        );
        assert!(
            first_pids.iter().all(|pid| !pid_alive(*pid)),
            "{lane:?} replacement spawned before every cancelled child was reaped"
        );

        abort_all(replacement).await;
        wait_until(Duration::from_secs(2), || {
            replacement_pids.iter().all(|pid| !pid_alive(*pid))
        })
        .await;
        for record in &records {
            remove_file(record);
        }
    }
}

#[cfg(unix)]
fn spawn_pooled_call(
    client: HerdrClient,
    lane: PooledLane,
    call: usize,
) -> tokio::task::JoinHandle<Result<(), HerdrError>> {
    tokio::spawn(async move {
        let id = format!("w1:p{call}");
        match lane {
            PooledLane::Mutation => client.focus_pane(&id).await.map(|_| ()),
            PooledLane::Read => client
                .read_visible(&id, VisibleLines::Background16)
                .await
                .map(|_| ()),
        }
    })
}

#[cfg(unix)]
async fn abort_all<T>(tasks: Vec<tokio::task::JoinHandle<T>>) {
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        assert!(
            task.await
                .is_err_and(|join_error| join_error.is_cancelled()),
            "aborted caller task did not report cancellation"
        );
    }
}

#[cfg(unix)]
async fn abort_and_join<T>(task: tokio::task::JoinHandle<T>) {
    abort_all(vec![task]).await;
}

#[tokio::test]
async fn missing_executable_is_typed() {
    let mut spec = scenario_spec("empty_success", limits(1, 1024, 1024));
    spec.executable = PathBuf::from("/definitely/not/an/agentdeck-herdr-binary");
    assert!(matches!(
        TokioProcessRunner.run(spec, standalone_permit()).await,
        Err(ProcessError::NotFound { .. })
    ));
}

#[test]
fn resolver_precedence_and_platform_fallbacks_are_deterministic() {
    let temp = TempDirectory::new("resolver");
    let explicit = temp.install("explicit-bin");
    let override_bin = temp.install("override-bin");
    let path_dir = temp.path.join("path-bin");
    create_dir(&path_dir);
    let path_bin = install_named(&path_dir, executable_name());
    let home = temp.path.join("home");
    let home_dir = home.join(".local/bin");
    create_dir(&home_dir);
    let home_bin = install_named(&home_dir, executable_name());
    let joined_path = std::env::join_paths([path_dir.as_path()])
        .unwrap_or_else(|error| panic!("could not build test PATH: {error}"));

    let all = env_map([
        ("HERDR_BIN_PATH", override_bin.as_os_str().to_owned()),
        ("PATH", joined_path.clone()),
        ("HOME", home.as_os_str().to_owned()),
    ]);
    assert_resolves(Some(&explicit), &all, &explicit);
    assert_resolves(None, &all, &override_bin);

    let without_override = env_map([("PATH", joined_path), ("HOME", home.as_os_str().to_owned())]);
    assert_resolves(None, &without_override, &path_bin);

    let home_only = env_map([("HOME", home.as_os_str().to_owned())]);
    assert_resolves(None, &home_only, &home_bin);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mut permissions = std::fs::metadata(&path_bin)
            .unwrap_or_else(|error| panic!("could not inspect PATH candidate: {error}"))
            .permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&path_bin, permissions)
            .unwrap_or_else(|error| panic!("could not disable PATH candidate: {error}"));
        assert_resolves(None, &without_override, &home_bin);
    }

    assert!(matches!(
        resolve_herdr_binary_with(
            Some(Path::new("/definitely/not/herdr")),
            |_| None,
            Vec::new(),
        ),
        Err(ProcessError::NotFound { .. })
    ));
}

#[test]
fn resolver_rejects_non_files_from_explicit_override_path_and_platform_sources() {
    let temp = TempDirectory::new("non-files");
    let non_file = temp.path.join("not-a-file");
    create_dir(&non_file);

    assert_not_found(Some(&non_file), &HashMap::new());
    let override_env = env_map([("HERDR_BIN_PATH", non_file.as_os_str().to_owned())]);
    assert_not_found(None, &override_env);

    let path_dir = temp.path.join("path");
    create_dir(&path_dir.join(executable_name()));
    let home = temp.path.join("home-with-valid-fallback");
    let home_dir = home.join(".local/bin");
    create_dir(&home_dir);
    let home_bin = install_named(&home_dir, executable_name());
    let path = std::env::join_paths([path_dir.as_path()])
        .unwrap_or_else(|error| panic!("could not build non-file PATH: {error}"));
    let path_then_home = env_map([("PATH", path), ("HOME", home.as_os_str().to_owned())]);
    assert_resolves(None, &path_then_home, &home_bin);

    #[cfg(not(windows))]
    {
        let platform_home = temp.path.join("home-with-non-file");
        create_dir(&platform_home.join(".local/bin").join(executable_name()));
        let environment = env_map([("HOME", platform_home.as_os_str().to_owned())]);
        assert_not_found(None, &environment);
    }

    #[cfg(windows)]
    {
        let local_app_data = temp.path.join("local-app-data-with-non-file");
        create_dir(
            &local_app_data
                .join("Programs/Herdr/bin")
                .join(executable_name()),
        );
        let environment = env_map([("LOCALAPPDATA", local_app_data.as_os_str().to_owned())]);
        assert_not_found(None, &environment);
    }
}

#[cfg(unix)]
#[test]
fn resolver_rejects_non_executable_files() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = TempDirectory::new("non-executable");
    let candidate = temp.install("herdr-disabled");
    let mut permissions = std::fs::metadata(&candidate)
        .unwrap_or_else(|error| panic!("could not inspect test executable: {error}"))
        .permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&candidate, permissions)
        .unwrap_or_else(|error| panic!("could not disable test executable: {error}"));
    assert!(matches!(
        resolve_herdr_binary_with(Some(&candidate), |_| None, Vec::new()),
        Err(ProcessError::NotFound { .. })
    ));
    let override_env = env_map([("HERDR_BIN_PATH", candidate.as_os_str().to_owned())]);
    assert_not_found(None, &override_env);

    let home = temp.path.join("home");
    let home_dir = home.join(".local/bin");
    create_dir(&home_dir);
    let home_candidate = install_named(&home_dir, executable_name());
    let mut home_permissions = std::fs::metadata(&home_candidate)
        .unwrap_or_else(|error| panic!("could not inspect home candidate: {error}"))
        .permissions();
    home_permissions.set_mode(0o644);
    std::fs::set_permissions(&home_candidate, home_permissions)
        .unwrap_or_else(|error| panic!("could not disable home candidate: {error}"));
    let home_env = env_map([("HOME", home.as_os_str().to_owned())]);
    assert_not_found(None, &home_env);
}

#[cfg(windows)]
#[test]
fn resolver_uses_windows_local_app_data_platform_candidate() {
    let temp = TempDirectory::new("local-app-data");
    let directory = temp.path.join("Programs/Herdr/bin");
    create_dir(&directory);
    let candidate = install_named(&directory, executable_name());
    let env = env_map([("LOCALAPPDATA", temp.path.as_os_str().to_owned())]);
    assert_resolves(None, &env, &candidate);
}

struct RouteCase {
    kind: &'static str,
    value: &'static str,
    expected_args: Value,
    expected_socket: Value,
    expected_session: Value,
}

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agentdeck-herdr-fake"))
}

fn scenario_spec(scenario: &str, limits: CommandLimits) -> CommandSpec {
    CommandSpec {
        executable: fake_binary(),
        args: vec![OsString::from("--version")],
        env_set: vec![(
            OsString::from("AGENTDECK_FAKE_SCENARIO"),
            OsString::from(scenario),
        )],
        env_remove: Vec::new(),
        limits,
        label: format!("fake {scenario}"),
    }
}

async fn run_scenario(
    scenario: &str,
    limits: CommandLimits,
) -> Result<CommandOutput, ProcessError> {
    TokioProcessRunner
        .run(scenario_spec(scenario, limits), standalone_permit())
        .await
}

fn standalone_permit() -> OwnedSemaphorePermit {
    Arc::new(Semaphore::new(1))
        .try_acquire_owned()
        .unwrap_or_else(|error| panic!("standalone process permit: {error}"))
}

fn limits(timeout: u64, stdout_bytes: usize, stderr_bytes: usize) -> CommandLimits {
    CommandLimits {
        timeout: Duration::from_secs(timeout),
        stdout_bytes,
        stderr_bytes,
    }
}

fn add_record(spec: &mut CommandSpec, record: &Path) {
    spec.env_set.push((
        OsString::from("AGENTDECK_FAKE_RECORD"),
        record.as_os_str().to_owned(),
    ));
}

fn temp_record(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "agentdeck-herdr-{name}-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

async fn read_record(record: &Path) -> Value {
    let timeout = Duration::from_secs(2);
    let started = tokio::time::Instant::now();
    let mut last_observation = "file not found".to_owned();
    loop {
        match std::fs::read(record) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => return value,
                Err(error) => last_observation = format!("incomplete JSON: {error}"),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not read fake record {}: {error}", record.display()),
        }
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for complete fake record {} ({last_observation})",
            record.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_recorded_process_reaped(record: &Path) {
    #[cfg(unix)]
    {
        let value = read_record(record).await;
        let pid = record_pid(&value);
        assert!(!pid_alive(pid), "fake process {pid} was not reaped");
    }
    #[cfg(not(unix))]
    let _ = read_record(record).await;
    remove_file(record);
}

#[cfg(unix)]
fn record_pid(value: &Value) -> u64 {
    value["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("fake record has no PID"))
}

#[cfg(unix)]
fn pid_alive(pid: u64) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap_or_else(|error| panic!("could not inspect fake PID: {error}"))
        .status
        .success()
}

#[cfg(unix)]
async fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
    let started = tokio::time::Instant::now();
    loop {
        if condition() {
            return;
        }
        assert!(
            started.elapsed() < timeout,
            "condition did not settle in {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn remove_file(path: &Path) {
    std::fs::remove_file(path)
        .unwrap_or_else(|error| panic!("could not remove test file {}: {error}", path.display()));
}

trait ResultTestExt<T> {
    fn error_or_panic(self) -> ProcessError;
}

impl<T> ResultTestExt<T> for Result<T, ProcessError> {
    fn error_or_panic(self) -> ProcessError {
        match self {
            Ok(_) => panic!("fake process unexpectedly succeeded"),
            Err(error) => error,
        }
    }
}

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "agentdeck-herdr-resolver-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        create_dir(&path);
        Self { path }
    }

    fn install(&self, name: &str) -> PathBuf {
        install_named(&self.path, name)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).unwrap_or_else(|error| {
            panic!(
                "could not remove test directory {}: {error}",
                self.path.display()
            )
        });
    }
}

fn create_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap_or_else(|error| {
        panic!(
            "could not create test directory {}: {error}",
            path.display()
        )
    });
}

fn install_named(directory: &Path, name: &str) -> PathBuf {
    let destination = directory.join(name);
    std::fs::copy(fake_binary(), &destination).unwrap_or_else(|error| {
        panic!(
            "could not install fake executable {}: {error}",
            destination.display()
        )
    });
    destination
}

fn executable_name() -> &'static str {
    if cfg!(windows) { "herdr.exe" } else { "herdr" }
}

fn env_map<const N: usize>(entries: [(&str, OsString); N]) -> HashMap<String, OsString> {
    entries
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

fn assert_resolves(
    explicit: Option<&Path>,
    environment: &HashMap<String, OsString>,
    expected: &Path,
) {
    let resolved =
        resolve_herdr_binary_with(explicit, |key| environment.get(key).cloned(), Vec::new())
            .unwrap_or_else(|error| panic!("resolver failed: {error}"));
    let expected = std::fs::canonicalize(expected)
        .unwrap_or_else(|error| panic!("could not canonicalize expected executable: {error}"));
    assert!(resolved.is_absolute());
    assert_eq!(resolved, expected);
}

fn assert_not_found(explicit: Option<&Path>, environment: &HashMap<String, OsString>) {
    assert!(matches!(
        resolve_herdr_binary_with(explicit, |key| environment.get(key).cloned(), Vec::new(),),
        Err(ProcessError::NotFound { .. })
    ));
}
