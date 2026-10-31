use crate::{
    config::Config,
    paths,
    platform::{self, CloudFiles, DriveAlias, Hydrator},
    remote::{Expected, Remote, RemoteError, RemoteObject},
    state::State,
    tray::{Control, SyncState},
};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

pub struct Engine {
    config: Config,
    remote: Arc<dyn Remote>,
    state: Arc<State>,
    observed: Mutex<HashMap<String, (u64, u64, Instant)>>,
    _cloud: CloudFiles,
    _drive: Option<DriveAlias>,
    _lock: File,
    root_id: String,
    missing: HashMap<String, Instant>,
}

struct Fetcher {
    remote: Arc<dyn Remote>,
    state: Arc<State>,
    runtime: tokio::runtime::Handle,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Hydrator for Fetcher {
    fn fetch(&self, key: &str, etag: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        paths::validate_key(key)?;
        self.runtime.block_on(async {
            let _permit = self.slots.acquire().await?;
            Ok(self.remote.read_range(key, etag, offset, length).await?)
        })
    }
    fn accessed(&self, key: &str) {
        if let Err(e) = self.state.touch(key) {
            tracing::warn!(%e,"cannot update cache access time");
        }
    }
}

impl Engine {
    pub fn start(
        config: Config,
        remote: Arc<dyn Remote>,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self> {
        config.validate()?;
        std::fs::create_dir_all(&config.state_dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config.state_dir.join("instance.lock"))?;
        lock.try_lock()
            .context("another Bucket Mount instance uses this state directory")?;
        let staging = config.state_dir.join("staging");
        if staging.exists() {
            ensure!(
                std::fs::symlink_metadata(&staging)?.file_attributes() & 0x400 == 0,
                "staging directory must not be a reparse point"
            );
            for entry in std::fs::read_dir(&staging)? {
                let entry = entry?;
                if entry.file_type()?.is_file()
                    && entry.path().extension().is_some_and(|e| e == "upload")
                {
                    // Crash remnants are redundant snapshots; the original edited files persist.
                    std::fs::remove_file(entry.path())?;
                }
            }
        }
        let binding = serde_json::to_string(&(
            &config.endpoint,
            &config.bucket,
            &config.prefix,
            &config.mount_dir,
        ))?;
        let state = Arc::new(State::open(
            &config.state_dir.join("state.sqlite3"),
            &binding,
        )?);
        ensure!(
            config.mount_dir.exists() || state.all()?.is_empty(),
            "the sync folder is missing; restore its original location before restarting"
        );
        std::fs::create_dir_all(&config.mount_dir)?;
        ensure!(
            std::fs::symlink_metadata(&config.mount_dir)?.file_attributes() & 0x400 == 0,
            "mount root must not be a junction or symlink"
        );
        let root_id = platform::root_id(&config.mount_dir)?;
        state.bind("mount directory identity", &root_id)?;
        let hydrator = Arc::new(Fetcher {
            remote: remote.clone(),
            state: state.clone(),
            runtime,
            slots: Arc::new(tokio::sync::Semaphore::new(config.upload_concurrency)),
        });
        let cloud = CloudFiles::connect(&config.mount_dir, hydrator)?;
        let drive = config
            .drive_letter
            .map(|letter| DriveAlias::create(letter, &config.mount_dir))
            .transpose()?;
        Ok(Self {
            config,
            remote,
            state,
            observed: Mutex::new(HashMap::new()),
            _cloud: cloud,
            _drive: drive,
            _lock: lock,
            root_id,
            missing: HashMap::new(),
        })
    }

    /// One complete remote listing must succeed before interpreting any deletions.
    pub async fn sync_once(&mut self) -> Result<SyncStats> {
        ensure!(
            platform::root_id(&self.config.mount_dir)? == self.root_id,
            "sync folder was replaced; refusing to process deletions"
        );
        let listing = self
            .remote
            .list()
            .await
            .context("list bucket; local edits remain queued")?;
        let remote: BTreeMap<_, _> = listing.into_iter().map(|o| (o.key.clone(), o)).collect();
        paths::validate_namespace(remote.keys().map(String::as_str))?;
        let baseline = self.state.all()?;
        let namespace = self._cloud.namespace();
        let generation = namespace
            .snapshot()
            .context("a local rename or deletion is in progress")?;
        let local = self.scan()?;
        ensure!(
            namespace.snapshot() == Some(generation),
            "local namespace changed during scan; retrying"
        );
        ensure!(
            platform::root_id(&self.config.mount_dir)? == self.root_id,
            "sync folder changed during scan"
        );
        // Do not allow a differently cased local name to overwrite a remote object.
        paths::validate_namespace(remote.keys().chain(local.keys()).map(String::as_str))?;
        let keys: BTreeSet<_> = remote
            .keys()
            .chain(baseline.keys())
            .chain(local.keys())
            .cloned()
            .collect();
        // Renames retain their original identity until the destination is uploaded.
        // Keep the old S3 object available for hydration throughout that upload.
        let referenced: BTreeSet<_> = local
            .values()
            .filter_map(|f| f.moved_from.clone())
            .collect();
        let deletions: BTreeSet<_> = baseline
            .keys()
            .filter(|k| !local.contains_key(*k) && remote.contains_key(*k))
            .cloned()
            .collect();
        self.missing.retain(|k, _| deletions.contains(k));
        for key in &deletions {
            self.missing.entry(key.clone()).or_insert_with(Instant::now);
        }
        let mut stats = SyncStats::default();
        {
            let this = &*self;
            let mut work =
                futures_util::stream::iter(keys.iter().filter(|k| !deletions.contains(*k)))
                    .map(|key| {
                        let (base, object, file) =
                            (baseline.get(key), remote.get(key), local.get(key));
                        async move {
                            let mut part = SyncStats::default();
                            if let Err(e) = this.reconcile(key, base, object, file, &mut part).await
                            {
                                part.errors += 1;
                                tracing::warn!(key,error=%e,"file sync deferred");
                            }
                            part
                        }
                    })
                    .buffer_unordered(self.config.upload_concurrency);
            while let Some(part) = work.next().await {
                stats.merge(part);
            }
        }
        // Run remote deletions after local mutations have completed: a namespace
        // freeze must never block a local CFAPI mutation on this same executor.
        for key in deletions {
            if referenced.contains(&key)
                || self.missing[&key].elapsed() < Duration::from_secs(self.config.settle_delay_secs)
            {
                stats.pending += 1;
                continue;
            }
            let old = &baseline[&key];
            let current = &remote[&key];
            if old.etag != current.etag {
                let path = paths::local_path(&self.config.mount_dir, &key)?;
                if let Err(e) = self.materialize(&path, current) {
                    stats.errors += 1;
                    tracing::warn!(key,%e,"restore concurrent remote edit deferred");
                } else {
                    stats.conflicts += 1;
                }
                continue;
            }
            let Some(freeze) = namespace.try_freeze(generation) else {
                stats.pending += 1;
                continue;
            };
            let path = paths::local_path(&self.config.mount_dir, &key)?;
            if path.exists() || platform::root_id(&self.config.mount_dir)? != self.root_id {
                stats.pending += 1;
                continue;
            }
            let result = match tokio::time::timeout(
                Duration::from_secs(30),
                self.remote.delete(&key, &old.etag),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(RemoteError::Other(anyhow::anyhow!(
                    "remote delete timed out; retained local baseline for retry"
                ))),
            };
            drop(freeze);
            match result {
                Ok(()) | Err(RemoteError::NotFound) => {
                    self.state.remove(&key)?;
                    self.missing.remove(&key);
                    stats.removed += 1;
                }
                Err(RemoteError::Conflict) => {
                    stats.pending += 1;
                }
                Err(e) => {
                    stats.errors += 1;
                    tracing::warn!(key,%e,"remote deletion deferred");
                }
            }
        }
        self.evict(&mut stats)?;
        self.observed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|k, _| local.contains_key(k));
        Ok(stats)
    }

