//! zaz - A modern file-watching task runner and process manager.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use zaz_daemon::{
    default_socket_path, ApiRequest, ApiResponse, Client, Engine, EngineCommand, Server,
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

    /// Show default ignore patterns
    Ignores,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let filter = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // Determine socket path
    let socket_path = cli.socket.unwrap_or_else(default_socket_path);

    match cli.command {
        Some(Commands::Task) => {
            let config_path = find_config(&cli.config)?;
            run_tasks(&config_path).await
        }
        Some(Commands::Daemon { detach }) => {
            let config_path = find_config(&cli.config)?;
            run_daemon(&config_path, &socket_path, detach).await
        }
        Some(Commands::Status) => show_status(&socket_path).await,
        Some(Commands::Restart { group }) => restart(&socket_path, group).await,
        Some(Commands::Stop) => stop_daemon(&socket_path).await,
        Some(Commands::Ignores) => show_ignores(),
        None => {
            let config_path = find_config(&cli.config)?;
            run_tui(&config_path, &socket_path).await
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

    // Shutdown daemons since we're in task-only mode
    engine.shutdown().await?;

    tracing::info!("all tasks completed");
    Ok(())
}

async fn run_daemon(config_path: &Path, socket_path: &Path, detach: bool) -> Result<()> {
    if detach {
        // TODO: implement daemonization
        anyhow::bail!("detached daemon mode not yet implemented");
    }

    tracing::info!(config = %config_path.display(), "starting daemon");

    let mut engine = Engine::new(config_path)?;
    let (command_tx, mut command_rx) = mpsc::channel::<EngineCommand>(32);

    // Start API server
    let server = Server::bind(socket_path, command_tx).await?;
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            tracing::error!(error = %e, "server error");
        }
    });

    // Run initial startup, but don't exit on failure
    // TODO(ripta): should this behavior be configurable?
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

fn show_ignores() -> Result<()> {
    println!("Default ignore patterns:");
    for pattern in zaz_watch::default_ignores() {
        println!("  {}", pattern);
    }
    Ok(())
}

async fn run_tui(config_path: &Path, socket_path: &Path) -> Result<()> {
    tracing::info!(config = %config_path.display(), "starting TUI");

    // For now, run in daemon mode until TUI is fully implemented
    // TODO: integrate TUI with engine
    run_daemon(config_path, socket_path, false).await
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
