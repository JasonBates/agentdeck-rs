//! CodexBar capacity adapter.
//!
//! Provider output is untrusted: command output is bounded, never included in an
//! error or capability, and only the two supported provider names reach the deck.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep_until},
};

use agentdeck_core::{
    CapabilityBackend, CapabilityReason, CapabilityState, CapabilityStatus, CapacityFeed,
    CapacityProvider, CapacityWindow,
};

use crate::config::{CapacityBackend as ConfigBackend, CapacityConfig};

use super::{capability, codexbar_setup_hint};

pub const CODEXBAR_ARGS: [&str; 4] = ["usage", "--provider", "both", "--json"];
pub const INITIAL_REFRESH_DELAY: Duration = Duration::from_secs(1);
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const PROCESS_LIMITS: ProcessLimits = ProcessLimits {
    timeout: Duration::from_secs(120),
    stdout_limit: 256 * 1024,
    stderr_limit: 64 * 1024,
};
const CACHE_VERSION: u32 = 2;
const MAX_WINDOW_MINUTES: i64 = 31 * 24 * 60;
const MAX_RESET_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub struct CapacityOutcome {
    pub capability: CapabilityStatus,
    pub feed: CapacityFeed,
    /// Typed collection times are adapter metadata, not an unversioned wire claim.
    /// A mixed fresh/stale feed has no misleading single collection timestamp.
    pub provider_collected_at: BTreeMap<String, i64>,
    pub collected_at: Option<i64>,
}

impl CapacityOutcome {
    fn disabled() -> Self {
        Self {
            capability: capability(
                CapabilityState::Disabled,
                Some(CapabilityBackend::Codexbar),
                Some(CapabilityReason::ProviderDisabled),
                None,
            ),
            feed: empty_feed("disabled"),
            provider_collected_at: BTreeMap::new(),
            collected_at: None,
        }
    }

    fn missing() -> Self {
        Self {
            capability: capability(
                CapabilityState::Missing,
                Some(CapabilityBackend::Codexbar),
                Some(CapabilityReason::ProviderMissing),
                Some(codexbar_setup_hint()),
            ),
            feed: empty_feed("provider_missing"),
            provider_collected_at: BTreeMap::new(),
            collected_at: None,
        }
    }

    fn unsupported() -> Self {
        Self {
            capability: capability(
                CapabilityState::Unsupported,
                Some(CapabilityBackend::Codexbar),
                Some(CapabilityReason::Unsupported),
                None,
            ),
            feed: empty_feed("unsupported"),
            provider_collected_at: BTreeMap::new(),
            collected_at: None,
        }
    }

    fn error(reason: CapabilityReason, cached: Option<&CapacityCache>) -> Self {
        let (providers, provider_collected_at) = cached.map_or_else(
            || (Vec::new(), BTreeMap::new()),
            |cache| {
                (
                    cache
                        .providers
                        .values()
                        .map(CachedCapacityProvider::stale_provider)
                        .collect(),
                    cache.provider_collected_at(),
                )
            },
        );
        Self {
            capability: capability(
                CapabilityState::Error,
                Some(CapabilityBackend::Codexbar),
                Some(reason),
                None,
            ),
            feed: CapacityFeed {
                ok: false,
                reason: Some(capability_reason_code(reason).to_owned()),
                providers,
            },
            provider_collected_at,
            collected_at: None,
        }
    }
}

const fn capability_reason_code(reason: CapabilityReason) -> &'static str {
    match reason {
        CapabilityReason::ProviderMissing => "provider_missing",
        CapabilityReason::ModelMissing => "model_missing",
        CapabilityReason::ModelUnconfigured => "model_unconfigured",
        CapabilityReason::ProviderDisabled => "provider_disabled",
        CapabilityReason::ProviderFailed => "provider_failed",
        CapabilityReason::ConnectionFailed => "connection_failed",
        CapabilityReason::Timeout => "timeout",
        CapabilityReason::InvalidData => "invalid_data",
        CapabilityReason::Unsupported => "unsupported",
        CapabilityReason::SamplerFailed => "sampler_failed",
        CapabilityReason::StateWriteFailed => "state_write_failed",
        CapabilityReason::NotRefreshed => "not_refreshed",
    }
}