    fn scan(&self) -> Result<BTreeMap<String, LocalFile>> {
        let mut files = BTreeMap::new();
        for entry in walkdir::WalkDir::new(&self.config.mount_dir).follow_links(false) {
            let entry =
                entry.context("scan local folder (deletion processing cancelled on error)")?;
            if entry.depth() == 0 {
                continue;
            }
            if entry.file_type().is_dir() {
                // Directory placeholders let CFAPI notify us before parent renames.
                // Placeholders keep the reparse attribute, so only plain directories
                // need the conversion walk on every pass.
                if entry.metadata()?.file_attributes() & 0x400 == 0 {
                    platform::ensure_directory_placeholder(entry.path())?;
                } else {
                    ensure!(
                        platform::inspect(entry.path())?.placeholder,
                        "unsupported reparse directory in sync folder"
                    );
                }
                continue;
            }
            ensure!(
                !entry.file_type().is_symlink(),
                "symlinks are unsupported in the sync folder: {}",
                entry.path().display()
            );
            let relative = entry.path().strip_prefix(&self.config.mount_dir)?;
            let key = relative
                .to_str()
                .context("non-Unicode filename")?
                .replace('\\', "/");
            paths::validate_key(&key)?;
            let (metadata, state, identity, version) = platform::scan_entry(entry.path())?;
            ensure!(
                metadata.file_attributes() & 0x400 == 0 || state.placeholder,
                "unsupported reparse point in sync folder"
            );
            let moved_from = identity
                .as_ref()
                .filter(|id| id.key != key)
                .map(|id| id.key.clone());
            let etag = identity.map(|id| id.etag);
            files.insert(
                key,
                LocalFile {
                    size: metadata.len(),
                    modified: metadata.last_write_time(),
                    clean: state.placeholder && state.in_sync && moved_from.is_none(),
                    moved_from,
                    etag,
                    version,
                },
            );
        }
        Ok(files)
    }

