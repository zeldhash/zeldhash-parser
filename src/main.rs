//! # mhinparser
//!
//! A high-performance Bitcoin blockchain parser implementing the **My Hash Is Nice** protocol.
//!
//! This binary application parses the Bitcoin blockchain to track MHIN rewards,
//! leveraging [`protoblock`](https://crates.io/crates/protoblock) for fast block
//! fetching and [`rollblock`](https://crates.io/crates/rollblock) for efficient
//! UTXO management with instant rollback support.
//!
//! ## Features
//!
//! - **High Performance** — Parallel block fetching with configurable thread pools
//! - **Instant Rollbacks** — O(1) chain reorganization handling via rollblock
//! - **Multi-Network** — Supports Mainnet, Testnet4, Signet, and Regtest
//! - **Interactive UI** — Live progress display powered by ratatui
//! - **Daemon Mode** — Background service with signal handling
//!
//! ## Usage
//!
//! ```bash
//! # Parse mainnet
//! mhinparser
//!
//! # Parse testnet4
//! mhinparser --network testnet4
//!
//! # Run as daemon
//! mhinparser --daemon
//! ```
//!
//! See the [README](https://github.com/ouziel-slama/mhinparser) for full documentation.

mod cli;
mod config;
mod parser;
mod progress;
mod stores;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use protoblock::runtime::config::FetcherConfig;
use protoblock::Runner;
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::str::FromStr;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Builder;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command as LifecycleCommand};
use crate::config::{load_runtime_paths, AppConfig, RuntimePaths};
use crate::parser::MhinParser;
use crate::progress::ProgressReporter;
use crate::stores::sqlite::DB_FILE_NAME;

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

const STOP_SIGINT_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_SIGTERM_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn main() -> Result<()> {
    let cli = Cli::parse();
    let lifecycle = cli.command.unwrap_or(LifecycleCommand::Run);

    match lifecycle {
        LifecycleCommand::Run => handle_run(cli),
        LifecycleCommand::Stop => handle_stop(cli),
    }
}

fn handle_run(cli: Cli) -> Result<()> {
    let launch = determine_launch(&cli);
    let app_config = AppConfig::load(cli)?;

    match launch {
        LaunchMode::DaemonParent => spawn_daemon(&app_config),
        LaunchMode::DaemonChild => {
            let options = RunOptions::daemon_child(&app_config.runtime);
            run_with_config(app_config, options)
        }
        LaunchMode::Foreground => run_with_config(app_config, RunOptions::foreground()),
    }
}

fn handle_stop(cli: Cli) -> Result<()> {
    let runtime_paths = load_runtime_paths(cli)?;
    stop_daemon(runtime_paths)
}

fn run_with_config(app_config: AppConfig, options: RunOptions) -> Result<()> {
    let RunOptions {
        interactive,
        log_path,
        pid_file,
    } = options;

    init_tracing(log_path.as_deref())?;
    announce_configuration(&app_config, interactive);

    let _pid_guard = pid_file.map(PidFileGuard::new);
    start_runtime(app_config, interactive)
}

fn start_runtime(app_config: AppConfig, interactive: bool) -> Result<()> {
    let fetcher_config = app_config.protoblock.fetcher_config().clone();
    let parser = MhinParser::new(app_config)?;
    let runtime = Builder::new_multi_thread().enable_all().build()?;

    runtime.block_on(async move { run_parser(fetcher_config, parser, interactive).await })?;
    Ok(())
}

async fn run_parser(
    fetcher_config: FetcherConfig,
    mut parser: MhinParser,
    interactive: bool,
) -> Result<()> {
    let mut progress = None;

    if interactive {
        let (reporter, handle) = ProgressReporter::start(fetcher_config.clone()).await?;
        parser.attach_progress(handle);
        progress = Some(reporter);
    }

    let mut runner = Runner::new(fetcher_config, parser);
    let run_result = runner.run_until_ctrl_c().await;

    if let Some(reporter) = progress {
        reporter.stop().await;
    }

    run_result
}

fn announce_configuration(app_config: &AppConfig, interactive: bool) {
    let emit = |line: String| {
        if interactive {
            println!("{line}");
        } else {
            tracing::info!("{line}");
        }
    };

    emit("Starting My Hash Is Nice parser.".to_string());
    if let Some(config_file) = &app_config.config_file {
        emit(format!("Config file: {}", config_file.display()));
    } else {
        emit("Config file: none (using defaults)".to_string());
    }
    emit(format!("UTXO db: {}", utxo_endpoint(app_config)));

    let stats_db = app_config.data_dir.join(DB_FILE_NAME);
    emit(format!("Stats db: {}", stats_db.display()));
    emit(String::new());
}

