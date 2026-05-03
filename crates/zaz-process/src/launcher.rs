//! Daemon launcher for detached background process startup.
//!
//! Encapsulates fork+setsid detachment and output log redirection behind a
//! safe public API. Both the TUI auto-start path and `zaz start` use this
//! interface.

use crate::ProcessError;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

/// Builds and launches a daemon process detached from the controlling terminal.
///
/// The spawned process runs in a new session (via `setsid`) with stdout/stderr
/// redirected to a log file. The caller receives a [`LaunchHandle`] for
/// non-blocking crash detection during a readiness polling window.
pub struct DaemonLauncher {
    exe: PathBuf,
    args: Vec<OsString>,
    output_log: PathBuf,
}

impl DaemonLauncher {
    /// Create a launcher for the given executable, logging output to `output_log`.
    pub fn new(exe: impl Into<PathBuf>, output_log: impl Into<PathBuf>) -> Self {
        Self {
            exe: exe.into(),
            args: Vec::new(),
            output_log: output_log.into(),
        }
    }

    /// Append a single argument.
    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    /// Append multiple arguments.
    pub fn args(&mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> &mut Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Spawn the daemon in a new session with output redirected to the log file.
    ///
    /// Returns a [`LaunchHandle`] that can check whether the child has already
    /// exited (useful for distinguishing crash from slow startup).
    pub fn launch(&mut self) -> Result<LaunchHandle, ProcessError> {
        let log_file = File::options()
            .create(true)
            .append(true)
            .open(&self.output_log)
            .map_err(|e| {
                ProcessError::LaunchDaemon(format!(
                    "opening output log {}: {}",
                    self.output_log.display(),
                    e
                ))
            })?;

        let stdout_file = log_file
            .try_clone()
            .map_err(|e| ProcessError::LaunchDaemon(format!("cloning log fd: {}", e)))?;

        let mut command = Command::new(&self.exe);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(stdout_file)
            .stderr(log_file);

        // Safety: setsid() is async-signal-safe and has no preconditions
        // beyond being called in the child process after fork.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }

        let child = command.spawn().map_err(|e| {
            ProcessError::LaunchDaemon(format!("spawning {}: {}", self.exe.display(), e))
        })?;

        Ok(LaunchHandle { child })
    }
}

/// Handle to a launched daemon process.
///
/// Provides non-blocking status checks so callers can detect an immediate
/// crash during the readiness polling window.
#[derive(Debug)]
pub struct LaunchHandle {
    child: Child,
}

impl LaunchHandle {
    /// PID of the spawned daemon.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Non-blocking check: returns `Some(status)` if the process has already
    /// exited, `None` if still running.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcessError> {
        self.child.try_wait().map_err(ProcessError::Spawn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn launch_captures_output_to_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test-output.log");

        let mut launcher = DaemonLauncher::new("/bin/sh", &log_path);
        launcher.args(["-c", "echo hello from daemon"]);

        let mut handle = launcher.launch().unwrap();

        // Wait for the short-lived process to finish.
        loop {
            match handle.try_wait().unwrap() {
                Some(status) => {
                    assert!(status.success());
                    break;
                }
                None => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }

        let mut contents = String::new();
        File::open(&log_path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents.trim(), "hello from daemon");
    }

    #[test]
    fn launch_returns_pid() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test-output.log");

        let mut launcher = DaemonLauncher::new("/bin/sh", &log_path);
        launcher.args(["-c", "sleep 0"]);

        let handle = launcher.launch().unwrap();
        assert!(handle.id() > 0);
    }

    #[test]
    fn launch_detects_crash() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("test-output.log");

        let mut launcher = DaemonLauncher::new("/bin/sh", &log_path);
        launcher.args(["-c", "exit 42"]);

        let mut handle = launcher.launch().unwrap();

        loop {
            match handle.try_wait().unwrap() {
                Some(status) => {
                    assert!(!status.success());
                    break;
                }
                None => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }

    #[test]
    fn launch_fails_on_bad_log_path() {
        let mut launcher = DaemonLauncher::new("/bin/sh", "/no/such/directory/output.log");
        launcher.args(["-c", "true"]);

        let err = launcher.launch().unwrap_err();
        assert!(
            matches!(err, ProcessError::LaunchDaemon(_)),
            "expected LaunchDaemon, got: {err}"
        );
    }
}
