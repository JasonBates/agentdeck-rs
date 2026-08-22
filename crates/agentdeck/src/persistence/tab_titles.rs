//! Durable ownership state for optional Herdr tab-title synchronization.
//!
//! The store never renames a tab. It only loads and atomically persists the pure
//! ownership policy from `agentdeck-core`. On Unix, pre-existing public, linked,
//! or non-regular state is rejected rather than repaired; Windows remains disabled
//! until its ACL and reparse-point behaviour has native coverage.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use agentdeck_core::tab_titles::{TAB_TITLE_STATE_VERSION, TabTitleOwnership};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const TAB_TITLE_STATE_FILE: &str = "tab-titles.json";
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_MANAGED_TABS: usize = 4096;
const MAX_TAB_ID_BYTES: usize = 1024;
const MAX_TITLE_BYTES: usize = 1024;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TabTitleStoreError {
    #[error("tab-title state does not exist")]
    Missing,
    #[error("tab-title state path is not a private regular file")]
    UnsafeFile,
    #[error("tab-title state exceeded its size bound")]
    TooLarge,
    #[error("tab-title state could not be read")]
    Read,
    #[error("tab-title state was malformed")]
    Malformed,
    #[error("tab-title state version {0} is unsupported")]
    UnsupportedVersion(u32),
    #[error("tab-title state contains invalid identifiers or titles")]
    InvalidValue,
    #[error("tab-title state could not be written atomically")]
    Write,
    #[error("tab-title state persistence is unsupported on this platform")]
    Unsupported,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredState {
    version: u32,
    managed: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct TabTitleStore {
    path: PathBuf,
}

impl TabTitleStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load ownership from the resolved state path.
    pub fn load(&self) -> Result<TabTitleOwnership, TabTitleStoreError> {
        #[cfg(unix)]
        {
            unix::load_path(&self.path)
        }
        #[cfg(not(unix))]
        {
            let _ = self;
            Err(TabTitleStoreError::Unsupported)
        }
    }

    /// Persist to the resolved path and clear the policy's dirty bit only after
    /// atomic replacement plus parent-directory durability succeeds.
    pub fn save(&self, ownership: &mut TabTitleOwnership) -> Result<(), TabTitleStoreError> {
        validate_managed(ownership.managed())?;
        let state = StoredState {
            version: TAB_TITLE_STATE_VERSION,
            managed: ownership.managed().clone(),
        };
        let bytes = serde_json::to_vec(&state).map_err(|_| TabTitleStoreError::Write)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(TabTitleStoreError::TooLarge);
        }

        #[cfg(unix)]
        {
            unix::save_path(&self.path, &bytes)?;
            ownership.mark_persisted();
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (self, bytes);
            Err(TabTitleStoreError::Unsupported)
        }
    }
}

fn validate_managed(managed: &BTreeMap<String, String>) -> Result<(), TabTitleStoreError> {
    let valid = managed.len() <= MAX_MANAGED_TABS
        && managed.iter().all(|(tab_id, title)| {
            (1..=MAX_TAB_ID_BYTES).contains(&tab_id.len())
                && (1..=MAX_TITLE_BYTES).contains(&title.len())
                && tab_id.trim() == tab_id
                && title.trim() == title
                && !tab_id.chars().any(char::is_control)
                && !title.chars().any(char::is_control)
        });
    if valid {
        Ok(())
    } else {
        Err(TabTitleStoreError::InvalidValue)
    }
}