fn empty_feed(reason: &str) -> CapacityFeed {
    CapacityFeed {
        ok: false,
        reason: Some(reason.to_owned()),
        providers: Vec::new(),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct CapacityCache {
    version: u32,
    providers: BTreeMap<String, CachedCapacityProvider>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct CachedCapacityProvider {
    collected_at: i64,
    provider: CapacityProvider,
}

impl CapacityCache {
    fn updated(previous: Option<&Self>, collected_at: i64, providers: &[CapacityProvider]) -> Self {
        let mut entries = previous.map_or_else(BTreeMap::new, |cache| cache.providers.clone());
        for provider in providers {
            entries.insert(
                provider.name.clone(),
                CachedCapacityProvider {
                    collected_at,
                    provider: provider.clone(),
                },
            );
        }
        Self {
            version: CACHE_VERSION,
            providers: entries,
        }
    }

    fn provider_collected_at(&self) -> BTreeMap<String, i64> {
        self.providers
            .iter()
            .map(|(name, entry)| (name.clone(), entry.collected_at))
            .collect()
    }
}

impl CachedCapacityProvider {
    fn stale_provider(&self) -> CapacityProvider {
        let mut provider = self.provider.clone();
        provider.note = Some(format!("last good — stale since {}", self.collected_at));
        provider
    }
}

/// Versioned, atomic cache. Corrupt and mismatched files are intentionally treated
/// as no cache, not as a provider error: the next successful refresh repairs them.
#[derive(Clone, Debug)]
pub struct CapacityCacheStore {
    path: PathBuf,
}

impl CapacityCacheStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn read(&self) -> Option<CapacityCache> {
        let bytes = std::fs::read(&self.path).ok()?;
        let cache: CapacityCache = serde_json::from_slice(&bytes).ok()?;
        (cache.version == CACHE_VERSION).then_some(cache)
    }

    fn write(&self, cache: &CapacityCache) -> Result<(), CapacityError> {
        let parent = self.path.parent().ok_or(CapacityError::StateWrite)?;
        std::fs::create_dir_all(parent).map_err(|_| CapacityError::StateWrite)?;
        let bytes = serde_json::to_vec(cache).map_err(|_| CapacityError::StateWrite)?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|_| CapacityError::StateWrite)?;
        #[cfg(unix)]
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| CapacityError::StateWrite)?;
        use std::io::Write as _;
        temporary
            .write_all(&bytes)
            .map_err(|_| CapacityError::StateWrite)?;
        temporary.flush().map_err(|_| CapacityError::StateWrite)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| CapacityError::StateWrite)?;
        temporary
            .persist(&self.path)
            .map_err(|_| CapacityError::StateWrite)?;
        sync_parent_directory(parent)?;
        Ok(())
    }
}

