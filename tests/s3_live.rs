//! Opt-in end-to-end tests for the local RustFS fixture. These never target production.
//! Run with scripts/test-rustfs.ps1; each test owns a fresh UUID prefix.

use std::{io::Write, path::Path};

use anyhow::{Context, Result, ensure};
use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region, RequestChecksumCalculation},
    error::ProvideErrorMetadata,
};
use bucket_mount::{
    config::Config,
    remote::{Expected, Remote, RemoteError, RemoteObject, S3Remote},
};

const TEST_BUCKET: &str = "bucket-mount-test";

struct Fixture {
    client: Client,
    config: Config,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn create() -> Result<Self> {
        let endpoint = std::env::var("BUCKET_MOUNT_TEST_ENDPOINT")
            .context("set BUCKET_MOUNT_TEST_ENDPOINT to the local RustFS API")?;
        let bucket = std::env::var("BUCKET_MOUNT_TEST_BUCKET")
            .context("set BUCKET_MOUNT_TEST_BUCKET=bucket-mount-test")?;
        let url = url::Url::parse(&endpoint)?;
        ensure!(
            matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
            "live tests only permit a loopback endpoint; never use production credentials"
        );
        ensure!(bucket == TEST_BUCKET, "live tests require {TEST_BUCKET}");
        let access_key = std::env::var("BUCKET_MOUNT_TEST_ACCESS_KEY")
            .map_err(|_| anyhow::anyhow!("set BUCKET_MOUNT_TEST_ACCESS_KEY"))?;
        let secret_key = std::env::var("BUCKET_MOUNT_TEST_SECRET_KEY")
            .map_err(|_| anyhow::anyhow!("set BUCKET_MOUNT_TEST_SECRET_KEY"))?;
        let directory = tempfile::Builder::new()
            .prefix(".bucket-mount-rustfs-test-")
            .tempdir_in(std::env::current_dir()?)?;
        let config = Config {
            endpoint,
            bucket,
            region: "us-east-1".into(),
            prefix: format!("integration/{}/", uuid::Uuid::new_v4()),
            mount_dir: directory.path().join("mount-a"),
            state_dir: directory.path().join("state-a"),
            allow_http: true,
            settle_delay_secs: 1,
            access_key_env: "BUCKET_MOUNT_TEST_ACCESS_KEY".into(),
            secret_key_env: "BUCKET_MOUNT_TEST_SECRET_KEY".into(),
            session_token_env: None,
            ..Config::default()
        };
        config.validate()?;
        let client = Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new(config.region.clone()))
                .endpoint_url(&config.endpoint)
                .force_path_style(true)
                .credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    None,
                    None,
                    "local-rustfs-test",
                ))
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .build(),
        );
        if let Err(error) = client.create_bucket().bucket(TEST_BUCKET).send().await {
            ensure!(
                error.as_service_error().is_some_and(|error| matches!(
                    error.code(),
                    Some("BucketAlreadyOwnedByYou" | "BucketAlreadyExists")
                )),
                "create local test bucket: {error}"
            );
        }
        client.head_bucket().bucket(TEST_BUCKET).send().await?;
        Ok(Self {
            client,
            config,
            directory,
        })
    }

    fn remote(&self) -> Result<S3Remote> {
        S3Remote::new(&self.config)
    }

    async fn upload_bytes(
        &self,
        remote: &dyn Remote,
        key: &str,
        bytes: &[u8],
        expected: Expected,
    ) -> Result<RemoteObject> {
        let mut source = tempfile::NamedTempFile::new_in(self.directory.path())?;
        source.write_all(bytes)?;
        source.flush()?;
        Ok(remote.upload(key, source.path(), expected).await?)
    }

    async fn assert_no_incomplete_uploads(&self) -> Result<()> {
        let uploads = self
            .client
            .list_multipart_uploads()
            .bucket(TEST_BUCKET)
            .prefix(&self.config.prefix)
            .send()
            .await?;
        ensure!(
            uploads.uploads().is_empty(),
            "multipart uploads were not completed or aborted"
        );
        Ok(())
    }

    async fn cleanup(&self) -> Result<()> {
        // This prefix is generated here, never supplied by an environment variable.
        ensure!(
            self.config.prefix.starts_with("integration/") && self.config.prefix.len() == 49,
            "refusing cleanup outside the generated test prefix"
        );
        loop {
            let uploads = self
                .client
                .list_multipart_uploads()
                .bucket(TEST_BUCKET)
                .prefix(&self.config.prefix)
                .send()
                .await?;
            if uploads.uploads().is_empty() {
                break;
            }
            for upload in uploads.uploads() {
                let key = upload.key().context("multipart upload has no key")?;
                ensure!(
                    key.starts_with(&self.config.prefix),
                    "unexpected cleanup key"
                );
                self.client
                    .abort_multipart_upload()
                    .bucket(TEST_BUCKET)
                    .key(key)
                    .upload_id(upload.upload_id().context("multipart upload has no ID")?)
                    .send()
                    .await?;
            }
        }
        loop {
            let objects = self
                .client
                .list_objects_v2()
                .bucket(TEST_BUCKET)
                .prefix(&self.config.prefix)
                .send()
                .await?;
            if objects.contents().is_empty() {
                break;
            }
            for object in objects.contents() {
                let key = object.key().context("listed object has no key")?;
                ensure!(
                    key.starts_with(&self.config.prefix),
                    "unexpected cleanup key"
                );
                self.client
                    .delete_object()
                    .bucket(TEST_BUCKET)
                    .key(key)
                    .send()
                    .await?;
            }
        }
        Ok(())
    }
}

