use bucket_mount::profiles::Profiles;
use std::{fs, process::Command};

#[test]
fn init_creates_profiles_and_refuses_to_overwrite_settings() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("settings with spaces.toml");
    let init = || {
        Command::new(env!("CARGO_BIN_EXE_bucket-mount"))
            .env("LOCALAPPDATA", directory.path())
            .env("USERPROFILE", directory.path())
            .args(["--config"])
            .arg(&config_path)
            .arg("init")
            .output()
            .unwrap()
    };

    let first = init();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let profiles = Profiles::load(&config_path).unwrap();
    assert_eq!(profiles.format_version, 1);
    assert_eq!(profiles.connections.len(), 1);
    let connection = &profiles.connections[0];
    assert!(connection.enabled);
    assert!(!connection.config.device_id.is_empty());
    assert!(connection.config.bucket.is_empty());
    assert!(connection.config.protected_credentials.is_none());
    assert!(
        connection
            .config
            .state_dir
            .starts_with(directory.path().join("BucketMountProfiles"))
    );

    let original = fs::read(&config_path).unwrap();
    let repeated = init();
    assert!(!repeated.status.success());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("configuration already exists"));
    assert_eq!(fs::read(&config_path).unwrap(), original);
}
