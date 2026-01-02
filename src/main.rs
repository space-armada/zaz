//! zaz - A modern file-watching task runner and process manager.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

    // Find config file
    let config_path = find_config(&cli.config)?;

    match cli.command {
        Some(Commands::Task) => run_tasks(&config_path).await,
        Some(Commands::Daemon { detach }) => run_daemon(&config_path, detach).await,
        Some(Commands::Status) => show_status().await,
        Some(Commands::Restart { group }) => restart(group).await,
        Some(Commands::Stop) => stop_daemon().await,
        Some(Commands::Ignores) => show_ignores(),
        None => run_tui(&config_path).await,
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

    let mut engine = zaz_daemon::Engine::new(config_path)?;
    engine.startup().await?;

    // Shutdown daemons since we're in task-only mode
    engine.shutdown().await?;

    tracing::info!("all tasks completed");
    Ok(())
}

async fn run_daemon(config_path: &Path, detach: bool) -> Result<()> {
    if detach {
        // TODO: implement daemonization
        anyhow::bail!("detached daemon mode not yet implemented");
    }

    tracing::info!(config = %config_path.display(), "starting daemon");

    let mut engine = zaz_daemon::Engine::new(config_path)?;

    // Run initial startup
    engine.startup().await?;

    tracing::info!("watching for file changes...");

    // Main event loop with graceful shutdown
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal");
                break;
            }
            result = async {
                // Poll for file changes
                engine.poll().await?;

                // Check daemon health
                engine.check_daemons().await?;

                // Small sleep to avoid busy loop
                tokio::time::sleep(Duration::from_millis(50)).await;

                Ok::<_, anyhow::Error>(())
            } => {
                result?;
            }
        }
    }

    engine.shutdown().await?;
    Ok(())
}

async fn show_status() -> Result<()> {
    // TODO: connect to daemon and query status
    println!("Status not yet implemented");
    Ok(())
}

async fn restart(group: Option<String>) -> Result<()> {
    match group {
        Some(name) => println!("Restarting group: {}", name),
        None => println!("Restarting all groups"),
    }
    // TODO: connect to daemon and send restart request
    Ok(())
}

async fn stop_daemon() -> Result<()> {
    // TODO: connect to daemon and send shutdown request
    println!("Stop not yet implemented");
    Ok(())
}

fn show_ignores() -> Result<()> {
    println!("Default ignore patterns:");
    for pattern in zaz_watch::default_ignores() {
        println!("  {}", pattern);
    }
    Ok(())
}

async fn run_tui(config_path: &Path) -> Result<()> {
    tracing::info!(config = %config_path.display(), "starting TUI");

    // For now, run in daemon mode until TUI is fully implemented
    // TODO: integrate TUI with engine
    run_daemon(config_path, false).await
}
