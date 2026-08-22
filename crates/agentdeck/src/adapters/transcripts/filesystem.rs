use std::{
    collections::{HashMap, VecDeque},
    fs::{File, Metadata},
    hash::Hash,
    io::{self, Read as _, Seek as _, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use agentdeck_core::{
    ContextUsage, HerdrAgentSession,
    context::{ContextOutcome, extract_context_window},
    transcript::{
        CONTEXT_TAIL_BYTES, DIGEST_TAIL_BYTES, OPENING_HEAD_BYTES, SafeRelativePath, TailRead,
        TranscriptCacheFingerprint, TranscriptKind, TranscriptLocationPlan, TranscriptOutcome,
        analyze_windows, cache_fingerprint, location_plan, select_codex_candidate,
    },
};
use async_trait::async_trait;
#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt as _;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsMaybeDirExt as _};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use tokio::{sync::Semaphore, task};

const DEFAULT_READ_CONCURRENCY: usize = 2;
const DEFAULT_CACHE_ENTRIES: usize = 128;
const MAX_CACHE_ENTRIES: usize = 4_096;
const MAX_CODEX_SCAN_ITEMS: usize = 4_096;
const CODEX_DATE_DEPTH: usize = 3;
const MAX_REQUEST_PATH_BYTES: usize = 4_096;

/// Explicit roots supplied by executable configuration or a platform adapter. This
/// module never reads a home directory or environment variable itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptRoots {
    pub claude_projects_root: PathBuf,
    pub codex_sessions_root: PathBuf,
    /// Copilot's local `session-state` directory; never a SQLite database.
    pub copilot_session_state_root: PathBuf,
}

/// A normalized request from the Herdr snapshot. Prompt content is never part of a
/// request or adapter error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptRequest {
    pub kind: TranscriptKind,
    pub session: Option<HerdrAgentSession>,
    pub cwd: String,
}

/// The independent transcript values a reconciler can use to enrich an agent.
/// `analysis` and `context` retain their own unavailable/not-yet/malformed/empty
/// state so a missing digest never fabricates context, or vice versa.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptObservation {
    pub analysis: TranscriptOutcome,
    pub context: ContextOutcome,
    pub written_at: Option<i64>,
}

impl TranscriptObservation {
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            analysis: TranscriptOutcome::Unavailable,
            context: ContextOutcome::Unavailable,
            written_at: None,
        }
    }

    #[must_use]
    pub fn not_yet_created() -> Self {
        Self {
            analysis: TranscriptOutcome::NotYetCreated,
            context: ContextOutcome::NotYetCreated,
            written_at: None,
        }
    }

    #[must_use]
    pub fn reply_key(&self) -> Option<&str> {
        let TranscriptOutcome::Ready(analysis) = &self.analysis else {
            return None;
        };
        analysis
            .digest
            .as_ref()
            .and_then(|digest| digest.last_reply_key.as_deref())
    }

    #[must_use]
    pub fn context_usage(&self) -> Option<&ContextUsage> {
        let ContextOutcome::Ready(context) = &self.context else {
            return None;
        };
        Some(context)
    }
}

/// The executable boundary used by reconciliation. Implementations must make no
/// assumptions about a global home directory, current directory, or environment.
#[async_trait]
pub trait TranscriptSource: Send + Sync {
    async fn observe(&self, request: TranscriptRequest) -> TranscriptObservation;
}

/// A full-resolution portable file timestamp used by the parse cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileTimestamp {
    pub unix_seconds: i64,
    pub nanoseconds: u32,
}

/// The exact independently usable bounded file windows. `context_tail` has at most
/// 1 MiB of payload plus its preceding-byte probe; the digest tail is derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptWindows {
    head: Vec<u8>,
    context_tail: OwnedTail,
    size: u64,
    modified: FileTimestamp,
    /// Testable accounting of bytes read from the opened descriptor, not file size.
    bytes_read: usize,
}

/// An owned internal form of core's borrowed tail-read contract.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedTail {
    preceding_byte: Option<u8>,
    bytes: Vec<u8>,
}

impl OwnedTail {
    fn new(preceding_byte: Option<u8>, bytes: Vec<u8>) -> Self {
        Self {
            preceding_byte,
            bytes,
        }
    }

    fn borrow(&self) -> TailRead<'_> {
        TailRead {
            preceding_byte: self.preceding_byte,
            bytes: &self.bytes,
        }
    }

    fn digest_tail(&self) -> TailRead<'_> {
        if self.bytes.len() <= DIGEST_TAIL_BYTES {
            return self.borrow();
        }
        let start = self.bytes.len() - DIGEST_TAIL_BYTES;
        TailRead {
            preceding_byte: Some(self.bytes[start - 1]),
            bytes: &self.bytes[start..],
        }
    }
}

impl TranscriptWindows {
    /// Constructs exact adapter windows. Alternate platform/test I/O cannot inject
    /// data beyond the core parsing contract.
    pub fn try_new(
        head: Vec<u8>,
        preceding_byte: Option<u8>,
        context_tail: Vec<u8>,
        size: u64,
        modified: FileTimestamp,
        bytes_read: usize,
    ) -> Result<Self, TranscriptIoError> {
        let expected_head = bounded_len(size, OPENING_HEAD_BYTES)?;
        let expected_tail = bounded_len(size, CONTEXT_TAIL_BYTES)?;
        let expected_probe = size > u64::try_from(CONTEXT_TAIL_BYTES).unwrap_or(u64::MAX);
        let maximum_read = head
            .len()
            .checked_add(context_tail.len())
            .and_then(|count| count.checked_add(usize::from(preceding_byte.is_some())))
            .ok_or(TranscriptIoError::BoundsExceeded)?;
        if head.len() != expected_head
            || context_tail.len() != expected_tail
            || preceding_byte.is_some() != expected_probe
            || bytes_read != maximum_read
        {
            return Err(TranscriptIoError::BoundsExceeded);
        }
        Ok(Self {
            head,
            context_tail: OwnedTail::new(preceding_byte, context_tail),
            size,
            modified,
            bytes_read,
        })
    }

