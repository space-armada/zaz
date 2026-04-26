//! zaz - A modern file-watching task runner and process manager.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::sync::mpsc;
use zaz_config::{load_user_config, TuiStylePreference, UserConfig};
use zaz_daemon::{
    resolve_socket, socket_path_for_config, ApiRequest, ApiResponse, Client, Engine, EngineCommand,
    Server,
};

#[derive(Parser)]
#[command(name = "zaz")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Configuration file path
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Socket path for daemon communication
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// Use full TUI style (split panes with group tree)
    #[arg(long, conflicts_with = "multi_pane")]
    full: bool,

    /// Use multi-pane TUI style (one pane per task)
    #[arg(long, conflicts_with = "full")]
    multi_pane: bool,

    /// Don't auto-start a daemon before opening the TUI
    #[arg(long)]
    no_autostart: bool,

    /// Stop the connected daemon when the TUI exits
    #[arg(long)]
    stop_on_exit: bool,

    /// Write debug logs to a file (works in both TUI and daemon modes)
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

// CLI exit policy:
//
// - Query commands report state. `status` exits 0 when the daemon is running,
//   exits 3 for "not running" per the LSB/systemctl convention, and exits 1
//   for operational errors.
// - Strict-mutating commands perform an action that requires a running daemon.
//   `restart` and `reload` exit 1 when no daemon is running or when the daemon
//   API returns an error.
// - Idempotent-mutating commands ensure a postcondition. `stop` exits 0 when
//   the daemon is already stopped, and exits 1 for API/operational errors.
//
// New CLI commands must declare which category they belong to before
// implementation so their exit behavior stays predictable in scripts.
#[derive(Subcommand)]
enum Commands {
    /// Run task commands once and exit
    Task,

    /// Run the daemon in the foreground
    Daemon {
        /// Suppress process output logging
        #[arg(short, long)]
        quiet: bool,
    },

    /// Show status of running daemon
    Status,

    /// Restart a group or all groups
    Restart {
        /// Group name to restart (omit for all)
        group: Option<String>,
    },

    /// Stop the running daemon
    Stop,

    /// Reload configuration (requires running daemon)
    Reload,

    /// Validate configuration file without starting daemon
    Check {
        /// Configuration file to check (defaults to zaz.toml or zaz.json)
        #[arg(value_name = "FILE")]
        config: Option<PathBuf>,

        /// Output as JSON for tooling integration
        #[arg(long)]
        json: bool,
    },

    /// Show default ignore patterns
    Ignores,
}

/// Effective TUI options after merging CLI flags with user config.
#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub style: TuiStylePreference,
    pub no_autostart: bool,
    pub stop_on_exit: bool,
    pub disable_animations: bool,
}

impl TuiOptions {
    /// Create TUI options by merging CLI flags with user config.
    /// CLI flags take precedence over user config.
    fn from_cli_and_user_config(cli: &Cli, user_config: &UserConfig) -> Self {
        let style = if cli.full {
            TuiStylePreference::Full
        } else if cli.multi_pane {
            TuiStylePreference::MultiPane
        } else {
            user_config.tui_style.unwrap_or(TuiStylePreference::Full)
        };

        Self {
            style,
            no_autostart: cli.no_autostart || user_config.no_autostart,
            stop_on_exit: cli.stop_on_exit,
            disable_animations: user_config.disable_animations,
        }
    }
}

/// Initialize tracing with optional file logging.
///
/// Returns a guard that must be kept alive for the duration of the program
/// when file logging is enabled (to ensure logs are flushed).
fn init_tracing(
    debug: bool,
    is_tui_mode: bool,
    log_file: Option<&Path>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;

    let filter = if debug { "debug,globset=info" } else { "info" };
    let env_filter = tracing_subscriber::EnvFilter::new(filter);

    match (is_tui_mode, log_file) {
        // TUI mode with file logging: log to file only
        (true, Some(path)) => {
            let file = std::fs::File::create(path).expect("Failed to create log file");
            let (non_blocking, guard) = tracing_appender::non_blocking(file);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(non_blocking)
                .with_ansi(false)
                .init();
            Some(guard)
        }
        // Non-TUI mode with file logging: log to both console and file
        (false, Some(path)) => {
            let file = std::fs::File::create(path).expect("Failed to create log file");
            let (non_blocking, guard) = tracing_appender::non_blocking(file);

            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false);

            let console_layer = tracing_subscriber::fmt::layer().with_target(false);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(console_layer)
                .with(file_layer)
                .init();
            Some(guard)
        }
        // Non-TUI mode without file logging: console only
        (false, None) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(false)
                .init();
            None
        }
        // Otherwise, no logging
        (true, None) => None,
    }
}

