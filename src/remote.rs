use std::{
    collections::HashSet,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, Credentials, Region, RequestChecksumCalculation, retry::RetryConfig,
        timeout::TimeoutConfig,
    },
    error::{ProvideErrorMetadata, SdkError},
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use aws_smithy_types::byte_stream::Length;
use tokio::{io::AsyncReadExt, sync::OnceCell};

use crate::config::{ConditionalEtagFormat, Config, canonical_prefix};

const PROBE_PREFIX: &str = ".bucket-mount/probes/";
const MAX_RANGE_BYTES: usize = 16 * 1024 * 1024;
const MULTIPART_THRESHOLD: u64 = 32 * 1024 * 1024;
const MIN_PART_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    pub key: String,
    pub etag: String,
    pub size: u64,
    pub modified_ms: i64,
}

#[derive(Clone, Debug)]
pub enum Expected {
    Absent,
    Etag(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("the remote object changed; preserve the local version as a conflict")]
    Conflict,
    #[error("the remote object no longer exists")]
    NotFound,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait::async_trait]
pub trait Remote: Send + Sync {
    async fn list(&self) -> Result<Vec<RemoteObject>, RemoteError>;
    async fn read_range(
        &self,
        key: &str,
        etag: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, RemoteError>;
    async fn upload(
        &self,
        key: &str,
        file: &Path,
        expected: Expected,
    ) -> Result<RemoteObject, RemoteError>;
    async fn delete(&self, key: &str, etag: &str) -> Result<(), RemoteError>;
}

pub struct S3Remote {
    client: Client,
    bucket: String,
    prefix: String,
    device_id: String,
    conditional_etag_format: ConditionalEtagFormat,
    conditionals_verified: OnceCell<()>,
    multipart_verified: OnceCell<()>,
}

impl S3Remote {
    pub fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let secrets = crate::credentials::Secrets::load(config)?;
        let credentials = Credentials::new(
            secrets.access_key.clone(),
            secrets.secret_key.clone(),
            secrets.session_token.clone(),
            None,
            "bucket-mount",
        );
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region.clone()))
            .endpoint_url(&config.endpoint)
            .force_path_style(true)
            .credentials_provider(credentials)
            // A streaming CRC trailer is not understood by several S3-compatible services.
            // HTTPS still protects transfer integrity; conditional writes protect concurrency.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .retry_config(RetryConfig::standard().with_max_attempts(3))
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .read_timeout(Duration::from_secs(60))
                    .operation_timeout(Duration::from_secs(15 * 60))
                    .build(),
            )
            .build();
        Ok(Self {
            client: Client::from_conf(sdk_config),
            bucket: config.bucket.clone(),
            prefix: canonical_prefix(&config.prefix)?,
            device_id: config.device_id.clone(),
            conditional_etag_format: config.conditional_etag_format,
            conditionals_verified: OnceCell::new(),
            multipart_verified: OnceCell::new(),
        })
    }

    /// Fail closed when a provider accepts but ignores conditional request headers.
    /// Only a uniquely named internal probe object is written, never user objects.
    pub async fn verify_conditionals(&self) -> Result<()> {
        self.conditionals_verified
            .get_or_try_init(|| self.run_conditional_probe())
            .await?;
        Ok(())
    }

    fn full_key(&self, key: &str) -> Result<String> {
        ensure!(
            !key.is_empty() && !key.starts_with('/') && !key.ends_with('/'),
            "object key must name a file"
        );
        ensure!(
            !key.contains('\\')
                && !key.chars().any(char::is_control)
                && key
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "object key contains invalid path components"
        );
        ensure!(
            !key.starts_with(PROBE_PREFIX),
            "object key is reserved for conditional-support probes"
        );
        ensure!(
            self.prefix.len() + key.len() <= 1024,
            "S3 object key exceeds 1024 bytes"
        );
        Ok(format!("{}{key}", self.prefix))
    }

    fn conditional_write_etag<'a>(&self, etag: &'a str) -> Result<&'a str> {
        validate_etag(etag)?;
        // OVH documents bare ETags specifically for PutObject, DeleteObject, and
        // CompleteMultipartUpload. Keep quoted tags for reads and local identity.
        Ok(match self.conditional_etag_format {
            ConditionalEtagFormat::Quoted => etag,
            ConditionalEtagFormat::Bare => &etag[1..etag.len() - 1],
        })
    }

    async fn put_bytes(
        &self,
        key: &str,
        bytes: &'static [u8],
        expected: Expected,
    ) -> Result<String, RemoteError> {
        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from_static(bytes));
        request = match expected {
            Expected::Absent => request.if_none_match("*"),
            Expected::Etag(etag) => request.if_match(self.conditional_write_etag(&etag)?),
        };
        let output = request.send().await.map_err(map_sdk_error)?;
        Ok(required_etag(output.e_tag())?)
    }

    async fn run_conditional_probe(&self) -> Result<()> {
        let key = format!("{}{PROBE_PREFIX}{}", self.prefix, uuid::Uuid::new_v4());
        let result = async {
            let original = self
                .put_bytes(&key, b"bucket-mount-probe-original", Expected::Absent)
                .await
                .context("PutObject If-None-Match: could not create the connection probe")?;
            require_conflict(
                self.put_bytes(&key, b"bucket-mount-probe-invalid-create", Expected::Absent)
                    .await,
                "PutObject If-None-Match",
            )?;
            let stale = format!("\"bucket-mount-stale-{}\"", uuid::Uuid::new_v4());
            require_conflict(
                self.put_bytes(
                    &key,
                    b"bucket-mount-probe-invalid-replace",
                    Expected::Etag(stale.clone()),
                )
                .await,
                "PutObject If-Match",
            )?;
            let deletion = self
                .client
                .delete_object()
                .bucket(&self.bucket)
                .key(&key)
                .if_match(self.conditional_write_etag(&stale)?)
                .send()
                .await
                .map_err(map_sdk_error);
            require_conflict(deletion, "DeleteObject If-Match")?;
            let head = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(map_sdk_error)
                .context("HeadObject: could not verify the unchanged connection probe")?;
            ensure!(
                required_etag(head.e_tag())? == original,
                "provider changed the probe despite rejecting conditional operations"
            );
            let replaced = self
                .put_bytes(
                    &key,
                    b"bucket-mount-probe-valid-replace",
                    Expected::Etag(original),
                )
                .await
                .context("PutObject If-Match: could not replace the connection probe with its current ETag; for OVHcloud, enable OVHcloud conditional requests")?;
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(&key)
                .if_match(self.conditional_write_etag(&replaced)?)
                .send()
                .await
                .map_err(map_sdk_error)
                .context("DeleteObject If-Match: could not delete the connection probe with its current ETag")?;
            let missing = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(map_sdk_error);
            ensure!(
                matches!(missing, Err(RemoteError::NotFound)),
                "provider did not remove the conditional-delete probe"
            );
            Ok(())
        }
        .await;
        if result.is_err() {
            self.cleanup_probe(&key).await;
        }
        result
    }

    async fn cleanup_probe(&self, key: &str) {
        // This UUID key was allocated solely by this test. Never use this cleanup for user keys.
        if let Ok(head) = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            && let Ok(etag) = required_etag(head.e_tag())
            && let Ok(condition) = self.conditional_write_etag(&etag)
        {
            let _ = self
                .client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .if_match(condition)
                .send()
                .await;
        }
    }

    async fn verify_multipart_conditionals(&self) -> Result<()> {
        self.multipart_verified.get_or_try_init(|| async {
            let key = format!("{}{PROBE_PREFIX}{}", self.prefix, uuid::Uuid::new_v4());
            let result = async {
                let original = self.put_bytes(&key, b"bucket-mount-multipart-original", Expected::Absent).await?;
                require_conflict(self.multipart_probe_attempt(&key, Expected::Absent).await, "CompleteMultipartUpload If-None-Match")?;
                let stale = format!("\"bucket-mount-stale-{}\"", uuid::Uuid::new_v4());
                require_conflict(self.multipart_probe_attempt(&key, Expected::Etag(stale)).await, "CompleteMultipartUpload If-Match")?;
                let head = self.client.head_object().bucket(&self.bucket).key(&key).send().await.map_err(map_sdk_error)?;
                ensure!(required_etag(head.e_tag())? == original, "provider modified a rejected multipart probe");
                self.multipart_probe_attempt(&key, Expected::Etag(original)).await?;
                Ok(())
            }.await;
            self.cleanup_probe(&key).await;
            result.context("S3 provider must support conditional multipart completion for large-file synchronization")
        }).await?;
        Ok(())
    }

    async fn multipart_probe_attempt(
        &self,
        key: &str,
        expected: Expected,
    ) -> Result<String, RemoteError> {
        let id = self.begin_multipart(key).await?;
        let result = async {
            let part = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&id)
                .part_number(1)
                .body(ByteStream::from_static(
                    b"bucket-mount-multipart-replacement",
                ))
                .send()
                .await
                .map_err(map_sdk_error)?;
            let completed = CompletedPart::builder()
                .part_number(1)
                .e_tag(required_etag(part.e_tag())?)
                .build();
            self.complete_multipart(key, &id, vec![completed], expected)
                .await
        }
        .await;
        if result.is_err() {
            self.abort_multipart(key, &id).await;
        }
        result
    }

    async fn begin_multipart(&self, key: &str) -> Result<String, RemoteError> {
        let output = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .metadata("bucket-mount-device", &self.device_id)
            .send()
            .await
            .map_err(map_sdk_error)?;
        Ok(output
            .upload_id()
            .context("provider omitted multipart upload ID")?
            .to_owned())
    }

    async fn abort_multipart(&self, key: &str, id: &str) {
        if let Err(error) = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(id)
            .send()
            .await
        {
            tracing::warn!(error = %error, "could not abort interrupted multipart upload; configure an incomplete-upload bucket lifecycle rule");
        }
    }

    async fn complete_multipart(
        &self,
        key: &str,
        id: &str,
        parts: Vec<CompletedPart>,
        expected: Expected,
    ) -> Result<String, RemoteError> {
        let mut request = self
            .client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            );
        request = match expected {
            Expected::Absent => request.if_none_match("*"),
            Expected::Etag(etag) => request.if_match(self.conditional_write_etag(&etag)?),
        };
        let output = request.send().await.map_err(map_sdk_error)?;
        Ok(required_etag(output.e_tag())?)
    }

    async fn upload_multipart(
        &self,
        key: &str,
        file: &Path,
        size: u64,
        expected: Expected,
    ) -> Result<String, RemoteError> {
        self.verify_multipart_conditionals().await?;
        let id = self.begin_multipart(key).await?;
        let result = async {
            // At most 10,000 parts; stream each part from disk without buffering it in RAM.
            // Multiple files are uploaded concurrently by the engine.
            let part_size = MIN_PART_BYTES.max(size.div_ceil(10_000));
            let mut parts = Vec::with_capacity(size.div_ceil(part_size) as usize);
            let mut offset = 0;
            while offset < size {
                let length = part_size.min(size - offset);
                let number = (parts.len() + 1) as i32;
                let body = ByteStream::read_from()
                    .path(file)
                    .offset(offset)
                    .length(Length::Exact(length))
                    .buffer_size(128 * 1024)
                    .build()
                    .await
                    .context("open staged multipart file range")?;
                let part = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&id)
                    .part_number(number)
                    .content_length(length as i64)
                    .body(body)
                    .send()
                    .await
                    .map_err(map_sdk_error)?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(number)
                        .e_tag(required_etag(part.e_tag())?)
                        .build(),
                );
                offset += length;
            }
            self.complete_multipart(key, &id, parts, expected).await
        }
        .await;
        if result.is_err() {
            self.abort_multipart(key, &id).await;
        }
        result
    }
}

