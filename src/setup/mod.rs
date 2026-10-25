//! Per-user setup. Only the two application executables and owned Windows
//! registration are changed; synchronization data is never traversed.
pub mod cleanup;
pub mod windows;

use crate::maintenance;
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

pub struct Installation {
    pub root: PathBuf,
    pub registration: windows::Registration,
}

impl Installation {
    pub fn current_user() -> Result<Self> {
        Ok(Self {
            root: maintenance::install_root()?,
            registration: windows::Registration::for_current_user()?,
        })
    }

    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn install(
        &self,
        application: &[u8],
        setup_executable: &Path,
        config: &Path,
        startup: bool,
    ) -> Result<()> {
        ensure!(
            application.starts_with(b"MZ"),
            "This development setup has no embedded application. Build a release with scripts/build.ps1."
        );
        ensure!(
            config.is_absolute() && !config.is_dir(),
            "configuration must name an absolute file path"
        );
        let _lock = maintenance::lock(&self.root, true)?;
        let bin = self.bin();
        maintenance::validate_path(&bin)?;
        let app = bin.join("bucket-mount.exe");
        let uninstall = bin.join("uninstall.exe");
        ensure!(
            !app.try_exists()? && !uninstall.try_exists()?,
            "Bucket Mount is already installed. Uninstall it before running setup again"
        );
        self.registration.ensure_available()?;
        let setup = fs::read(setup_executable).context("read setup executable")?;
        ensure!(
            setup.starts_with(b"MZ"),
            "setup must be a Windows executable"
        );
        fs::create_dir_all(&bin)?;
        let app_stage = StagedFile::new(&bin, application)?;
        let uninstall_stage = StagedFile::new(&bin, &setup)?;
        app_stage.commit(&app)?;
        if let Err(error) = uninstall_stage.commit(&uninstall) {
            remove_file_if_present(&app).context("clean up incomplete installation")?;
            return Err(error);
        }
        // Keep the uninstaller available if Windows registration fails.
        self.registration.install(&bin, config, startup)
            .context("Windows registration failed. Run setup with --uninstall to remove the incomplete installation before trying again")?;
        Ok(())
    }

    pub fn uninstall(&self) -> Result<()> {
        let _lock = maintenance::lock(&self.root, true)?;
        let bin = self.bin();
        maintenance::validate_path(&bin)?;
        let app = bin.join("bucket-mount.exe");
        let uninstall = bin.join("uninstall.exe");
        let app_guard = writable_existing(&app)?;
        let uninstall_guard = writable_existing(&uninstall)?;
        // Retain the uninstaller until registration cleanup succeeds so failed
        // cleanup can be retried from the same executable.
        remove_file_if_present(&app)?;
        drop(app_guard);
        self.registration.uninstall(&bin)?;
        remove_file_if_present(&uninstall)?;
        drop(uninstall_guard);
        match fs::remove_dir(&bin) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(145) => {
            }
            Err(e) => return Err(e).context("remove empty application bin directory"),
        }
        Ok(())
    }
}

