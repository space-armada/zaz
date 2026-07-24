//! Shared harness for CLI-daemon integration tests: spawning the real `zaz`
//! binary, waiting for a daemon to become ready, and tearing it down.
//!
//! Lives under `tests/support/` rather than `tests/support.rs` so Cargo does
//! not treat it as its own (empty) integration-test target.
//!
//! Each `tests/*.rs` file compiles as its own crate, with this module linked
//! in separately, so a given binary only exercises the subset of this API its
//! own tests need. Dead-code analysis can't see usage in sibling binaries,
//! hence the blanket allow rather than fighting per-item warnings.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

pub fn zaz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zaz")
}

pub fn run_zaz(current_dir: &Path, args: &[&str]) -> Output {
    run_zaz_with_envs(current_dir, std::iter::empty::<(&str, &str)>(), args)
}

pub fn run_zaz_with_envs<I, S, E, K, V>(current_dir: &Path, envs: E, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut cmd = Command::new(zaz_bin());
    cmd.args(args).current_dir(current_dir);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("failed to run zaz binary")
}

pub fn stdout_string(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout should be valid utf-8")
}

pub fn stderr_string(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr should be valid utf-8")
}

pub fn wait_for_socket_gone(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !socket.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

/// A daemon started via the real `zaz start` command, torn down automatically
/// on drop so a failed assertion does not leak processes or sockets.
pub struct StartedDaemon {
    current_dir: PathBuf,
    config: PathBuf,
    socket: PathBuf,
    envs: Vec<(String, PathBuf)>,
    stopped: bool,
}

impl StartedDaemon {
    pub fn launch(current_dir: &Path, config: &Path, socket: &Path) -> Self {
        Self::launch_with_envs(current_dir, config, socket, &[])
    }

    pub fn launch_with_envs(
        current_dir: &Path,
        config: &Path,
        socket: &Path,
        envs: &[(&str, &Path)],
    ) -> Self {
        let owned_envs: Vec<(String, PathBuf)> = envs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_path_buf()))
            .collect();
        let log_path = current_dir.join("zaz.log");
        let start_args: Vec<&OsStr> = vec![
            OsStr::new("--config"),
            config.as_os_str(),
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("--log-file"),
            log_path.as_os_str(),
            OsStr::new("start"),
        ];
        let output = run_zaz_with_envs(
            current_dir,
            owned_envs.iter().map(|(k, v)| (k.as_str(), v.as_path())),
            start_args,
        );

        if !output.status.success() {
            let daemon_log = current_dir.join("zaz.daemon-output.log");
            let daemon_log_contents = std::fs::read_to_string(&daemon_log).unwrap_or_else(|e| {
                format!("(no daemon-output.log at {}: {e})", daemon_log.display())
            });
            panic!(
                "zaz start exited with {:?}\nstdout: {}\nstderr: {}\ndaemon-output.log:\n{}",
                output.status.code(),
                stdout_string(&output),
                stderr_string(&output),
                daemon_log_contents,
            );
        }

        let daemon = Self {
            current_dir: current_dir.to_path_buf(),
            config: config.to_path_buf(),
            socket: socket.to_path_buf(),
            envs: owned_envs,
            stopped: false,
        };
        daemon.wait_for_ready();
        daemon
    }

    fn run(&self, args: &[&str]) -> Output {
        run_zaz_with_envs(
            &self.current_dir,
            self.envs.iter().map(|(k, v)| (k.as_str(), v.as_path())),
            args,
        )
    }

    fn wait_for_ready(&self) {
        let socket = self.socket.to_str().expect("socket path should be utf-8");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.run(&["--socket", socket, "status"]);
            if output.status.code() == Some(0) && stdout_string(&output).contains("Daemon Status:")
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not become ready in time");
    }

    /// Stop the daemon and wait for its socket file to disappear, so a
    /// subsequent `zaz start` against the same path can bind cleanly.
    /// Idempotent: a second call is a no-op.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        let socket = self.socket.to_str().expect("socket path should be utf-8");
        let _ = self.run(&["--socket", socket, "stop"]);
        wait_for_socket_gone(&self.socket, Duration::from_secs(5));
        self.stopped = true;
    }

    pub fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    pub fn config_path(&self) -> &Path {
        &self.config
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }
}

impl Drop for StartedDaemon {
    fn drop(&mut self) {
        self.stop();
    }
}
