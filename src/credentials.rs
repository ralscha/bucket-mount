//! Per-user Windows DPAPI protection. Plaintext is never persisted or formatted.
use crate::config::Config;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const PREFIX: &str = "dpapi-v1:";
const MAX_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets([redacted])")
    }
}

pub(crate) fn erase(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

impl Drop for Secrets {
    fn drop(&mut self) {
        // Replacing UTF-8 bytes with NUL preserves String's UTF-8 invariant.
        unsafe {
            erase(self.access_key.as_bytes_mut());
            erase(self.secret_key.as_bytes_mut());
            if let Some(token) = &mut self.session_token {
                erase(token.as_bytes_mut());
            }
        }
    }
}

impl Secrets {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.access_key.trim().is_empty(), "Enter an access key");
        ensure!(!self.secret_key.trim().is_empty(), "Enter a secret key");
        ensure!(
            self.access_key.len()
                + self.secret_key.len()
                + self.session_token.as_ref().map_or(0, String::len)
                <= MAX_BYTES / 2,
            "Credentials are too long"
        );
        Ok(())
    }

    pub fn load(config: &Config) -> Result<Self> {
        if let Some(protected) = &config.protected_credentials {
            return Self::unprotect(protected, &config.device_id);
        }
        let required = |name: &str| -> Result<String> {
            std::env::var(name)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .with_context(|| format!("Set {name}, or enter credentials in Connections"))
        };
        Ok(Self {
            access_key: required(&config.access_key_env)?,
            secret_key: required(&config.secret_key_env)?,
            session_token: config
                .session_token_env
                .as_ref()
                .and_then(|name| std::env::var(name).ok())
                .filter(|s| !s.is_empty()),
        })
    }

    pub fn protect(&self, identity: &str) -> Result<String> {
        self.validate()?;
        let mut plaintext = serde_json::to_vec(self)?;
        let result = crypt(&mut plaintext, identity, true);
        erase(&mut plaintext);
        let ciphertext = result?;
        let mut encoded = String::from(PREFIX);
        use std::fmt::Write;
        for byte in ciphertext {
            write!(encoded, "{byte:02x}")?;
        }
        Ok(encoded)
    }

    pub fn unprotect(encoded: &str, identity: &str) -> Result<Self> {
        validate_protected(encoded)?;
        let hex = &encoded[PREFIX.len()..];
        let mut ciphertext = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let pair =
                    std::str::from_utf8(pair).context("Invalid protected credential data")?;
                u8::from_str_radix(pair, 16).context("Invalid protected credential data")
            })
            .collect::<Result<Vec<_>>>()?;
        let mut plaintext = crypt(&mut ciphertext, identity, false).context(
            "Cannot unlock saved credentials. Enter them again under this Windows account",
        )?;
        // Do not include parsing details, which can contain fragments of secrets.
        let result = serde_json::from_slice::<Self>(&plaintext)
            .map_err(|_| anyhow::anyhow!("Saved credentials are invalid; enter them again"));
        erase(&mut plaintext);
        let secrets = result?;
        secrets.validate()?;
        Ok(secrets)
    }
}

pub fn validate_protected(encoded: &str) -> Result<()> {
    let hex = encoded
        .strip_prefix(PREFIX)
        .context("Unsupported protected credential format")?;
    ensure!(
        !hex.is_empty()
            && hex.len() <= MAX_BYTES * 2
            && hex.len().is_multiple_of(2)
            && hex.bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid protected credential data"
    );
    Ok(())
}

#[cfg(windows)]
fn crypt(input: &mut [u8], identity: &str, protect: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    ensure!(input.len() <= MAX_BYTES, "Credential data is too long");
    let data = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let mut entropy = format!("BucketMount.credentials.v1/{identity}").into_bytes();
    let binding = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        let ok = if protect {
            // Intentionally omit CRYPTPROTECT_LOCAL_MACHINE: bind to the user.
            CryptProtectData(
                &data,
                std::ptr::null(),
                &binding,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &data,
                std::ptr::null_mut(),
                &binding,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .context("Windows credential protection failed");
        }
        let bytes = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let result = bytes.to_vec();
        erase(bytes);
        LocalFree(output.pbData.cast());
        Ok(result)
    }
}

#[cfg(not(windows))]
fn crypt(_: &mut [u8], _: &str, _: bool) -> Result<Vec<u8>> {
    anyhow::bail!(
        "Protected credentials require Windows; use environment variables on this platform"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_ciphertext_and_redacts_debug() {
        for text in [
            "",
            "dpapi-v2:00",
            "dpapi-v1:f",
            "dpapi-v1:xx",
            "dpapi-v1:💾",
        ] {
            assert!(validate_protected(text).is_err());
        }
        let secrets = Secrets {
            access_key: "example-access".into(),
            secret_key: "example-secret".into(),
            session_token: None,
        };
        assert_eq!(format!("{secrets:?}"), "Secrets([redacted])");
    }
    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_is_bound_to_identity_and_contains_no_plaintext() {
        let secrets = Secrets {
            access_key: "test-access-key".into(),
            secret_key: "test-secret-key".into(),
            session_token: Some("test-session-token".into()),
        };
        let encrypted = secrets.protect("test-profile").unwrap();
        for value in [
            &secrets.access_key,
            &secrets.secret_key,
            secrets.session_token.as_ref().unwrap(),
        ] {
            assert!(!encrypted.contains(value));
        }
        let loaded = Secrets::unprotect(&encrypted, "test-profile").unwrap();
        assert_eq!(loaded.access_key, secrets.access_key);
        assert_eq!(loaded.secret_key, secrets.secret_key);
        assert_eq!(loaded.session_token, secrets.session_token);
        assert!(Secrets::unprotect(&encrypted, "different-profile").is_err());
        let mut corrupt = encrypted.into_bytes();
        let last = corrupt.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        assert!(
            Secrets::unprotect(std::str::from_utf8(&corrupt).unwrap(), "test-profile").is_err()
        );
    }
}
