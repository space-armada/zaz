//! zaz - A modern file-watching task runner and process manager.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use zaz_config::{load_user_config, TuiStylePreference, UserConfig};
use zaz_daemon::{
    default_socket_path, socket_path_for_config, ApiRequest, ApiResponse, Client, Engine,
    EngineCommand, Server,
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

    /// Don't auto-start daemon when running TUI
    #[arg(long)]
    no_autostart: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run task commands once and exit
    Task,

    /// Start the daemon in the background
    Daemon {
        /// Detach from terminal
        #[arg(short, long)]
        detach: bool,

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
            disable_animations: user_config.disable_animations,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Determine if we're running in TUI mode (default command with no subcommand)
    let is_tui_mode = cli.command.is_none();

    // Initialize logging - suppress for TUI mode to avoid corrupting display
    if !is_tui_mode {
        let filter = if cli.debug {
            "debug,globset=info"
        } else {
            "info"
        };
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    }

    // Helper to get socket path: CLI override, or config-specific path
    let get_socket_path = |config_path: &Path| -> PathBuf {
        cli.socket
            .clone()
            .unwrap_or_else(|| socket_path_for_config(config_path))
    };

    match cli.command {
        Some(Commands::Task) => {
            let config_path = find_config(&cli.config)?;
            run_tasks(&config_path).await
        }
        Some(Commands::Daemon { detach, quiet }) => {
            let config_path = find_config(&cli.config)?;
            let socket_path = get_socket_path(&config_path);
            run_daemon(&config_path, &socket_path, detach, quiet).await
        }
        Some(Commands::Status) => {
            // For status/restart/stop without config, try to find config for socket path
            let socket_path = if let Ok(config_path) = find_config(&cli.config) {
                get_socket_path(&config_path)
            } else {
                cli.socket.clone().unwrap_or_else(default_socket_path)
            };
            show_status(&socket_path).await
        }
        Some(Commands::Restart { group }) => {
            let socket_path = if let Ok(config_path) = find_config(&cli.config) {
                get_socket_path(&config_path)
            } else {
                cli.socket.clone().unwrap_or_else(default_socket_path)
            };
            restart(&socket_path, group).await
        }
        Some(Commands::Stop) => {
            let socket_path = if let Ok(config_path) = find_config(&cli.config) {
                get_socket_path(&config_path)
            } else {
                cli.socket.clone().unwrap_or_else(default_socket_path)
            };
            stop_daemon(&socket_path).await
        }
        Some(Commands::Reload) => {
            let socket_path = if let Ok(config_path) = find_config(&cli.config) {
                get_socket_path(&config_path)
            } else {
                cli.socket.clone().unwrap_or_else(default_socket_path)
            };
            reload_config(&socket_path).await
        }
        Some(Commands::Check { config, json }) => {
            let config_path = find_config(&config.or(cli.config.clone()))?;
            check_config(&config_path, json)
        }
        Some(Commands::Ignores) => show_ignores(),
        None => {
            let config_path = find_config(&cli.config)?;
            let socket_path = get_socket_path(&config_path);
            let user_config = load_user_config();
            let tui_options = TuiOptions::from_cli_and_user_config(&cli, &user_config);
            run_tui(&config_path, &socket_path, &tui_options).await
        }
    }
}

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

async fn run_tasks(config_path: &Path) -> Result<()> {
    tracing::info!(config = %config_path.display(), "running task commands");

    let mut engine = Engine::new(config_path)?;
    engine.startup().await?;
    engine.wait_for_tasks().await;

    // Shutdown daemons since we're in task-only mode
    engine.shutdown().await?;

    tracing::info!("all tasks completed");
    Ok(())
}

async fn run_daemon(
    config_path: &Path,
    socket_path: &Path,
    detach: bool,
    quiet: bool,
) -> Result<()> {
    if detach {
        // TODO: implement daemonization
        anyhow::bail!("detached daemon mode not yet implemented");
    }

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

    let mut engine = Engine::with_options(config_path, !quiet, false)?;
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
            return Ok(());
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
            println!("Error: {}", message);
        }
        _ => {
            println!("Unexpected response");
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
            println!("Error: {}", message);
        }
        _ => {
            println!("Unexpected response");
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
            println!("Error: {}", message);
        }
        _ => {
            println!("Unexpected response");
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
                std::process::exit(1);
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
            std::process::exit(1);
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
                std::process::exit(1);
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
            std::process::exit(1);
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

async fn run_tui(config_path: &Path, socket_path: &Path, options: &TuiOptions) -> Result<()> {
    tracing::info!(
        config = %config_path.display(),
        style = ?options.style,
        "starting TUI"
    );

    // Check if daemon is running
    let check_timeout = Duration::from_secs(1);
    let daemon_running = match tokio::time::timeout(check_timeout, Client::connect(socket_path))
        .await
    {
        Ok(Ok(mut client)) => {
            // Try to communicate
            match tokio::time::timeout(check_timeout, client.request(&ApiRequest::Status)).await {
                Ok(Ok(_)) => {
                    tracing::debug!("daemon is running");
                    true
                }
                _ => {
                    tracing::debug!("daemon socket exists but not responsive");
                    false
                }
            }
        }
        _ => {
            tracing::debug!("no daemon running");
            false
        }
    };

    // Start daemon if needed
    let started_daemon = if !daemon_running && !options.no_autostart {
        tracing::info!("starting daemon in background");

        // Spawn daemon as a background process
        let config_path_str = config_path.to_string_lossy().to_string();
        let socket_path_str = socket_path.to_string_lossy().to_string();

        let daemon_handle = tokio::spawn(async move {
            let config_path = std::path::Path::new(&config_path_str);
            let socket_path = std::path::Path::new(&socket_path_str);

            // Use embedded mode to prevent remote shutdown
            let mut engine = match Engine::new_embedded(config_path) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, "failed to create engine");
                    return;
                }
            };

            let (command_tx, mut command_rx) = mpsc::channel::<EngineCommand>(32);

            // Start API server
            let server = match Server::bind(socket_path, command_tx).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "failed to start server");
                    return;
                }
            };

            let server_handle = tokio::spawn(async move {
                if let Err(e) = server.run().await {
                    tracing::error!(error = %e, "server error");
                }
            });

            // Run startup (non-blocking, tasks run in background)
            if let Err(e) = engine.startup().await {
                tracing::error!(error = %e, "startup failed");
            }

            // Main loop
            loop {
                tokio::select! {
                    Some(cmd) = command_rx.recv() => {
                        // Drain log channel before handling request (ensures GetLogs returns fresh data)
                        engine.process_incoming_logs();

                        let is_shutdown = matches!(cmd.request, ApiRequest::Shutdown);
                        let response = engine.handle_request(cmd.request).await;
                        let _ = cmd.response_tx.send(response);

                        if is_shutdown {
                            break;
                        }
                    }

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

            server_handle.abort();
            let _ = engine.shutdown().await;
        });

        // Wait for daemon to be ready
        let mut attempts = 0;
        while attempts < 20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(mut client) = Client::connect(socket_path).await {
                if client.request(&ApiRequest::Status).await.is_ok() {
                    break;
                }
            }
            attempts += 1;
        }

        if attempts >= 20 {
            tracing::warn!("daemon may not be ready after 2s");
        }

        // Store handle to prevent immediate drop
        std::mem::forget(daemon_handle);
        true
    } else {
        false
    };

    // Create TUI app
    use zaz_tui::{App, TuiStyle};
    let style = TuiStyle::from(options.style);
    let user_config = zaz_config::UserConfig {
        no_autostart: options.no_autostart,
        disable_animations: options.disable_animations,
        tui_style: Some(options.style),
        log_colors: zaz_config::LogColorConfig::default(),
        notifications: zaz_config::NotificationConfig::default(),
    };

    let config_name = config_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "zaz.toml".to_string());

    let mut app = App::new(style, user_config, config_name);
    app.started_daemon = started_daemon;

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