fn utxo_endpoint(app_config: &AppConfig) -> String {
    let store_config = &app_config.rollblock.store_config;
    if !store_config.enable_server {
        return format!(
            "embedded rollblock server disabled (data dir: {})",
            store_config.data_dir.display()
        );
    }

    match &store_config.remote_server {
        Some(settings) => {
            let scheme = if settings.tls.is_some() {
                "https"
            } else {
                "http"
            };
            format!(
                "{scheme}://{}:{}@{}",
                settings.auth.username, settings.auth.password, settings.bind_address
            )
        }
        None => "embedded rollblock server settings unavailable".to_string(),
    }
}

fn determine_launch(cli: &Cli) -> LaunchMode {
    if cli.daemon_child {
        LaunchMode::DaemonChild
    } else if cli.daemon {
        LaunchMode::DaemonParent
    } else {
        LaunchMode::Foreground
    }
}

fn init_tracing(log_path: Option<&Path>) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let filter = filter
        .add_directive(Directive::from_str("protoblock=warn").expect("valid protoblock directive"))
        .add_directive(Directive::from_str("rollblock=warn").expect("valid rollblock directive"));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true);

    let init_result = if let Some(path) = log_path {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        let (writer, guard) = tracing_appender::non_blocking(file);
        let _ = LOG_GUARD.set(guard);
        builder.with_writer(writer).try_init()
    } else {
        builder.try_init()
    };

    if init_result.is_err() {
        // The global subscriber was already installed elsewhere (tests, etc.); ignore.
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum LaunchMode {
    Foreground,
    DaemonParent,
    DaemonChild,
}

struct RunOptions {
    interactive: bool,
    log_path: Option<PathBuf>,
    pid_file: Option<PathBuf>,
}

impl RunOptions {
    fn foreground() -> Self {
        Self {
            interactive: true,
            log_path: None,
            pid_file: None,
        }
    }

    fn daemon_child(paths: &RuntimePaths) -> Self {
        Self {
            interactive: false,
            log_path: Some(paths.log_file().to_path_buf()),
            pid_file: Some(paths.pid_file().to_path_buf()),
        }
    }
}

struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn spawn_daemon(app_config: &AppConfig) -> Result<()> {
    use libc::pid_t;

    let pid_path = app_config.runtime.pid_file();
    ensure_pid_slot(pid_path)?;

    let exec_path = env::current_exe().context("failed to resolve current executable path")?;
    let daemon_child_flag = OsString::from("--daemon-child");
    let mut child_args: Vec<OsString> = env::args_os()
        .skip(1)
        .filter(|arg| arg != &daemon_child_flag)
        .collect();
    child_args.push(daemon_child_flag);

    let mut command = ProcessCommand::new(exec_path);
    command.args(child_args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    let child = command.spawn().context("failed to spawn daemon child")?;
    write_pid_file(pid_path, child.id() as pid_t)?;

    println!("Starting mhinparser in daemon mode (pid {}).", child.id());
    println!("Logs → {}", app_config.runtime.log_file().display());
    println!("PID file → {}", pid_path.display());

    Ok(())
}

#[cfg(not(unix))]
fn spawn_daemon(_app_config: &AppConfig) -> Result<()> {
    bail!("Daemon mode is only supported on Unix-like systems");
}

#[cfg(unix)]
fn stop_daemon(runtime_paths: RuntimePaths) -> Result<()> {
    use libc::{SIGINT, SIGTERM};

    let pid_path = runtime_paths.pid_file();
    let pid = read_pid_file(pid_path)?.ok_or_else(|| {
        anyhow!(
            "No running daemon found (missing PID file at {})",
            pid_path.display()
        )
    })?;

    if !process_alive(pid) {
        cleanup_pid_file(pid_path);
        bail!("Found stale PID file referencing pid {pid}");
    }

    send_signal(pid, SIGINT).context("failed to send SIGINT to daemon")?;
    if wait_for_shutdown(pid, pid_path, STOP_SIGINT_TIMEOUT) {
        cleanup_pid_file(pid_path);
        println!("Sent SIGINT to daemon (pid {}).", pid);
        return Ok(());
    }

    println!("Daemon did not exit after SIGINT; sending SIGTERM.");
    send_signal(pid, SIGTERM).context("failed to send SIGTERM to daemon")?;
    if wait_for_shutdown(pid, pid_path, STOP_SIGTERM_TIMEOUT) {
        cleanup_pid_file(pid_path);
        println!("Daemon stopped after SIGTERM.");
        return Ok(());
    }

    bail!("Daemon did not exit after SIGINT/SIGTERM");
}

#[cfg(not(unix))]
fn stop_daemon(_runtime_paths: RuntimePaths) -> Result<()> {
    bail!("Stopping the daemon is only supported on Unix-like systems");
}

#[cfg(unix)]
fn ensure_pid_slot(pid_path: &Path) -> Result<()> {
    if let Some(pid) = read_pid_file(pid_path)? {
        if process_alive(pid) {
            bail!("mhinparser already running (pid {pid})");
        }
        cleanup_pid_file(pid_path);
    }
    Ok(())
}

#[cfg(unix)]
fn write_pid_file(pid_path: &Path, pid: libc::pid_t) -> Result<()> {
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create pid directory {}", parent.display()))?;
    }
    fs::write(pid_path, pid.to_string()).with_context(|| {
        format!(
            "failed to write pid file {} for pid {pid}",
            pid_path.display()
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn read_pid_file(pid_path: &Path) -> Result<Option<libc::pid_t>> {
    if !pid_path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(pid_path)
        .with_context(|| format!("failed to read pid file {}", pid_path.display()))?;
    let pid: libc::pid_t = raw
        .trim()
        .parse::<i64>()
        .with_context(|| format!("invalid pid in {}", pid_path.display()))?
        .try_into()
        .map_err(|_| anyhow!("pid value does not fit pid_t"))?;
    Ok(Some(pid))
}

#[cfg(unix)]
fn send_signal(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    unsafe {
        if libc::kill(pid, signal) != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to send signal {signal} to pid {pid}"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn process_alive(pid: libc::pid_t) -> bool {
    unsafe {
        match libc::kill(pid, 0) {
            0 => true,
            _ => {
                let err = std::io::Error::last_os_error();
                err.raw_os_error().is_some_and(|code| code != libc::ESRCH)
            }
        }
    }
}

#[cfg(unix)]
fn wait_for_shutdown(pid: libc::pid_t, pid_path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) || !pid_path.exists() {
            return true;
        }
        thread::sleep(WAIT_POLL_INTERVAL);
    }
    false
}

#[cfg(unix)]
fn cleanup_pid_file(pid_path: &Path) {
    if pid_path.exists() {
        let _ = fs::remove_file(pid_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ProtoblockOptions, RollblockOptions};
    use rollblock::{RemoteServerSettings, StoreConfig};
    use std::net::SocketAddr;
    use tempfile::TempDir;

    fn build_cli(daemon: bool, daemon_child: bool) -> Cli {
        Cli {
            config: None,
            network: None,
            data_dir: None,
            protoblock: ProtoblockOptions::default(),
            rollblock: RollblockOptions::default(),
            daemon,
            daemon_child,
            command: None,
        }
    }

    fn build_app_config(data_dir: &Path, store_config: StoreConfig) -> AppConfig {
        let protoblock = ProtoblockOptions::default()
            .build(0)
            .expect("default protoblock settings");
        let runtime = RuntimePaths::prepare(data_dir).expect("runtime paths");
        let mut rollblock = RollblockOptions::default()
            .build(data_dir)
            .expect("rollblock settings");
        rollblock.store_config = store_config;

        AppConfig {
            config_file: None,
            data_dir: data_dir.to_path_buf(),
            network: mhinprotocol::MhinNetwork::Mainnet,
            protoblock,
            rollblock,
            runtime,
        }
    }

    #[test]
    fn determine_launch_prefers_daemon_child_flag() {
        let cli = build_cli(true, true);
        assert!(matches!(determine_launch(&cli), LaunchMode::DaemonChild));
    }

    #[test]
    fn determine_launch_uses_daemon_when_child_absent() {
        let cli = build_cli(true, false);
        assert!(matches!(determine_launch(&cli), LaunchMode::DaemonParent));
    }

    #[test]
    fn determine_launch_defaults_to_foreground() {
        let cli = build_cli(false, false);
        assert!(matches!(determine_launch(&cli), LaunchMode::Foreground));
    }

    #[test]
    fn utxo_endpoint_reports_disabled_server() {
        let temp = TempDir::new().expect("temp dir");
        let mut store_config = StoreConfig::existing(temp.path());
        store_config.enable_server = false;
        store_config.remote_server = None;

        let app_config = build_app_config(temp.path(), store_config);
        let endpoint = utxo_endpoint(&app_config);

        assert!(
            endpoint.starts_with("embedded rollblock server disabled"),
            "should mention disabled embedded server"
        );
    }

    #[test]
    fn utxo_endpoint_formats_remote_settings() {
        let temp = TempDir::new().expect("temp dir");
        let mut store_config = StoreConfig::existing(temp.path());
        let mut remote = RemoteServerSettings::default().with_basic_auth("user", "pass");
        remote.bind_address = "127.0.0.1:9000"
            .parse::<SocketAddr>()
            .expect("parse socket address");
        store_config.enable_server = true;
        store_config.remote_server = Some(remote);

        let app_config = build_app_config(temp.path(), store_config);
        let endpoint = utxo_endpoint(&app_config);

        assert_eq!(endpoint, "http://user:pass@127.0.0.1:9000");
    }

    #[test]
    fn utxo_endpoint_formats_none_remote_settings() {
        let temp = TempDir::new().expect("temp dir");
        let mut store_config = StoreConfig::existing(temp.path());
        store_config.enable_server = true;
        store_config.remote_server = None;

        let app_config = build_app_config(temp.path(), store_config);
        let endpoint = utxo_endpoint(&app_config);

        assert_eq!(endpoint, "embedded rollblock server settings unavailable");
    }

    #[test]
    fn run_options_foreground_is_interactive() {
        let opts = RunOptions::foreground();
        assert!(opts.interactive);
        assert!(opts.log_path.is_none());
        assert!(opts.pid_file.is_none());
    }

    #[test]
    fn run_options_daemon_child_uses_paths() {
        let temp = TempDir::new().expect("temp dir");
        let paths = RuntimePaths::prepare(temp.path()).expect("runtime paths");

        let opts = RunOptions::daemon_child(&paths);
        assert!(!opts.interactive);
        assert!(opts.log_path.is_some());
        assert!(opts.pid_file.is_some());

        assert_eq!(opts.log_path.as_ref().unwrap(), paths.log_file());
        assert_eq!(opts.pid_file.as_ref().unwrap(), paths.pid_file());
    }

    #[test]
    fn pid_file_guard_removes_file_on_drop() {
        let temp = TempDir::new().expect("temp dir");
        let pid_path = temp.path().join("test.pid");
        fs::write(&pid_path, "12345").expect("write pid");
        assert!(pid_path.exists());

        {
            let _guard = PidFileGuard::new(pid_path.clone());
        }

        assert!(!pid_path.exists(), "pid file should be removed on drop");
    }

    #[test]
    fn init_tracing_returns_ok_without_path() {
        // Running this test in isolation should succeed
        // Note: tracing subscriber can only be initialized once per process,
        // so this may fail if another test already initialized it.
        // We just check it doesn't panic.
        let result = init_tracing(None);
        assert!(result.is_ok());
    }

    #[test]
    fn init_tracing_with_log_path_creates_file() {
        let temp = TempDir::new().expect("temp dir");
        let log_path = temp.path().join("test.log");

        let result = init_tracing(Some(&log_path));
        assert!(result.is_ok());
        assert!(log_path.exists(), "log file should be created");
    }

    #[cfg(unix)]
    #[test]
    fn process_alive_returns_false_for_invalid_pid() {
        // PID 0 is never valid for kill()
        // A very high PID is unlikely to exist
        assert!(!process_alive(999999999));
    }

    #[cfg(unix)]
    #[test]
    fn read_pid_file_returns_none_for_missing_file() {
        let temp = TempDir::new().expect("temp dir");
        let missing_path = temp.path().join("nonexistent.pid");

        let result = read_pid_file(&missing_path).expect("should not error");
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_pid_file_parses_valid_pid() {
        let temp = TempDir::new().expect("temp dir");
        let pid_path = temp.path().join("valid.pid");
        fs::write(&pid_path, "12345").expect("write pid");

        let result = read_pid_file(&pid_path).expect("should parse");
        assert_eq!(result, Some(12345));
    }

    #[cfg(unix)]
    #[test]
    fn write_pid_file_creates_parent_dirs() {
        let temp = TempDir::new().expect("temp dir");
        let nested_path = temp.path().join("nested").join("dir").join("test.pid");

        write_pid_file(&nested_path, 99999).expect("should create");
        assert!(nested_path.exists());

        let contents = fs::read_to_string(&nested_path).expect("read");
        assert_eq!(contents, "99999");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_pid_file_removes_existing_file() {
        let temp = TempDir::new().expect("temp dir");
        let pid_path = temp.path().join("cleanup.pid");
        fs::write(&pid_path, "12345").expect("write pid");
        assert!(pid_path.exists());

        cleanup_pid_file(&pid_path);
        assert!(!pid_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_pid_file_handles_missing_file() {
        let temp = TempDir::new().expect("temp dir");
        let missing_path = temp.path().join("missing.pid");

        // Should not panic or error
        cleanup_pid_file(&missing_path);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_pid_slot_allows_new_daemon_when_no_file() {
        let temp = TempDir::new().expect("temp dir");
        let pid_path = temp.path().join("new.pid");

        let result = ensure_pid_slot(&pid_path);
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_pid_slot_cleans_stale_pid_file() {
        let temp = TempDir::new().expect("temp dir");
        let pid_path = temp.path().join("stale.pid");
        // Write a PID that definitely doesn't exist
        fs::write(&pid_path, "999999999").expect("write stale pid");
        assert!(pid_path.exists());

        let result = ensure_pid_slot(&pid_path);
        assert!(result.is_ok());
        // Stale file should be cleaned up
        assert!(!pid_path.exists());
    }

    #[test]
    fn utxo_endpoint_formats_remote_settings_with_tls() {
        let temp = TempDir::new().expect("temp dir");
        let mut store_config = StoreConfig::existing(temp.path());
        let mut remote = RemoteServerSettings::default().with_basic_auth("user", "pass");
        remote.bind_address = "127.0.0.1:9000"
            .parse::<SocketAddr>()
            .expect("parse socket address");
        // Simulate TLS enabled by creating a dummy TlsConfig through the builder
        remote = remote.with_tls("cert.pem", "key.pem");
        store_config.enable_server = true;
        store_config.remote_server = Some(remote);

        let app_config = build_app_config(temp.path(), store_config);
        let endpoint = utxo_endpoint(&app_config);

        assert_eq!(endpoint, "https://user:pass@127.0.0.1:9000");
    }

    #[test]
    fn launch_mode_variants_are_distinct() {
        let foreground = LaunchMode::Foreground;
        let daemon_parent = LaunchMode::DaemonParent;
        let daemon_child = LaunchMode::DaemonChild;

        // Just verify the variants exist and can be matched
        assert!(matches!(foreground, LaunchMode::Foreground));
        assert!(matches!(daemon_parent, LaunchMode::DaemonParent));
        assert!(matches!(daemon_child, LaunchMode::DaemonChild));
    }

    #[test]
    fn run_options_fields_are_accessible() {
        let opts = RunOptions::foreground();
        assert!(opts.interactive);
        assert!(opts.log_path.is_none());
        assert!(opts.pid_file.is_none());
    }

    #[test]
    fn pid_file_guard_new_creates_guard() {
        let temp = TempDir::new().expect("temp dir");
        let pid_path = temp.path().join("guard_test.pid");
        fs::write(&pid_path, "12345").expect("write pid");

        let guard = PidFileGuard::new(pid_path.clone());
        assert!(pid_path.exists());

        drop(guard);
        assert!(!pid_path.exists(), "file should be removed on drop");
    }

    #[test]
    fn determine_launch_all_combinations() {
        // Test all combinations
        let cli_ff = build_cli(false, false);
        let cli_tf = build_cli(true, false);
        let cli_ft = build_cli(false, true);
        let cli_tt = build_cli(true, true);

        assert!(matches!(determine_launch(&cli_ff), LaunchMode::Foreground));
        assert!(matches!(
            determine_launch(&cli_tf),
            LaunchMode::DaemonParent
        ));
        assert!(matches!(determine_launch(&cli_ft), LaunchMode::DaemonChild));
        assert!(matches!(determine_launch(&cli_tt), LaunchMode::DaemonChild));
    }
}