fn sync_parent_directory(parent: &Path) -> Result<(), CapacityError> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| CapacityError::StateWrite)
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexBarCapture {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CapacityError {
    #[error("CodexBar was not found")]
    NotFound,
    #[error("CodexBar command timed out")]
    Timeout,
    #[error("CodexBar command output exceeded its bound")]
    OutputLimit,
    #[error("CodexBar command could not be run")]
    Process,
    #[error("CodexBar returned malformed capacity data")]
    InvalidData,
    #[error("capacity cache could not be written")]
    StateWrite,
}

/// Bounds are kept together so the production runner has one auditable policy and
/// Unix fake-process tests can exercise timeout and output cleanup promptly.
#[derive(Clone, Copy, Debug)]
struct ProcessLimits {
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

#[async_trait]
pub trait CodexBarRunner: Send + Sync {
    async fn usage(&self, executable: &Path) -> Result<CodexBarCapture, CapacityError>;
}

/// Tokio implementation with explicit argv, bounded concurrent stream draining,
/// timeout, kill, and reap. `success` is descriptive only: CodexBar can write a
/// valid result before a non-zero exit, so parsing decides the outcome.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioCodexBarRunner;

#[async_trait]
impl CodexBarRunner for TokioCodexBarRunner {
    async fn usage(&self, executable: &Path) -> Result<CodexBarCapture, CapacityError> {
        let executable = executable.to_path_buf();
        let (cancel_sender, cancel_receiver) = oneshot::channel();
        let mut cancellation = CancelOnDrop(Some(cancel_sender));
        let supervisor = tokio::spawn(run_codexbar_process(
            executable,
            cancel_receiver,
            PROCESS_LIMITS,
        ));
        let result = supervisor.await.map_err(|_| CapacityError::Process)?;
        cancellation.0.take();
        result
    }
}

struct CancelOnDrop(Option<oneshot::Sender<()>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn run_codexbar_process(
    executable: PathBuf,
    mut cancellation: oneshot::Receiver<()>,
    limits: ProcessLimits,
) -> Result<CodexBarCapture, CapacityError> {
    let mut command = Command::new(executable);
    command
        .args(CODEXBAR_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CapacityError::NotFound
        } else {
            CapacityError::Process
        }
    })?;
    let stdout = child.stdout.take().ok_or(CapacityError::Process)?;
    let stderr = child.stderr.take().ok_or(CapacityError::Process)?;
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut stdout_task = spawn_reader(stdout, Stream::Stdout, limits.stdout_limit, sender.clone());
    let mut stderr_task = spawn_reader(stderr, Stream::Stderr, limits.stderr_limit, sender);
    let deadline = Instant::now() + limits.timeout;
    let mut status = None;
    let mut captured_stdout = None;
    let mut captured_stderr = None;
    while status.is_none() || captured_stdout.is_none() || captured_stderr.is_none() {
        tokio::select! {
            _ = &mut cancellation => {
                terminate_and_reap(&mut child).await;
                abort_readers(&mut stdout_task, &mut stderr_task).await;
                return Err(CapacityError::Process);
            }
            message = receiver.recv() => match message {
                Some(ReaderMessage { stream, result: Ok(bytes) }) => match stream {
                    Stream::Stdout => captured_stdout = Some(bytes),
                    Stream::Stderr => captured_stderr = Some(bytes),
                },
                Some(ReaderMessage { result: Err(error), .. }) => {
                    terminate_and_reap(&mut child).await;
                    abort_readers(&mut stdout_task, &mut stderr_task).await;
                    return Err(error);
                }
                None => {
                    terminate_and_reap(&mut child).await;
                    abort_readers(&mut stdout_task, &mut stderr_task).await;
                    return Err(CapacityError::Process);
                }
            },
            () = sleep_until(deadline) => {
                terminate_and_reap(&mut child).await;
                abort_readers(&mut stdout_task, &mut stderr_task).await;
                return Err(CapacityError::Timeout);
            }
        }
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(_) => {
                    terminate_and_reap(&mut child).await;
                    abort_readers(&mut stdout_task, &mut stderr_task).await;
                    return Err(CapacityError::Process);
                }
            };
        }
    }
    join_reader(stdout_task).await?;
    join_reader(stderr_task).await?;
    let status = status.ok_or(CapacityError::Process)?;
    Ok(CodexBarCapture {
        success: status.success(),
        stdout: captured_stdout.ok_or(CapacityError::Process)?,
        stderr: captured_stderr.ok_or(CapacityError::Process)?,
    })
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

struct ReaderMessage {
    stream: Stream,
    result: Result<Vec<u8>, CapacityError>,
}