fn conflict<T: std::fmt::Debug>(result: Result<T, RemoteError>, operation: &str) -> Result<()> {
    ensure!(
        matches!(result, Err(RemoteError::Conflict)),
        "{operation} did not reject a stale write: {result:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local RustFS; run scripts/test-rustfs.ps1"]
async fn rustfs_remote_crud_ranges_and_multipart() -> Result<()> {
    let fixture = Fixture::create().await?;
    let result = async {
        let remote = fixture.remote()?;
        remote
            .verify_conditionals()
            .await
            .context("RustFS atomic conditional operations")?;
        ensure!(remote.list().await?.is_empty(), "fresh prefix is not empty");
        let first = fixture
            .upload_bytes(
                &remote,
                "nested/hello.txt",
                b"0123456789abcdef",
                Expected::Absent,
            )
            .await?;
        let listing = remote.list().await?;
        ensure!(
            listing.len() == 1 && listing[0].etag == first.etag && listing[0].size == 16,
            "listing did not retain object metadata"
        );
        ensure!(
            remote.read_range(&first.key, &first.etag, 4, 5).await? == b"45678",
            "incorrect partial range"
        );
        ensure!(
            remote.read_range(&first.key, &first.etag, 14, 8).await? == b"ef",
            "range was not truncated at EOF"
        );
        let source = fixture.directory.path().join("replacement");
        std::fs::write(&source, b"replacement bytes")?;
        conflict(
            remote.upload(&first.key, &source, Expected::Absent).await,
            "conditional create",
        )?;
        conflict(
            remote
                .upload(
                    &first.key,
                    &source,
                    Expected::Etag("\"stale-version\"".into()),
                )
                .await,
            "conditional update",
        )?;
        let second = remote
            .upload(&first.key, &source, Expected::Etag(first.etag.clone()))
            .await?;
        conflict(
            remote.read_range(&first.key, &first.etag, 0, 1).await,
            "stale range read",
        )?;
        conflict(
            remote.delete(&first.key, &first.etag).await,
            "conditional delete",
        )?;
        ensure!(
            remote
                .read_range(&second.key, &second.etag, 0, second.size as usize)
                .await?
                == b"replacement bytes",
            "rejected requests changed object contents"
        );
        remote.delete(&second.key, &second.etag).await?;
        ensure!(
            matches!(
                remote.read_range(&second.key, &second.etag, 0, 1).await,
                Err(RemoteError::NotFound)
            ),
            "deleted object remains readable"
        );
        let empty = fixture
            .upload_bytes(&remote, "empty.bin", b"", Expected::Absent)
            .await?;
        ensure!(empty.size == 0, "zero-byte upload changed size");
        remote.delete(&empty.key, &empty.etag).await?;

        // Independent clients competing from one baseline must produce exactly one winner.
        let competitor = fixture.remote()?;
        competitor.verify_conditionals().await?;
        let competing_source = fixture.directory.path().join("competing-replacement");
        std::fs::write(&competing_source, b"a different device revision")?;
        let (left, right) = tokio::join!(
            remote.upload("race.bin", &source, Expected::Absent),
            competitor.upload("race.bin", &competing_source, Expected::Absent),
        );
        let winner = race_winner(left, right)?;
        std::fs::write(&source, b"new revision from first device")?;
        std::fs::write(&competing_source, b"new revision from second device")?;
        let (left, right) = tokio::join!(
            remote.upload("race.bin", &source, Expected::Etag(winner.etag.clone())),
            competitor.upload("race.bin", &competing_source, Expected::Etag(winner.etag)),
        );
        let winner = race_winner(left, right)?;
        remote.delete(&winner.key, &winner.etag).await?;

        // Cross the real 32 MiB multipart threshold, with data spanning part boundaries.
        let large_size = 32 * 1024 * 1024 + 777;
        let large_path = fixture.directory.path().join("multipart.bin");
        write_pattern(&large_path, large_size, 0)?;
        let original = remote
            .upload("large.bin", &large_path, Expected::Absent)
            .await
            .context("RustFS conditional multipart completion")?;
        ensure!(
            original.size == large_size as u64 && original.etag.contains('-'),
            "multipart upload was not used"
        );
        let offset = 16 * 1024 * 1024 - 13;
        ensure!(
            remote
                .read_range(&original.key, &original.etag, offset as u64, 64)
                .await?
                == pattern(offset, 64, 0),
            "multipart boundary data mismatch"
        );
        write_pattern(&large_path, large_size, 7)?;
        conflict(
            remote
                .upload(&original.key, &large_path, Expected::Absent)
                .await,
            "multipart conditional create",
        )?;
        conflict(
            remote
                .upload(
                    &original.key,
                    &large_path,
                    Expected::Etag("\"stale-version\"".into()),
                )
                .await,
            "multipart conditional update",
        )?;
        ensure!(
            remote
                .read_range(&original.key, &original.etag, offset as u64, 64)
                .await?
                == pattern(offset, 64, 0),
            "rejected multipart completion damaged original"
        );
        let replaced = remote
            .upload(&original.key, &large_path, Expected::Etag(original.etag))
            .await?;
        ensure!(
            remote
                .read_range(&replaced.key, &replaced.etag, (large_size - 64) as u64, 64)
                .await?
                == pattern(large_size - 64, 64, 7),
            "multipart replacement content mismatch"
        );
        remote.delete(&replaced.key, &replaced.etag).await?;
        ensure!(
            remote.list().await?.is_empty(),
            "deleted objects or probes leaked into listing"
        );
        fixture.assert_no_incomplete_uploads().await?;
        Ok(())
    }
    .await;
    let cleanup = fixture.cleanup().await;
    result.and(cleanup)
}

fn race_winner(
    left: Result<RemoteObject, RemoteError>,
    right: Result<RemoteObject, RemoteError>,
) -> Result<RemoteObject> {
    match (left, right) {
        (Ok(winner), Err(RemoteError::Conflict)) | (Err(RemoteError::Conflict), Ok(winner)) => {
            Ok(winner)
        }
        result => anyhow::bail!(
            "concurrent writes did not produce one winner and one conflict: {result:?}"
        ),
    }
}

fn pattern(offset: usize, length: usize, salt: usize) -> Vec<u8> {
    (offset..offset + length)
        .map(|index| ((index * 31 + salt) % 251) as u8)
        .collect()
}

fn write_pattern(path: &Path, size: usize, salt: usize) -> Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(64 * 1024);
        file.write_all(&pattern(offset, length, salt))?;
        offset += length;
    }
    file.flush()?;
    Ok(())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use bucket_mount::{engine::Engine, platform};
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    struct CountingRemote {
        remote: S3Remote,
        reads: AtomicUsize,
    }

    impl CountingRemote {
        fn new(config: &Config) -> Result<Self> {
            Ok(Self {
                remote: S3Remote::new(config)?,
                reads: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl Remote for CountingRemote {
        async fn list(&self) -> Result<Vec<RemoteObject>, RemoteError> {
            self.remote.list().await
        }
        async fn read_range(
            &self,
            key: &str,
            etag: &str,
            offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, RemoteError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.remote.read_range(key, etag, offset, length).await
        }
        async fn upload(
            &self,
            key: &str,
            file: &Path,
            expected: Expected,
        ) -> Result<RemoteObject, RemoteError> {
            self.remote.upload(key, file, expected).await
        }
        async fn delete(&self, key: &str, etag: &str) -> Result<(), RemoteError> {
            self.remote.delete(key, etag).await
        }
    }

    struct CloudRoots(Vec<PathBuf>);
    impl Drop for CloudRoots {
        fn drop(&mut self) {
            use std::os::windows::ffi::OsStrExt;
            for root in &self.0 {
                let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
                // The test owns these roots; engines have disconnected before this guard drops.
                unsafe {
                    windows_sys::Win32::Storage::CloudFilters::CfUnregisterSyncRoot(wide.as_ptr());
                }
            }
        }
    }

    #[test]
    #[ignore = "requires local RustFS, Windows Cloud Files and NTFS; run scripts/test-rustfs.ps1"]
    fn rustfs_two_device_sync_conflict_cache_and_delete() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter("warn,bucket_mount=debug")
            .try_init();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let fixture = runtime.block_on(Fixture::create())?;
        let result = two_devices(&fixture, &runtime);
        let cleanup = runtime.block_on(fixture.cleanup());
        result.and(cleanup)
    }

    fn two_devices(fixture: &Fixture, runtime: &tokio::runtime::Runtime) -> Result<()> {
        let a = Config {
            cache_max_files: 1,
            ..fixture.config.clone()
        };
        let b = Config {
            mount_dir: fixture.directory.path().join("mount-b"),
            state_dir: fixture.directory.path().join("state-b"),
            device_id: "rustfs-device-b".into(),
            ..a.clone()
        };
        let _roots = CloudRoots(vec![a.mount_dir.clone(), b.mount_dir.clone()]);
        let remote_a = Arc::new(CountingRemote::new(&a)?);
        let remote_b = Arc::new(CountingRemote::new(&b)?);
        runtime.block_on(fixture.upload_bytes(
            remote_a.as_ref(),
            "hello.txt",
            b"original remote bytes",
            Expected::Absent,
        ))?;
        let mut one = Engine::start(a.clone(), remote_a.clone(), runtime.handle().clone())?;
        let mut two = Engine::start(b.clone(), remote_b.clone(), runtime.handle().clone())?;
        ensure!(
            runtime.block_on(one.sync_once())?.errors == 0,
            "first listing failed"
        );
        ensure!(
            runtime.block_on(two.sync_once())?.errors == 0,
            "second listing failed"
        );
        ensure!(
            remote_a.reads.load(Ordering::SeqCst) == 0
                && remote_b.reads.load(Ordering::SeqCst) == 0,
            "initial metadata listing downloaded object bodies"
        );
        ensure!(
            platform::inspect(&a.mount_dir.join("hello.txt"))?.hydrated_bytes == 0,
            "new placeholder unexpectedly hydrated"
        );
        ensure!(
            std::fs::read(a.mount_dir.join("hello.txt"))? == b"original remote bytes",
            "on-demand hydration failed"
        );
        ensure!(
            remote_a.reads.load(Ordering::SeqCst) > 0,
            "opening a placeholder did not request S3 bytes"
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
            "simultaneous edit conflict: {second:?}"
        );
        ensure!(
            std::fs::read(b.mount_dir.join("hello.txt"))? == b"edit from machine A",
            "winning revision missing"
        );
        let conflict = runtime
            .block_on(remote_a.list())?
            .into_iter()
            .find(|object| object.key.contains(".conflict-"))
            .context("conflict copy missing from S3")?;
        ensure!(
            std::fs::read(b.mount_dir.join(&conflict.key))? == b"edit from machine B",
            "losing edit was not preserved"
        );
        let cached = runtime.block_on(two.sync_once())?;
        ensure!(
            cached.cached_files <= 1 && cached.evicted > 0,
            "cache did not evict clean files: {cached:?}"
        );

        std::fs::remove_file(a.mount_dir.join("hello.txt"))?;
        let current = runtime
            .block_on(remote_a.list())?
            .into_iter()
            .find(|object| object.key == "hello.txt")
            .context("original missing")?;
        runtime.block_on(fixture.upload_bytes(
            remote_a.as_ref(),
            "hello.txt",
            b"remote edit concurrent with local delete",
            Expected::Etag(current.etag),
        ))?;
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
            "remote edit was lost"
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
                .block_on(remote_a.list())?
                .iter()
                .any(|object| object.key == "hello.txt"),
            "local delete did not reach S3"
        );
        ensure!(
            runtime.block_on(two.sync_once())?.errors == 0
                && !b.mount_dir.join("hello.txt").exists(),
            "remote deletion did not reach second machine"
        );

        runtime.block_on(fixture.upload_bytes(
            remote_a.as_ref(),
            "before-rename.bin",
            b"dehydrated rename must keep its backing object",
            Expected::Absent,
        ))?;
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
            "rename upload: {renamed:?}"
        );
        ensure!(
            std::fs::read(a.mount_dir.join("after-rename.bin"))?
                == b"dehydrated rename must keep its backing object",
            "rename lost contents"
        );
        runtime.block_on(one.sync_once())?;
        let listing = runtime.block_on(remote_a.list())?;
        ensure!(
            listing
                .iter()
                .any(|object| object.key == "after-rename.bin")
                && !listing
                    .iter()
                    .any(|object| object.key == "before-rename.bin"),
            "rename did not propagate safely"
        );
        runtime.block_on(two.sync_once())?;
        ensure!(
            std::fs::read(b.mount_dir.join("after-rename.bin"))?
                == b"dehydrated rename must keep its backing object",
            "renamed contents missing on second machine"
        );
        runtime.block_on(fixture.assert_no_incomplete_uploads())?;
        Ok(())
    }
}