#[cfg(unix)]
mod unix {
    use super::{
        MAX_STATE_BYTES, StoredState, TAB_TITLE_STATE_VERSION, TabTitleOwnership,
        TabTitleStoreError, validate_managed,
    };
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
        io::{Read as _, Seek as _, SeekFrom, Write as _},
        os::unix::fs::{MetadataExt as StdMetadataExt, PermissionsExt as _},
        path::{Component, Path},
    };

    #[derive(Debug)]
    pub(super) struct StateDirectory {
        dir: Dir,
        state: OsString,
        lock: OsString,
        temporary: OsString,
    }

    impl StateDirectory {
        pub(super) fn open(path: &Path, create: bool) -> Result<Self, TabTitleStoreError> {
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .filter(|name| !name.is_empty() && name.len() <= 96)
                .ok_or(TabTitleStoreError::UnsafeFile)?;
            if !path.is_absolute() {
                return Err(TabTitleStoreError::UnsafeFile);
            }
            let parent = path.parent().ok_or(TabTitleStoreError::UnsafeFile)?;
            let dir = open_private_parent(parent, create)?;
            Ok(Self {
                dir,
                state: name.into(),
                lock: format!(".{name}.lock").into(),
                temporary: format!(".{name}.next").into(),
            })
        }

        fn open_existing(&self, name: &OsStr) -> Result<fs::File, TabTitleStoreError> {
            let mut options = OpenOptions::new();
            options.read(true).write(true).follow(FollowSymlinks::No);
            let file = self
                .dir
                .open_with(name, &options)
                .map_err(map_open_error)?
                .into_std();
            self.verify(name, &file)?;
            Ok(file)
        }

        fn create_new(&self, name: &OsStr) -> Result<fs::File, TabTitleStoreError> {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .follow(FollowSymlinks::No);
            let file = self
                .dir
                .open_with(name, &options)
                .map_err(|_| TabTitleStoreError::Write)?
                .into_std();
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| TabTitleStoreError::Write)?;
            self.verify(name, &file)?;
            Ok(file)
        }

        fn open_or_create_lock(&self) -> Result<fs::File, TabTitleStoreError> {
            match self.open_existing(&self.lock) {
                Ok(file) => Ok(file),
                Err(TabTitleStoreError::Missing) => self.create_new(&self.lock),
                Err(error) => Err(error),
            }
        }

        fn verify(&self, name: &OsStr, file: &fs::File) -> Result<(), TabTitleStoreError> {
            let named = self
                .dir
                .symlink_metadata(name)
                .map_err(|_| TabTitleStoreError::UnsafeFile)?;
            if !named.is_file()
                || named.nlink() != 1
                || CapPermissionsExt::mode(&named.permissions()) & 0o7777 != 0o600
            {
                return Err(TabTitleStoreError::UnsafeFile);
            }
            let open = file.metadata().map_err(|_| TabTitleStoreError::Read)?;
            if !open.is_file()
                || StdMetadataExt::nlink(&open) != 1
                || open.permissions().mode() & 0o7777 != 0o600
                || CapMetadataExt::dev(&named) != StdMetadataExt::dev(&open)
                || CapMetadataExt::ino(&named) != StdMetadataExt::ino(&open)
            {
                return Err(TabTitleStoreError::UnsafeFile);
            }
            Ok(())
        }

        fn remove_safe_temporary(&self) -> Result<(), TabTitleStoreError> {
            match self.open_existing(&self.temporary) {
                Ok(file) => {
                    drop(file);
                    self.dir
                        .remove_file(&self.temporary)
                        .map_err(|_| TabTitleStoreError::Write)
                }
                Err(TabTitleStoreError::Missing) => Ok(()),
                Err(error) => Err(error),
            }
        }

        pub(super) fn save(&self, bytes: &[u8]) -> Result<(), TabTitleStoreError> {
            // Reject an unsafe existing state before creating the advisory lock.
            // `save_locked` repeats this check after taking the lock to close the
            // cooperating-writer race between this preflight and replacement.
            match self.open_existing(&self.state) {
                Ok(file) => drop(file),
                Err(TabTitleStoreError::Missing) => {}
                Err(error) => return Err(error),
            }
            let lock = self.open_or_create_lock()?;
            lock.try_lock_exclusive()
                .map_err(|_| TabTitleStoreError::Write)?;
            let result = self
                .verify(&self.lock, &lock)
                .and_then(|()| self.save_locked(bytes));
            let _ = fs2::FileExt::unlock(&lock);
            result
        }

        fn save_locked(&self, bytes: &[u8]) -> Result<(), TabTitleStoreError> {
            match self.open_existing(&self.state) {
                Ok(file) => drop(file),
                Err(TabTitleStoreError::Missing) => {}
                Err(error) => return Err(error),
            }
            self.remove_safe_temporary()?;
            let mut temporary = self.create_new(&self.temporary)?;
            temporary
                .write_all(bytes)
                .map_err(|_| TabTitleStoreError::Write)?;
            temporary.flush().map_err(|_| TabTitleStoreError::Write)?;
            temporary
                .sync_all()
                .map_err(|_| TabTitleStoreError::Write)?;
            self.verify(&self.temporary, &temporary)?;
            drop(temporary);

            match self.open_existing(&self.state) {
                Ok(file) => drop(file),
                Err(TabTitleStoreError::Missing) => {}
                Err(error) => return Err(error),
            }
            self.dir
                .rename(&self.temporary, &self.dir, &self.state)
                .map_err(|_| TabTitleStoreError::Write)?;
            let state = self.open_existing(&self.state)?;
            drop(state);
            sync_directory(&self.dir)
        }
    }

    pub(super) fn load_path(path: &Path) -> Result<TabTitleOwnership, TabTitleStoreError> {
        let directory = StateDirectory::open(path, false)?;
        let mut file = directory.open_existing(&directory.state)?;
        read_state(&directory, &mut file)
    }

    pub(super) fn save_path(path: &Path, bytes: &[u8]) -> Result<(), TabTitleStoreError> {
        StateDirectory::open(path, true)?.save(bytes)
    }

    fn read_state(
        directory: &StateDirectory,
        file: &mut fs::File,
    ) -> Result<TabTitleOwnership, TabTitleStoreError> {
        let size = file.metadata().map_err(|_| TabTitleStoreError::Read)?.len();
        if size > MAX_STATE_BYTES {
            return Err(TabTitleStoreError::TooLarge);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| TabTitleStoreError::Read)?;
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| TabTitleStoreError::Read)?;
        directory.verify(&directory.state, file)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(TabTitleStoreError::TooLarge);
        }
        let after = file.metadata().map_err(|_| TabTitleStoreError::Read)?.len();
        if after != size || bytes.len() as u64 != size {
            return Err(TabTitleStoreError::Read);
        }
        let state: StoredState =
            serde_json::from_slice(&bytes).map_err(|_| TabTitleStoreError::Malformed)?;
        if state.version != TAB_TITLE_STATE_VERSION {
            return Err(TabTitleStoreError::UnsupportedVersion(state.version));
        }
        validate_managed(&state.managed)?;
        Ok(TabTitleOwnership::from_managed(state.managed))
    }

    fn open_private_parent(path: &Path, create: bool) -> Result<Dir, TabTitleStoreError> {
        let mut current = Dir::open_ambient_dir("/", ambient_authority())
            .map_err(|_| TabTitleStoreError::Read)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => current = open_child(&current, name, create)?,
                _ => return Err(TabTitleStoreError::UnsafeFile),
            }
        }
        private_directory(&current)?;
        Ok(current)
    }

    fn open_child(parent: &Dir, name: &OsStr, create: bool) -> Result<Dir, TabTitleStoreError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .follow(FollowSymlinks::No)
            .maybe_dir(true);
        let file = match parent.open_with(name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                parent
                    .create_dir(name)
                    .map_err(|_| TabTitleStoreError::Write)?;
                parent
                    .set_permissions(
                        name,
                        CapPermissions::from_std(fs::Permissions::from_mode(0o700)),
                    )
                    .map_err(|_| TabTitleStoreError::Write)?;
                parent
                    .open_with(name, &options)
                    .map_err(|_| TabTitleStoreError::Write)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(TabTitleStoreError::Missing);
            }
            Err(_) => return Err(TabTitleStoreError::UnsafeFile),
        };
        Ok(Dir::from_std_file(file.into_std()))
    }

    fn private_directory(dir: &Dir) -> Result<(), TabTitleStoreError> {
        let metadata = dir
            .dir_metadata()
            .map_err(|_| TabTitleStoreError::UnsafeFile)?;
        if !metadata.is_dir() || CapPermissionsExt::mode(&metadata.permissions()) & 0o7777 != 0o700
        {
            return Err(TabTitleStoreError::UnsafeFile);
        }
        Ok(())
    }

    fn sync_directory(dir: &Dir) -> Result<(), TabTitleStoreError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .follow(FollowSymlinks::No)
            .maybe_dir(true);
        dir.open_with(OsStr::new("."), &options)
            .map_err(|_| TabTitleStoreError::Write)?
            .into_std()
            .sync_all()
            .map_err(|_| TabTitleStoreError::Write)
    }

    fn map_open_error(error: std::io::Error) -> TabTitleStoreError {
        if error.kind() == std::io::ErrorKind::NotFound {
            TabTitleStoreError::Missing
        } else {
            TabTitleStoreError::UnsafeFile
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    #[cfg(unix)]
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Barrier},
    };

    use tempfile::tempdir;

    use super::*;

    fn ownership(title: &str) -> TabTitleOwnership {
        TabTitleOwnership::from_managed(BTreeMap::from([("w1:t1".to_owned(), title.to_owned())]))
    }

    #[cfg(unix)]
    fn private(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::create_dir_all(path)
            .unwrap_or_else(|error| panic!("create private directory: {error}"));
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("private permissions: {error}"));
    }

    #[cfg(unix)]
    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize()
            .unwrap_or_else(|error| panic!("canonical path: {error}"))
    }

    #[cfg(unix)]
    fn write_private(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::write(path, bytes).unwrap_or_else(|error| panic!("write fixture: {error}"));
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|error| panic!("fixture permissions: {error}"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_corrupt_future_and_unknown_fields_are_distinct() {
        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        private(&root);
        let path = root.join(TAB_TITLE_STATE_FILE);
        let store = TabTitleStore::new(path.clone());
        assert_eq!(store.load(), Err(TabTitleStoreError::Missing));

        write_private(&path, b"not json");
        assert_eq!(store.load(), Err(TabTitleStoreError::Malformed));
        write_private(&path, br#"{"version":2,"managed":{}}"#);
        assert_eq!(store.load(), Err(TabTitleStoreError::UnsupportedVersion(2)));
        write_private(&path, br#"{"version":1,"managed":{},"secret":"no"}"#);
        assert_eq!(store.load(), Err(TabTitleStoreError::Malformed));
    }

    #[cfg(unix)]
    #[test]
    fn round_trip_replaces_last_good_and_clears_dirty_only_after_success() {
        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        private(&root);
        let path = root.join("state").join(TAB_TITLE_STATE_FILE);
        let store = TabTitleStore::new(path);
        let mut state = ownership("Initial");
        state.rename_succeeded(&agentdeck_core::tab_titles::TabRename {
            tab_id: "w1:t1".to_owned(),
            expected_current_label: "Initial".to_owned(),
            title: "Settled".to_owned(),
        });
        assert!(state.is_dirty());
        store
            .save(&mut state)
            .unwrap_or_else(|error| panic!("save: {error}"));
        assert!(!state.is_dirty());
        let loaded = store.load().unwrap_or_else(|error| panic!("load: {error}"));
        assert_eq!(
            loaded.managed().get("w1:t1").map(String::as_str),
            Some("Settled")
        );

        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(store.path())
            .unwrap_or_else(|error| panic!("metadata: {error}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let parent = store.path().parent().unwrap_or(&root);
        assert_eq!(
            fs::metadata(parent)
                .unwrap_or_else(|error| panic!("parent metadata: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_keeps_dirty_state() {
        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        private(&root);
        let blocker = root.join("blocker");
        fs::write(&blocker, b"file").unwrap_or_else(|error| panic!("blocker: {error}"));
        let store = TabTitleStore::new(blocker.join(TAB_TITLE_STATE_FILE));
        let mut state = ownership("Title");
        state.rename_succeeded(&agentdeck_core::tab_titles::TabRename {
            tab_id: "w1:t1".to_owned(),
            expected_current_label: "Title".to_owned(),
            title: "New title".to_owned(),
        });
        assert!(store.save(&mut state).is_err());
        assert!(state.is_dirty());
    }

    #[cfg(unix)]
    #[test]
    fn bounds_and_unsafe_files_are_rejected() {
        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        private(&root);
        let path = root.join(TAB_TITLE_STATE_FILE);
        write_private(&path, &vec![b'x'; MAX_STATE_BYTES as usize + 1]);
        assert_eq!(
            TabTitleStore::new(path.clone()).load(),
            Err(TabTitleStoreError::TooLarge)
        );

        use std::os::unix::fs::symlink;
        let target = root.join("target.json");
        write_private(&target, br#"{"version":1,"managed":{}}"#);
        fs::remove_file(&path).unwrap_or_else(|error| panic!("remove large fixture: {error}"));
        symlink(&target, &path).unwrap_or_else(|error| panic!("symlink fixture: {error}"));
        assert_eq!(
            TabTitleStore::new(path).load(),
            Err(TabTitleStoreError::UnsafeFile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_public_and_nonregular_state_or_lock_fail_closed() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        private(&root);
        let path = root.join(TAB_TITLE_STATE_FILE);
        let target = root.join("target.json");
        write_private(&target, br#"{"version":1,"managed":{}}"#);
        fs::hard_link(&target, &path).unwrap_or_else(|error| panic!("hardlink: {error}"));
        assert_eq!(
            TabTitleStore::new(path.clone()).load(),
            Err(TabTitleStoreError::UnsafeFile)
        );
        fs::remove_file(&path).unwrap_or_else(|error| panic!("remove hardlink: {error}"));

        write_private(&path, br#"{"version":1,"managed":{}}"#);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("public state: {error}"));
        assert_eq!(
            TabTitleStore::new(path.clone()).load(),
            Err(TabTitleStoreError::UnsafeFile)
        );
        let mut public_state = ownership("New title");
        assert_eq!(
            TabTitleStore::new(path.clone()).save(&mut public_state),
            Err(TabTitleStoreError::UnsafeFile)
        );
        assert!(!root.join(".tab-titles.json.lock").exists());
        fs::remove_file(&path).unwrap_or_else(|error| panic!("remove public state: {error}"));
        fs::create_dir(&path).unwrap_or_else(|error| panic!("directory state: {error}"));
        assert_eq!(
            TabTitleStore::new(path).load(),
            Err(TabTitleStoreError::UnsafeFile)
        );

        let public_parent = root.join("public-parent");
        private(&public_parent);
        let public_parent_state = public_parent.join(TAB_TITLE_STATE_FILE);
        write_private(&public_parent_state, br#"{"version":1,"managed":{}}"#);
        fs::set_permissions(&public_parent, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("public parent: {error}"));
        assert_eq!(
            TabTitleStore::new(public_parent_state).load(),
            Err(TabTitleStoreError::UnsafeFile)
        );

        let saved = root.join("saved").join(TAB_TITLE_STATE_FILE);
        let store = TabTitleStore::new(saved.clone());
        let mut ownership = ownership("Initial");
        store
            .save(&mut ownership)
            .unwrap_or_else(|error| panic!("initial save: {error}"));
        let lock = saved
            .parent()
            .unwrap_or(&root)
            .join(".tab-titles.json.lock");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("public lock: {error}"));
        ownership.rename_succeeded(&agentdeck_core::tab_titles::TabRename {
            tab_id: "w1:t1".to_owned(),
            expected_current_label: "Initial".to_owned(),
            title: "Improved".to_owned(),
        });
        assert_eq!(
            store.save(&mut ownership),
            Err(TabTitleStoreError::UnsafeFile)
        );
        assert!(ownership.is_dirty());
    }

    #[cfg(unix)]
    #[test]
    fn retained_directory_cannot_be_redirected_by_a_parent_swap() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        let parent = root.join("private");
        let outside = root.join("outside");
        private(&parent);
        private(&outside);
        let path = parent.join(TAB_TITLE_STATE_FILE);
        let retained = unix::StateDirectory::open(&path, true)
            .unwrap_or_else(|error| panic!("retain directory: {error}"));
        let moved = root.join("moved");
        fs::rename(&parent, &moved).unwrap_or_else(|error| panic!("move parent: {error}"));
        symlink(&outside, &parent).unwrap_or_else(|error| panic!("replace parent: {error}"));
        assert_eq!(
            TabTitleStore::new(path.clone()).load(),
            Err(TabTitleStoreError::UnsafeFile)
        );
        let bytes = serde_json::to_vec(&StoredState {
            version: TAB_TITLE_STATE_VERSION,
            managed: BTreeMap::from([("w1:t1".to_owned(), "Safe".to_owned())]),
        })
        .unwrap_or_else(|error| panic!("state bytes: {error}"));
        retained
            .save(&bytes)
            .unwrap_or_else(|error| panic!("retained save: {error}"));
        assert!(moved.join(TAB_TITLE_STATE_FILE).exists());
        assert!(!outside.join(TAB_TITLE_STATE_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_saves_leave_a_complete_private_state() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        private(&root);
        let state_parent = root.join("state");
        private(&state_parent);
        let path = state_parent.join(TAB_TITLE_STATE_FILE);
        let barrier = Arc::new(Barrier::new(3));
        let spawn = |title: &'static str, barrier: Arc<Barrier>| {
            let path = path.clone();
            std::thread::spawn(move || {
                let store = TabTitleStore::new(path);
                let mut state = ownership(title);
                barrier.wait();
                store.save(&mut state)
            })
        };
        let left = spawn("Left", Arc::clone(&barrier));
        let right = spawn("Right", Arc::clone(&barrier));
        barrier.wait();
        let results = [
            left.join().unwrap_or_else(|_| panic!("left writer panic")),
            right
                .join()
                .unwrap_or_else(|_| panic!("right writer panic")),
        ];
        assert!(results.iter().any(Result::is_ok));
        let loaded = TabTitleStore::new(path.clone())
            .load()
            .unwrap_or_else(|error| panic!("load final state: {error}"));
        assert!(matches!(
            loaded.managed().get("w1:t1").map(String::as_str),
            Some("Left" | "Right")
        ));
        assert_eq!(
            fs::metadata(&path)
                .unwrap_or_else(|error| panic!("state metadata: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!state_parent.join(".tab-titles.json.next").exists());
    }

    #[cfg(unix)]
    #[test]
    fn pruning_is_durably_saved_as_the_complete_ownership_set() {
        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = canonical(directory.path());
        private(&root);
        let store = TabTitleStore::new(root.join(TAB_TITLE_STATE_FILE));
        let mut ownership = TabTitleOwnership::from_managed(BTreeMap::from([
            ("w1:gone".to_owned(), "Old title".to_owned()),
            ("w1:live".to_owned(), "Live title".to_owned()),
        ]));
        let plan = ownership.plan(&[agentdeck_core::tab_titles::TabTitleObservation {
            tab_id: "w1:live".to_owned(),
            current_label: "Live title".to_owned(),
            model_title: None,
            agent_count: 1,
        }]);
        assert!(plan.is_empty());
        assert!(ownership.is_dirty());
        store
            .save(&mut ownership)
            .unwrap_or_else(|error| panic!("save pruned ownership: {error}"));
        let loaded = store
            .load()
            .unwrap_or_else(|error| panic!("load pruned state: {error}"));
        assert_eq!(
            loaded.managed(),
            &BTreeMap::from([("w1:live".to_owned(), "Live title".to_owned())])
        );
        assert!(!ownership.is_dirty());
    }

    #[cfg(not(unix))]
    #[test]
    fn persistence_is_explicitly_unsupported_off_unix() {
        let directory = tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let store = TabTitleStore::new(directory.path().join(TAB_TITLE_STATE_FILE));
        let mut state = ownership("Title");
        assert_eq!(store.load(), Err(TabTitleStoreError::Unsupported));
        assert_eq!(store.save(&mut state), Err(TabTitleStoreError::Unsupported));
    }
}
