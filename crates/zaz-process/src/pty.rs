//! PTY (pseudo-terminal) support for interactive processes.

use crate::ProcessError;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

/// A child process that may be running in a PTY.
pub enum ManagedChild {
    /// Regular process spawned via tokio.
    Regular(tokio::process::Child),

    /// Process running in a PTY.
    Pty {
        /// The child process handle.
        child: Box<dyn portable_pty::Child + Send + Sync>,
        /// The master side of the PTY (for I/O).
        #[allow(dead_code)]
        master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
        /// Process ID.
        pid: u32,
    },
}

impl ManagedChild {
    /// Spawn a command in a PTY.
    pub fn spawn_pty(
        shell: &str,
        command: &str,
        working_dir: Option<&str>,
    ) -> Result<Self, ProcessError> {
        Self::spawn_pty_with_env(shell, command, working_dir, &HashMap::new())
    }

    /// Spawn a command in a PTY with custom environment variables.
    pub fn spawn_pty_with_env(
        shell: &str,
        command: &str,
        working_dir: Option<&str>,
        env: &HashMap<String, String>,
    ) -> Result<Self, ProcessError> {
        let pty_system = native_pty_system();

        // Create PTY with reasonable default size
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ProcessError::Pty(e.to_string()))?;

        // Build the command
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-c");
        cmd.arg(command);

        if let Some(dir) = working_dir {
            cmd.cwd(dir);
        }

        // Add environment variables
        for (key, value) in env {
            cmd.env(key, value);
        }

        // Spawn in the PTY
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| ProcessError::Pty(e.to_string()))?;

        let pid = child
            .process_id()
            .ok_or_else(|| ProcessError::Pty("failed to get process ID".to_string()))?;

        Ok(ManagedChild::Pty {
            child,
            master: Arc::new(Mutex::new(pair.master)),
            pid,
        })
    }

    /// Get the process ID.
    pub fn id(&self) -> Option<u32> {
        match self {
            ManagedChild::Regular(child) => child.id(),
            ManagedChild::Pty { pid, .. } => Some(*pid),
        }
    }

    /// Check if the process has exited without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            ManagedChild::Regular(child) => child.try_wait(),
            ManagedChild::Pty { child, .. } => {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // Convert portable_pty::ExitStatus to std::process::ExitStatus
                        // We need to construct an ExitStatus - on Unix we can use the raw code
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            if status.success() {
                                Ok(Some(ExitStatus::from_raw(0)))
                            } else {
                                // Exit code or signal - portable_pty doesn't give us details
                                // so we use a generic failure code
                                Ok(Some(ExitStatus::from_raw(1 << 8))) // exit code 1
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            // On non-Unix, we can't easily construct ExitStatus
                            // This is a limitation - we'd need platform-specific handling
                            Ok(Some(std::process::ExitStatus::default()))
                        }
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(std::io::Error::other(e)),
                }
            }
        }
    }

    /// Wait for the process to exit.
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match self {
            ManagedChild::Regular(child) => child.wait().await,
            ManagedChild::Pty { child, .. } => {
                // portable_pty::Child::wait() is blocking, so we spawn it on a blocking thread
                let status = child.wait().map_err(std::io::Error::other)?;

                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if status.success() {
                        Ok(ExitStatus::from_raw(0))
                    } else {
                        Ok(ExitStatus::from_raw(1 << 8))
                    }
                }
                #[cfg(not(unix))]
                {
                    Ok(std::process::ExitStatus::default())
                }
            }
        }
    }

    /// Kill the process.
    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            ManagedChild::Regular(child) => child.start_kill(),
            ManagedChild::Pty { child, .. } => child.kill().map_err(std::io::Error::other),
        }
    }

    /// Write to the process stdin (only works for PTY processes).
    #[allow(dead_code)]
    pub fn write(&self, data: &[u8]) -> std::io::Result<usize> {
        match self {
            ManagedChild::Regular(_) => Err(std::io::Error::other(
                "cannot write to non-PTY process stdin",
            )),
            ManagedChild::Pty { master, .. } => {
                let master = master
                    .lock()
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let mut writer = master.take_writer().map_err(std::io::Error::other)?;
                writer.write(data)
            }
        }
    }

    /// Read from the process output (only works for PTY processes).
    #[allow(dead_code)]
    pub fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ManagedChild::Regular(_) => Err(std::io::Error::other(
                "cannot read from non-PTY process - use stdout/stderr handles",
            )),
            ManagedChild::Pty { master, .. } => {
                let master = master
                    .lock()
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let mut reader = master.try_clone_reader().map_err(std::io::Error::other)?;
                reader.read(buf)
            }
        }
    }

    /// Get a cloneable reader for PTY output.
    ///
    /// Returns None for non-PTY processes. The reader can be used in a
    /// background thread to stream output lines.
    pub fn try_clone_reader(&self) -> Option<Box<dyn Read + Send>> {
        match self {
            ManagedChild::Regular(_) => None,
            ManagedChild::Pty { master, .. } => {
                let master = master.lock().ok()?;
                master.try_clone_reader().ok()
            }
        }
    }

    /// Check if this is a PTY-managed process.
    pub fn is_pty(&self) -> bool {
        matches!(self, ManagedChild::Pty { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check if PTY allocation is available in this environment.
    fn pty_available() -> bool {
        let pty_system = native_pty_system();
        pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok()
    }

    #[test]
    fn test_pty_spawn_and_exit() {
        if !pty_available() {
            eprintln!("PTY not available in this environment, skipping test");
            return;
        }

        let result = ManagedChild::spawn_pty("/bin/sh", "echo hello", None);
        assert!(result.is_ok(), "Failed to spawn PTY: {:?}", result.err());

        let mut child = result.unwrap();
        assert!(child.id().is_some());

        // Wait a bit for the command to complete
        std::thread::sleep(std::time::Duration::from_millis(100));

        let status = child.try_wait();
        assert!(status.is_ok());
    }

    #[test]
    fn test_pty_with_working_dir() {
        if !pty_available() {
            eprintln!("PTY not available in this environment, skipping test");
            return;
        }

        let result = ManagedChild::spawn_pty("/bin/sh", "pwd", Some("/tmp"));
        assert!(result.is_ok(), "Failed to spawn PTY: {:?}", result.err());
    }
}