fn spawn_reader(
    reader: impl AsyncRead + Unpin + Send + 'static,
    stream: Stream,
    limit: usize,
    sender: mpsc::UnboundedSender<ReaderMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = read_capped(reader, limit).await;
        let _ = sender.send(ReaderMessage { stream, result });
    })
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, CapacityError> {
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| CapacityError::Process)?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(count) > limit {
            return Err(CapacityError::OutputLimit);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

async fn terminate_and_reap(child: &mut tokio::process::Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

async fn abort_readers(stdout: &mut JoinHandle<()>, stderr: &mut JoinHandle<()>) {
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
}

async fn join_reader(reader: JoinHandle<()>) -> Result<(), CapacityError> {
    reader.await.map_err(|_| CapacityError::Process)
}

pub trait CodexBarLocator: Send + Sync {
    fn locate(&self) -> Option<PathBuf>;
}

/// CodexBar is a macOS integration today. Keeping this policy explicit avoids a
/// Windows/Linux PATH miss being presented as an installable capability.
pub trait CapacityPlatform: Send + Sync {
    fn supports_codexbar(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeCapacityPlatform;

impl CapacityPlatform for NativeCapacityPlatform {
    fn supports_codexbar(&self) -> bool {
        cfg!(target_os = "macos")
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
struct TestCapacityPlatform(bool);

#[cfg(test)]
impl CapacityPlatform for TestCapacityPlatform {
    fn supports_codexbar(&self) -> bool {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PathCodexBarLocator;

impl CodexBarLocator for PathCodexBarLocator {
    fn locate(&self) -> Option<PathBuf> {
        let paths = std::env::var_os("PATH")?;
        std::env::split_paths(&paths)
            .map(|directory| directory.join(executable_name()))
            .find(|candidate| is_executable(candidate))
    }
}

fn is_executable(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        candidate
            .metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn executable_name() -> OsString {
    if cfg!(windows) {
        OsString::from("codexbar.exe")
    } else {
        OsString::from("codexbar")
    }
}

/// Schedule policy is clock-injected by its caller: one refresh one second after
/// startup, then five-minute intervals. It never starts a second command while one
/// is running.
#[derive(Debug)]
pub struct CapacityRefresher<R> {
    executable: PathBuf,
    runner: R,
    cache_store: CapacityCacheStore,
    running: AtomicBool,
    next_refresh_at: Mutex<i64>,
    current: Mutex<CapacityOutcome>,
}

struct RefreshPermit<'a>(&'a AtomicBool);

impl Drop for RefreshPermit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<R: CodexBarRunner> CapacityRefresher<R> {
    #[must_use]
    pub fn new(
        executable: PathBuf,
        runner: R,
        cache_store: CapacityCacheStore,
        started_at: i64,
    ) -> Self {
        let cached = cache_store.read();
        let initial = cached.as_ref().map_or_else(
            || CapacityOutcome::error(CapabilityReason::NotRefreshed, None),
            |cache| CapacityOutcome::error(CapabilityReason::NotRefreshed, Some(cache)),
        );
        Self {
            executable,
            runner,
            cache_store,
            running: AtomicBool::new(false),
            next_refresh_at: Mutex::new(started_at + INITIAL_REFRESH_DELAY.as_secs() as i64),
            current: Mutex::new(initial),
        }
    }

    pub async fn current(&self) -> CapacityOutcome {
        self.current.lock().await.clone()
    }

    pub async fn refresh_if_due(&self, now: i64) -> CapacityOutcome {
        if now < *self.next_refresh_at.lock().await {
            return self.current().await;
        }
        if self.running.swap(true, Ordering::AcqRel) {
            return self.current().await;
        }
        let _permit = RefreshPermit(&self.running);
        let result = self.refresh(now).await;
        *self.next_refresh_at.lock().await = now + REFRESH_INTERVAL.as_secs() as i64;
        *self.current.lock().await = result.clone();
        result
    }

    async fn refresh(&self, now: i64) -> CapacityOutcome {
        let cached = self.cache_store.read();
        let capture = match self.runner.usage(&self.executable).await {
            Ok(capture) => capture,
            Err(error) => return CapacityOutcome::error(reason_for(&error), cached.as_ref()),
        };
        let providers = match parse_codexbar_usage(&capture.stdout) {
            Ok(providers) if !providers.is_empty() => providers,
            Ok(_) | Err(_) => {
                return CapacityOutcome::error(CapabilityReason::InvalidData, cached.as_ref());
            }
        };
        let cache = CapacityCache::updated(cached.as_ref(), now, &providers);
        if self.cache_store.write(&cache).is_err() {
            return CapacityOutcome::error(CapabilityReason::StateWriteFailed, cached.as_ref());
        }
        let failed = failed_provider_names(&capture.stdout);
        if !failed.is_empty() {
            let mut displayed = providers;
            let provider_collected_at = cache.provider_collected_at();
            for name in failed {
                if let Some(previous) = cached.as_ref().and_then(|cache| cache.providers.get(name))
                {
                    displayed.push(previous.stale_provider());
                } else {
                    displayed.push(CapacityProvider {
                        name: name.to_owned(),
                        percent_used: None,
                        label: String::new(),
                        windows: Vec::new(),
                        note: Some("provider data unavailable".to_owned()),
                    });
                }
            }
            return CapacityOutcome {
                capability: capability(
                    CapabilityState::Error,
                    Some(CapabilityBackend::Codexbar),
                    Some(CapabilityReason::ProviderFailed),
                    None,
                ),
                feed: CapacityFeed {
                    ok: false,
                    reason: Some("provider_failed".to_owned()),
                    providers: displayed,
                },
                provider_collected_at,
                collected_at: None,
            };
        }
        CapacityOutcome {
            capability: capability(
                CapabilityState::Available,
                Some(CapabilityBackend::Codexbar),
                None,
                None,
            ),
            feed: CapacityFeed {
                ok: true,
                reason: None,
                providers,
            },
            provider_collected_at: cache.provider_collected_at(),
            collected_at: Some(now),
        }
    }
}

fn reason_for(error: &CapacityError) -> CapabilityReason {
    match error {
        CapacityError::NotFound => CapabilityReason::ProviderMissing,
        CapacityError::Timeout => CapabilityReason::Timeout,
        CapacityError::InvalidData | CapacityError::OutputLimit => CapabilityReason::InvalidData,
        CapacityError::StateWrite => CapabilityReason::StateWriteFailed,
        CapacityError::Process => CapabilityReason::ProviderFailed,
    }
}

/// Explicit configuration/discovery boundary. Disabled and unsupported outcomes do
/// not obtain a locator result and therefore cannot execute a subprocess.
pub fn select_capacity<R: CodexBarRunner>(
    config: &CapacityConfig,
    platform: &dyn CapacityPlatform,
    locator: &dyn CodexBarLocator,
    runner: R,
    cache_store: CapacityCacheStore,
    started_at: i64,
) -> CapacitySelection<R> {
    if config.backend == ConfigBackend::Off {
        return CapacitySelection::Inactive(CapacityOutcome::disabled());
    }
    if !platform.supports_codexbar() {
        return CapacitySelection::Inactive(CapacityOutcome::unsupported());
    }
    let Some(executable) = locator.locate() else {
        return CapacitySelection::Inactive(CapacityOutcome::missing());
    };
    CapacitySelection::Active(CapacityRefresher::new(
        executable,
        runner,
        cache_store,
        started_at,
    ))
}

pub enum CapacitySelection<R> {
    Inactive(CapacityOutcome),
    Active(CapacityRefresher<R>),
}

/// Parse a complete first JSON array after any CodexBar diagnostic prefix. A non-zero
/// command exit is intentionally irrelevant to this function.
pub fn parse_codexbar_usage(stdout: &[u8]) -> Result<Vec<CapacityProvider>, CapacityError> {
    let array = first_json_array(stdout).ok_or(CapacityError::InvalidData)?;
    let mut providers = BTreeMap::new();
    for entry in array {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let Some(name) = object
            .get("provider")
            .and_then(Value::as_str)
            .and_then(provider_name)
        else {
            continue;
        };
        if object.get("error").is_some() {
            continue;
        }
        let Some(usage) = object.get("usage").and_then(Value::as_object) else {
            continue;
        };
        let pace = object.get("pace").and_then(Value::as_object);
        let mut windows = Vec::new();
        for key in ["primary", "secondary"] {
            let Some(window) = usage.get(key).and_then(Value::as_object) else {
                continue;
            };
            let Some(used) = window
                .get("usedPercent")
                .and_then(Value::as_f64)
                .filter(|value| sane_percentage(*value))
            else {
                continue;
            };
            let minutes = window
                .get("windowMinutes")
                .and_then(Value::as_i64)
                .filter(|minutes| (1..=MAX_WINDOW_MINUTES).contains(minutes));
            let Some(minutes) = minutes else { continue };
            let span = if minutes >= 10_080 {
                "wk".to_owned()
            } else if minutes >= 60 {
                format!("{}h", minutes / 60)
            } else {
                format!("{minutes}m")
            };
            let expected = pace
                .and_then(|pace| pace.get(key))
                .and_then(Value::as_object)
                .and_then(|item| item.get("expectedUsedPercent"))
                .and_then(Value::as_f64)
                .filter(|value| sane_percentage(*value));
            windows.push(CapacityWindow {
                span,
                used,
                expected,
                resets: window
                    .get("resetDescription")
                    .and_then(Value::as_str)
                    .filter(|value| valid_reset(value))
                    .map(ToOwned::to_owned),
            });
        }
        if windows.is_empty() {
            continue;
        }
        let label = windows
            .iter()
            .map(|window| format!("{} {:.0}%", window.span, window.used))
            .collect::<Vec<_>>()
            .join(" ");
        providers.insert(
            name.to_owned(),
            CapacityProvider {
                name: name.to_owned(),
                percent_used: windows.first().map(|window| window.used),
                label,
                windows,
                note: None,
            },
        );
    }
    Ok(providers.into_values().collect())
}

fn first_json_array(bytes: &[u8]) -> Option<Vec<Value>> {
    let mut start = None;
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if start.is_none() {
            if byte == b'[' {
                start = Some(index);
                depth = 1;
                quoted = false;
                escaped = false;
            }
            continue;
        }
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'[' => depth = depth.saturating_add(1),
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let candidate = &bytes[start?..=index];
                    if let Ok(Value::Array(array)) = serde_json::from_slice(candidate) {
                        return Some(array);
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    None
}

fn sane_percentage(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

fn valid_reset(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_RESET_BYTES && !value.chars().any(char::is_control)
}

fn failed_provider_names(bytes: &[u8]) -> Vec<&'static str> {
    first_json_array(bytes)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let object = entry.as_object()?;
            object
                .get("error")
                .is_some()
                .then(|| {
                    object
                        .get("provider")
                        .and_then(Value::as_str)
                        .and_then(provider_name)
                })
                .flatten()
        })
        .collect()
}

fn provider_name(value: &str) -> Option<&'static str> {
    if value.eq_ignore_ascii_case("claude") {
        Some("claude")
    } else if value.eq_ignore_ascii_case("codex") {
        Some("codex")
    } else {
        None
    }
}

#[must_use]
pub fn unix_now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt as _;

    #[derive(Clone)]
    struct FakeRunner {
        calls: Arc<AtomicUsize>,
        result: Result<CodexBarCapture, CapacityError>,
    }
    #[async_trait]
    impl CodexBarRunner for FakeRunner {
        async fn usage(&self, executable: &Path) -> Result<CodexBarCapture, CapacityError> {
            assert_eq!(executable, Path::new("/fixture/codexbar"));
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.result.clone()
        }
    }
    struct Missing;
    impl CodexBarLocator for Missing {
        fn locate(&self) -> Option<PathBuf> {
            None
        }
    }
    struct Found;
    impl CodexBarLocator for Found {
        fn locate(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/fixture/codexbar"))
        }
    }

    #[derive(Clone)]
    struct SlowThenReady {
        calls: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl CodexBarRunner for SlowThenReady {
        async fn usage(&self, _executable: &Path) -> Result<CodexBarCapture, CapacityError> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                self.entered.notify_one();
                pending().await
            }
            Ok(CodexBarCapture {
                success: true,
                stdout: BOTH.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    fn cache() -> CapacityCacheStore {
        CapacityCacheStore::new(
            tempdir()
                .unwrap_or_else(|error| panic!("temporary cache directory: {error}"))
                .keep()
                .join("capacity.json"),
        )
    }

    #[cfg(unix)]
    fn executable_script(directory: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))
            .unwrap_or_else(|error| panic!("write fake CodexBar: {error}"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("make fake CodexBar executable: {error}"));
        path
    }

    #[cfg(unix)]
    fn pid_recording_sleep_script(directory: &Path, name: &str) -> (PathBuf, PathBuf) {
        let pid_path = directory.join(format!("{name}.pid"));
        let body = format!("echo \"$$\" > \"{}\"\nexec sleep 30", pid_path.display());
        (executable_script(directory, name, &body), pid_path)
    }
    fn runner(stdout: &str) -> FakeRunner {
        FakeRunner {
            calls: Arc::new(AtomicUsize::new(0)),
            result: Ok(CodexBarCapture {
                success: true,
                stdout: stdout.as_bytes().to_vec(),
                stderr: b"secret stderr".to_vec(),
            }),
        }
    }
    const BOTH: &str = r#"[{"provider":"claude","usage":{"primary":{"usedPercent":40,"windowMinutes":300},"secondary":{"usedPercent":10,"windowMinutes":10080}},"pace":{"primary":{"expectedUsedPercent":20}}},{"provider":"codex","usage":{"primary":{"usedPercent":2.5,"windowMinutes":60}}}]"#;

    #[test]
    fn parser_accepts_both_one_noisy_and_only_safe_providers() {
        let both = parse_codexbar_usage(BOTH.as_bytes())
            .unwrap_or_else(|error| panic!("both provider fixture: {error}"));
        assert_eq!(both.len(), 2);
        let noisy = parse_codexbar_usage(br#"notice [ignored] [ {"provider":"codex","usage":{"primary":{"usedPercent":2,"windowMinutes":60}}}]"#)
            .unwrap_or_else(|error| panic!("noisy fixture: {error}"));
        assert_eq!(noisy.len(), 1);
        let unknown = parse_codexbar_usage(br#"[{"provider":"unknown-secret","usage":{"primary":{"usedPercent":2,"windowMinutes":60}}}]"#)
            .unwrap_or_else(|error| panic!("unknown provider fixture: {error}"));
        assert!(unknown.is_empty());
        assert_eq!(
            failed_provider_names(br#"[{"provider":"codex","error":{"message":"private"}}]"#),
            vec!["codex"]
        );
    }

    #[test]
    fn parser_rejects_malformed_or_truncated_json() {
        for bytes in [
            b"not json".as_slice(),
            b"[{\"provider\":\"claude\"".as_slice(),
        ] {
            assert_eq!(parse_codexbar_usage(bytes), Err(CapacityError::InvalidData));
        }
    }

    #[test]
    fn parser_rejects_implausible_window_percent_and_reset_values() {
        for value in [
            r#"[{"provider":"claude","usage":{"primary":{"usedPercent":-1,"windowMinutes":60}}}]"#,
            r#"[{"provider":"claude","usage":{"primary":{"usedPercent":101,"windowMinutes":60}}}]"#,
            r#"[{"provider":"claude","usage":{"primary":{"usedPercent":1,"windowMinutes":0}}}]"#,
        ] {
            assert!(
                parse_codexbar_usage(value.as_bytes())
                    .unwrap_or_else(|error| panic!("well-formed fixture: {error}"))
                    .is_empty()
            );
        }
        assert!(!valid_reset("resets\nin 5h"));
        assert!(valid_reset("resets in 5h"));
    }

    #[tokio::test]
    async fn capped_reader_rejects_excess_output_without_retaining_it() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(b"this is longer than eight bytes")
                .await
                .unwrap_or_else(|error| panic!("fixture write: {error}"));
        });
        assert_eq!(
            read_capped(reader, 8).await,
            Err(CapacityError::OutputLimit)
        );
        writer_task
            .await
            .unwrap_or_else(|error| panic!("fixture writer join: {error}"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_runner_uses_exact_argv_against_a_fake_executable() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("temporary script directory: {error}"));
        let script = executable_script(
            directory.path(),
            "codexbar",
            "printf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s' '[{\"provider\":\"codex\",\"usage\":{\"primary\":{\"usedPercent\":1,\"windowMinutes\":60}}}]'",
        );
        let output = TokioCodexBarRunner
            .usage(&script)
            .await
            .unwrap_or_else(|error| panic!("fake CodexBar execution: {error}"));
        assert!(output.success);
        assert_eq!(
            std::fs::read_to_string(format!("{}.args", script.display()))
                .unwrap_or_else(|error| panic!("read argv record: {error}")),
            "usage\n--provider\nboth\n--json\n"
        );
        assert_eq!(
            parse_codexbar_usage(&output.stdout).map(|providers| providers.len()),
            Ok(1)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_runner_kills_and_reaps_its_fake_process() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("temporary script directory: {error}"));
        let (script, pid_path) = pid_recording_sleep_script(directory.path(), "slow-codexbar");
        let runner_script = script.clone();
        let task = tokio::spawn(async move { TokioCodexBarRunner.usage(&runner_script).await });
        for _ in 0..100 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = std::fs::read_to_string(&pid_path)
            .unwrap_or_else(|error| panic!("fake process must record its pid: {error}"));
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!process_is_alive(pid.trim()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runner_timeout_kills_and_reaps_its_fake_process() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("temporary script directory: {error}"));
        let (script, pid_path) = pid_recording_sleep_script(directory.path(), "slow-codexbar");
        let (_cancel, receiver) = oneshot::channel();
        assert_eq!(
            run_codexbar_process(
                script,
                receiver,
                ProcessLimits {
                    timeout: Duration::from_secs(5),
                    stdout_limit: 64,
                    stderr_limit: 64,
                },
            )
            .await,
            Err(CapacityError::Timeout)
        );
        let pid = std::fs::read_to_string(pid_path)
            .unwrap_or_else(|error| panic!("timed-out fake process must record its pid: {error}"));
        assert!(!process_is_alive(pid.trim()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runner_enforces_output_cap_without_returning_output() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("temporary script directory: {error}"));
        let script = executable_script(
            directory.path(),
            "verbose-codexbar",
            "printf 0123456789abcdef",
        );
        let (_cancel, receiver) = oneshot::channel();
        assert_eq!(
            run_codexbar_process(
                script,
                receiver,
                ProcessLimits {
                    timeout: Duration::from_secs(1),
                    stdout_limit: 8,
                    stderr_limit: 8,
                },
            )
            .await,
            Err(CapacityError::OutputLimit)
        );
    }

    #[cfg(unix)]
    fn process_is_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| panic!("inspect fake process: {error}"))
            .success()
    }

    #[tokio::test]
    async fn off_and_missing_never_run_a_command_and_hint_is_exact() {
        let run = runner(BOTH);
        let calls = run.calls.clone();
        let off = select_capacity(
            &CapacityConfig {
                backend: ConfigBackend::Off,
            },
            &TestCapacityPlatform(true),
            &Found,
            run,
            cache(),
            0,
        );
        let CapacitySelection::Inactive(outcome) = off else {
            panic!("off must be inactive")
        };
        assert_eq!(outcome.capability.state, CapabilityState::Disabled);
        assert!(outcome.capability.setup_hint.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let missing = select_capacity(
            &CapacityConfig::default(),
            &TestCapacityPlatform(true),
            &Missing,
            runner(BOTH),
            cache(),
            0,
        );
        let CapacitySelection::Inactive(outcome) = missing else {
            panic!("missing must be inactive")
        };
        assert_eq!(outcome.capability.state, CapabilityState::Missing);
        let Some(setup_hint) = outcome.capability.setup_hint else {
            panic!("missing capacity must include a setup hint")
        };
        assert_eq!(
            setup_hint.message,
            "Install CodexBar to show Claude and Codex quota."
        );
    }

    #[tokio::test]
    async fn unsupported_has_no_hint_and_refresh_schedule_is_bounded() {
        let unsupported = select_capacity(
            &CapacityConfig::default(),
            &TestCapacityPlatform(false),
            &Found,
            runner(BOTH),
            cache(),
            0,
        );
        let CapacitySelection::Inactive(outcome) = unsupported else {
            panic!("unsupported must be inactive")
        };
        assert_eq!(outcome.capability.state, CapabilityState::Unsupported);
        assert!(outcome.capability.setup_hint.is_none());
        let run = runner(BOTH);
        let calls = run.calls.clone();
        let selected = select_capacity(
            &CapacityConfig::default(),
            &TestCapacityPlatform(true),
            &Found,
            run,
            cache(),
            100,
        );
        let CapacitySelection::Active(refresh) = selected else {
            panic!("must be active")
        };
        let initial = refresh.refresh_if_due(100).await;
        assert_eq!(
            initial.capability.reason,
            Some(CapabilityReason::NotRefreshed)
        );
        assert_eq!(initial.feed.reason.as_deref(), Some("not_refreshed"));
        assert!(refresh.refresh_if_due(101).await.feed.ok);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let _ = refresh.refresh_if_due(400).await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let _ = refresh.refresh_if_due(401).await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn nonzero_valid_json_is_accepted_and_cache_becomes_stale_on_failure() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("temporary cache directory: {error}"));
        let store = CapacityCacheStore::new(directory.path().join("capacity.json"));
        let first = FakeRunner {
            calls: Arc::new(AtomicUsize::new(0)),
            result: Ok(CodexBarCapture {
                success: false,
                stdout: BOTH.as_bytes().to_vec(),
                stderr: vec![],
            }),
        };
        let refresh =
            CapacityRefresher::new(PathBuf::from("/fixture/codexbar"), first, store.clone(), 0);
        assert!(refresh.refresh_if_due(1).await.feed.ok);
        let failed = CapacityRefresher::new(
            PathBuf::from("/fixture/codexbar"),
            FakeRunner {
                calls: Arc::new(AtomicUsize::new(0)),
                result: Err(CapacityError::Timeout),
            },
            store,
            2,
        );
        let outcome = failed.refresh_if_due(3).await;
        assert_eq!(outcome.capability.reason, Some(CapabilityReason::Timeout));
        assert_eq!(outcome.feed.reason.as_deref(), Some("timeout"));
        assert_eq!(outcome.feed.providers.len(), 2);
        let Some(note) = outcome.feed.providers[0].note.as_deref() else {
            panic!("stale provider must say that it is stale")
        };
        assert!(note.starts_with("last good — stale since 1"));
    }

    #[tokio::test]
    async fn cancelled_refresh_releases_admission_for_a_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let refresh = Arc::new(CapacityRefresher::new(
            PathBuf::from("/fixture/codexbar"),
            SlowThenReady {
                calls: calls.clone(),
                entered: entered.clone(),
            },
            cache(),
            0,
        ));
        let pending_refresh = {
            let refresh = refresh.clone();
            tokio::spawn(async move { refresh.refresh_if_due(1).await })
        };
        entered.notified().await;
        pending_refresh.abort();
        let _ = pending_refresh.await;
        assert!(!refresh.running.load(Ordering::Acquire));
        assert!(refresh.refresh_if_due(2).await.feed.ok);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn cache_rejects_corruption_and_version_mismatch() {
        let directory =
            tempdir().unwrap_or_else(|error| panic!("temporary cache directory: {error}"));
        let path = directory.path().join("capacity.json");
        std::fs::write(&path, b"not-json")
            .unwrap_or_else(|error| panic!("write corrupt cache fixture: {error}"));
        let store = CapacityCacheStore::new(path.clone());
        assert!(store.read().is_none());
        std::fs::write(&path, br#"{"version":99,"collected_at":1,"providers":[]}"#)
            .unwrap_or_else(|error| panic!("write future cache fixture: {error}"));
        assert!(store.read().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_replace_is_readable_and_restrictive_after_sync() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory =
            tempdir().unwrap_or_else(|error| panic!("temporary cache directory: {error}"));
        let path = directory.path().join("capacity.json");
        let store = CapacityCacheStore::new(path.clone());
        let providers = parse_codexbar_usage(BOTH.as_bytes())
            .unwrap_or_else(|error| panic!("provider fixture: {error}"));
        let cache = CapacityCache::updated(None, 42, &providers);
        store
            .write(&cache)
            .unwrap_or_else(|error| panic!("durable cache write: {error}"));
        assert_eq!(store.read(), Some(cache));
        let mode = std::fs::metadata(path)
            .unwrap_or_else(|error| panic!("cache metadata: {error}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
