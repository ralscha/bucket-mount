#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use bucket_mount::{
    config::Config,
    profiles::Profiles,
    remote::{Remote, S3Remote},
};
#[cfg(windows)]
use bucket_mount::{engine::Engine, tray};
use clap::{Parser, Subcommand};
use std::{path::Path, path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(version, about = "S3 files on demand for Windows")]
struct Cli {
    /// Settings file; defaults to %LOCALAPPDATA%\BucketMount\config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Start the notification-area application (the default).
    Run,
    /// Write a configuration template. Never replaces an existing file.
    Init,
    /// Validate settings, credentials, listing, and conditional request support.
    Check,
    /// Run two reconciliation passes, allowing local files to settle.
    SyncOnce,
}

fn main() {
    #[cfg(all(windows, not(debug_assertions)))]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
    let cli = Cli::parse();
    #[cfg(all(windows, not(debug_assertions)))]
    let tray_mode = matches!(&cli.command, None | Some(Command::Run));
    if let Err(e) = run(cli) {
        eprintln!("Bucket Mount: {e:#}");
        #[cfg(all(windows, not(debug_assertions)))]
        if tray_mode {
            unsafe {
                let text: Vec<u16> = format!("{e:#}").encode_utf16().chain(Some(0)).collect();
                let title: Vec<u16> = "Bucket Mount".encode_utf16().chain(Some(0)).collect();
                windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    std::ptr::null_mut(),
                    text.as_ptr(),
                    title.as_ptr(),
                    windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // Hold this from startup through worker shutdown, including settings-only
    // and reconnecting sessions which do not yet have an engine/state lock.
    #[cfg(windows)]
    let _installation_guard = bucket_mount::maintenance::application_guard()?;
    let config_path = cli
        .config
        .unwrap_or_else(|| Config::default().state_dir.join("config.toml"));
    match cli.command.unwrap_or(Command::Run) {
        Command::Init => {
            anyhow::ensure!(
                !config_path.exists(),
                "configuration already exists: {}",
                config_path.display()
            );
            Profiles::default().save(&config_path)?;
            println!("Created {}", config_path.display());
            Ok(())
        }
        Command::Run => {
            if !config_path.exists() {
                Profiles::default().save(&config_path)?;
            }
            run_tray(config_path)
        }
        command => run_once(&config_path, command),
    }
}

/// Run the non-interactive commands: one verified connection pass per command.
fn run_once(config_path: &Path, command: Command) -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init()
        .ok();
    let profiles = Profiles::load(config_path)?;
    let runtime = runtime()?;
    let _enter = runtime.enter();
    for (index, connection) in profiles
        .connections
        .iter()
        .enumerate()
        .filter(|(_, c)| c.enabled)
    {
        profiles
            .validate_connection(index)
            .with_context(|| format!("Connection {}", connection.name))?;
        let config = connection.config.clone();
        let remote = Arc::new(S3Remote::new(&config)?);
        runtime.block_on(remote.verify_conditionals())?;
        if matches!(command, Command::Check) {
            let objects = runtime.block_on(remote.list())?;
            bucket_mount::paths::validate_namespace(objects.iter().map(|o| o.key.as_str()))?;
            println!(
                "{}: connection verified, {} objects",
                connection.name,
                objects.len()
            );
            continue;
        }
        #[cfg(windows)]
        {
            let settle = config.settle_delay_secs;
            let mut engine = Engine::start(config, remote, runtime.handle().clone())?;
            println!(
                "{}: {}",
                connection.name,
                runtime.block_on(engine.sync_once())?
            );
            std::thread::sleep(Duration::from_secs(settle));
            let stats = runtime.block_on(engine.sync_once())?;
            println!("{}: {stats}", connection.name);
            anyhow::ensure!(
                stats.errors == 0,
                "Some files could not synchronize; resolve the reported errors and retry"
            );
        }
        #[cfg(not(windows))]
        anyhow::bail!("Cloud Files mounting requires Windows 10 1709 or newer");
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .context("start network runtime")
}

#[cfg(windows)]
fn run_tray(config_path: PathBuf) -> Result<()> {
    // Prevent two managers from overwriting the same collection of profiles.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config_path.with_extension("manager.lock"))?;
    lock.try_lock()
        .context("Bucket Mount is already managing this configuration")?;
    let profiles = Profiles::load(&config_path)?;
    let logs = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .join("logs");
    std::fs::create_dir_all(&logs)?;
    let appender = tracing_appender::rolling::daily(logs, "bucket-mount.log");
    let (writer, _guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bucket_mount=info".into()),
        )
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .ok();
    tray::run(config_path, profiles)
}

#[cfg(not(windows))]
fn run_tray(_: PathBuf) -> Result<()> {
    anyhow::bail!("The tray and Cloud Files integration require Windows")
}
