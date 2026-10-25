#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use bucket_mount::maintenance;
#[cfg(windows)]
#[path = "../setup/mod.rs"]
mod setup;

#[cfg(windows)]
mod native {
    use super::{maintenance, setup};
    use anyhow::{Context, Result, ensure};
    use clap::Parser;
    use std::{path::PathBuf, process::Command, ptr::null_mut};
    use windows_sys::Win32::{
        System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetConsoleWindow},
        UI::WindowsAndMessaging::{
            IDCANCEL, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION,
            MB_OK, MB_SETFOREGROUND, MB_YESNO, MB_YESNOCANCEL, MessageBoxW,
        },
    };

    include!(concat!(env!("OUT_DIR"), "/setup_payload.rs"));

    #[derive(Parser)]
    #[command(
        version,
        about = "Install or uninstall Bucket Mount for the current Windows user"
    )]
    struct Cli {
        /// Remove the application, preserving settings and synchronized files.
        #[arg(long)]
        uninstall: bool,
        /// Perform the operation without dialogs; never starts the application.
        #[arg(long)]
        quiet: bool,
        /// Start the tray app when you sign in.
        #[arg(long, conflicts_with_all = ["no_start_at_logon", "uninstall"])]
        start_at_logon: bool,
        /// Install without starting the tray app at sign-in.
        #[arg(long, conflicts_with = "uninstall")]
        no_start_at_logon: bool,
        /// Settings file used by the Start menu shortcut and login entry.
        #[arg(long, conflicts_with = "uninstall")]
        config: Option<PathBuf>,
        #[arg(long, hide = true, requires_all = ["parent_pid", "uninstall"])]
        cleanup_worker: bool,
        #[arg(long, hide = true, requires = "cleanup_worker")]
        parent_pid: Option<u32>,
    }

    pub fn main() -> i32 {
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        let cli = match Cli::try_parse() {
            Ok(cli) => cli,
            Err(error) => {
                let code = error.exit_code();
                let _ = error.print();
                if unsafe { GetConsoleWindow().is_null() }
                    && !std::env::args_os().any(|a| a == "--quiet")
                {
                    message(
                        &error.to_string(),
                        MB_OK
                            | if code == 0 {
                                MB_ICONINFORMATION
                            } else {
                                MB_ICONERROR
                            },
                    );
                }
                return code;
            }
        };
        let quiet = cli.quiet;
        let worker = cli.cleanup_worker;
        let result = run(cli);
        if worker {
            // Some Windows versions retain the running helper in the user's
            // temporary directory. Installed files have their own checked cleanup.
            let _ = setup::cleanup::cleanup_worker_file();
        }
        match result {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("Bucket Mount Setup: {error:#}");
                if !quiet {
                    message(&format!("{error:#}"), MB_OK | MB_ICONERROR);
                }
                1
            }
        }
    }

    fn run(mut cli: Cli) -> Result<()> {
        let installation = setup::Installation::current_user()?;
        let current_exe = std::env::current_exe()?;
        if current_exe
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("uninstall.exe"))
        {
            cli.uninstall = true;
        }
        if cli.cleanup_worker {
            setup::cleanup::validate_worker()?;
            setup::cleanup::wait_for_parent(cli.parent_pid.context("missing parent process")?)?;
        }
        if cli.uninstall {
            ensure!(
                !cli.start_at_logon && !cli.no_start_at_logon && cli.config.is_none(),
                "startup and configuration options apply only to installation"
            );
            if !cli.quiet
                && !cli.cleanup_worker
                && message(
                    "Uninstall Bucket Mount for your Windows account?\n\nQuit Bucket Mount from its tray menu first.\n\nYour settings, sync state, and sync folders will be kept. Online-only files will need Bucket Mount reinstalled to open. No files in your S3 bucket will be changed.",
                    MB_YESNO | MB_DEFBUTTON2 | MB_ICONQUESTION,
                ) != IDYES
            {
                return Ok(());
            }
            if !cli.cleanup_worker {
                // Give immediate feedback for running apps before handing off.
                let _preflight = maintenance::lock(&installation.root, true)?;
                if setup::cleanup::handoff_if_installed(
                    &installation.bin().join("uninstall.exe"),
                    cli.quiet,
                )? {
                    return Ok(());
                }
            }
            // A shell may launch downloaded setup while its working directory
            // is the installed bin folder. Release that directory before removal.
            std::env::set_current_dir(std::env::temp_dir())?;
            installation.uninstall()?;
            println!("Bucket Mount uninstalled. Settings and synchronized files were preserved.");
            if !cli.quiet {
                message(
                    "Bucket Mount has been uninstalled.\n\nYour settings and sync folders have been kept. Reinstall Bucket Mount to open online-only files or resume synchronization.",
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            return Ok(());
        }

        ensure!(
            !APPLICATION.is_empty(),
            "This development setup has no embedded application. Build a release with scripts/build.ps1."
        );
        let config = cli
            .config
            .unwrap_or_else(|| installation.root.join("config.toml"));
        let config = std::path::absolute(config)?;
        let mut startup = cli.start_at_logon;
        if !cli.quiet {
            if message(
                &format!(
                    "Install Bucket Mount {} for your Windows account?\n\nLocation: {}\n\nNo administrator rights are needed. Open Bucket Mount after setup to configure your connections.",
                    env!("CARGO_PKG_VERSION"),
                    installation.bin().display()
                ),
                MB_YESNO | MB_ICONQUESTION,
            ) != IDYES
            {
                return Ok(());
            }
            if !cli.start_at_logon && !cli.no_start_at_logon {
                let choice = message(
                    "Start Bucket Mount automatically when you sign in?\n\nEnabled connections will synchronize using their saved credential settings.",
                    MB_YESNOCANCEL | MB_DEFBUTTON2 | MB_ICONQUESTION,
                );
                if choice == IDCANCEL {
                    return Ok(());
                }
                startup = choice == IDYES;
            }
        }
        installation.install(APPLICATION, &current_exe, &config, startup)?;
        println!("Installed Bucket Mount in {}", installation.bin().display());
        if !cli.quiet
            && message(
                "Bucket Mount is installed and available in the Start menu.\n\nYou can uninstall it from Windows Settings > Apps.\n\nOpen Bucket Mount now to configure your connections?",
                MB_YESNO | MB_ICONINFORMATION,
            ) == IDYES
        {
            Command::new(installation.bin().join("bucket-mount.exe"))
                .arg("--config")
                .arg(config)
                .arg("run")
                .spawn()
                .context("launch installed Bucket Mount")?;
        }
        Ok(())
    }

    fn message(text: &str, flags: u32) -> i32 {
        let text: Vec<_> = text.encode_utf16().chain(Some(0)).collect();
        let title: Vec<_> = "Bucket Mount Setup".encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(
                null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                flags | MB_SETFOREGROUND,
            )
        }
    }
}

#[cfg(windows)]
fn main() {
    std::process::exit(native::main());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Bucket Mount Setup requires Windows.");
    std::process::exit(1);
}