    #[must_use]
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    #[must_use]
    pub fn context_tail(&self) -> TailRead<'_> {
        self.context_tail.borrow()
    }

    #[must_use]
    pub fn digest_tail(&self) -> TailRead<'_> {
        self.context_tail.digest_tail()
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub const fn modified(&self) -> FileTimestamp {
        self.modified
    }

    #[must_use]
    pub const fn bytes_read(&self) -> usize {
        self.bytes_read
    }
}

/// Sanitized blocking-I/O failures. No variant holds a file path or transcript text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptIoError {
    #[error("transcript not found")]
    NotFound,
    #[error("transcript unavailable")]
    Unavailable,
    #[error("transcript bound exceeded")]
    BoundsExceeded,
    #[error("transcript changed during read")]
    ChangedDuringRead,
}

/// Fallible source construction prevents invalid semaphore counts from reaching
/// Tokio's panicking constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptSourceBuildError {
    #[error("invalid transcript read concurrency")]
    InvalidReadConcurrency,
    #[error("invalid transcript cache capacity")]
    InvalidCacheEntries,
    #[error("invalid Codex scan limits")]
    InvalidCodexScanLimits,
}

impl TranscriptIoError {
    fn from_io(error: &io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Unavailable
        }
    }
}

/// A deliberately small scanner policy. The standard implementation only descends
/// through `sessions/YYYY/MM/DD`, never follows symlink directories, and stops at
/// every cap rather than turning a reconciliation tick into an unbounded walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodexScanLimits {
    pub max_candidates: usize,
    pub max_directories: usize,
    pub max_entries_per_directory: usize,
    pub max_depth: usize,
}

impl Default for CodexScanLimits {
    fn default() -> Self {
        Self {
            max_candidates: 128,
            max_directories: 512,
            max_entries_per_directory: 512,
            max_depth: 3,
        }
    }
}

/// Limits owned by this adapter rather than hidden in task-local state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptAdapterLimits {
    pub read_concurrency: usize,
    pub cache_entries: usize,
    pub codex_scan: CodexScanLimits,
}

impl Default for TranscriptAdapterLimits {
    fn default() -> Self {
        Self {
            read_concurrency: DEFAULT_READ_CONCURRENCY,
            cache_entries: DEFAULT_CACHE_ENTRIES,
            codex_scan: CodexScanLimits::default(),
        }
    }
}

/// One capability-bound descriptor read. The cache key is identity only and is never
/// reopened; all bytes come from the descriptor acquired inside `read_plan`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockingTranscriptRead {
    cache_key: PathBuf,
    windows: TranscriptWindows,
    codex_candidate: Option<SafeRelativePath>,
}

impl BlockingTranscriptRead {
    #[must_use]
    pub fn new(
        cache_key: PathBuf,
        windows: TranscriptWindows,
        codex_candidate: Option<SafeRelativePath>,
    ) -> Self {
        Self {
            cache_key,
            windows,
            codex_candidate,
        }
    }

    #[must_use]
    pub fn windows(&self) -> &TranscriptWindows {
        &self.windows
    }
}

/// The injected synchronous filesystem boundary. Implementations return bytes from
/// an already-acquired descriptor rather than returning a validated pathname for a
/// later reopen. All calls run in a bounded `spawn_blocking` task.
pub trait BlockingTranscriptIo: Send + Sync + 'static {
    fn read_plan(
        &self,
        roots: &TranscriptRoots,
        plan: &TranscriptLocationPlan,
        cached_codex_candidate: Option<&SafeRelativePath>,
        limits: CodexScanLimits,
    ) -> Result<BlockingTranscriptRead, TranscriptIoError>;
}

/// Capability-based production I/O. Configured roots become open directory handles;
/// descendants are resolved beneath those handles, final symlinks/reparse points are
/// rejected, and Unix opens are nonblocking before the descriptor is verified regular.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdTranscriptIo;

impl BlockingTranscriptIo for StdTranscriptIo {
    fn read_plan(
        &self,
        roots: &TranscriptRoots,
        plan: &TranscriptLocationPlan,
        cached_codex_candidate: Option<&SafeRelativePath>,
        limits: CodexScanLimits,
    ) -> Result<BlockingTranscriptRead, TranscriptIoError> {
        match plan {
            TranscriptLocationPlan::Unavailable => Err(TranscriptIoError::Unavailable),
            TranscriptLocationPlan::ClaudeRelative(relative) => {
                let root = open_root(&roots.claude_projects_root)?;
                let windows = read_relative_windows(&root, Path::new(relative.as_str()))?;
                Ok(BlockingTranscriptRead::new(
                    roots.claude_projects_root.join(relative.as_str()),
                    windows,
                    None,
                ))
            }
            TranscriptLocationPlan::CopilotRelative(relative) => {
                let root = open_root(&roots.copilot_session_state_root)?;
                let windows = read_relative_windows(&root, Path::new(relative.as_str()))?;
                Ok(BlockingTranscriptRead::new(
                    roots.copilot_session_state_root.join(relative.as_str()),
                    windows,
                    None,
                ))
            }
            TranscriptLocationPlan::PiExact(path) => {
                let absolute = Path::new(path.as_str());
                let windows = read_absolute_windows(absolute)?;
                Ok(BlockingTranscriptRead::new(
                    absolute.to_path_buf(),
                    windows,
                    None,
                ))
            }
            TranscriptLocationPlan::Codex(plan) => {
                read_codex(roots, plan, cached_codex_candidate, limits)
            }
        }
    }
}