#[tokio::main]
async fn main() {
    let exit_code = match try_main().await {
        Ok(()) => 0,
        Err(err) if err.downcast_ref::<StatusNotRunning>().is_some() => 3,
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    };

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

async fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let current_dir = std::env::current_dir()?;

    // Determine if we're running in TUI mode (default command with no subcommand)
    let is_tui_mode = cli.command.is_none();
    let _log_guard = init_tracing(cli.debug, is_tui_mode, cli.log_file.as_deref());

    match cli.command {
        Some(Commands::Task) => {
            let config_path = find_config(&cli.config)?;
            run_tasks(&config_path).await
        }
        Some(Commands::Daemon { quiet }) => {
            let config_path = find_config(&cli.config)?;
            let socket_path =
                resolve_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            run_daemon(&config_path, &socket_path, quiet).await
        }
        Some(Commands::Status) => {
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            show_status(&socket_path).await
        }
        Some(Commands::Restart { group }) => {
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            restart(&socket_path, group).await
        }
        Some(Commands::Stop) => {
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            stop_daemon(&socket_path).await
        }
        Some(Commands::Reload) => {
            let socket_path =
                resolve_control_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            reload_config(&socket_path).await
        }
        Some(Commands::Check { config, json }) => {
            let config_path = find_config(&config.or(cli.config.clone()))?;
            check_config(&config_path, json)
        }
        Some(Commands::Ignores) => show_ignores(),
        None => {
            let config_path = find_config(&cli.config)?;
            let socket_path =
                resolve_command_socket(&cli.config, cli.socket.clone(), &current_dir)?;
            let user_config = load_user_config();
            let tui_options = TuiOptions::from_cli_and_user_config(&cli, &user_config);
            run_tui(&config_path, &socket_path, &tui_options).await
        }
    }
}

#[derive(Debug)]
struct StatusNotRunning;

impl fmt::Display for StatusNotRunning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("daemon not running")
    }
}

impl std::error::Error for StatusNotRunning {}

/// Find the configuration file.
fn find_config(explicit: &Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.clone());
        }
        anyhow::bail!("config file not found: {}", path.display());
    }

    // Try to discover config file
    match zaz_config::discover() {
        Ok((path, _)) => Ok(path),
        Err(e) => anyhow::bail!("no config file found: {}", e),
    }
}

fn resolve_control_command_socket(
    explicit_config: &Option<PathBuf>,
    explicit_socket: Option<PathBuf>,
    start_dir: &Path,
) -> Result<PathBuf> {
    resolve_command_socket(explicit_config, explicit_socket, start_dir)
}

fn resolve_command_socket(
    explicit_config: &Option<PathBuf>,
    explicit_socket: Option<PathBuf>,
    start_dir: &Path,
) -> Result<PathBuf> {
    if let Some(socket_path) = explicit_socket {
        return Ok(socket_path);
    }

    if explicit_config.is_some() {
        let config_path = find_config(explicit_config)?;
        return Ok(socket_path_for_config(&config_path));
    }

    Ok(resolve_socket(None, start_dir)?)
}

async fn run_tasks(config_path: &Path) -> Result<()> {
    tracing::info!(config = %config_path.display(), "running task commands");

    let mut engine = Engine::new_task_only(config_path)?;
    engine.startup().await?;
    let success = engine.wait_for_tasks().await;

    // Shutdown still performs normal cleanup, even though task-only mode never starts daemons.
    engine.shutdown().await?;

    if !success {
        bail!("one or more tasks failed");
    }

    tracing::info!("all tasks completed");
    Ok(())
}