    async fn reconcile(
        &self,
        key: &str,
        base: Option<&RemoteObject>,
        remote: Option<&RemoteObject>,
        local: Option<&LocalFile>,
        stats: &mut SyncStats,
    ) -> Result<()> {
        let path = paths::local_path(&self.config.mount_dir, key)?;
        match local {
            Some(local) if !local.clean => {
                {
                    let mut observed = self.observed.lock().unwrap_or_else(|p| p.into_inner());
                    let seen = observed.entry(key.to_owned()).or_insert((
                        local.size,
                        local.modified,
                        Instant::now(),
                    ));
                    if (seen.0, seen.1) != (local.size, local.modified) {
                        *seen = (local.size, local.modified, Instant::now());
                    }
                    if seen.2.elapsed() < Duration::from_secs(self.config.settle_delay_secs) {
                        stats.pending += 1;
                        return Ok(());
                    }
                }
                let source = path.clone();
                let state_dir = self.config.state_dir.clone();
                let snapshot =
                    tokio::task::spawn_blocking(move || Snapshot::take(&source, &state_dir))
                        .await??;
                let expected = base.map_or(Expected::Absent, |b| Expected::Etag(b.etag.clone()));
                match self.remote.upload(key, &snapshot.path, expected).await {
                    Ok(object) => {
                        // Commit the acknowledged server version first. If marking fails or the app
                        // crashes, retry the edit against this version rather than an obsolete ETag.
                        self.state.save(&object)?;
                        platform::mark_synced_if_unchanged(
                            &path,
                            key,
                            &object.etag,
                            snapshot.version,
                        )?;
                        stats.uploaded += 1;
                        self.observed
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(key);
                    }
                    Err(RemoteError::Conflict | RemoteError::NotFound) => {
                        let conflict_key = paths::conflict_key(key, &self.config.device_id);
                        let conflict = self
                            .remote
                            .upload(&conflict_key, &snapshot.path, Expected::Absent)
                            .await?;
                        let conflict_path =
                            paths::local_path(&self.config.mount_dir, &conflict_key)?;
                        ensure!(!conflict_path.exists(), "conflict path already exists");
                        self.materialize(&conflict_path, &conflict)?;
                        // This deliberate baseline mismatch also makes recovery safe if interrupted
                        // between converting the original to the saved conflict and refreshing it.
                        let mut saved = conflict.clone();
                        saved.key = key.to_owned();
                        self.state.save(&saved)?;
                        let committed_version = platform::mark_synced_if_unchanged(
                            &path,
                            &conflict_key,
                            &conflict.etag,
                            snapshot.version,
                        )?;
                        if let Some(latest) = remote {
                            platform::update_placeholder_if_unchanged(
                                &path,
                                key,
                                &latest.etag,
                                latest.size,
                                latest.modified_ms,
                                committed_version,
                            )?;
                            self.state.save(latest)?;
                        } else {
                            platform::remove_clean_if_unchanged(&path, committed_version)?;
                            self.state.remove(key)?;
                        }
                        stats.conflicts += 1;
                        self.observed
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(key);
                        tracing::warn!(
                            key,
                            conflict_key,
                            "concurrent edit preserved as conflict file"
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Some(local) => match remote {
                Some(object) => {
                    if local.etag.as_deref() != Some(object.etag.as_str()) {
                        platform::update_placeholder_if_unchanged(
                            &path,
                            key,
                            &object.etag,
                            object.size,
                            object.modified_ms,
                            local.version,
                        )?;
                        stats.refreshed += 1;
                    }
                    if base != Some(object) {
                        self.state.save(object)?;
                    }
                }
                None => {
                    platform::remove_clean_if_unchanged(&path, local.version)?;
                    self.state.remove(key)?;
                    stats.removed += 1;
                }
            },
            None => match (base, remote) {
                (Some(_), Some(_)) => {
                    stats.pending += 1;
                } // deletion phase owns this case
                (None, Some(object)) => {
                    self.materialize(&path, object)?;
                    stats.refreshed += 1;
                }
                (_, None) => {
                    self.state.remove(key)?;
                }
            },
        }
        Ok(())
    }
    fn materialize(&self, path: &Path, object: &RemoteObject) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        platform::create_placeholder(
            path,
            &object.key,
            &object.etag,
            object.size,
            object.modified_ms,
        )?;
        self.state.save(object)
    }

    fn evict(&self, stats: &mut SyncStats) -> Result<()> {
        let mut candidates = Vec::new();
        let (mut bytes, mut count) = (0u64, 0usize);
        for key in self.state.lru()? {
            let path = paths::local_path(&self.config.mount_dir, &key)?;
            if !path.exists() {
                continue;
            }
            let info = platform::inspect(&path)?;
            if info.hydrated_bytes == 0 {
                continue;
            }
            bytes = bytes.saturating_add(info.hydrated_bytes);
            count += 1;
            if info.placeholder && info.in_sync && !info.pinned {
                candidates.push((path, info.hydrated_bytes));
            }
        }
        for (path, size) in candidates {
            if bytes <= self.config.cache_max_bytes && count <= self.config.cache_max_files {
                break;
            }
            match platform::dehydrate(&path) {
                Ok(()) => {
                    bytes = bytes.saturating_sub(size);
                    count = count.saturating_sub(1);
                    stats.evicted += 1;
                }
                Err(e) => tracing::debug!(path=%path.display(),%e,"cache file busy; keep local"),
            }
        }
        stats.cached_bytes = bytes;
        stats.cached_files = count;
        stats.over_budget =
            bytes > self.config.cache_max_bytes || count > self.config.cache_max_files;
        Ok(())
    }
}

struct LocalFile {
    size: u64,
    modified: u64,
    clean: bool,
    moved_from: Option<String>,
    etag: Option<String>,
    version: u64,
}
struct Snapshot {
    path: PathBuf,
    version: u64,
}
impl Snapshot {
    fn take(path: &Path, state_dir: &Path) -> Result<Self> {
        // Hydration changes NTFS metadata, including the close-time USN. Finish
        // and close that IO before capturing the version of the upload snapshot.
        if platform::inspect(path)?.placeholder {
            platform::hydrate(path)?;
        }
        // Deny writes and deletion during the copy. Network IO operates on the
        // durable snapshot, so an editor is never blocked for an entire upload.
        let mut source = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(path)
            .context("file busy; waiting for writer to close")?;
        let staging = state_dir.join("staging");
        std::fs::create_dir_all(&staging)?;
        let mut snapshot = Self {
            path: staging.join(format!("{}.upload", uuid::Uuid::new_v4())),
            version: 0,
        };
        let mut dest = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&snapshot.path)?;
        std::io::copy(&mut source, &mut dest)?;
        dest.sync_all()?;
        snapshot.version = platform::file_version_from_file(&source)?;
        Ok(snapshot)
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default, Debug)]
pub struct SyncStats {
    pub uploaded: usize,
    pub refreshed: usize,
    pub removed: usize,
    pub conflicts: usize,
    pub pending: usize,
    pub errors: usize,
    pub evicted: usize,
    pub cached_bytes: u64,
    pub cached_files: usize,
    pub over_budget: bool,
}
impl SyncStats {
    fn merge(&mut self, other: Self) {
        self.uploaded += other.uploaded;
        self.refreshed += other.refreshed;
        self.removed += other.removed;
        self.conflicts += other.conflicts;
        self.pending += other.pending;
        self.errors += other.errors;
    }
}
impl std::fmt::Display for SyncStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} cached files, {} MiB; {} pending, {} conflicts, {} errors{}",
            self.cached_files,
            self.cached_bytes / 1024 / 1024,
            self.pending,
            self.conflicts,
            self.errors,
            if self.over_budget {
                " (cache pinned or busy)"
            } else {
                ""
            }
        )
    }
}

