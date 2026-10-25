//! Saved connections and validation of their shared resources.
use crate::config::{Config, canonical_prefix, comparable_path, write_atomic};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub name: String,
    pub enabled: bool,
    pub config: Config,
}

impl Connection {
    pub fn new(name: String) -> Self {
        let mut config = Config::default();
        let id = &config.device_id;
        config.mount_dir = config
            .mount_dir
            .with_file_name(format!("BucketMount-{}", &id[..8]));
        config.state_dir = config
            .state_dir
            .with_file_name("BucketMountProfiles")
            .join(id);
        config.session_token_env = None;
        Self {
            name,
            enabled: true,
            config,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profiles {
    pub format_version: u32,
    pub connections: Vec<Connection>,
}

impl Default for Profiles {
    fn default() -> Self {
        Self {
            format_version: 1,
            connections: vec![Connection::new("My bucket".into())],
        }
    }
}

impl Profiles {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?;
        let document: toml::Value = toml::from_str(&text).context("Parse connections")?;
        let rows = document
            .get("connections")
            .and_then(toml::Value::as_array)
            .context("Missing connections list")?;
        for row in rows {
            ensure!(
                row.get("config")
                    .and_then(|c| c.get("device_id"))
                    .and_then(toml::Value::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "Connection is missing device_id; restore its saved configuration"
            );
        }
        let mut profiles: Self = toml::from_str(&text).context("Read connection profiles")?;
        profiles.validate_structure()?;
        for connection in &mut profiles.connections {
            // Keep incomplete connection settings available for repair in the UI.
            if let Ok(prefix) = canonical_prefix(&connection.config.prefix) {
                connection.config.prefix = prefix;
            }
            if let Some(letter) = &mut connection.config.drive_letter {
                *letter = letter.to_ascii_uppercase();
            }
        }
        Ok(profiles)
    }

    pub fn validate_structure(&self) -> Result<()> {
        ensure!(
            self.format_version == 1,
            "Unsupported connections format version"
        );
        ensure!(
            self.connections.len() <= 100,
            "At most 100 connections are supported"
        );
        let mut names = HashSet::new();
        let mut identities = HashSet::new();
        for connection in &self.connections {
            ensure!(
                !connection.name.trim().is_empty()
                    && connection.name.chars().count() <= 80
                    && !connection.name.chars().any(char::is_control),
                "Connection names must contain 1 to 80 printable characters"
            );
            ensure!(
                names.insert(connection.name.to_lowercase()),
                "Each connection needs a different name"
            );
            ensure!(
                !connection.config.device_id.is_empty()
                    && connection.config.device_id.len() <= 64
                    && connection
                        .config
                        .device_id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
                    && identities.insert(&connection.config.device_id),
                "Each connection needs a unique device identity"
            );
        }
        Ok(())
    }

    pub fn validate_connection(&self, index: usize) -> Result<()> {
        let connection = self
            .connections
            .get(index)
            .context("Connection no longer exists")?;
        connection.config.validate()?;
        if connection.enabled {
            for (other_index, other) in self.connections.iter().enumerate() {
                if other_index != index && other.enabled && other.config.validate().is_ok() {
                    separate_resources(&connection.config, &other.config)
                        .with_context(|| format!("Connection conflicts with {}", other.name))?;
                }
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate_structure()?;
        let mut normalized = self.clone();
        for connection in &mut normalized.connections {
            if let Ok(prefix) = canonical_prefix(&connection.config.prefix) {
                connection.config.prefix = prefix;
            }
        }
        write_atomic(path, toml::to_string_pretty(&normalized)?.as_bytes())
    }
}

/// Include state folders in cross-profile checks to prevent either connection
/// from uploading another connection's database, logs, or staged writes.
pub fn separate_resources(a: &Config, b: &Config) -> Result<()> {
    if let (Some(a), Some(b)) = (a.drive_letter, b.drive_letter) {
        ensure!(
            !a.eq_ignore_ascii_case(&b),
            "Drive letters must be different"
        );
    }
    for left in [&a.mount_dir, &a.state_dir] {
        for right in [&b.mount_dir, &b.state_dir] {
            let left = comparable_path(left)?;
            let right = comparable_path(right)?;
            ensure!(
                !left.starts_with(&right) && !right.starts_with(&left),
                "Mounted folders and state folders must not overlap between connections"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn connection(root: &Path, name: &str) -> Connection {
        let mut connection = Connection::new(name.into());
        connection.config.bucket = name.into();
        connection.config.mount_dir = root.join(format!("mount-{name}"));
        connection.config.state_dir = root.join(format!("state-{name}"));
        connection
    }
    #[test]
    fn saved_connections_preserve_identity_paths_and_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let profiles = Profiles {
            format_version: 1,
            connections: vec![connection(dir.path(), "one"), connection(dir.path(), "two")],
        };
        profiles.save(&path).unwrap();
        // Exercise atomic replacement when saving settings again.
        profiles.save(&path).unwrap();
        assert_eq!(
            Profiles::load(&path).unwrap().connections,
            profiles.connections
        );
    }

    #[test]
    fn requires_the_connections_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let flat_config = toml::to_string(&connection(dir.path(), "one").config).unwrap();
        fs::write(&path, &flat_config).unwrap();
        assert!(Profiles::load(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), flat_config);

        let mut document = toml::Value::try_from(Profiles::default()).unwrap();
        document["format_version"] = 2.into();
        fs::write(&path, toml::to_string(&document).unwrap()).unwrap();
        assert!(Profiles::load(&path).is_err());
        document.as_table_mut().unwrap().remove("format_version");
        fs::write(&path, toml::to_string(&document).unwrap()).unwrap();
        assert!(Profiles::load(&path).is_err());
    }

    #[test]
    fn generated_template_round_trips_through_the_saved_format() {
        // `bucket-mount init` and the app write this exact document; every field
        // must survive the parser that reads user-edited files back.
        let template = Profiles::default();
        template.validate_structure().unwrap();
        let encoded = toml::to_string_pretty(&template).unwrap();
        let decoded: Profiles = toml::from_str(&encoded).unwrap();
        decoded.validate_structure().unwrap();
        assert_eq!(decoded.connections.len(), 1);
        assert_eq!(
            decoded.connections[0].config.endpoint,
            template.connections[0].config.endpoint
        );
        assert_eq!(
            decoded.connections[0].config.device_id,
            template.connections[0].config.device_id
        );
        // The template is deliberately incomplete until a bucket is chosen.
        assert!(decoded.validate_connection(0).is_err());
    }
    #[test]
    fn rejects_overlapping_mount_state_and_drive_resources() {
        let dir = tempfile::tempdir().unwrap();
        let a = connection(dir.path(), "one");
        let mut b = connection(dir.path(), "two");
        let mut profiles = Profiles {
            format_version: 1,
            connections: vec![a.clone(), b.clone()],
        };
        profiles.validate_connection(0).unwrap();
        b.config.mount_dir = a.config.mount_dir.join("nested");
        profiles.connections[1] = b.clone();
        assert!(profiles.validate_connection(0).is_err());
        profiles.connections[1].enabled = false;
        profiles.validate_connection(0).unwrap();
        b.config.mount_dir = a.config.state_dir.clone();
        assert!(separate_resources(&a.config, &b.config).is_err());
        profiles.connections[0].config.drive_letter = Some('S');
        profiles.connections[1] = connection(dir.path(), "two");
        profiles.connections[1].config.drive_letter = Some('s');
        assert!(profiles.validate_connection(0).is_err());
    }
    #[test]
    fn remove_profile_keeps_files_and_empty_collection_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let profile = connection(dir.path(), "one");
        fs::create_dir_all(&profile.config.mount_dir).unwrap();
        let local_file = profile.config.mount_dir.join("keep.txt");
        fs::write(&local_file, "keep").unwrap();
        let mut profiles = Profiles {
            format_version: 1,
            connections: vec![profile],
        };
        profiles.save(&path).unwrap();
        profiles.connections.clear();
        profiles.save(&path).unwrap();
        assert!(Profiles::load(&path).unwrap().connections.is_empty());
        assert_eq!(fs::read_to_string(local_file).unwrap(), "keep");
    }
    #[test]
    fn rejects_duplicate_names_and_missing_identity() {
        let mut profiles = Profiles::default();
        profiles
            .connections
            .push(Connection::new("MY BUCKET".into()));
        assert!(profiles.validate_structure().is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(&path, "format_version = 1\n[[connections]]\nname = 'one'\nenabled = true\n[connections.config]\nbucket = 'test'\n").unwrap();
        assert!(Profiles::load(&path).is_err());
    }
}