fn open_root(path: &Path) -> Result<Dir, TranscriptIoError> {
    Dir::open_ambient_dir(path, ambient_authority())
        .map_err(|error| TranscriptIoError::from_io(&error))
}

fn read_absolute_windows(path: &Path) -> Result<TranscriptWindows, TranscriptIoError> {
    if !path.is_absolute() {
        return Err(TranscriptIoError::Unavailable);
    }
    let parent = path.parent().ok_or(TranscriptIoError::Unavailable)?;
    let file_name = path.file_name().ok_or(TranscriptIoError::Unavailable)?;
    let directory = open_root(parent)?;
    read_relative_windows(&directory, Path::new(file_name))
}

fn read_relative_windows(
    root: &Dir,
    relative: &Path,
) -> Result<TranscriptWindows, TranscriptIoError> {
    let parent = relative.parent().ok_or(TranscriptIoError::Unavailable)?;
    let file_name = relative
        .file_name()
        .map(Path::new)
        .ok_or(TranscriptIoError::Unavailable)?;
    let parent = open_directory_components_no_follow(root, parent)?;
    let file = open_regular_relative_after(&parent, file_name, || {})?;
    read_descriptor_windows(file)
}

fn open_directory_components_no_follow(
    root: &Dir,
    relative: &Path,
) -> Result<Dir, TranscriptIoError> {
    let mut current = root
        .try_clone()
        .map_err(|error| TranscriptIoError::from_io(&error))?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(TranscriptIoError::Unavailable);
        };
        current = open_child_directory_no_follow(&current, Path::new(component))?;
    }
    Ok(current)
}

fn open_child_directory_no_follow(parent: &Dir, name: &Path) -> Result<Dir, TranscriptIoError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent
        .open_with(name, &options)
        .map_err(|error| TranscriptIoError::from_io(&error))?
        .into_std();
    let metadata = file
        .metadata()
        .map_err(|error| TranscriptIoError::from_io(&error))?;
    metadata
        .is_dir()
        .then(|| Dir::from_std_file(file))
        .ok_or(TranscriptIoError::Unavailable)
}

fn open_regular_relative_after<F: FnOnce()>(
    root: &Dir,
    relative: &Path,
    before_open: F,
) -> Result<File, TranscriptIoError> {
    let entry = root
        .symlink_metadata(relative)
        .map_err(|error| TranscriptIoError::from_io(&error))?;
    if !entry.is_file() {
        return Err(TranscriptIoError::Unavailable);
    }
    before_open();

    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.nonblock(true);
    let file = root
        .open_with(relative, &options)
        .map_err(|error| TranscriptIoError::from_io(&error))?
        .into_std();
    let metadata = file
        .metadata()
        .map_err(|error| TranscriptIoError::from_io(&error))?;
    metadata
        .is_file()
        .then_some(file)
        .ok_or(TranscriptIoError::Unavailable)
}

fn read_descriptor_windows(mut file: File) -> Result<TranscriptWindows, TranscriptIoError> {
    let before = file
        .metadata()
        .map_err(|error| TranscriptIoError::from_io(&error))?;
    if !before.is_file() {
        return Err(TranscriptIoError::Unavailable);
    }
    let modified = timestamp(&before)?;
    let size = before.len();
    let head_len = bounded_len(size, OPENING_HEAD_BYTES)?;
    let mut head = vec![0_u8; head_len];
    read_exact(&mut file, &mut head)?;

    let tail_len = bounded_len(size, CONTEXT_TAIL_BYTES)?;
    let tail_start = size
        .checked_sub(u64::try_from(tail_len).map_err(|_| TranscriptIoError::Unavailable)?)
        .ok_or(TranscriptIoError::Unavailable)?;
    let preceding_byte = if tail_start == 0 {
        file.seek(SeekFrom::Start(0))
            .map_err(|error| TranscriptIoError::from_io(&error))?;
        None
    } else {
        let probe_position = tail_start
            .checked_sub(1)
            .ok_or(TranscriptIoError::Unavailable)?;
        file.seek(SeekFrom::Start(probe_position))
            .map_err(|error| TranscriptIoError::from_io(&error))?;
        let mut probe = [0_u8; 1];
        read_exact(&mut file, &mut probe)?;
        Some(probe[0])
    };
    let mut tail = vec![0_u8; tail_len];
    read_exact(&mut file, &mut tail)?;

    let after = file
        .metadata()
        .map_err(|error| TranscriptIoError::from_io(&error))?;
    if after.len() != size || timestamp(&after)? != modified {
        return Err(TranscriptIoError::ChangedDuringRead);
    }
    let bytes_read = head_len
        .checked_add(tail_len)
        .and_then(|count| count.checked_add(usize::from(preceding_byte.is_some())))
        .ok_or(TranscriptIoError::Unavailable)?;
    TranscriptWindows::try_new(head, preceding_byte, tail, size, modified, bytes_read)
}