pub fn run_loop(mut engine: Engine, runtime: &tokio::runtime::Runtime, control: &Control) {
    let interval = Duration::from_secs(engine.config.poll_interval_secs);
    let mut due = Instant::now();
    while !control.stopping.load(Ordering::Relaxed) {
        if !control.paused.load(Ordering::Relaxed)
            && (control.sync_requested.swap(false, Ordering::Relaxed) || Instant::now() >= due)
        {
            control.set_status(SyncState::Syncing, "Synchronizing…".into());
            match runtime.block_on(engine.sync_once()) {
                Ok(stats) => {
                    tracing::info!(?stats, "sync complete");
                    control.set_status(
                        if stats.errors == 0 {
                            SyncState::Idle
                        } else {
                            SyncState::Error
                        },
                        stats.to_string(),
                    );
                }
                Err(e) => {
                    tracing::warn!(error=%e,"sync deferred");
                    control.set_status(SyncState::Error, format!("Sync deferred: {e}"));
                }
            }
            due = Instant::now() + interval;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct MemoryRemote {
        objects: Mutex<BTreeMap<String, (RemoteObject, Vec<u8>)>>,
        serial: AtomicUsize,
        reads: AtomicUsize,
    }
    impl MemoryRemote {
        fn put(&self, key: &str, bytes: &[u8]) -> RemoteObject {
            let n = self.serial.fetch_add(1, Ordering::SeqCst);
            let object = RemoteObject {
                key: key.into(),
                etag: format!("\"v{n}\""),
                size: bytes.len() as u64,
                modified_ms: crate::state::now_ms(),
            };
            self.objects
                .lock()
                .unwrap()
                .insert(key.into(), (object.clone(), bytes.to_vec()));
            object
        }
    }
    #[async_trait::async_trait]
    impl Remote for MemoryRemote {
        async fn list(&self) -> Result<Vec<RemoteObject>, RemoteError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .values()
                .map(|(o, _)| o.clone())
                .collect())
        }
        async fn read_range(
            &self,
            key: &str,
            etag: &str,
            offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, RemoteError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let objects = self.objects.lock().unwrap();
            let (o, bytes) = objects.get(key).ok_or(RemoteError::NotFound)?;
            if o.etag != etag {
                return Err(RemoteError::Conflict);
            }
            Ok(bytes[offset as usize..offset as usize + length].to_vec())
        }
        async fn upload(
            &self,
            key: &str,
            path: &Path,
            expected: Expected,
        ) -> Result<RemoteObject, RemoteError> {
            let bytes = tokio::fs::read(path).await.map_err(anyhow::Error::from)?;
            let mut objects = self.objects.lock().unwrap();
            let allowed = match (expected, objects.get(key)) {
                (Expected::Absent, None) => true,
                (Expected::Etag(e), Some((o, _))) => e == o.etag,
                _ => false,
            };
            if !allowed {
                return Err(RemoteError::Conflict);
            }
            let n = self.serial.fetch_add(1, Ordering::SeqCst);
            let o = RemoteObject {
                key: key.into(),
                etag: format!("\"v{n}\""),
                size: bytes.len() as u64,
                modified_ms: crate::state::now_ms(),
            };
            objects.insert(key.into(), (o.clone(), bytes));
            Ok(o)
        }
        async fn delete(&self, key: &str, etag: &str) -> Result<(), RemoteError> {
            let mut objects = self.objects.lock().unwrap();
            match objects.get(key) {
                Some((o, _)) if o.etag != etag => return Err(RemoteError::Conflict),
                None => return Err(RemoteError::NotFound),
                _ => {}
            }
            objects.remove(key);
            Ok(())
        }
    }

    #[test]
    #[ignore = "requires a running Windows Cloud Files filter and local NTFS"]
    fn live_two_device_sync_conflict_cache_and_delete() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
        let temp = tempfile::Builder::new()
            .prefix(".bucket-mount-engine-test-")
            .tempdir_in(std::env::current_dir()?)?;
        struct Roots(Vec<PathBuf>);
        impl Drop for Roots {
            fn drop(&mut self) {
                use std::os::windows::ffi::OsStrExt;
                for p in &self.0 {
                    let w: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
                    unsafe {
                        windows_sys::Win32::Storage::CloudFilters::CfUnregisterSyncRoot(w.as_ptr());
                    }
                }
            }
        }
        let a = Config {
            bucket: "test".into(),
            mount_dir: temp.path().join("a"),
            state_dir: temp.path().join("state-a"),
            settle_delay_secs: 1,
            cache_max_files: 1,
            ..Config::default()
        };
        let b = Config {
            mount_dir: temp.path().join("b"),
            state_dir: temp.path().join("state-b"),
            device_id: "device-b".into(),
            ..a.clone()
        };
        let _roots = Roots(vec![a.mount_dir.clone(), b.mount_dir.clone()]);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let remote = Arc::new(MemoryRemote::default());
        remote.put("hello.txt", b"original remote bytes");
        let mut one = Engine::start(a.clone(), remote.clone(), runtime.handle().clone())?;
        let mut two = Engine::start(b.clone(), remote.clone(), runtime.handle().clone())?;
        ensure!(
            runtime.block_on(one.sync_once())?.errors == 0,
            "first listing failed"
        );
        ensure!(
            runtime.block_on(two.sync_once())?.errors == 0,
            "second listing failed"
        );
        ensure!(
            remote.reads.load(Ordering::SeqCst) == 0,
            "listing downloaded file data"
        );
        ensure!(
            std::fs::read(a.mount_dir.join("hello.txt"))? == b"original remote bytes",
            "on-demand read failed"
        );
        std::fs::write(a.mount_dir.join("hello.txt"), b"edit from machine A")?;
        std::fs::write(b.mount_dir.join("hello.txt"), b"edit from machine B")?;
        runtime.block_on(one.sync_once())?;
        runtime.block_on(two.sync_once())?;
        std::thread::sleep(Duration::from_millis(1100));
        let first = runtime.block_on(one.sync_once())?;
        ensure!(
            first.uploaded == 1 && first.errors == 0,
            "first upload: {first:?}"
        );
        let second = runtime.block_on(two.sync_once())?;
        ensure!(
            second.conflicts == 1 && second.errors == 0,
            "conflict: {second:?}"
        );
        ensure!(
            std::fs::read(b.mount_dir.join("hello.txt"))? == b"edit from machine A",
            "winning revision missing"
        );
        let conflict = runtime
            .block_on(remote.list())?
            .into_iter()
            .find(|o| o.key.contains(".conflict-"))
            .context("conflict file missing")?;
        ensure!(
            std::fs::read(b.mount_dir.join(&conflict.key))? == b"edit from machine B",
            "losing edit was not preserved"
        );
        let cached = runtime.block_on(two.sync_once())?;
        ensure!(cached.cached_files <= 1, "cache count exceeded: {cached:?}");
        std::fs::remove_file(a.mount_dir.join("hello.txt"))?;
        remote.put("hello.txt", b"remote edit concurrent with local delete");
        runtime.block_on(one.sync_once())?;
        std::thread::sleep(Duration::from_millis(1100));
        let recovered = runtime.block_on(one.sync_once())?;
        ensure!(
            recovered.conflicts == 1 && recovered.errors == 0,
            "delete/edit conflict: {recovered:?}"
        );
        ensure!(
            std::fs::read(a.mount_dir.join("hello.txt"))?
                == b"remote edit concurrent with local delete",
            "remote edit deleted"
        );
        std::fs::remove_file(a.mount_dir.join("hello.txt"))?;
        runtime.block_on(one.sync_once())?;
        std::thread::sleep(Duration::from_millis(1100));
        ensure!(
            runtime.block_on(one.sync_once())?.errors == 0,
            "conditional deletion failed"
        );
        ensure!(
            !runtime
                .block_on(remote.list())?
                .iter()
                .any(|o| o.key == "hello.txt"),
            "local delete did not propagate"
        );
        ensure!(
            runtime.block_on(two.sync_once())?.errors == 0,
            "remote delete failed"
        );
        ensure!(
            !b.mount_dir.join("hello.txt").exists(),
            "deleted file remains on second machine"
        );
        remote.put(
            "before-rename.bin",
            b"dehydrated rename must keep its backing object",
        );
        runtime.block_on(one.sync_once())?;
        ensure!(
            platform::inspect(&a.mount_dir.join("before-rename.bin"))?.hydrated_bytes == 0,
            "rename fixture unexpectedly hydrated"
        );
        std::fs::rename(
            a.mount_dir.join("before-rename.bin"),
            a.mount_dir.join("after-rename.bin"),
        )?;
        runtime.block_on(one.sync_once())?;
        std::thread::sleep(Duration::from_millis(1100));
        let renamed = runtime.block_on(one.sync_once())?;
        ensure!(
            renamed.uploaded == 1 && renamed.errors == 0,
            "rename upload failed: {renamed:?}"
        );
        ensure!(
            std::fs::read(a.mount_dir.join("after-rename.bin"))?
                == b"dehydrated rename must keep its backing object",
            "rename lost contents"
        );
        runtime.block_on(one.sync_once())?;
        let listed = runtime.block_on(remote.list())?;
        ensure!(
            listed.iter().any(|o| o.key == "after-rename.bin")
                && !listed.iter().any(|o| o.key == "before-rename.bin"),
            "rename did not propagate safely"
        );
        drop(one);
        drop(two);
        Ok(())
    }
}
