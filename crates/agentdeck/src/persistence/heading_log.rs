//! Optional privacy-safe local JSONL audit for heading attempts.
//!
//! This log deliberately contains only closed codes.  Unix uses a retained,
//! no-follow directory capability and advisory locking. Windows is disabled until
//! reparse-point and ACL guarantees have native coverage. Advisory locks only
//! coordinate cooperating same-UID processes: a malicious same-UID writer can
//! still tamper with or deny this optional log, but every read/allocation is capped.

use agentdeck_core::headings::HeadingKind;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
const VERSION: u8 = 1;
#[cfg(unix)]
const MAX_BYTES: u64 = 16 * 1024;
#[cfg(unix)]
const MAX_LINE: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadingAttemptOutcome {
    Accepted,
    Rejected,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadingAttemptReason {
    Accepted,
    PolicyRejected,
    InvalidCandidate,
    ProviderUnavailable,
    Timeout,
    Transport,
    Decode,
    Cancelled,
    Internal,
}

/// Text-free input for the optional audit record.
#[derive(Clone, Copy, Debug)]
pub struct HeadingAttempt {
    pub kind: HeadingKind,
    pub outcome: HeadingAttemptOutcome,
    pub reason: HeadingAttemptReason,
    pub latency: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct HeadingAttemptLog {
    #[cfg(unix)]
    directory: Option<std::sync::Arc<unix::LogDirectory>>,
}

impl HeadingAttemptLog {
    /// Fails closed: an unsafe or unverifiable path returns a disabled sink.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        #[cfg(unix)]
        {
            Self {
                directory: unix::LogDirectory::open(path.as_ref())
                    .ok()
                    .map(std::sync::Arc::new),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Self::default()
        }
    }

    /// Best effort only: no logging failure can change heading generation.
    pub fn record(&self, attempt: HeadingAttempt) {
        #[cfg(unix)]
        {
            let record = match Record::new(attempt) {
                Ok(value) => value,
                Err(()) => return,
            };
            let mut line = match serde_json::to_vec(&record) {
                Ok(value) => value,
                Err(_) => return,
            };
            line.push(b'\n');
            if line.len() > MAX_LINE {
                return;
            }
            if let Some(directory) = &self.directory {
                let _ = directory.append(&line);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = attempt;
        }
    }

    #[cfg(all(test, unix))]
    fn enabled(&self) -> bool {
        self.directory.is_some()
    }
}

#[cfg(unix)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    time_unix_seconds: u64,
    kind: LogKind,
    outcome: HeadingAttemptOutcome,
    reason: HeadingAttemptReason,
    latency: Latency,
}
#[cfg(unix)]
impl Record {
    fn new(attempt: HeadingAttempt) -> Result<Self, ()> {
        Ok(Self {
            version: VERSION,
            time_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ())?
                .as_secs(),
            kind: LogKind::from(attempt.kind),
            outcome: attempt.outcome,
            reason: attempt.reason,
            latency: Latency::from(attempt.latency),
        })
    }
    fn valid(&self) -> bool {
        self.version == VERSION
    }
}

#[cfg(unix)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Latency {
    Under100Ms,
    Under1S,
    Under10S,
    Over10S,
}

