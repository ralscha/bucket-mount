//! Move installed uninstall work into a temporary native executable so the
//! installed copy can be removed after its process exits.
use anyhow::{Context, Result, bail, ensure};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{self, Command},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{CREATE_NO_WINDOW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
};

const WORKER_PREFIX: &str = "bucket-mount-uninstall-";

/// Return true after launching the temporary worker; the caller must exit so
/// that worker can continue. A setup executable outside the installation needs
/// no handoff. No installation/deletion paths are accepted by the worker CLI.
pub fn handoff_if_installed(installed_uninstaller: &Path, quiet: bool) -> Result<bool> {
    let installed = match fs::canonicalize(installed_uninstaller) {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).context("locate installed uninstaller"),
    };
    let current = fs::canonicalize(env::current_exe()?).context("locate running setup")?;
    if current != installed {
        return Ok(false);
    }

    let temporary = fs::canonicalize(env::temp_dir()).context("locate temporary directory")?;
    let worker = temporary.join(format!("{WORKER_PREFIX}{}.exe", uuid::Uuid::new_v4()));
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&worker)
        .context("create temporary uninstaller")?;
    let prepared = (|| -> Result<()> {
        let mut source = File::open(&current).context("read installed uninstaller")?;
        io::copy(&mut source, &mut destination).context("copy temporary uninstaller")?;
        destination.sync_all().context("save temporary uninstaller")
    })();
    drop(destination);
    if let Err(error) = prepared {
        let _ = fs::remove_file(&worker);
        return Err(error);
    }

    let mut command = Command::new(&worker);
    command
        .args(["--uninstall", "--cleanup-worker", "--parent-pid"])
        .arg(process::id().to_string())
        .current_dir(&temporary)
        .creation_flags(CREATE_NO_WINDOW);
    if quiet {
        command.arg("--quiet");
    }
    match command.spawn() {
        Ok(_) => Ok(true),
        Err(error) => {
            let _ = fs::remove_file(&worker);
            Err(error).context("start temporary uninstaller")
        }
    }
}

/// Wait for the installed uninstaller to release its image without terminating
/// any process. A process that has already exited needs no further waiting.
pub fn wait_for_parent(pid: u32) -> Result<()> {
    ensure!(
        pid != 0 && pid != process::id(),
        "invalid uninstaller parent process ID"
    );
    let parent = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if parent.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(());
        }
        return Err(error).context("open uninstaller parent process");
    }
    let result = unsafe { WaitForSingleObject(parent, 30_000) };
    let wait_error = (result == WAIT_FAILED).then(io::Error::last_os_error);
    unsafe { CloseHandle(parent) };
    match result {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => {
            bail!("The original uninstaller did not exit within 30 seconds. Close it and try again")
        }
        _ => Err(wait_error.unwrap_or_else(|| io::Error::other("unexpected process wait result")))
            .context("wait for uninstaller parent process"),
    }
}

/// Best effort only: Windows normally refuses to delete a running executable.
/// If this returns an error after uninstall succeeded, leave the helper in the
/// temporary directory and report it as informational, not an uninstall failure.
pub fn cleanup_worker_file() -> Result<()> {
    let current = worker_path()?;
    fs::remove_file(&current).with_context(|| {
        format!(
            "Temporary uninstaller remains at {} and can be deleted after setup closes",
            current.display()
        )
    })
}

/// Validate the internal worker mode before it can skip the ordinary uninstall
/// confirmation. This check does not change any files.
pub fn validate_worker() -> Result<()> {
    worker_path().map(|_| ())
}

fn worker_path() -> Result<PathBuf> {
    let current = fs::canonicalize(env::current_exe()?).context("locate temporary uninstaller")?;
    let temporary = fs::canonicalize(env::temp_dir()).context("locate temporary directory")?;
    ensure!(
        current.parent() == Some(temporary.as_path()),
        "cleanup worker is outside the temporary directory"
    );
    ensure!(
        current
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_worker_name),
        "cleanup worker filename is invalid"
    );
    Ok(current)
}

fn is_worker_name(name: &str) -> bool {
    let Some(id) = name
        .strip_prefix(WORKER_PREFIX)
        .and_then(|s| s.strip_suffix(".exe"))
    else {
        return false;
    };
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_requires_the_exact_generated_filename_format() {
        let id = uuid::Uuid::new_v4();
        assert!(is_worker_name(&format!("{WORKER_PREFIX}{id}.exe")));
        assert!(!is_worker_name("bucket-mount-uninstall.exe"));
        assert!(!is_worker_name("bucket-mount-uninstall-personal.exe"));
        assert!(!is_worker_name(&format!(
            "{WORKER_PREFIX}{}.exe",
            id.simple()
        )));
        assert!(!is_worker_name(&format!("{WORKER_PREFIX}{id}.exe.backup")));
    }
}
