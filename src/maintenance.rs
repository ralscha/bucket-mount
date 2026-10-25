//! Coordinate application processes with per-user setup without killing uploads.
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    os::windows::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

pub fn install_root() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let root = PathBuf::from(local).join("BucketMount");
    validate_path(&root)?;
    Ok(root)
}

/// Check every existing component before any filesystem mutation. Installation
/// never traverses a junction, symlink, or other reparse point.
pub fn validate_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "installation path must be absolute");
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "installation path must not contain parent traversal"
    );
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "installation path must not contain a reparse point: {}",
                ancestor.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("inspect installation path"),
        }
    }
    Ok(())
}

pub fn lock(root: &Path, exclusive: bool) -> Result<File> {
    validate_path(root)?;
    fs::create_dir_all(root).context("create per-user application directory")?;
    let path = root.join("install.lock");
    validate_path(&path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .context("open installation lock")?;
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.context(if exclusive {
        "Quit Bucket Mount from its tray menu and close other setup windows, then try again"
    } else {
        "Bucket Mount setup is running; wait until it finishes and try again"
    })?;
    Ok(file)
}

pub fn application_guard() -> Result<File> {
    lock(&install_root()?, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_excludes_apps_but_allows_multiple_mounts() {
        let directory = tempfile::tempdir().unwrap();
        let first = lock(directory.path(), false).unwrap();
        let second = lock(directory.path(), false).unwrap();
        assert!(lock(directory.path(), true).is_err());
        drop((first, second));
        let setup = lock(directory.path(), true).unwrap();
        assert!(lock(directory.path(), false).is_err());
        assert!(lock(directory.path(), true).is_err());
        drop(setup);
        assert!(lock(directory.path(), false).is_ok());
    }
}