fn read_codex(
    roots: &TranscriptRoots,
    plan: &agentdeck_core::transcript::CodexLocatorPlan,
    cached: Option<&SafeRelativePath>,
    limits: CodexScanLimits,
) -> Result<BlockingTranscriptRead, TranscriptIoError> {
    let root = open_root(&roots.codex_sessions_root)?;
    if let Some(relative) = cached {
        if let Ok(windows) = read_relative_windows(&root, Path::new(relative.as_str())) {
            return Ok(BlockingTranscriptRead::new(
                roots.codex_sessions_root.join(relative.as_str()),
                windows,
                Some(relative.clone()),
            ));
        }
    }

    let suffix = format!("{}.jsonl", plan.session_uuid);
    let mut state = ScanState {
        candidates: Vec::new(),
        directories: 1,
        suffix: &suffix,
        limits,
    };
    scan_codex_directory(&root, Path::new(""), 0, &mut state)?;
    if state.candidates.len() > plan.max_candidates {
        return Err(TranscriptIoError::BoundsExceeded);
    }
    let selected = select_codex_candidate(plan, &state.candidates)
        .map_err(|_| TranscriptIoError::BoundsExceeded)?
        .ok_or(TranscriptIoError::NotFound)?;
    let windows = read_relative_windows(&root, Path::new(selected.as_str()))?;
    Ok(BlockingTranscriptRead::new(
        roots.codex_sessions_root.join(selected.as_str()),
        windows,
        Some(selected),
    ))
}

struct ScanState<'a> {
    candidates: Vec<String>,
    directories: usize,
    suffix: &'a str,
    limits: CodexScanLimits,
}

fn scan_codex_directory(
    directory: &Dir,
    relative_directory: &Path,
    depth: usize,
    state: &mut ScanState<'_>,
) -> Result<(), TranscriptIoError> {
    let mut entries = Vec::new();
    for entry in directory
        .entries()
        .map_err(|error| TranscriptIoError::from_io(&error))?
    {
        if entries.len() >= state.limits.max_entries_per_directory {
            return Err(TranscriptIoError::BoundsExceeded);
        }
        entries.push(entry.map_err(|error| TranscriptIoError::from_io(&error))?);
    }
    entries.sort_by_key(cap_std::fs::DirEntry::file_name);
    for entry in entries {
        let file_name = entry.file_name();
        let file_type = entry
            .file_type()
            .map_err(|error| TranscriptIoError::from_io(&error))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if depth >= state.limits.max_depth || !is_expected_date_component(depth, &file_name) {
                continue;
            }
            state.directories = state
                .directories
                .checked_add(1)
                .ok_or(TranscriptIoError::BoundsExceeded)?;
            if state.directories > state.limits.max_directories {
                return Err(TranscriptIoError::BoundsExceeded);
            }
            let child = open_child_directory_no_follow(directory, Path::new(&file_name))?;
            scan_codex_directory(
                &child,
                &relative_directory.join(&file_name),
                depth + 1,
                state,
            )?;
        } else if file_type.is_file()
            && depth == state.limits.max_depth
            && file_name.to_string_lossy().ends_with(state.suffix)
        {
            if state.candidates.len() >= state.limits.max_candidates {
                return Err(TranscriptIoError::BoundsExceeded);
            }
            let candidate = relative_to_safe_string(&relative_directory.join(file_name))
                .ok_or(TranscriptIoError::Unavailable)?;
            state.candidates.push(candidate);
        }
    }
    Ok(())
}

fn is_expected_date_component(depth: usize, component: &std::ffi::OsStr) -> bool {
    let value = component.to_string_lossy();
    let expected_length = match depth {
        0 => 4,
        1 | 2 => 2,
        _ => return false,
    };
    value.len() == expected_length && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn read_exact(file: &mut File, bytes: &mut [u8]) -> Result<(), TranscriptIoError> {
    file.read_exact(bytes)
        .map_err(|error| TranscriptIoError::from_io(&error))
}

fn bounded_len(size: u64, cap: usize) -> Result<usize, TranscriptIoError> {
    let cap = u64::try_from(cap).map_err(|_| TranscriptIoError::Unavailable)?;
    usize::try_from(size.min(cap)).map_err(|_| TranscriptIoError::Unavailable)
}

fn timestamp(metadata: &Metadata) -> Result<FileTimestamp, TranscriptIoError> {
    let modified = metadata
        .modified()
        .map_err(|error| TranscriptIoError::from_io(&error))?;
    Ok(timestamp_from_system_time(modified))
}

fn timestamp_from_system_time(time: SystemTime) -> FileTimestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => FileTimestamp {
            unix_seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            nanoseconds: duration.subsec_nanos(),
        },
        Err(error) => {
            let duration = error.duration();
            FileTimestamp {
                unix_seconds: i64::try_from(duration.as_secs())
                    .unwrap_or(i64::MAX)
                    .saturating_neg(),
                nanoseconds: duration.subsec_nanos(),
            }
        }
    }
}

/// Filesystem implementation of [`TranscriptSource`]. Parsing and cache lookup run
/// in the blocking task after the semaphore permit is moved into it; cancelling the
/// async caller therefore cannot accidentally exceed the blocking-read concurrency.
pub struct FilesystemTranscriptSource<I = StdTranscriptIo> {
    roots: TranscriptRoots,
    io: Arc<I>,
    limits: TranscriptAdapterLimits,
    read_limiter: Arc<Semaphore>,
    parse_cache: Arc<Mutex<BoundedCache<ObservationCacheKey, CachedObservation>>>,
    codex_candidates: Arc<Mutex<BoundedCache<String, SafeRelativePath>>>,
}

