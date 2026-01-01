//! Command execution.

use crate::ProcessError;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
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
pub struct Executor {
    shell: String,
    working_dir: Option<String>,
}

impl Executor {
    /// Create a new executor with the given shell.
    pub fn new(shell: Option<String>) -> Self {
        let shell = shell
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));

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
    pub fn spawn(&self, command: &str, use_pty: bool) -> Result<Child, ProcessError> {
        let _ = use_pty; // TODO: implement PTY support

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

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(ProcessError::Spawn)
    }

    /// Run a command to completion and capture output.
    pub async fn run(&self, command: &str) -> Result<CommandOutput, ProcessError> {
        let mut child = self.spawn(command, false)?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (stdout_tx, mut stdout_rx) = mpsc::channel(100);
        let (stderr_tx, mut stderr_rx) = mpsc::channel(100);

        // Spawn tasks to read stdout/stderr
        if let Some(stdout) = stdout {
            let tx = stdout_tx;
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(line).await;
                }
            });
        }

        if let Some(stderr) = stderr {
            let tx = stderr_tx;
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(line).await;
                }
            });
        }

        // Collect output
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();

        // Wait for process to complete
        let status = child.wait().await.map_err(ProcessError::Spawn)?;

        // Drain remaining output
        stdout_rx.close();
        stderr_rx.close();

        while let Some(line) = stdout_rx.recv().await {
            stdout_lines.push(line);
        }
        while let Some(line) = stderr_rx.recv().await {
            stderr_lines.push(line);
        }

        let exit_code = status.code();

        if !status.success() {
            if let Some(code) = exit_code {
                return Err(ProcessError::ExitStatus(code));
            }
        }

        Ok(CommandOutput {
            stdout: stdout_lines,
            stderr: stderr_lines,
            exit_code,
        })
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new(None)
    }
}