fn writable_existing(path: &Path) -> Result<Option<File>> {
    maintenance::validate_path(path)?;
    match OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| {
            format!(
                "Cannot change {}. Quit Bucket Mount and close other setup windows, then try again",
                path.display()
            )
        }),
    }
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    maintenance::validate_path(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

struct StagedFile(PathBuf);

impl StagedFile {
    fn new(directory: &Path, bytes: &[u8]) -> Result<Self> {
        let path = directory.join(format!("setup-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        // Only take cleanup ownership after create_new succeeds.
        let staged = Self(path);
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        ensure!(
            fs::read(&staged.0)? == bytes,
            "installed file failed verification"
        );
        Ok(staged)
    }

    fn commit(self, destination: &Path) -> Result<()> {
        maintenance::validate_path(destination)?;
        let source = wide(&self.0);
        let destination = wide(destination);
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("create installed executable");
        }
        Ok(())
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct IsolatedInstallation {
        installation: Installation,
        _registry_cleanup: windows::TestRegistryCleanup,
    }

    impl std::ops::Deref for IsolatedInstallation {
        type Target = Installation;

        fn deref(&self) -> &Self::Target {
            &self.installation
        }
    }

    fn isolated(root: &Path) -> IsolatedInstallation {
        let registry = format!(r"Software\BucketMountTests\{}", uuid::Uuid::new_v4());
        IsolatedInstallation {
            _registry_cleanup: windows::TestRegistryCleanup::new(&registry),
            installation: Installation {
                root: root.join("Bucket Mount test"),
                registration: windows::Registration {
                    uninstall_key: format!(r"{registry}\Uninstall"),
                    run_key: format!(r"{registry}\Run"),
                    shortcut: root.join("Bucket Mount test.lnk"),
                },
            },
        }
    }

    #[test]
    fn fresh_install_and_uninstall_preserve_user_files() {
        let directory = tempfile::tempdir().unwrap();
        let installation = isolated(directory.path());
        fs::create_dir_all(installation.bin()).unwrap();
        let config = installation.root.join("config.toml");
        fs::write(&config, "valuable settings").unwrap();
        let state = installation.root.join("state.sqlite3");
        fs::write(&state, "valuable state").unwrap();
        let unrelated = installation.bin().join("personal-file.txt");
        fs::write(&unrelated, "keep this too").unwrap();
        let setup = directory.path().join("setup.exe");
        fs::write(&setup, b"MZ setup fixture").unwrap();
        installation
            .install(b"MZ application", &setup, &config, true)
            .unwrap();
        assert_eq!(
            fs::read(installation.bin().join("bucket-mount.exe")).unwrap(),
            b"MZ application"
        );
        assert!(
            installation
                .install(b"MZ different application", &setup, &config, false)
                .is_err()
        );
        assert_eq!(
            fs::read(installation.bin().join("bucket-mount.exe")).unwrap(),
            b"MZ application"
        );
        installation.uninstall().unwrap();
        installation.uninstall().unwrap();
        assert!(!installation.bin().join("bucket-mount.exe").exists());
        assert!(!installation.bin().join("uninstall.exe").exists());
        assert!(
            !installation.registration.shortcut.exists(),
            "Start menu shortcut was kept; it targets {:?}",
            windows::shortcut_target(&installation.registration.shortcut)
                .map(|target| target.to_string_lossy().into_owned())
        );
        assert_eq!(fs::read_to_string(config).unwrap(), "valuable settings");
        assert_eq!(fs::read_to_string(state).unwrap(), "valuable state");
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep this too");
    }

    #[test]
    fn busy_app_blocks_install_and_uninstall_without_changing_files() {
        let directory = tempfile::tempdir().unwrap();
        let installation = isolated(directory.path());
        fs::create_dir_all(installation.bin()).unwrap();
        let app = installation.bin().join("bucket-mount.exe");
        fs::write(&app, b"MZ existing application").unwrap();
        let setup = directory.path().join("setup.exe");
        fs::write(&setup, b"MZ setup fixture").unwrap();
        let running = maintenance::lock(&installation.root, false).unwrap();
        assert!(
            installation
                .install(
                    b"MZ replacement",
                    &setup,
                    &installation.root.join("config.toml"),
                    false
                )
                .is_err()
        );
        assert!(installation.uninstall().is_err());
        assert_eq!(fs::read(&app).unwrap(), b"MZ existing application");
        drop(running);
        // An external file handle can also prevent removing the executable.
        let _open_file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&app)
            .unwrap();
        assert!(installation.uninstall().is_err());
        assert_eq!(fs::read(app).unwrap(), b"MZ existing application");
    }

    #[test]
    fn rejects_payloadless_setup_before_creating_an_install() {
        let directory = tempfile::tempdir().unwrap();
        let installation = isolated(directory.path());
        assert!(
            installation
                .install(
                    &[],
                    Path::new("missing.exe"),
                    &installation.root.join("config.toml"),
                    false
                )
                .is_err()
        );
        assert!(!installation.root.exists());
    }
}