#[async_trait::async_trait]
impl Remote for S3Remote {
    async fn list(&self) -> Result<Vec<RemoteObject>, RemoteError> {
        let mut objects = Vec::new();
        let mut continuation = None;
        let mut seen_tokens = HashSet::new();
        loop {
            let output = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&self.prefix)
                .set_continuation_token(continuation.clone())
                .max_keys(1000)
                .send()
                .await
                .map_err(map_sdk_error)?;
            for object in output.contents() {
                let full_key = object
                    .key()
                    .context("provider omitted a listed object key")?;
                let key = full_key
                    .strip_prefix(&self.prefix)
                    .context("provider listed an object outside the requested prefix")?;
                if key.is_empty() || key.ends_with('/') || key.starts_with(PROBE_PREFIX) {
                    continue;
                }
                let size = object.size().context("provider omitted object size")?;
                ensure_remote(size >= 0, "provider returned negative object size")?;
                objects.push(RemoteObject {
                    key: key.to_owned(),
                    etag: required_etag(object.e_tag())?,
                    size: size as u64,
                    modified_ms: object
                        .last_modified()
                        .context("provider omitted last-modified time")?
                        .to_millis()
                        .context("object timestamp is out of range")?,
                });
            }
            if output.is_truncated() != Some(true) {
                return Ok(objects);
            }
            let next = output
                .next_continuation_token()
                .filter(|token| !token.is_empty())
                .context("provider truncated the listing without a continuation token")?
                .to_owned();
            ensure_remote(
                seen_tokens.insert(next.clone()),
                "provider repeated its continuation token",
            )?;
            continuation = Some(next);
        }
    }

    async fn read_range(
        &self,
        key: &str,
        etag: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, RemoteError> {
        if length == 0 {
            return Ok(Vec::new());
        }
        ensure_remote(
            length <= MAX_RANGE_BYTES,
            "one read request must not exceed 16 MiB",
        )?;
        let full_key = self.full_key(key)?;
        validate_etag(etag)?;
        let end = offset
            .checked_add(length as u64 - 1)
            .context("read range overflow")?;
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(full_key)
            .if_match(etag)
            .range(format!("bytes={offset}-{end}"))
            .send()
            .await
            .map_err(map_sdk_error)?;
        if required_etag(output.e_tag())? != etag {
            return Err(RemoteError::Conflict);
        }
        let expected_length = validate_content_range(output.content_range(), offset, end)?;
        if let Some(actual) = output.content_length() {
            ensure_remote(
                actual >= 0 && actual as u64 == expected_length,
                "provider returned an unexpected range content length",
            )?;
        }
        // Bound the body even when a broken server ignores its own Content-Length.
        let mut body = output.body.into_async_read().take(expected_length + 1);
        let mut bytes = Vec::with_capacity(expected_length as usize);
        tokio::time::timeout(Duration::from_secs(60), body.read_to_end(&mut bytes))
            .await
            .context("S3 range body timed out")?
            .context("read S3 range body")?;
        ensure_remote(
            bytes.len() as u64 == expected_length,
            "provider returned a truncated or oversized range body",
        )?;
        Ok(bytes)
    }

    async fn upload(
        &self,
        key: &str,
        file: &Path,
        expected: Expected,
    ) -> Result<RemoteObject, RemoteError> {
        let full_key = self.full_key(key)?;
        if let Expected::Etag(etag) = &expected {
            validate_etag(etag)?;
        }
        let metadata = tokio::fs::metadata(file)
            .await
            .context("inspect staged upload")?;
        ensure_remote(metadata.is_file(), "staged upload must be a regular file")?;
        let size = metadata.len();
        ensure_remote(
            size <= MAX_OBJECT_BYTES,
            "object exceeds the supported 5 TiB limit",
        )?;
        self.verify_conditionals().await?;
        let etag = if size >= MULTIPART_THRESHOLD {
            self.upload_multipart(&full_key, file, size, expected)
                .await?
        } else {
            let body = ByteStream::read_from()
                .path(file)
                .buffer_size(128 * 1024)
                .build()
                .await
                .context("open staged upload")?;
            let mut request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(full_key)
                .content_length(size as i64)
                .metadata("bucket-mount-device", &self.device_id)
                .body(body);
            request = match expected {
                Expected::Absent => request.if_none_match("*"),
                Expected::Etag(etag) => request.if_match(self.conditional_write_etag(&etag)?),
            };
            let output = request.send().await.map_err(map_sdk_error)?;
            required_etag(output.e_tag())?
        };
        // Use the write's ETag. A subsequent HEAD could observe another device's overwrite.
        Ok(RemoteObject {
            key: key.to_owned(),
            etag,
            size,
            modified_ms: now_ms(),
        })
    }

    async fn delete(&self, key: &str, etag: &str) -> Result<(), RemoteError> {
        let key = self.full_key(key)?;
        validate_etag(etag)?;
        self.verify_conditionals().await?;
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .if_match(self.conditional_write_etag(etag)?)
            .send()
            .await
            .map_err(map_sdk_error)?;
        Ok(())
    }
}