impl FilesystemTranscriptSource<StdTranscriptIo> {
    /// Builds the production source with conservative defaults.
    pub fn new(roots: TranscriptRoots) -> Result<Self, TranscriptSourceBuildError> {
        Self::with_io(roots, StdTranscriptIo, TranscriptAdapterLimits::default())
    }
}

impl<I: BlockingTranscriptIo> FilesystemTranscriptSource<I> {
    /// Builds an injected source after validating every semaphore bound.
    pub fn with_io(
        roots: TranscriptRoots,
        io: I,
        limits: TranscriptAdapterLimits,
    ) -> Result<Self, TranscriptSourceBuildError> {
        if limits.read_concurrency == 0 || limits.read_concurrency > Semaphore::MAX_PERMITS {
            return Err(TranscriptSourceBuildError::InvalidReadConcurrency);
        }
        if limits.cache_entries > MAX_CACHE_ENTRIES {
            return Err(TranscriptSourceBuildError::InvalidCacheEntries);
        }
        if limits.codex_scan.max_candidates > MAX_CODEX_SCAN_ITEMS
            || limits.codex_scan.max_directories > MAX_CODEX_SCAN_ITEMS
            || limits.codex_scan.max_entries_per_directory > MAX_CODEX_SCAN_ITEMS
            || limits.codex_scan.max_depth != CODEX_DATE_DEPTH
        {
            return Err(TranscriptSourceBuildError::InvalidCodexScanLimits);
        }
        Ok(Self {
            roots,
            io: Arc::new(io),
            limits,
            read_limiter: Arc::new(Semaphore::new(limits.read_concurrency)),
            parse_cache: Arc::new(Mutex::new(BoundedCache::new(limits.cache_entries))),
            codex_candidates: Arc::new(Mutex::new(BoundedCache::new(limits.cache_entries))),
        })
    }

    fn observe_blocking(&self, request: TranscriptRequest) -> TranscriptObservation {
        if request.cwd.len() > MAX_REQUEST_PATH_BYTES
            || request
                .session
                .as_ref()
                .is_some_and(|session| session.value.len() > MAX_REQUEST_PATH_BYTES)
        {
            return TranscriptObservation::unavailable();
        }
        let codex_root = self.roots.codex_sessions_root.to_str().unwrap_or_default();
        let plan = location_plan(
            request.kind,
            request.session.as_ref(),
            &request.cwd,
            codex_root,
            self.limits.codex_scan.max_candidates,
        );
        let cached_codex_candidate = match &plan {
            TranscriptLocationPlan::Codex(plan) => self.cached_codex_candidate(&plan.session_uuid),
            TranscriptLocationPlan::Unavailable
            | TranscriptLocationPlan::ClaudeRelative(_)
            | TranscriptLocationPlan::CopilotRelative(_)
            | TranscriptLocationPlan::PiExact(_) => None,
        };
        let read = match self.io.read_plan(
            &self.roots,
            &plan,
            cached_codex_candidate.as_ref(),
            self.limits.codex_scan,
        ) {
            Ok(read) => read,
            Err(TranscriptIoError::NotFound) => return TranscriptObservation::not_yet_created(),
            Err(
                TranscriptIoError::Unavailable
                | TranscriptIoError::BoundsExceeded
                | TranscriptIoError::ChangedDuringRead,
            ) => return TranscriptObservation::unavailable(),
        };
        if let (TranscriptLocationPlan::Codex(plan), Some(relative)) =
            (&plan, read.codex_candidate.clone())
        {
            self.cache_codex_candidate(plan.session_uuid.clone(), relative);
        }
        let cache_key = ObservationCacheKey::new(read.cache_key, request.kind);
        let fingerprint = fingerprint_for(&cache_key.path, &read.windows);
        if let Some(cached) = self.cached_observation(&cache_key, &fingerprint) {
            return cached;
        }
        let observation = TranscriptObservation {
            analysis: analyze_windows(
                request.kind,
                read.windows.head(),
                read.windows.digest_tail(),
                read.windows.modified().unix_seconds,
            ),
            context: extract_context_window(request.kind, read.windows.context_tail()),
            written_at: Some(read.windows.modified().unix_seconds),
        };
        self.cache_observation(cache_key, fingerprint, observation.clone());
        observation
    }

    fn cached_observation(
        &self,
        key: &ObservationCacheKey,
        fingerprint: &TranscriptCacheFingerprint,
    ) -> Option<TranscriptObservation> {
        self.parse_cache
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(key))
            .filter(|cached| cached.fingerprint == *fingerprint)
            .map(|cached| cached.observation)
    }

    fn cache_observation(
        &self,
        key: ObservationCacheKey,
        fingerprint: TranscriptCacheFingerprint,
        observation: TranscriptObservation,
    ) {
        if let Ok(mut cache) = self.parse_cache.lock() {
            cache.insert(
                key,
                CachedObservation {
                    fingerprint,
                    observation,
                },
            );
        }
    }

    fn cached_codex_candidate(&self, session_uuid: &str) -> Option<SafeRelativePath> {
        self.codex_candidates
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(&session_uuid.to_owned()))
    }

    fn cache_codex_candidate(&self, session_uuid: String, relative: SafeRelativePath) {
        if let Ok(mut cache) = self.codex_candidates.lock() {
            cache.insert(session_uuid, relative);
        }
    }
}

