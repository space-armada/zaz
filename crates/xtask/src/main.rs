//! Project-internal task runner. Hosts code-generation jobs that need access
//! to the workspace's compiled `clap` command tree (the `zaz` library).

mod docs_cli;
mod splicer;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "xtask", about = "Workspace tooling for zaz")]
struct Args {
    #[command(subcommand)]
    command: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Regenerate or validate the CLI reference snapshot in `docs/cli.md`.
    DocsCli {
        /// Rewrite `docs/cli.md` in place. Without this, the command runs
        /// in drift-detection mode and exits non-zero if regenerated content
        /// differs from the file on disk.
        #[arg(long)]
        write: bool,

        /// Override the path to `docs/cli.md`.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("xtask: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Args) -> Result<ExitCode> {
    match args.command {
        Task::DocsCli { write, path } => docs_cli::run(write, path),
    }
}