fn required_etag(etag: Option<&str>) -> Result<String> {
    let etag = etag.context("provider omitted ETag required for safe synchronization")?;
    // S3-compatible services may omit the quotes in response headers or XML.
    // One canonical representation prevents spurious conflicts across operations.
    let canonical = if etag.starts_with('"') {
        etag.to_owned()
    } else {
        format!("\"{etag}\"")
    };
    validate_etag(&canonical)?;
    Ok(canonical)
}

fn validate_etag(etag: &str) -> Result<()> {
    ensure!(
        etag.len() > 2
            && etag.starts_with('"')
            && etag.ends_with('"')
            && !etag[1..etag.len() - 1].starts_with("W/")
            && etag[1..etag.len() - 1].bytes().all(|byte| {
                byte.is_ascii_graphic() && !matches!(byte, b'"' | b',' | b'*' | b'\\')
            }),
        "provider must supply a single quoted strong object ETag"
    );
    Ok(())
}

fn validate_content_range(header: Option<&str>, offset: u64, requested_end: u64) -> Result<u64> {
    let header = header.context("provider ignored the range request (missing Content-Range)")?;
    let (bounds, total) = header
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .context("invalid Content-Range header")?;
    let (start, end) = bounds
        .split_once('-')
        .context("invalid Content-Range bounds")?;
    let start: u64 = start.parse().context("invalid range start")?;
    let end: u64 = end.parse().context("invalid range end")?;
    let total: u64 = total.parse().context("invalid range total")?;
    ensure!(
        total > 0 && start == offset && end >= start && end == requested_end.min(total - 1),
        "provider returned a different range than requested"
    );
    Ok(end - start + 1)
}