#[async_trait]
impl<I: BlockingTranscriptIo> TranscriptSource for FilesystemTranscriptSource<I> {
    async fn observe(&self, request: TranscriptRequest) -> TranscriptObservation {
        let Ok(permit) = Arc::clone(&self.read_limiter).acquire_owned().await else {
            return TranscriptObservation::unavailable();
        };
        let roots = self.roots.clone();
        let io = Arc::clone(&self.io);
        let limits = self.limits;
        let parse_cache = Arc::clone(&self.parse_cache);
        let codex_candidates = Arc::clone(&self.codex_candidates);
        task::spawn_blocking(move || {
            let _permit = permit;
            let source = Self {
                roots,
                io,
                limits,
                read_limiter: Arc::new(Semaphore::new(1)),
                parse_cache,
                codex_candidates,
            };
            source.observe_blocking(request)
        })
        .await
        .unwrap_or_else(|_| TranscriptObservation::unavailable())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedObservation {
    fingerprint: TranscriptCacheFingerprint,
    observation: TranscriptObservation,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ObservationCacheKey {
    path: PathBuf,
    kind: u8,
}

impl ObservationCacheKey {
    fn new(path: PathBuf, kind: TranscriptKind) -> Self {
        let kind = match kind {
            TranscriptKind::Claude => 0,
            TranscriptKind::Pi => 1,
            TranscriptKind::Codex => 2,
            TranscriptKind::Copilot => 3,
            TranscriptKind::Unknown => 4,
        };
        Self { path, kind }
    }
}

fn relative_to_safe_string(path: &Path) -> Option<String> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return None;
        };
        components.push(component.to_str()?.to_owned());
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn fingerprint_for(path: &Path, windows: &TranscriptWindows) -> TranscriptCacheFingerprint {
    let mut raw_windows = Vec::with_capacity(
        windows
            .head
            .len()
            .saturating_add(windows.context_tail.bytes.len())
            .saturating_add(17),
    );
    append_window(&mut raw_windows, &windows.head);
    raw_windows.push(u8::from(windows.context_tail.preceding_byte.is_some()));
    if let Some(byte) = windows.context_tail.preceding_byte {
        raw_windows.push(byte);
    }
    append_window(&mut raw_windows, &windows.context_tail.bytes);
    cache_fingerprint(
        &path.to_string_lossy(),
        windows.size,
        windows.modified.unix_seconds,
        windows.modified.nanoseconds,
        Some(&raw_windows),
    )
}

fn append_window(output: &mut Vec<u8>, window: &[u8]) {
    let length = u64::try_from(window.len()).unwrap_or(u64::MAX);
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(window);
}

struct BoundedCache<K, V> {
    capacity: usize,
    values: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Clone + Eq + Hash, V: Clone> BoundedCache<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let value = self.values.get(key)?.clone();
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: K, value: V) {
        if self.capacity == 0 {
            return;
        }
        self.values.insert(key.clone(), value);
        self.touch(&key);
        while self.values.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn touch(&mut self, key: &K) {
        self.order.retain(|candidate| candidate != key);
        self.order.push_back(key.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{Duration, SystemTime},
    };

    #[cfg(unix)]
    use std::{process::Command, sync::mpsc, thread};

    use agentdeck_core::{
        HerdrAgentSession,
        transcript::{DIGEST_TAIL_BYTES, TranscriptKind},
    };
    use tempfile::TempDir;

    use super::*;

    fn fixture_windows(bytes: Vec<u8>, timestamp: FileTimestamp) -> TranscriptWindows {
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        TranscriptWindows::try_new(
            bytes.clone(),
            None,
            bytes.clone(),
            size,
            timestamp,
            bytes.len().saturating_mul(2),
        )
        .unwrap_or_else(|error| panic!("valid windows: {error:?}"))
    }

    fn copilot_request(session_id: &str) -> TranscriptRequest {
        TranscriptRequest {
            kind: TranscriptKind::Copilot,
            session: Some(HerdrAgentSession {
                source: "herdr".to_owned(),
                agent: "copilot".to_owned(),
                kind: "id".to_owned(),
                value: session_id.to_owned(),
            }),
            cwd: "/fixture".to_owned(),
        }
    }

    fn roots(root: &Path) -> TranscriptRoots {
        TranscriptRoots {
            claude_projects_root: root.join("claude"),
            codex_sessions_root: root.join("codex"),
            copilot_session_state_root: root.join("copilot-session-state"),
        }
    }

    #[test]
    fn copilot_id_uses_only_session_state_events_with_mtime_and_missing_softness() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
        let roots = roots(temp.path());
        let session = roots.copilot_session_state_root.join("safe-id");
        fs::create_dir_all(&session).unwrap_or_else(|error| panic!("session root: {error}"));
        fs::write(
            session.join("events.jsonl"),
            b"{\"type\":\"user.message\",\"data\":{\"content\":\"Review bounded Copilot reads safely.\",\"source\":\"user\"}}\n{\"type\":\"assistant.message\",\"data\":{\"content\":\"Bounded result.\",\"toolRequests\":[]}}\n{\"type\":\"session.usage_info\",\"data\":{\"currentTokens\":10,\"tokenLimit\":20}}\n",
        ).unwrap_or_else(|error| panic!("events fixture: {error}"));
        let source = FilesystemTranscriptSource::new(roots)
            .unwrap_or_else(|error| panic!("source: {error}"));
        let observation = source.observe_blocking(copilot_request("safe-id"));
        assert!(matches!(observation.analysis, TranscriptOutcome::Ready(_)));
        assert!(
            matches!(observation.context, ContextOutcome::Ready(ref usage) if usage.used == 10 && usage.limit == 20)
        );
        assert!(observation.written_at.is_some());
        assert_eq!(
            source
                .observe_blocking(copilot_request("missing-id"))
                .analysis,
            TranscriptOutcome::NotYetCreated
        );
        assert_eq!(
            source
                .observe_blocking(TranscriptRequest {
                    session: Some(HerdrAgentSession {
                        kind: "path".to_owned(),
                        value: "/outside/events.jsonl".to_owned(),
                        ..copilot_request("safe-id")
                            .session
                            .unwrap_or_else(|| panic!("fixture session"))
                    }),
                    ..copilot_request("safe-id")
                })
                .analysis,
            TranscriptOutcome::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn copilot_session_symlink_cannot_escape_configured_state_root() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
        let roots = roots(temp.path());
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap_or_else(|error| panic!("outside: {error}"));
        fs::write(outside.join("events.jsonl"), b"outside")
            .unwrap_or_else(|error| panic!("outside events: {error}"));
        fs::create_dir_all(&roots.copilot_session_state_root)
            .unwrap_or_else(|error| panic!("state root: {error}"));
        symlink(&outside, roots.copilot_session_state_root.join("safe-id"))
            .unwrap_or_else(|error| panic!("session symlink: {error}"));
        let source = FilesystemTranscriptSource::new(roots)
            .unwrap_or_else(|error| panic!("source: {error}"));
        assert_eq!(
            source.observe_blocking(copilot_request("safe-id")).analysis,
            TranscriptOutcome::Unavailable
        );
    }

    #[test]
    fn window_constructor_rejects_oversized_inconsistent_or_false_accounting() {
        let timestamp = FileTimestamp {
            unix_seconds: 1,
            nanoseconds: 2,
        };
        let too_large = vec![0_u8; OPENING_HEAD_BYTES + 1];
        assert_eq!(
            TranscriptWindows::try_new(
                too_large,
                None,
                vec![0_u8; OPENING_HEAD_BYTES + 1],
                u64::try_from(OPENING_HEAD_BYTES + 1).unwrap_or(u64::MAX),
                timestamp,
                (OPENING_HEAD_BYTES + 1).saturating_mul(2),
            ),
            Err(TranscriptIoError::BoundsExceeded)
        );
        assert_eq!(
            TranscriptWindows::try_new(vec![0], Some(0), vec![0], 1, timestamp, 3),
            Err(TranscriptIoError::BoundsExceeded)
        );
        assert_eq!(
            TranscriptWindows::try_new(vec![0], None, vec![0], 1, timestamp, 0),
            Err(TranscriptIoError::BoundsExceeded)
        );
    }

    #[test]
    fn descriptor_windows_obey_every_head_digest_context_and_probe_boundary() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
        for size in [
            OPENING_HEAD_BYTES - 1,
            OPENING_HEAD_BYTES,
            OPENING_HEAD_BYTES + 1,
            DIGEST_TAIL_BYTES - 1,
            DIGEST_TAIL_BYTES,
            DIGEST_TAIL_BYTES + 1,
            CONTEXT_TAIL_BYTES - 1,
            CONTEXT_TAIL_BYTES,
            CONTEXT_TAIL_BYTES + 1,
        ] {
            let path = temp.path().join(format!("{size}.jsonl"));
            let bytes = (0..size)
                .map(|offset| u8::try_from(offset % 251).unwrap_or(0))
                .collect::<Vec<_>>();
            fs::write(&path, &bytes).unwrap_or_else(|error| panic!("write fixture: {error}"));
            let file = File::open(&path).unwrap_or_else(|error| panic!("open fixture: {error}"));
            let windows = read_descriptor_windows(file)
                .unwrap_or_else(|error| panic!("read bounded descriptor: {error:?}"));

            assert_eq!(windows.head().len(), size.min(OPENING_HEAD_BYTES));
            assert_eq!(
                windows.digest_tail().bytes.len(),
                size.min(DIGEST_TAIL_BYTES)
            );
            assert_eq!(
                windows.digest_tail().preceding_byte.is_some(),
                size > DIGEST_TAIL_BYTES
            );
            assert_eq!(
                windows.context_tail().bytes.len(),
                size.min(CONTEXT_TAIL_BYTES)
            );
            assert_eq!(
                windows.context_tail().preceding_byte.is_some(),
                size > CONTEXT_TAIL_BYTES
            );
            assert_eq!(
                windows.bytes_read(),
                size.min(OPENING_HEAD_BYTES)
                    + size.min(CONTEXT_TAIL_BYTES)
                    + usize::from(size > CONTEXT_TAIL_BYTES)
            );
            assert_eq!(windows.head(), &bytes[..size.min(OPENING_HEAD_BYTES)]);
            assert_eq!(
                windows.context_tail().bytes,
                &bytes[size.saturating_sub(CONTEXT_TAIL_BYTES)..]
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn final_and_ancestor_replacement_cannot_escape_an_open_root_capability() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("safe"))
            .unwrap_or_else(|error| panic!("create root: {error}"));
        fs::create_dir_all(&outside).unwrap_or_else(|error| panic!("create outside: {error}"));
        fs::write(root_path.join("safe/final.jsonl"), b"safe")
            .unwrap_or_else(|error| panic!("write safe final: {error}"));
        fs::write(outside.join("final.jsonl"), b"secret outside final")
            .unwrap_or_else(|error| panic!("write outside final: {error}"));
        let root = open_root(&root_path).unwrap_or_else(|error| panic!("open root: {error:?}"));
        let result = open_regular_relative_after(&root, Path::new("safe/final.jsonl"), || {
            fs::remove_file(root_path.join("safe/final.jsonl"))
                .unwrap_or_else(|error| panic!("remove checked final: {error}"));
            symlink(
                outside.join("final.jsonl"),
                root_path.join("safe/final.jsonl"),
            )
            .unwrap_or_else(|error| panic!("replace final with symlink: {error}"));
        });
        assert_eq!(result.map(drop), Err(TranscriptIoError::Unavailable));

        fs::remove_file(root_path.join("safe/final.jsonl"))
            .unwrap_or_else(|error| panic!("remove final link: {error}"));
        fs::write(root_path.join("safe/final.jsonl"), b"safe")
            .unwrap_or_else(|error| panic!("restore final: {error}"));
        let result = open_regular_relative_after(&root, Path::new("safe/final.jsonl"), || {
            fs::rename(root_path.join("safe"), root_path.join("moved"))
                .unwrap_or_else(|error| panic!("move checked ancestor: {error}"));
            symlink(&outside, root_path.join("safe"))
                .unwrap_or_else(|error| panic!("replace ancestor with symlink: {error}"));
        });
        assert_eq!(result.map(drop), Err(TranscriptIoError::Unavailable));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_replacement_is_nonblocking_and_rejected_as_non_regular() {
        let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap_or_else(|error| panic!("create root: {error}"));
        let path = root_path.join("transcript.jsonl");
        fs::write(&path, b"checked regular file")
            .unwrap_or_else(|error| panic!("write checked file: {error}"));
        let root = open_root(&root_path).unwrap_or_else(|error| panic!("open root: {error:?}"));
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = open_regular_relative_after(&root, Path::new("transcript.jsonl"), || {
                fs::remove_file(&path)
                    .unwrap_or_else(|error| panic!("remove checked file: {error}"));
                let status = Command::new("/usr/bin/mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap_or_else(|error| panic!("run mkfifo: {error}"));
                assert!(status.success());
            });
            let _ = sender.send(result.map(drop));
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|error| panic!("FIFO open blocked: {error}"));
        assert_eq!(result, Err(TranscriptIoError::Unavailable));
    }

    #[test]
    fn fingerprint_uses_size_second_nanosecond_path_and_bounded_content() {
        let timestamp = FileTimestamp {
            unix_seconds: 10,
            nanoseconds: 20,
        };
        let base = fixture_windows(b"same".to_vec(), timestamp);
        let content = fixture_windows(b"diff".to_vec(), timestamp);
        let seconds = fixture_windows(
            b"same".to_vec(),
            FileTimestamp {
                unix_seconds: 11,
                ..timestamp
            },
        );
        let nanos = fixture_windows(
            b"same".to_vec(),
            FileTimestamp {
                nanoseconds: 21,
                ..timestamp
            },
        );
        let larger = fixture_windows(b"same+".to_vec(), timestamp);
        let same = fixture_windows(b"same".to_vec(), timestamp);
        let fingerprint = fingerprint_for(Path::new("a"), &base);
        assert_eq!(fingerprint, fingerprint_for(Path::new("a"), &same));
        assert_ne!(fingerprint, fingerprint_for(Path::new("b"), &base));
        assert_ne!(fingerprint, fingerprint_for(Path::new("a"), &content));
        assert_ne!(fingerprint, fingerprint_for(Path::new("a"), &seconds));
        assert_ne!(fingerprint, fingerprint_for(Path::new("a"), &nanos));
        assert_ne!(fingerprint, fingerprint_for(Path::new("a"), &larger));
    }

    #[test]
    fn parse_cache_is_lru_bounded_and_separates_agent_kinds() {
        let mut cache = BoundedCache::new(2);
        cache.insert(
            ObservationCacheKey::new("a".into(), TranscriptKind::Claude),
            1,
        );
        cache.insert(ObservationCacheKey::new("a".into(), TranscriptKind::Pi), 2);
        assert_eq!(
            cache.get(&ObservationCacheKey::new(
                "a".into(),
                TranscriptKind::Claude
            )),
            Some(1)
        );
        cache.insert(
            ObservationCacheKey::new("c".into(), TranscriptKind::Codex),
            3,
        );
        assert_eq!(
            cache.get(&ObservationCacheKey::new("a".into(), TranscriptKind::Pi)),
            None
        );
        assert_eq!(
            cache.get(&ObservationCacheKey::new(
                "a".into(),
                TranscriptKind::Claude
            )),
            Some(1)
        );
    }

    #[test]
    fn parser_entry_points_do_not_panic_on_deterministic_arbitrary_bytes() {
        let mut state = 0x5eed_cafe_u64;
        for length in 0..512 {
            let mut bytes = Vec::with_capacity(length);
            for _ in 0..length {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                bytes.push(state.to_le_bytes()[4]);
            }
            let windows = fixture_windows(
                bytes,
                FileTimestamp {
                    unix_seconds: 0,
                    nanoseconds: 0,
                },
            );
            for kind in [
                TranscriptKind::Claude,
                TranscriptKind::Pi,
                TranscriptKind::Codex,
                TranscriptKind::Copilot,
            ] {
                let _ = analyze_windows(kind, windows.head(), windows.digest_tail(), 0);
                let _ = extract_context_window(kind, windows.context_tail());
            }
        }
    }

    #[test]
    fn timestamp_supports_pre_epoch_values_without_panicking() {
        let timestamp = timestamp_from_system_time(UNIX_EPOCH - Duration::from_nanos(1));
        assert!(timestamp.unix_seconds <= 0);
        assert_eq!(timestamp.nanoseconds, 1);
        let _ = timestamp_from_system_time(SystemTime::now());
    }
}
