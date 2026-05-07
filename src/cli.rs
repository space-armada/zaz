//! CLI argument definitions for the `zaz` binary.
//!
//! This module is kept separate from `main.rs` so that tooling (the
//! `xtask` crate's `docs-cli` generator) can introspect the same `clap`
//! command tree without depending on the binary's runtime wiring.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "zaz")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Configuration file path
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    pub debug: bool,

    /// Socket path for daemon communication
    #[arg(short, long)]
    pub socket: Option<PathBuf>,

    /// Use full TUI style (split panes with group tree)
    #[arg(long, conflicts_with = "multi_pane")]
    pub full: bool,

    /// Use multi-pane TUI style (one pane per task)
    #[arg(long, conflicts_with = "full")]
    pub multi_pane: bool,

    /// Don't auto-start a daemon before opening the TUI
    #[arg(long)]
    pub no_autostart: bool,

    /// Stop the connected daemon when the TUI exits
    #[arg(long)]
    pub stop_on_exit: bool,

    /// Write debug logs to a file (works in both TUI and daemon modes)
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
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
//   the daemon is already stopped, `start` exits 0 when the daemon is already
//   running, and both exit 1 for API/operational errors.
//
// New CLI commands must declare which category they belong to before
// implementation so their exit behavior stays predictable in scripts.
#[derive(Subcommand)]
pub enum Commands {
    /// Run task commands once and exit
    Task,

    /// Run the daemon in the foreground
    Daemon {
        /// Suppress process output logging
        #[arg(short, long)]
        quiet: bool,
    },

    /// Start the daemon in the background and exit
    Start,

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

    /// Run the MCP tool server over stdio
    Mcp {
        /// Spawn a background daemon at startup if one is not already running
        #[arg(long)]
        autostart: bool,
    },

    /// Print a shell completion script to stdout
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}