async fn run_daemon(config_path: &Path, socket_path: &Path, quiet: bool) -> Result<()> {
    // Check if a daemon is already running by trying to connect and send a request
    let check_timeout = Duration::from_secs(1);
    match tokio::time::timeout(check_timeout, Client::connect(socket_path)).await {
        Ok(Ok(mut client)) => {
            // Try to actually communicate with the daemon
            match tokio::time::timeout(check_timeout, client.request(&ApiRequest::Status)).await {
                Ok(Ok(_)) => {
                    anyhow::bail!(
                        "daemon already running (socket {} is active)",
                        socket_path.display()
                    );
                }
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "stale socket (request failed), will replace");
                }
                Err(_) => {
                    tracing::debug!("stale socket (request timed out), will replace");
                }
            }
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "no existing daemon (connect failed)");
        }
        Err(_) => {
            tracing::debug!("no existing daemon (connect timed out)");
        }
    }

    tracing::info!(config = %config_path.display(), "starting daemon");

    let mut engine = Engine::with_options(config_path, !quiet)?;
    let (command_tx, mut command_rx) = mpsc::channel::<EngineCommand>(32);

    // Start API server
    let server = Server::bind(socket_path, command_tx).await?;
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            tracing::error!(error = %e, "server error");
        }
    });

    // Run initial startup, but don't exit on failure
    if let Err(e) = engine.startup().await {
        tracing::error!(error = %e, "initial startup failed, waiting for file changes to retry");
    }

    tracing::info!("watching for file changes...");
    let mut shutdown_requested = false;
    loop {
        tokio::select! {
            // Handle Ctrl+C
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal");
                break;
            }

            // Handle API commands
            Some(cmd) = command_rx.recv() => {
                // Drain log channel before handling request (ensures GetLogs returns fresh data)
                engine.process_incoming_logs();

                let is_shutdown = matches!(cmd.request, ApiRequest::Shutdown);
                let response = engine.handle_request(cmd.request).await;
                let _ = cmd.response_tx.send(response);

                if is_shutdown {
                    shutdown_requested = true;
                    break;
                }
            }

            // Poll for file changes and check daemons
            _ = async {
                // Process incoming logs from PTY readers
                engine.process_incoming_logs();

                if let Err(e) = engine.poll().await {
                    tracing::error!(error = %e, "poll error");
                }
                if let Err(e) = engine.check_daemons().await {
                    tracing::error!(error = %e, "daemon check error");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            } => {}
        }
    }

    // Cleanup
    server_handle.abort();
    engine.shutdown().await?;

    if shutdown_requested {
        tracing::info!("shutdown complete");
    }

    Ok(())
}

async fn show_status(socket_path: &Path) -> Result<()> {
    let mut client = match Client::connect(socket_path).await {
        Ok(c) => c,
        Err(_) => {
            println!(
                "No daemon running (could not connect to {})",
                socket_path.display()
            );
            return Err(StatusNotRunning.into());
        }
    };

    let response = client.request(&ApiRequest::Status).await?;
    match response {
        ApiResponse::Status { state } => {
            println!("Daemon Status: {:?}", state.status);
            println!("Groups:");
            for (name, group) in &state.groups {
                println!("  {} ({:?})", name, group.status);
                for task in &group.tasks {
                    let duration = task
                        .duration_ms
                        .map(|d| format!(" ({}ms)", d))
                        .unwrap_or_default();
                    println!("    [task] {} - {:?}{}", task.name, task.status, duration);
                }
                for daemon in &group.daemons {
                    let pid = daemon
                        .pid
                        .map(|p| format!(" (pid {})", p))
                        .unwrap_or_default();
                    println!("    [daemon] {} - {:?}{}", daemon.name, daemon.status, pid);
                }
            }
            if let Some(ts) = state.last_change {
                println!("Last change: {} ms ago", now_ms() - ts);
            }
        }
        ApiResponse::Error { message } => {
            bail!("status request failed: {}", message);
        }
        _ => {
            bail!("status request returned unexpected response");
        }
    }

    Ok(())
}

async fn restart(socket_path: &Path, group: Option<String>) -> Result<()> {
    let mut client = match Client::connect(socket_path).await {
        Ok(c) => c,
        Err(_) => {
            anyhow::bail!(
                "No daemon running (could not connect to {})",
                socket_path.display()
            );
        }
    };

    let request = match group {
        Some(name) => ApiRequest::RestartGroup { name },
        None => ApiRequest::RestartAll,
    };

    let response = client.request(&request).await?;
    match response {
        ApiResponse::Ok { message } => {
            println!("{}", message.unwrap_or_else(|| "OK".to_string()));
        }
        ApiResponse::Error { message } => {
            anyhow::bail!("Error: {}", message);
        }
        _ => {
            anyhow::bail!("Unexpected response");
        }
    }

    Ok(())
}

