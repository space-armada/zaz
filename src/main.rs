//! zaz - A modern file-watching task runner and process manager.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "zaz")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Configuration file path
    #[arg(short, long, default_value = "zaz.toml")]
    config: PathBuf,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run prep commands once and exit
    Prep,

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

    match cli.command {
        Some(Commands::Prep) => run_prep(&cli.config).await,
        Some(Commands::Daemon { detach }) => run_daemon(&cli.config, detach).await,
        Some(Commands::Status) => show_status().await,
        Some(Commands::Restart { group }) => restart(group).await,
        Some(Commands::Stop) => stop_daemon().await,
        Some(Commands::Ignores) => show_ignores(),
        None => run_tui(&cli.config).await,
    }
}

async fn run_prep(config_path: &PathBuf) -> Result<()> {
    tracing::info!(config = %config_path.display(), "running prep commands");

    let config = zaz_config::load(config_path)?;
    tracing::debug!(groups = config.groups.len(), "loaded configuration");

    // TODO: implement prep runner
    println!("Prep mode not yet implemented");
    Ok(())
}

async fn run_daemon(config_path: &PathBuf, detach: bool) -> Result<()> {
    tracing::info!(
        config = %config_path.display(),
        detach = detach,
        "starting daemon"
    );

    let config = zaz_config::load(config_path)?;
    tracing::debug!(groups = config.groups.len(), "loaded configuration");

    // TODO: implement daemon
    println!("Daemon mode not yet implemented");
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

async fn run_tui(config_path: &PathBuf) -> Result<()> {
    tracing::info!(config = %config_path.display(), "starting TUI");

    let _config = zaz_config::load(config_path)?;

    let mut app = zaz_tui::App::new();
    app.run()?;

    Ok(())
}