#[cfg(unix)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LogKind {
    Title,
    Subtitle,
    Outcome,
    Activity,
}
#[cfg(unix)]
impl From<HeadingKind> for LogKind {
    fn from(value: HeadingKind) -> Self {
        match value {
            HeadingKind::Title => Self::Title,
            HeadingKind::Subtitle => Self::Subtitle,
            HeadingKind::Outcome => Self::Outcome,
            HeadingKind::Activity => Self::Activity,
        }
    }
}
#[cfg(unix)]
impl From<Duration> for Latency {
    fn from(value: Duration) -> Self {
        match value.as_millis() {
            0..100 => Self::Under100Ms,
            100..1000 => Self::Under1S,
            1000..10_000 => Self::Under10S,
            _ => Self::Over10S,
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::{Existing, MAX_BYTES, MAX_LINE, Record};
    use cap_fs_ext::{
        FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsExt as CapOpenOptionsExt,
        OpenOptionsFollowExt, OpenOptionsMaybeDirExt,
    };
    use cap_std::{
        ambient_authority,
        fs::{
            Dir, OpenOptions, Permissions as CapPermissions, PermissionsExt as CapPermissionsExt,
        },
    };
    use fs2::FileExt as _;
    use std::{
        ffi::{OsStr, OsString},
        fs,
        io::{Read, Seek, SeekFrom, Write},
        os::unix::fs::{MetadataExt as StdMetadataExt, PermissionsExt},
        path::{Component, Path},
    };

    #[derive(Debug)]
    pub(super) struct LogDirectory {
        dir: Dir,
        log: OsString,
        lock: OsString,
        temporary: OsString,
    }
    #[derive(Debug)]
    pub(super) enum Error {
        Unsafe,
        Locked,
        Io,
    }

    impl LogDirectory {
        pub(super) fn open(path: &Path) -> Result<Self, Error> {
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or(Error::Unsafe)?;
            if !path.is_absolute() || name.is_empty() || name.len() > 96 {
                return Err(Error::Unsafe);
            }
            let dir = open_private_parent(path.parent().ok_or(Error::Unsafe)?)?;
            Ok(Self {
                dir,
                log: name.into(),
                lock: format!(".{name}.lock").into(),
                temporary: format!(".{name}.next").into(),
            })
        }

        pub(super) fn append(&self, line: &[u8]) -> Result<(), Error> {
            let lock = self.leaf(&self.lock, true)?;
            private_file(&lock)?;
            self.verify(&self.lock, &lock)?;
            lock.try_lock_exclusive().map_err(|_| Error::Locked)?;
            let result = self
                .verify(&self.lock, &lock)
                .and_then(|()| self.append_locked(line));
            let _ = fs2::FileExt::unlock(&lock);
            result
        }

        fn append_locked(&self, line: &[u8]) -> Result<(), Error> {
            let mut log = self.leaf(&self.log, true)?;
            private_file(&log)?;
            self.verify(&self.log, &log)?;
            match read_valid(&mut log)? {
                Existing::Valid(size) if size.saturating_add(line.len() as u64) <= MAX_BYTES => {
                    self.verify(&self.log, &log)?;
                    if log.metadata().map_err(|_| Error::Io)?.len() != size {
                        return Err(Error::Unsafe);
                    }
                    log.seek(SeekFrom::Start(size)).map_err(|_| Error::Io)?;
                    log.write_all(line).map_err(|_| Error::Io)?;
                    log.sync_data().map_err(|_| Error::Io)
                }
                Existing::Valid(_) | Existing::Replace => self.rotate(line),
            }
        }

        fn rotate(&self, line: &[u8]) -> Result<(), Error> {
            self.remove_safe_temporary()?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .follow(FollowSymlinks::No);
            let mut temporary = self
                .dir
                .open_with(&self.temporary, &options)
                .map_err(|_| Error::Io)?
                .into_std();
            private_file(&temporary)?;
            self.verify(&self.temporary, &temporary)?;
            temporary.write_all(line).map_err(|_| Error::Io)?;
            temporary.sync_data().map_err(|_| Error::Io)?;
            self.verify(&self.temporary, &temporary)?;
            let old = self.leaf(&self.log, false)?;
            self.verify(&self.log, &old)?;
            self.dir
                .rename(&self.temporary, &self.dir, &self.log)
                .map_err(|_| Error::Io)?;
            Ok(())
        }

        fn remove_safe_temporary(&self) -> Result<(), Error> {
            match self.leaf(&self.temporary, false) {
                Ok(file) => {
                    private_file(&file)?;
                    self.verify(&self.temporary, &file)?;
                    self.dir.remove_file(&self.temporary).map_err(|_| Error::Io)
                }
                Err(Error::Io) => Ok(()),
                Err(error) => Err(error),
            }
        }

        fn leaf(&self, name: &OsStr, create: bool) -> Result<fs::File, Error> {
            let mut options = OpenOptions::new();
            options.read(true).write(true).follow(FollowSymlinks::No);
            if create {
                options.create(true).mode(0o600);
            }
            let file = self
                .dir
                .open_with(name, &options)
                .map_err(|_| Error::Io)?
                .into_std();
            one_link_regular(&file)?;
            Ok(file)
        }

        fn verify(&self, name: &OsStr, file: &fs::File) -> Result<(), Error> {
            let named = self.dir.symlink_metadata(name).map_err(|_| Error::Io)?;
            if !named.is_file() || named.nlink() != 1 {
                return Err(Error::Unsafe);
            }
            private_file(file)?;
            let open = file.metadata().map_err(|_| Error::Io)?;
            if CapMetadataExt::dev(&named) != StdMetadataExt::dev(&open)
                || CapMetadataExt::ino(&named) != StdMetadataExt::ino(&open)
            {
                return Err(Error::Unsafe);
            }
            Ok(())
        }
    }

    fn open_private_parent(path: &Path) -> Result<Dir, Error> {
        let mut current = Dir::open_ambient_dir("/", ambient_authority()).map_err(|_| Error::Io)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => current = open_child(&current, name)?,
                _ => return Err(Error::Unsafe),
            }
        }
        private_dir(&current)?;
        Ok(current)
    }
    fn open_child(parent: &Dir, name: &OsStr) -> Result<Dir, Error> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .follow(FollowSymlinks::No)
            .maybe_dir(true);
        let file = match parent.open_with(name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                parent.create_dir(name).map_err(|_| Error::Io)?;
                parent
                    .set_permissions(
                        name,
                        CapPermissions::from_std(fs::Permissions::from_mode(0o700)),
                    )
                    .map_err(|_| Error::Unsafe)?;
                parent
                    .open_with(name, &options)
                    .map_err(|_| Error::Unsafe)?
            }
            Err(_) => return Err(Error::Unsafe),
        };
        Dir::from_std_file(file.into_std()).pipe(Ok)
    }
    fn private_dir(dir: &Dir) -> Result<(), Error> {
        let metadata = dir.dir_metadata().map_err(|_| Error::Io)?;
        if !metadata.is_dir() || CapPermissionsExt::mode(&metadata.permissions()) & 0o077 != 0 {
            return Err(Error::Unsafe);
        }
        Ok(())
    }
    fn one_link_regular(file: &fs::File) -> Result<(), Error> {
        let metadata = file.metadata().map_err(|_| Error::Io)?;
        if !metadata.is_file() || StdMetadataExt::nlink(&metadata) != 1 {
            return Err(Error::Unsafe);
        }
        Ok(())
    }
    fn private_file(file: &fs::File) -> Result<(), Error> {
        one_link_regular(file)?;
        if file.metadata().map_err(|_| Error::Io)?.permissions().mode() & 0o077 != 0 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| Error::Io)?;
        }
        if file.metadata().map_err(|_| Error::Io)?.permissions().mode() & 0o077 != 0 {
            return Err(Error::Unsafe);
        }
        Ok(())
    }
    trait Pipe: Sized {
        fn pipe<T>(self, function: impl FnOnce(Self) -> T) -> T {
            function(self)
        }
    }
    impl<T> Pipe for T {}
    fn read_valid(file: &mut fs::File) -> Result<Existing, Error> {
        let size = file.metadata().map_err(|_| Error::Io)?.len();
        if size > MAX_BYTES {
            return Ok(Existing::Replace);
        }
        read_valid_from_snapshot(file, size)
    }

    pub(super) fn read_valid_from_snapshot(
        file: &mut fs::File,
        size: u64,
    ) -> Result<Existing, Error> {
        if size > MAX_BYTES {
            return Ok(Existing::Replace);
        }
        file.seek(SeekFrom::Start(0)).map_err(|_| Error::Io)?;
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Io)?;
        if bytes.len() as u64 > MAX_BYTES
            || file.metadata().map_err(|_| Error::Io)?.len() != size
            || bytes.len() as u64 != size
            || (!bytes.is_empty() && !bytes.ends_with(b"\n"))
        {
            return Ok(Existing::Replace);
        }
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            if line.len() + 1 > MAX_LINE {
                return Ok(Existing::Replace);
            }
            let record: Record = match serde_json::from_slice(line) {
                Ok(record) => record,
                Err(_) => return Ok(Existing::Replace),
            };
            if !record.valid() {
                return Ok(Existing::Replace);
            }
        }
        Ok(Existing::Valid(size))
    }
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
enum Existing {
    Valid(u64),
    Replace,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use fs2::FileExt as _;
    use std::{
        fs,
        os::unix::fs::{MetadataExt as _, PermissionsExt, symlink},
        sync::{Arc, Barrier},
    };