async fn stop_daemon(socket_path: &Path) -> Result<()> {
    let mut client = match Client::connect(socket_path).await {
        Ok(c) => c,
        Err(_) => {
            println!("No daemon running");
            return Ok(());
        }
    };

    let response = client.request(&ApiRequest::Shutdown).await?;
    match response {
        ApiResponse::Ok { message } => {
            println!(
                "{}",
                message.unwrap_or_else(|| "Shutdown initiated".to_string())
            );
        }
        ApiResponse::Error { message } => {
            anyhow::bail!("Error: {}", message);
        }
        _ => {
            anyhow::bail!("Unexpected response");
        }
    }

    Ok(())
}

async fn reload_config(socket_path: &Path) -> Result<()> {
    let mut client = match Client::connect(socket_path).await {
        Ok(c) => c,
        Err(_) => {
            anyhow::bail!(
                "No daemon running (could not connect to {})",
                socket_path.display()
            );
        }
    };

    let response = client.request(&ApiRequest::ReloadConfig).await?;
    match response {
        ApiResponse::Ok { message } => {
            println!(
                "{}",
                message.unwrap_or_else(|| "Configuration reloaded".to_string())
            );
        }
        ApiResponse::Error { message } => {
            anyhow::bail!("Error: {}", message);
        }
        _ => {
            anyhow::bail!("Unexpected response");
        }
    }

    Ok(())
}

fn check_config(config_path: &Path, json_output: bool) -> Result<()> {
    use serde::Serialize;
    use zaz_config::{ConfigError, ValidationErrors};

    #[derive(Serialize)]
    struct CheckResult {
        valid: bool,
        path: String,
        errors: Vec<CheckError>,
    }

    #[derive(Serialize)]
    struct CheckError {
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<usize>,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
        code: String,
    }

    // Helper to convert validation errors to CheckErrors
    fn validation_errors_to_check_errors(errors: &ValidationErrors) -> Vec<CheckError> {
        errors
            .iter()
            .map(|e| CheckError {
                line: e.span.as_ref().map(|s| s.line),
                column: e.span.as_ref().map(|s| s.column),
                message: e.kind.to_string(),
                hint: e.hint.clone(),
                code: e.code().to_string(),
            })
            .collect()
    }

    fn check_failed(message: &str) -> Result<()> {
        anyhow::bail!(message.to_string())
    }

    let result = zaz_config::load(config_path);
    match result {
        Ok(_config) => {
            if json_output {
                let result = CheckResult {
                    valid: true,
                    path: config_path.display().to_string(),
                    errors: vec![],
                };

                println!("{}", serde_json::to_string(&result)?);
                return Ok(());
            }

            use yansi::Paint;
            println!("{}: {}", config_path.display(), "OK".green().bold());
            Ok(())
        }

        Err(ConfigError::Validation(ref validation_errors)) => {
            use yansi::Paint;

            let error_count = validation_errors.len();

            if json_output {
                let result = CheckResult {
                    valid: false,
                    path: config_path.display().to_string(),
                    errors: validation_errors_to_check_errors(validation_errors),
                };

                println!("{}", serde_json::to_string(&result)?);
                return check_failed("configuration validation failed");
            }

            // Pretty print each error with colors
            for error in validation_errors.iter() {
                // Format: "path:line:column: error: message" or "path: error: message"
                if let Some(span) = &error.span {
                    eprint!(
                        "{}:{}:{}: ",
                        config_path.display().bold(),
                        span.line.cyan(),
                        span.column.cyan()
                    );
                } else {
                    eprint!("{}: ", config_path.display().bold());
                }
                eprintln!("{}: {}", "error".red().bold(), error.kind);

                if let Some(hint) = &error.hint {
                    eprintln!("               {}: {}", "hint".cyan().bold(), hint);
                }
                eprintln!();
            }

            let plural = if error_count == 1 { "error" } else { "errors" };
            eprintln!(
                "{} {} {} in {}",
                "Found".red().bold(),
                error_count.red().bold(),
                plural.red().bold(),
                config_path.display().bold()
            );
            check_failed("configuration validation failed")
        }

        Err(e) => {
            use yansi::Paint;

            // Non-validation errors (parse errors, IO errors, etc.)
            if json_output {
                let result = CheckResult {
                    valid: false,
                    path: config_path.display().to_string(),
                    errors: vec![CheckError {
                        line: None,
                        column: None,
                        message: e.to_string(),
                        hint: None,
                        code: "parse_error".to_string(),
                    }],
                };

                println!("{}", serde_json::to_string(&result)?);
                return check_failed("configuration parse failed");
            }

            eprintln!(
                "{}: {}: {}",
                config_path.display().bold(),
                "error".red().bold(),
                e
            );
            eprintln!(
                "\n{} in {}",
                "Found 1 error".red().bold(),
                config_path.display().bold()
            );
            check_failed("configuration parse failed")
        }
    }
}

