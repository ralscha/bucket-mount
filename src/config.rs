use std::{
    env, fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// Format accepted by the provider for conditional write/delete If-Match headers.
/// Reads and persisted ETags always use the standard quoted HTTP format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConditionalEtagFormat {
    #[default]
    Quoted,
    Bare,
}

/// Secrets are either read from the environment or stored as a DPAPI ciphertext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub mount_dir: PathBuf,
    pub state_dir: PathBuf,
    pub drive_letter: Option<char>,
    pub cache_max_bytes: u64,
    pub cache_max_files: usize,
    pub poll_interval_secs: u64,
    pub settle_delay_secs: u64,
    pub upload_concurrency: usize,
    pub allow_http: bool,
    pub conditional_etag_format: ConditionalEtagFormat,
    pub device_id: String,
    pub access_key_env: String,
    pub secret_key_env: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protected_credentials: Option<String>,
    // An omitted field means explicitly disabled, including after saving None.
    // Do not restore Config::default's first-run AWS session-token suggestion.
    #[serde(default)]
    pub session_token_env: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let profile = env::var_os("USERPROFILE")
            .or_else(|| env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        let local = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| profile.join(".local/share"));
        Self {
            endpoint: "https://s3.amazonaws.com".into(),
            bucket: String::new(),
            region: "us-east-1".into(),
            prefix: String::new(),
            mount_dir: profile.join("BucketMount"),
            state_dir: local.join("BucketMount"),
            drive_letter: None,
            cache_max_bytes: 10 * 1024 * 1024 * 1024,
            cache_max_files: 10_000,
            poll_interval_secs: 15,
            settle_delay_secs: 3,
            upload_concurrency: 4,
            allow_http: false,
            conditional_etag_format: ConditionalEtagFormat::Quoted,
            device_id: uuid::Uuid::new_v4().simple().to_string(),
            access_key_env: "AWS_ACCESS_KEY_ID".into(),
            secret_key_env: "AWS_SECRET_ACCESS_KEY".into(),
            protected_credentials: None,
            session_token_env: Some("AWS_SESSION_TOKEN".into()),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        let endpoint =
            url::Url::parse(&self.endpoint).context("endpoint must be an absolute URL")?;
        ensure!(endpoint.has_host(), "endpoint must have a host");
        ensure!(
            endpoint.scheme() == "https" || (self.allow_http && endpoint.scheme() == "http"),
            "endpoint must use HTTPS (HTTP requires allow_http = true)"
        );
        ensure!(
            endpoint.username().is_empty() && endpoint.password().is_none(),
            "endpoint must not contain credentials; use the credential fields"
        );
        ensure!(
            endpoint.query().is_none() && endpoint.fragment().is_none(),
            "endpoint must not contain a query or fragment"
        );
        ensure!(
            !self.bucket.trim().is_empty() && self.bucket.len() <= 255,
            "bucket must contain 1 to 255 characters"
        );
        ensure!(
            !self
                .bucket
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\')),
            "bucket must not contain whitespace, slashes, or control characters"
        );
        ensure!(
            !self.region.trim().is_empty() && self.region.len() <= 128,
            "region must contain 1 to 128 characters"
        );
        ensure!(
            self.region
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "region must contain only letters, numbers, and hyphens"
        );
        canonical_prefix(&self.prefix)?;
        let mount = comparable_path(&self.mount_dir).context("invalid mount_dir")?;
        let state = comparable_path(&self.state_dir).context("invalid state_dir")?;
        ensure!(
            !mount.starts_with(&state) && !state.starts_with(&mount),
            "mount_dir and state_dir must be separate directories and must not contain one another"
        );
        ensure!(
            self.mount_dir.parent().is_some() && self.state_dir.parent().is_some(),
            "mount_dir and state_dir must not be filesystem roots"
        );
        if let Some(letter) = self.drive_letter {
            ensure!(
                letter.is_ascii_alphabetic(),
                "drive_letter must be one letter, for example S"
            );
        }
        ensure!(
            self.cache_max_bytes >= 4096,
            "cache_max_bytes must be at least 4096"
        );
        ensure!(
            self.cache_max_files > 0,
            "cache_max_files must be at least 1"
        );
        ensure!(
            (1..=86_400).contains(&self.poll_interval_secs),
            "poll_interval_secs must be between 1 and 86400"
        );
        ensure!(
            (1..=3600).contains(&self.settle_delay_secs),
            "settle_delay_secs must be between 1 and 3600"
        );
        ensure!(
            (1..=64).contains(&self.upload_concurrency),
            "upload_concurrency must be between 1 and 64"
        );
        ensure!(
            !self.device_id.is_empty()
                && self.device_id.len() <= 64
                && self
                    .device_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
            "device_id must contain 1 to 64 ASCII letters, numbers, hyphens, or underscores"
        );
        if let Some(protected) = &self.protected_credentials {
            crate::credentials::validate_protected(protected)?;
        } else {
            for name in [&self.access_key_env, &self.secret_key_env]
                .into_iter()
                .chain(self.session_token_env.iter())
            {
                ensure!(
                    !name.is_empty()
                        && name.len() <= 255
                        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                    "credential environment variable names must contain only ASCII letters, numbers, and underscores"
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_impl(path, bytes, true)
}

pub(crate) fn write_atomic_new(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_impl(path, bytes, false)
}

fn write_atomic_impl(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path, overwrite)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("save configuration {}", path.display()))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path, overwrite: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<_> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Both paths are owned, NUL-terminated buffers valid for the duration of this call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH
                | if overwrite {
                    MOVEFILE_REPLACE_EXISTING
                } else {
                    0
                },
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path, overwrite: bool) -> std::io::Result<()> {
    if overwrite {
        fs::rename(source, destination)
    } else {
        // The temporary is on the same filesystem. Linking publishes the whole
        // file atomically and fails if another creator won the destination.
        fs::hard_link(source, destination)?;
        fs::remove_file(source)
    }
}

pub(crate) fn canonical_prefix(prefix: &str) -> Result<String> {
    if prefix.is_empty() {
        return Ok(String::new());
    }
    ensure!(
        !prefix.starts_with('/') && !prefix.contains('\\') && !prefix.chars().any(char::is_control),
        "prefix must be a relative S3 key prefix using forward slashes"
    );
    let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
    ensure!(
        trimmed
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "prefix must not contain empty, dot, or parent components"
    );
    ensure!(
        trimmed.len() < 1024,
        "prefix must be shorter than 1024 bytes"
    );
    Ok(format!("{trimmed}/"))
}

/// Canonicalize existing ancestors to detect junctions/symlinks even before first run.
pub(crate) fn comparable_path(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "path must be absolute");
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, Component::ParentDir)),
        "path must not contain parent components"
    );
    #[cfg(windows)]
    for part in path.components() {
        if let Component::Normal(name) = part {
            let name = name.to_str().context("path must contain valid Unicode")?;
            ensure!(
                !name.ends_with(['.', ' '])
                    && !name
                        .chars()
                        .any(|c| c.is_control() || "<>:\"|?*".contains(c)),
                "path must not contain Windows aliases, streams, or invalid characters"
            );
        }
    }
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            bail!("path has no accessible existing ancestor");
        };
        suffix.push(name.to_owned());
        existing = existing.parent().context("path has no existing ancestor")?;
    }
    let mut resolved = existing
        .canonicalize()
        .context("resolve existing path ancestor")?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    #[cfg(windows)]
    {
        // Windows cloud roots use the ordinary case-insensitive namespace.
        resolved = PathBuf::from(resolved.to_string_lossy().to_lowercase());
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> Config {
        let base = env::temp_dir().join(format!("bucket-mount-config-{}", uuid::Uuid::new_v4()));
        Config {
            bucket: "test-bucket".into(),
            mount_dir: base.join("mount"),
            state_dir: base.join("state"),
            ..Config::default()
        }
    }

    #[test]
    fn rejects_overlapping_directories() {
        let mut config = valid_config();
        config.state_dir = config.mount_dir.join("state");
        assert!(config.validate().is_err());
        config.state_dir = config.mount_dir.clone();
        assert!(config.validate().is_err());
        config.state_dir = config.mount_dir.with_extension("sibling");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn requires_explicit_http_and_rejects_embedded_secrets() {
        let mut config = valid_config();
        config.endpoint = "http://localhost:9000".into();
        assert!(config.validate().is_err());
        config.allow_http = true;
        assert!(config.validate().is_ok());
        config.endpoint = "https://user:password@localhost".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn canonical_prefix_keeps_bucket_boundary() {
        assert_eq!(canonical_prefix("photos").unwrap(), "photos/");
        assert_eq!(canonical_prefix("photos/").unwrap(), "photos/");
        assert_eq!(canonical_prefix("").unwrap(), "");
        for invalid in [
            "/photos",
            "photos//",
            "photos/../private",
            "photos/./private",
            "photos\\private",
        ] {
            assert!(canonical_prefix(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn rejects_bad_resource_limits_and_plaintext_secrets() {
        let mut config = valid_config();
        config.upload_concurrency = 0;
        assert!(config.validate().is_err());
        assert!(toml::from_str::<Config>("secret_key = 'do-not-store'").is_err());
        assert!(
            !toml::to_string(&Config::default())
                .unwrap()
                .contains("secret_key =")
        );
    }

    #[test]
    fn conditional_etag_format_defaults_to_quoted_and_accepts_bare() {
        let config: Config = toml::from_str("bucket = 'test-bucket'").unwrap();
        assert_eq!(
            config.conditional_etag_format,
            ConditionalEtagFormat::Quoted
        );
        let ovh: Config = toml::from_str("conditional_etag_format = 'bare'").unwrap();
        assert_eq!(ovh.conditional_etag_format, ConditionalEtagFormat::Bare);
        let encoded = toml::to_string(&ovh).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&encoded)
                .unwrap()
                .conditional_etag_format,
            ConditionalEtagFormat::Bare
        );
        assert!(toml::from_str::<Config>("conditional_etag_format = 'disabled'").is_err());
    }

    #[test]
    fn disabled_session_token_stays_disabled_after_serialization() {
        let omitted: Config = toml::from_str("bucket = 'test-bucket'").unwrap();
        assert_eq!(omitted.session_token_env, None);
        let explicit: Config =
            toml::from_str("session_token_env = 'CUSTOM_SESSION_TOKEN'").unwrap();
        assert_eq!(
            explicit.session_token_env.as_deref(),
            Some("CUSTOM_SESSION_TOKEN")
        );

        let mut config = valid_config();
        config.session_token_env = None;
        let encoded = toml::to_string(&config).unwrap();
        assert!(!encoded.contains("session_token_env"));
        assert_eq!(
            toml::from_str::<Config>(&encoded)
                .unwrap()
                .session_token_env,
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_path_aliases_that_bypass_directory_isolation() {
        let mut config = valid_config();
        config.state_dir = PathBuf::from(format!("{}.", config.mount_dir.display()));
        assert!(config.validate().is_err());
        config.state_dir = PathBuf::from(format!("{} ", config.mount_dir.display()));
        assert!(config.validate().is_err());
    }
}