    fn attempt(outcome: HeadingAttemptOutcome) -> HeadingAttempt {
        HeadingAttempt {
            kind: HeadingKind::Subtitle,
            outcome,
            reason: match outcome {
                HeadingAttemptOutcome::Accepted => HeadingAttemptReason::Accepted,
                HeadingAttemptOutcome::Rejected => HeadingAttemptReason::PolicyRejected,
                HeadingAttemptOutcome::Error => HeadingAttemptReason::Timeout,
            },
            latency: Duration::from_millis(200),
        }
    }
    fn private(path: &std::path::Path) {
        fs::create_dir_all(path).unwrap_or_else(|e| panic!("create: {e}"));
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|e| panic!("permissions: {e}"));
    }
    fn canonical(path: &std::path::Path) -> std::path::PathBuf {
        path.canonicalize()
            .unwrap_or_else(|e| panic!("canonical test root: {e}"))
    }
    fn records(path: &std::path::Path) -> Vec<Record> {
        fs::read(path)
            .unwrap_or_else(|e| panic!("read: {e}"))
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let record: Record =
                    serde_json::from_slice(line).unwrap_or_else(|e| panic!("json: {e}"));
                assert!(record.valid());
                record
            })
            .collect()
    }

    #[test]
    fn closed_schema_permissions_and_privacy() {
        let temp = tempfile::tempdir().unwrap_or_else(|e| panic!("temp: {e}"));
        let parent = canonical(temp.path()).join("private");
        private(&parent);
        let path = parent.join("attempts.jsonl");
        let log = HeadingAttemptLog::new(&path);
        assert!(log.enabled());
        for value in [
            HeadingAttemptOutcome::Accepted,
            HeadingAttemptOutcome::Rejected,
            HeadingAttemptOutcome::Error,
        ] {
            log.record(attempt(value));
        }
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("log: {e}"));
        for forbidden in [
            "prompt",
            "transcript",
            "screen",
            "endpoint",
            "token",
            "workspace",
            "candidate",
            "secret",
        ] {
            assert!(!text.contains(forbidden));
        }
        assert_eq!(records(&path).len(), 3);
        for leaf in ["attempts.jsonl", ".attempts.jsonl.lock"] {
            let metadata =
                fs::metadata(parent.join(leaf)).unwrap_or_else(|e| panic!("metadata: {e}"));
            assert_eq!(metadata.nlink(), 1);
            assert_eq!(metadata.permissions().mode() & 0o077, 0);
        }
        let created_parent = canonical(temp.path()).join("created").join("nested");
        let created_path = created_parent.join("attempts.jsonl");
        let created = HeadingAttemptLog::new(&created_path);
        assert!(created.enabled());
        created.record(attempt(HeadingAttemptOutcome::Accepted));
        assert_eq!(
            fs::metadata(&created_parent)
                .unwrap_or_else(|e| panic!("created parent metadata: {e}"))
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }

    #[test]
    fn concurrent_writers_stay_bounded_and_valid() {
        let temp = tempfile::tempdir().unwrap_or_else(|e| panic!("temp: {e}"));
        let parent = canonical(temp.path()).join("private");
        private(&parent);
        let path = parent.join("attempts.jsonl");
        let log = Arc::new(HeadingAttemptLog::new(&path));
        let start = Arc::new(Barrier::new(17));
        let mut joins = Vec::new();
        for _ in 0..16 {
            let log = Arc::clone(&log);
            let start = Arc::clone(&start);
            joins.push(std::thread::spawn(move || {
                start.wait();
                for _ in 0..200 {
                    log.record(attempt(HeadingAttemptOutcome::Accepted));
                }
            }));
        }
        start.wait();
        for join in joins {
            join.join().unwrap_or_else(|_| panic!("writer panic"));
        }
        assert!(
            fs::metadata(&path)
                .unwrap_or_else(|e| panic!("metadata: {e}"))
                .len()
                <= MAX_BYTES
        );
        assert!(!records(&path).is_empty());
    }

    #[test]
    fn symlink_ancestor_and_parent_swap_cannot_escape() {
        let temp = tempfile::tempdir().unwrap_or_else(|e| panic!("temp: {e}"));
        let root = canonical(temp.path());
        let outside = root.join("outside");
        private(&outside);
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"unchanged").unwrap_or_else(|e| panic!("sentinel: {e}"));
        let linked = root.join("linked");
        symlink(&outside, &linked).unwrap_or_else(|e| panic!("symlink: {e}"));
        let disabled = HeadingAttemptLog::new(linked.join("attempts.jsonl"));
        assert!(!disabled.enabled());
        disabled.record(attempt(HeadingAttemptOutcome::Accepted));
        assert!(!outside.join("attempts.jsonl").exists());

        let parent = root.join("private");
        private(&parent);
        let path = parent.join("attempts.jsonl");
        let log = HeadingAttemptLog::new(&path);
        let moved = root.join("moved");
        fs::rename(&parent, &moved).unwrap_or_else(|e| panic!("move: {e}"));
        symlink(&outside, &parent).unwrap_or_else(|e| panic!("swap: {e}"));
        log.record(attempt(HeadingAttemptOutcome::Accepted));
        assert_eq!(
            fs::read(&sentinel).unwrap_or_else(|e| panic!("sentinel: {e}")),
            b"unchanged"
        );
        assert!(moved.join("attempts.jsonl").exists());
        assert!(!outside.join("attempts.jsonl").exists());
    }

    #[test]
    fn hardlinked_log_and_lock_are_refused() {
        let temp = tempfile::tempdir().unwrap_or_else(|e| panic!("temp: {e}"));
        let parent = canonical(temp.path()).join("private");
        private(&parent);
        let target = canonical(temp.path()).join("target");
        fs::write(&target, b"do not mutate").unwrap_or_else(|e| panic!("target: {e}"));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("target permissions: {e}"));
        let path = parent.join("attempts.jsonl");
        fs::hard_link(&target, &path).unwrap_or_else(|e| panic!("log link: {e}"));
        HeadingAttemptLog::new(&path).record(attempt(HeadingAttemptOutcome::Accepted));
        assert_eq!(
            fs::read(&target).unwrap_or_else(|e| panic!("target: {e}")),
            b"do not mutate"
        );
        fs::remove_file(&path).unwrap_or_else(|e| panic!("unlink: {e}"));
        fs::remove_file(parent.join(".attempts.jsonl.lock"))
            .unwrap_or_else(|e| panic!("remove lock: {e}"));
        fs::hard_link(&target, parent.join(".attempts.jsonl.lock"))
            .unwrap_or_else(|e| panic!("lock link: {e}"));
        HeadingAttemptLog::new(&path).record(attempt(HeadingAttemptOutcome::Accepted));
        assert_eq!(
            fs::read(&target).unwrap_or_else(|e| panic!("target: {e}")),
            b"do not mutate"
        );
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_or_oversize_is_replaced_and_held_lock_skips() {
        let temp = tempfile::tempdir().unwrap_or_else(|e| panic!("temp: {e}"));
        let parent = canonical(temp.path()).join("private");
        private(&parent);
        let path = parent.join("attempts.jsonl");
        fs::write(&path, vec![b'x'; MAX_BYTES as usize + 1])
            .unwrap_or_else(|e| panic!("oversize: {e}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("permissions: {e}"));
        let log = HeadingAttemptLog::new(&path);
        log.record(attempt(HeadingAttemptOutcome::Accepted));
        assert_eq!(records(&path).len(), 1);
        fs::write(&path, b"{not-json}\n").unwrap_or_else(|e| panic!("malformed: {e}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("malformed permissions: {e}"));
        log.record(attempt(HeadingAttemptOutcome::Accepted));
        assert_eq!(records(&path).len(), 1);
        assert!(!parent.join(".attempts.jsonl.next").exists());
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(parent.join(".attempts.jsonl.lock"))
            .unwrap_or_else(|e| panic!("lock: {e}"));
        lock.lock_exclusive()
            .unwrap_or_else(|e| panic!("hold: {e}"));
        let before = fs::read(&path).unwrap_or_else(|e| panic!("before: {e}"));
        log.record(attempt(HeadingAttemptOutcome::Accepted));
        fs2::FileExt::unlock(&lock).unwrap_or_else(|e| panic!("unlock: {e}"));
        assert_eq!(
            fs::read(&path).unwrap_or_else(|e| panic!("after: {e}")),
            before
        );
    }

    #[test]
    fn post_snapshot_growth_is_capped_and_replaced() {
        let temp = tempfile::tempdir().unwrap_or_else(|e| panic!("temp: {e}"));
        let parent = canonical(temp.path()).join("private");
        private(&parent);
        let path = parent.join("attempts.jsonl");
        fs::write(&path, b"").unwrap_or_else(|e| panic!("empty log: {e}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("permissions: {e}"));
        let mut descriptor = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap_or_else(|e| panic!("open: {e}"));
        let snapshot = descriptor
            .metadata()
            .unwrap_or_else(|e| panic!("snapshot: {e}"))
            .len();
        fs::write(&path, vec![b'x'; MAX_BYTES as usize * 2])
            .unwrap_or_else(|e| panic!("post-snapshot growth: {e}"));
        assert!(matches!(
            unix::read_valid_from_snapshot(&mut descriptor, snapshot),
            Ok(Existing::Replace)
        ));
    }
}