fn require_conflict<T>(result: Result<T, RemoteError>, operation: &str) -> Result<()> {
    match result {
        Err(RemoteError::Conflict) => Ok(()),
        Err(error) => Err(anyhow!(error))
            .with_context(|| format!("conditional capability probe for {operation} failed")),
        Ok(_) => bail!("provider ignored {operation}; writes are disabled to prevent data loss"),
    }
}

fn ensure_remote(condition: bool, message: &'static str) -> Result<(), RemoteError> {
    if condition {
        Ok(())
    } else {
        Err(anyhow!(message).into())
    }
}

fn map_sdk_error<E>(error: SdkError<E>) -> RemoteError
where
    E: std::error::Error + ProvideErrorMetadata + Send + Sync + 'static,
{
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    let code = error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code);
    match (status, code) {
        (Some(409 | 412), _) | (_, Some("PreconditionFailed" | "ConditionalRequestConflict")) => {
            RemoteError::Conflict
        }
        (_, Some("NoSuchKey" | "NotFound")) | (Some(404), None) => RemoteError::NotFound,
        _ => {
            let detail = match (status, code) {
                (Some(status), Some(code)) => format!("S3 request failed: HTTP {status}, {code}"),
                (Some(status), None) => format!("S3 request failed: HTTP {status}"),
                (None, _) => "S3 request failed before a service response".into(),
            };
            RemoteError::Other(anyhow!(error).context(detail))
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    #[derive(Clone, Debug)]
    struct TestRequest {
        method: String,
        target: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl TestResponse {
        fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }
        fn header(mut self, name: &str, value: impl Into<String>) -> Self {
            self.headers.push((name.into(), value.into()));
            self
        }
        fn conflict() -> Self {
            Self::new(
                412,
                b"<Error><Code>PreconditionFailed</Code><Message>ETag mismatch</Message></Error>"
                    .to_vec(),
            )
            .header("Content-Type", "application/xml")
        }
    }

    struct TestServer {
        endpoint: String,
        requests: Arc<Mutex<Vec<TestRequest>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start(mut handler: impl FnMut(&TestRequest) -> TestResponse + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = stop.clone();
            let thread_requests = requests.clone();
            let worker = thread::spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    };
                    // Windows accepted sockets inherit the listener's nonblocking mode.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let request = read_request(&mut stream);
                    let response = handler(&request);
                    let is_head = request.method == "HEAD";
                    thread_requests.lock().unwrap().push(request);
                    let reason = match response.status {
                        200 => "OK",
                        204 => "No Content",
                        206 => "Partial Content",
                        404 => "Not Found",
                        412 => "Precondition Failed",
                        _ => "Test Response",
                    };
                    let mut headers = format!(
                        "HTTP/1.1 {} {reason}\r\nConnection: close\r\n",
                        response.status
                    );
                    if !response
                        .headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    {
                        headers.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
                    }
                    for (name, value) in response.headers {
                        headers.push_str(&format!("{name}: {value}\r\n"));
                    }
                    headers.push_str("\r\n");
                    stream.write_all(headers.as_bytes()).unwrap();
                    if !is_head {
                        stream.write_all(&response.body).unwrap();
                    }
                }
            });
            Self {
                endpoint,
                requests,
                stop,
                worker: Some(worker),
            }
        }

        fn remote(&self) -> S3Remote {
            self.remote_with_format(ConditionalEtagFormat::Quoted)
        }

        fn remote_with_format(&self, format: ConditionalEtagFormat) -> S3Remote {
            let config = aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .endpoint_url(&self.endpoint)
                .region(Region::new("us-east-1"))
                .credentials_provider(Credentials::new(
                    "test-key",
                    "test-secret",
                    None,
                    None,
                    "fixture",
                ))
                .force_path_style(true)
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .retry_config(RetryConfig::standard().with_max_attempts(1))
                .timeout_config(
                    TimeoutConfig::builder()
                        .operation_timeout(Duration::from_secs(10))
                        .build(),
                )
                .build();
            S3Remote {
                client: Client::from_conf(config),
                bucket: "bucket".into(),
                prefix: "prefix/".into(),
                device_id: "test-device".into(),
                conditional_etag_format: format,
                conditionals_verified: OnceCell::new(),
                multipart_verified: OnceCell::new(),
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !thread::panicking() {
                    result.unwrap();
                }
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> TestRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        let end = loop {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "fixture request ended before headers");
            bytes.extend_from_slice(&buffer[..count]);
            assert!(
                bytes.len() < 2 * 1024 * 1024,
                "fixture request unexpectedly large"
            );
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let head = std::str::from_utf8(&bytes[..end]).unwrap();
        let mut lines = head.split("\r\n");
        let mut first = lines.next().unwrap().split_whitespace();
        let method = first.next().unwrap().to_owned();
        let target = first.next().unwrap().to_owned();
        let headers: HashMap<_, _> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        assert!(
            !headers.contains_key("transfer-encoding"),
            "fixture expects bounded, non-chunked request bodies"
        );
        let length: usize = headers
            .get("content-length")
            .map_or(0, |value| value.parse().unwrap());
        while bytes.len() - end < length {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "fixture body truncated");
            bytes.extend_from_slice(&buffer[..count]);
        }
        TestRequest {
            method,
            target,
            headers,
            body: bytes[end..end + length].to_vec(),
        }
    }

    fn conditional_server_with_format(
        ignore_conditions: bool,
        format: ConditionalEtagFormat,
    ) -> TestServer {
        let mut objects: HashMap<String, (String, Vec<u8>)> = HashMap::new();
        let mut version = 0;
        TestServer::start(move |request| {
            let key = request.target.split('?').next().unwrap().to_owned();
            let current = objects.get(&key);
            if !ignore_conditions {
                if request
                    .headers
                    .get("if-none-match")
                    .is_some_and(|value| value == "*")
                    && current.is_some()
                {
                    return TestResponse::conflict();
                }
                if request.headers.get("if-match").is_some_and(|value| {
                    current.is_none_or(|(etag, _)| {
                        let expected = match format {
                            ConditionalEtagFormat::Quoted => etag.as_str(),
                            ConditionalEtagFormat::Bare => &etag[1..etag.len() - 1],
                        };
                        expected != value
                    })
                }) {
                    return TestResponse::conflict();
                }
            }
            match request.method.as_str() {
                "PUT" => {
                    version += 1;
                    let etag = format!("\"v{version}\"");
                    objects.insert(key, (etag.clone(), request.body.clone()));
                    let response_etag = match format {
                        ConditionalEtagFormat::Quoted => etag,
                        // Deliberately mix bare PUT and quoted HEAD responses.
                        ConditionalEtagFormat::Bare => etag[1..etag.len() - 1].to_owned(),
                    };
                    TestResponse::new(200, Vec::new()).header("ETag", response_etag)
                }
                "HEAD" => match current {
                    Some((etag, bytes)) => TestResponse::new(200, Vec::new())
                        .header("ETag", etag)
                        .header("Content-Length", bytes.len().to_string()),
                    None => TestResponse::new(404, Vec::new()),
                },
                "DELETE" => {
                    objects.remove(&key);
                    TestResponse::new(204, Vec::new())
                }
                other => panic!("unexpected mock method {other}"),
            }
        })
    }

    #[tokio::test]
    async fn actual_sdk_probes_and_streamed_writes_use_atomic_preconditions() {
        for format in [ConditionalEtagFormat::Quoted, ConditionalEtagFormat::Bare] {
            let server = conditional_server_with_format(false, format);
            let remote = server.remote_with_format(format);
            remote.verify_conditionals().await.unwrap();
            let probe_requests = server.requests.lock().unwrap().len();
            remote.verify_conditionals().await.unwrap();
            assert_eq!(
                probe_requests,
                server.requests.lock().unwrap().len(),
                "capability probe is cached"
            );
            let temp = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(temp.path(), b"important local edit").unwrap();
            let first = remote
                .upload("file.txt", temp.path(), Expected::Absent)
                .await
                .unwrap();
            assert_eq!(first.size, 20);
            assert!(matches!(
                remote
                    .upload("file.txt", temp.path(), Expected::Absent)
                    .await,
                Err(RemoteError::Conflict)
            ));
            let second = remote
                .upload("file.txt", temp.path(), Expected::Etag(first.etag.clone()))
                .await
                .unwrap();
            assert!(matches!(
                remote.delete("file.txt", &first.etag).await,
                Err(RemoteError::Conflict)
            ));
            remote.delete("file.txt", &second.etag).await.unwrap();
            let requests = server.requests.lock().unwrap();
            let writes: Vec<_> = requests
                .iter()
                .filter(|r| r.method == "PUT" && r.target.starts_with("/bucket/prefix/file.txt"))
                .collect();
            assert_eq!(writes.len(), 3);
            assert_eq!(writes[0].headers.get("if-none-match").unwrap(), "*");
            assert_eq!(
                writes[2].headers.get("if-match").unwrap(),
                remote.conditional_write_etag(&first.etag).unwrap()
            );
            assert_eq!(writes[0].body, b"important local edit");
            assert!(requests.iter().filter(|r| r.method == "DELETE").all(|r| {
                let condition = r.headers.get("if-match").unwrap();
                condition != "*"
                    && condition.starts_with('"') == (format == ConditionalEtagFormat::Quoted)
            }));
        }
    }

    #[tokio::test]
    async fn provider_ignoring_conditional_headers_is_rejected_before_user_writes() {
        for format in [ConditionalEtagFormat::Quoted, ConditionalEtagFormat::Bare] {
            let server = conditional_server_with_format(true, format);
            let remote = server.remote_with_format(format);
            let error = remote.verify_conditionals().await.unwrap_err();
            assert!(format!("{error:#}").contains("ignored PutObject If-None-Match"));
            assert!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.target.contains(".bucket-mount/probes/"))
            );
        }
    }

    #[tokio::test]
    async fn probe_authentication_failures_preserve_the_service_error() {
        // An invalid key can fail immediately or during a later probe request.
        for fail_on in [1, 2] {
            let mut puts = 0;
            let server = TestServer::start(move |request| {
                if request.method == "PUT" {
                    puts += 1;
                    if puts < fail_on {
                        return TestResponse::new(200, Vec::new()).header("ETag", "\"original\"");
                    }
                    return TestResponse::new(403, b"<Error><Code>InvalidAccessKeyId</Code><Message>The access key does not exist</Message></Error>".to_vec())
                        .header("Content-Type", "application/xml");
                }
                TestResponse::new(404, Vec::new())
            });
            let remote = server.remote();
            let error = remote.verify_conditionals().await.unwrap_err();
            let detail = format!("{error:#}");
            assert!(detail.contains("PutObject If-None-Match"), "{detail}");
            assert!(detail.contains("HTTP 403, InvalidAccessKeyId"), "{detail}");
            assert!(detail.contains("The access key does not exist"), "{detail}");
            assert!(!detail.contains("must support"), "{detail}");
            assert!(remote.conditionals_verified.get().is_none());
            assert!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| { request.target.contains(".bucket-mount/probes/") })
            );
        }
    }

    #[tokio::test]
    async fn wrong_write_etag_format_fails_closed_before_user_writes() {
        let server = conditional_server_with_format(false, ConditionalEtagFormat::Bare);
        let error = server.remote().verify_conditionals().await.unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains("PutObject If-Match"), "{detail}");
        assert!(
            detail.contains("enable OVHcloud conditional requests"),
            "{detail}"
        );
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.target.contains(".bucket-mount/probes/"))
        );
    }

    #[tokio::test]
    async fn actual_sdk_sends_ranges_and_rejects_ignored_ranges() {
        for format in [ConditionalEtagFormat::Quoted, ConditionalEtagFormat::Bare] {
            let server = TestServer::start(|request| {
                assert_eq!(request.method, "GET");
                assert!(request.target.starts_with("/bucket/prefix/file.txt"));
                assert_eq!(request.headers.get("range").unwrap(), "bytes=4-7");
                assert_eq!(request.headers.get("if-match").unwrap(), "\"version\"");
                TestResponse::new(206, b"data".to_vec())
                    .header("ETag", "version")
                    .header("Content-Range", "bytes 4-7/10")
            });
            assert_eq!(
                server
                    .remote_with_format(format)
                    .read_range("file.txt", "\"version\"", 4, 4)
                    .await
                    .unwrap(),
                b"data"
            );
        }
        let ignored = TestServer::start(|_| {
            TestResponse::new(200, b"whole object".to_vec()).header("ETag", "\"version\"")
        });
        assert!(
            ignored
                .remote()
                .read_range("file.txt", "\"version\"", 4, 4)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn paginated_listing_keeps_prefix_boundary_and_metadata_only() {
        let mut page = 0;
        let server = TestServer::start(move |request| {
            assert_eq!(request.method, "GET");
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            let query: HashMap<_, _> = url.query_pairs().collect();
            assert_eq!(query.get("prefix").unwrap(), "prefix/");
            assert_eq!(query.get("list-type").unwrap(), "2");
            page += 1;
            let body = if page == 1 {
                "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>next token</NextContinuationToken><Contents><Key>prefix/first.txt</Key><ETag>&quot;a&quot;</ETag><Size>123</Size><LastModified>2026-10-01T00:00:00Z</LastModified></Contents></ListBucketResult>"
            } else {
                assert_eq!(query.get("continuation-token").unwrap(), "next token");
                "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>prefix/second.txt</Key><ETag>&quot;b&quot;</ETag><Size>456</Size><LastModified>2026-10-01T00:00:00Z</LastModified></Contents></ListBucketResult>"
            };
            TestResponse::new(200, body.as_bytes().to_vec())
                .header("Content-Type", "application/xml")
        });
        let listing = server.remote().list().await.unwrap();
        assert_eq!(
            listing
                .iter()
                .map(|o| (o.key.as_str(), o.size))
                .collect::<Vec<_>>(),
            vec![("first.txt", 123), ("second.txt", 456)]
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn listing_rejects_continuation_token_cycles() {
        let mut page = 0;
        let server = TestServer::start(move |_| {
            page += 1;
            // The old check caught A -> A, but would loop forever on A -> B -> A.
            let token = if page % 2 == 1 { "a" } else { "b" };
            TestResponse::new(200, format!(
                "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>{token}</NextContinuationToken></ListBucketResult>"
            ).into_bytes()).header("Content-Type", "application/xml")
        });
        let error = tokio::time::timeout(Duration::from_secs(5), server.remote().list())
            .await
            .expect("listing must stop when a token repeats")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("repeated its continuation token")
        );
        assert_eq!(server.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn multipart_completion_uses_preconditions_and_maps_conflicts() {
        for format in [ConditionalEtagFormat::Quoted, ConditionalEtagFormat::Bare] {
            let mut attempt = 0;
            let server = TestServer::start(move |request| {
                assert_eq!(request.method, "POST");
                let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
                assert!(
                    url.query_pairs()
                        .any(|(key, value)| key == "uploadId" && value == "upload-123")
                );
                assert!(
                    String::from_utf8_lossy(&request.body).contains("<PartNumber>1</PartNumber>")
                );
                attempt += 1;
                if attempt == 1 {
                    assert_eq!(request.headers.get("if-none-match").unwrap(), "*");
                } else {
                    let expected = match format {
                        ConditionalEtagFormat::Quoted => "\"old-version\"",
                        ConditionalEtagFormat::Bare => "old-version",
                    };
                    assert_eq!(request.headers.get("if-match").unwrap(), expected);
                }
                TestResponse::conflict()
            });
            let remote = server.remote_with_format(format);
            for expected in [Expected::Absent, Expected::Etag("\"old-version\"".into())] {
                let parts = vec![
                    CompletedPart::builder()
                        .part_number(1)
                        .e_tag("\"part\"")
                        .build(),
                ];
                assert!(matches!(
                    remote
                        .complete_multipart("prefix/file", "upload-123", parts, expected)
                        .await,
                    Err(RemoteError::Conflict)
                ));
            }
        }
    }

    #[test]
    fn range_response_must_match_request_exactly() {
        assert_eq!(
            validate_content_range(Some("bytes 4-7/8"), 4, 20).unwrap(),
            4
        );
        assert_eq!(
            validate_content_range(Some("bytes 4-7/100"), 4, 7).unwrap(),
            4
        );
        for bad in [
            None,
            Some("bytes 0-7/8"),
            Some("bytes 4-6/8"),
            Some("bytes 4-8/8"),
            Some("bytes 4-7/*"),
        ] {
            assert!(validate_content_range(bad, 4, 7).is_err());
        }
    }

    #[test]
    fn wildcard_etag_cannot_disable_concurrency_protection() {
        assert!(validate_etag("*").is_err());
        assert!(validate_etag("").is_err());
        assert!(validate_etag("etag\r\nInjected: header").is_err());
        assert!(validate_etag("\"old\", *").is_err());
        assert!(validate_etag("\"1234-abcd\"").is_ok());
        for value in [
            "*",
            "\"*\"",
            "",
            "\"\"",
            "W/\"etag\"",
            "\"old\",\"new\"",
            "old,new",
            "old *",
            "old\r\nInjected: value",
        ] {
            assert!(required_etag(Some(value)).is_err(), "{value:?}");
        }
        assert_eq!(
            required_etag(Some("1234-abcd-2")).unwrap(),
            "\"1234-abcd-2\""
        );
        assert_eq!(
            required_etag(Some("\"1234-abcd-2\"")).unwrap(),
            "\"1234-abcd-2\""
        );
    }

    #[test]
    fn success_does_not_pass_a_negative_conditional_probe() {
        assert!(require_conflict::<()>(Ok(()), "test").is_err());
        assert!(require_conflict::<()>(Err(RemoteError::NotFound), "test").is_err());
        assert!(require_conflict::<()>(Err(RemoteError::Conflict), "test").is_ok());
    }
}
