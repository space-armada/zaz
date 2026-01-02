//! Command execution.

use crate::pty::ManagedChild;
use crate::ProcessError;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Output from a command execution.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Standard output lines.
    pub stdout: Vec<String>,

    /// Standard error lines.
    pub stderr: Vec<String>,

    /// Exit code (if process exited normally).
    pub exit_code: Option<i32>,
}

/// Executes shell commands.
#[derive(Clone)]
pub struct Executor {
    shell: String,
    working_dir: Option<String>,
}

const SHELL_ENV_VAR: &str = "SHELL";
const DEFAULT_SHELL: &str = "/bin/sh";

impl Executor {
    /// Create a new executor with the given shell.
    pub fn new(shell: Option<String>) -> Self {
        let shell = shell.unwrap_or_else(|| std::env::var(SHELL_ENV_VAR).unwrap_or_else(|_| DEFAULT_SHELL.to_string()));

        Self {
            shell,
            working_dir: None,
        }
    }

    /// Set the working directory for commands.
    pub fn with_working_dir(mut self, dir: String) -> Self {
        self.working_dir = Some(dir);
        self
    }

    /// Spawn a command and return the child process.
    ///
    /// If `use_pty` is true, the command runs in a pseudo-terminal,
    /// which is required for some interactive programs.
    pub fn spawn(&self, command: &str, use_pty: bool) -> Result<ManagedChild, ProcessError> {
        if use_pty {
            self.spawn_pty(command)
        } else {
            self.spawn_regular(command)
        }
    }

    /// Spawn a command in a PTY.
    fn spawn_pty(&self, command: &str) -> Result<ManagedChild, ProcessError> {
        ManagedChild::spawn_pty(&self.shell, command, self.working_dir.as_deref())
    }

    /// Spawn a regular (non-PTY) command.
    fn spawn_regular(&self, command: &str) -> Result<ManagedChild, ProcessError> {
        let mut cmd = Command::new(&self.shell);
        cmd.arg("-c").arg(command);

        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }

        // Create new process group
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::other)?;
                Ok(())
            });
        }

        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(ProcessError::Spawn)?;

        Ok(ManagedChild::Regular(child))
    }

    /// Run a command to completion and capture output.
    ///
    /// Note: This always runs without PTY since we need to capture stdout/stderr separately.
    pub async fn run(&self, command: &str) -> Result<CommandOutput, ProcessError> {
        self.run_with_callback(command, |_| {}).await
    }

    /// Run a command, streaming output through an unbounded channel as it arrives.
    ///
    /// Lines are sent to the channel immediately as they're produced, allowing
    /// real-time output display. Uses unbounded channel to avoid dropping lines
    /// when callback can't be async. The final CommandOutput is still returned
    /// for exit code checking.
    pub async fn run_streaming(
        &self,
        command: &str,
        output_tx: mpsc::UnboundedSender<OutputLine>,
    ) -> Result<CommandOutput, ProcessError> {
        self.run_with_callback(command, move |line| {
            // Unbounded send never blocks and only fails if receiver dropped
            let _ = output_tx.send(line);
        })
        .await
    }

    /// Run a command to completion, streaming output to a callback.
    ///
    /// The callback is called for each line of stdout/stderr as it arrives.
    /// Lines are also collected and returned in the final output.
    pub async fn run_with_callback<F>(
        &self,
        command: &str,
        on_output: F,
    ) -> Result<CommandOutput, ProcessError>
    where
        F: Fn(OutputLine) + Send + 'static,
    {
        // For run-to-completion commands, we use regular spawn to capture output
        let child = self.spawn_regular(command)?;

        let ManagedChild::Regular(mut child) = child else {
            unreachable!("spawn_regular always returns Regular variant");
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (stdout_tx, mut stdout_rx) = mpsc::channel(100);
        let (stderr_tx, mut stderr_rx) = mpsc::channel(100);

        // Spawn tasks to read stdout/stderr
        let cmd_for_debug = command.to_string();
        if let Some(stdout) = stdout {
            let tx = stdout_tx;
            let cmd = cmd_for_debug.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                let mut count = 0;
                while let Ok(Some(line)) = lines.next_line().await {
                    count += 1;
                    eprintln!("[DEBUG stdout #{} cmd={}] {}", count, cmd, line);
                    let _ = tx.send(line).await;
                }
                eprintln!("[DEBUG stdout DONE cmd={}] total {} lines", cmd, count);
            });
        }

        if let Some(stderr) = stderr {
            let tx = stderr_tx;
            let cmd = cmd_for_debug;
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut count = 0;
                while let Ok(Some(line)) = lines.next_line().await {
                    count += 1;
                    eprintln!("[DEBUG stderr #{} cmd={}] {}", count, cmd, line);
                    let _ = tx.send(line).await;
                }
                eprintln!("[DEBUG stderr DONE cmd={}] total {} lines", cmd, count);
            });
        }

        // Wrap callback in Arc for sharing
        let on_output = std::sync::Arc::new(on_output);

        // Collect output while streaming to callback
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();

        // Process output as it arrives
        let mut recv_stdout_count = 0;
        let mut recv_stderr_count = 0;
        loop {
            tokio::select! {
                Some(line) = stdout_rx.recv() => {
                    recv_stdout_count += 1;
                    eprintln!("[DEBUG recv stdout #{}] {}", recv_stdout_count, line);
                    on_output(OutputLine::Stdout(line.clone()));
                    stdout_lines.push(line);
                }
                Some(line) = stderr_rx.recv() => {
                    recv_stderr_count += 1;
                    eprintln!("[DEBUG recv stderr #{}] {}", recv_stderr_count, line);
                    on_output(OutputLine::Stderr(line.clone()));
                    stderr_lines.push(line);
                }
                status = child.wait() => {
                    eprintln!("[DEBUG child.wait() returned, draining...]");
                    let status = status.map_err(ProcessError::Spawn)?;

                    // Drain remaining output
                    stdout_rx.close();
                    stderr_rx.close();

                    while let Some(line) = stdout_rx.recv().await {
                        recv_stdout_count += 1;
                        eprintln!("[DEBUG drain stdout #{}] {}", recv_stdout_count, line);
                        on_output(OutputLine::Stdout(line.clone()));
                        stdout_lines.push(line);
                    }
                    while let Some(line) = stderr_rx.recv().await {
                        recv_stderr_count += 1;
                        eprintln!("[DEBUG drain stderr #{}] {}", recv_stderr_count, line);
                        on_output(OutputLine::Stderr(line.clone()));
                        stderr_lines.push(line);
                    }
                    eprintln!("[DEBUG drained: {} stdout, {} stderr]", recv_stdout_count, recv_stderr_count);

                    let exit_code = status.code();

                    // Always return output, even on non-zero exit.
                    // Caller can check exit_code to determine success/failure.
                    return Ok(CommandOutput {
                        stdout: stdout_lines,
                        stderr: stderr_lines,
                        exit_code,
                    });
                }
            }
        }
    }
}

/// A line of output from a running command.
#[derive(Debug, Clone)]
pub enum OutputLine {
    /// Standard output line.
    Stdout(String),
    /// Standard error line.
    Stderr(String),
}

impl Default for Executor {
    fn default() -> Self {
        Self::new(None)
    }
}