fn show_ignores() -> Result<()> {
    println!("Default ignore patterns:");
    for pattern in zaz_watch::default_ignores() {
        println!("  {}", pattern);
    }

    Ok(())
}

async fn is_daemon_responsive(socket_path: &Path, timeout: Duration) -> bool {
    match tokio::time::timeout(timeout, Client::connect(socket_path)).await {
        Ok(Ok(mut client)) => {
            matches!(
                tokio::time::timeout(timeout, client.request(&ApiRequest::Status)).await,
                Ok(Ok(_))
            )
        }
        _ => false,
    }
}

fn resolve_autostart_executable() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_zaz") {
        return Ok(PathBuf::from(path));
    }

    let current_exe = std::env::current_exe()?;
    if let Some(deps_dir) = current_exe.parent() {
        if deps_dir.file_name().and_then(|name| name.to_str()) == Some("deps") {
            if let Some(target_dir) = deps_dir.parent() {
                let candidate = target_dir.join(env!("CARGO_PKG_NAME"));
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    Ok(current_exe)
}

fn start_daemon_subprocess(config_path: &Path, socket_path: &Path) -> Result<()> {
    let current_exe = resolve_autostart_executable()?;

    Command::new(current_exe)
        .args([
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("config path is not valid utf-8"))?,
            "--socket",
            socket_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("socket path is not valid utf-8"))?,
            "daemon",
            "--quiet",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(())
}

async fn wait_for_daemon_ready(socket_path: &Path, attempts: usize, delay: Duration) -> bool {
    for _ in 0..attempts {
        tokio::time::sleep(delay).await;
        if is_daemon_responsive(socket_path, Duration::from_secs(1)).await {
            return true;
        }
    }

    false
}

async fn run_tui(config_path: &Path, socket_path: &Path, options: &TuiOptions) -> Result<()> {
    tracing::info!(
        config = %config_path.display(),
        style = ?options.style,
        "starting TUI"
    );

    let check_timeout = Duration::from_secs(1);
    let daemon_running = is_daemon_responsive(socket_path, check_timeout).await;
    if daemon_running {
        tracing::debug!("daemon is running");
    } else {
        tracing::debug!("no responsive daemon running");
    }

    if !daemon_running && !options.no_autostart {
        tracing::info!("starting daemon in background");

        match start_daemon_subprocess(config_path, socket_path) {
            Ok(()) => {
                let ready =
                    wait_for_daemon_ready(socket_path, 20, Duration::from_millis(100)).await;
                if !ready {
                    tracing::warn!("daemon may not be ready after 2s");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to auto-start daemon");
            }
        }
    }

    // Create TUI app
    use zaz_tui::{App, TuiStyle};
    let style = TuiStyle::from(options.style);
    let user_config = zaz_config::UserConfig {
        no_autostart: options.no_autostart,
        disable_animations: options.disable_animations,
        tui_style: Some(options.style),
        log_colors: zaz_config::LogColorConfig::default(),
        notifications: zaz_config::NotificationConfig::default(),
        log_storage: zaz_config::LogStorageConfig::default(),
    };

    let config_name = config_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "zaz.toml".to_string());

    let mut app = App::new(style, user_config, config_name);
    app.stop_on_exit = options.stop_on_exit;

    // Connect to daemon
    if let Err(e) = app.connect(socket_path).await {
        tracing::warn!(error = %e, "failed to connect to daemon");
    }

    // Run TUI
    app.run()?;

    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;
    use zaz_daemon::{socket_path_for_config, ApiRequest, ApiResponse, Client, DaemonError};

    fn parse_cli(args: &[&str]) -> Cli {
        Cli::parse_from(args)
    }

    #[test]
    fn tui_options_stop_on_exit_defaults_false() {
        let cli = parse_cli(&["zaz"]);
        let options = TuiOptions::from_cli_and_user_config(&cli, &UserConfig::default());

        assert!(!options.stop_on_exit);
        assert!(!options.no_autostart);
    }

    #[test]
    fn tui_options_include_cli_stop_on_exit_flag() {
        let cli = parse_cli(&["zaz", "--stop-on-exit"]);
        let options = TuiOptions::from_cli_and_user_config(&cli, &UserConfig::default());

        assert!(options.stop_on_exit);
    }

    #[test]
    fn control_command_socket_uses_explicit_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let explicit_socket = temp.path().join("explicit.sock");
        let resolved = resolve_control_command_socket(
            &Some(config_path),
            Some(explicit_socket.clone()),
            temp.path(),
        )?;

        assert_eq!(resolved, explicit_socket);
        Ok(())
    }

    #[test]
    fn command_socket_uses_explicit_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let explicit_socket = temp.path().join("explicit.sock");
        let resolved = resolve_command_socket(
            &Some(config_path),
            Some(explicit_socket.clone()),
            temp.path(),
        )?;

        assert_eq!(resolved, explicit_socket);
        Ok(())
    }

    #[test]
    fn command_socket_uses_explicit_config() -> Result<()> {
        let temp = TempDir::new()?;
        let project_dir = temp.path().join("project");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&project_dir)?;
        std::fs::create_dir_all(&elsewhere)?;

        let config_path = project_dir.join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let resolved = resolve_command_socket(&Some(config_path.clone()), None, &elsewhere)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn command_socket_discovers_project_from_start_dir() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let nested = temp.path().join("a/b/c");
        std::fs::create_dir_all(&nested)?;

        let resolved = resolve_command_socket(&None, None, &nested)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn command_socket_errors_outside_project_without_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside)?;

        let err = resolve_command_socket(&None, None, &outside).unwrap_err();
        let daemon_err = err.downcast_ref::<DaemonError>().expect("daemon error");

        match daemon_err {
            DaemonError::SocketResolution { start_dir } => {
                assert_eq!(start_dir, &PathBuf::from(&outside));
            }
            other => panic!("expected socket resolution error, got {:?}", other),
        }

        let message = err.to_string();
        assert!(message.contains("--socket <PATH>"));
        assert!(message.contains("zaz project directory"));
        Ok(())
    }

    #[test]
    fn control_command_socket_uses_explicit_config() -> Result<()> {
        let temp = TempDir::new()?;
        let project_dir = temp.path().join("project");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&project_dir)?;
        std::fs::create_dir_all(&elsewhere)?;

        let config_path = project_dir.join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let resolved =
            resolve_control_command_socket(&Some(config_path.clone()), None, &elsewhere)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn control_command_socket_discovers_project_from_start_dir() -> Result<()> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("zaz.toml");
        std::fs::write(&config_path, "")?;

        let nested = temp.path().join("a/b/c");
        std::fs::create_dir_all(&nested)?;

        let resolved = resolve_control_command_socket(&None, None, &nested)?;

        assert_eq!(resolved, socket_path_for_config(&config_path));
        Ok(())
    }

    #[test]
    fn control_command_socket_errors_outside_project_without_socket() -> Result<()> {
        let temp = TempDir::new()?;
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside)?;

        let err = resolve_control_command_socket(&None, None, &outside).unwrap_err();
        let daemon_err = err.downcast_ref::<DaemonError>().expect("daemon error");

        match daemon_err {
            DaemonError::SocketResolution { start_dir } => {
                assert_eq!(start_dir, &PathBuf::from(&outside));
            }
            other => panic!("expected socket resolution error, got {:?}", other),
        }

        let message = err.to_string();
        assert!(message.contains("--socket <PATH>"));
        assert!(message.contains("zaz project directory"));
        Ok(())
    }

    #[tokio::test]
    async fn daemon_responsive_reports_false_for_missing_socket() {
        let temp = TempDir::new().unwrap();
        let socket_path = temp.path().join("missing.sock");

        assert!(!is_daemon_responsive(&socket_path, Duration::from_millis(100)).await);
    }

    #[tokio::test]
    async fn start_daemon_subprocess_starts_reachable_daemon() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("zaz.toml");
        let socket_path = temp.path().join("daemon.sock");

        std::fs::write(
            &config_path,
            r#"
[[group]]
name = "backend"
patterns = ["**/*.rs"]

[[group.task]]
name = "noop"
command = "true"
"#,
        )
        .unwrap();

        start_daemon_subprocess(&config_path, &socket_path).unwrap();

        assert!(wait_for_daemon_ready(&socket_path, 50, Duration::from_millis(100)).await);
        assert!(is_daemon_responsive(&socket_path, Duration::from_secs(1)).await);

        let mut client = Client::connect(&socket_path).await.unwrap();
        let response = client.request(&ApiRequest::Shutdown).await.unwrap();
        assert!(matches!(response, ApiResponse::Ok { .. }));
    }
}
