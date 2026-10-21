//! End-to-end coverage for daemon startup: the event loop answers before the config loads, and each
//! launcher waits on its own policy.
//!
//! A slow evaluation is an `env_file` that is a named pipe. The daemon's read blocks until the
//! test writes to it. A failing evaluation is a missing or malformed `env_file`, or a `zaz.toml`
//! that does not parse.

mod support;

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use support::{run_zaz, stderr_string, stdout_string, wait_for_socket_gone, zaz_bin};
use tempfile::TempDir;

const CONFIG: &str = r#"
[[group]]
name = "app"
env_file = "slow.env"

[[group.task]]
name = "noop"
command = "true"
"#;

const FIXED_CONFIG: &str = r#"
[[group]]
name = "app"
env_file = "slow.env"

[[group.task]]
name = "noop"
command = "true"

[[group]]
name = "extra"

[[group.task]]
name = "noop"
command = "true"
"#;

/// One project directory with a config, a socket, and a log, all under a temp dir.
struct Project {
    temp: TempDir,
}

impl Project {
    fn new(config: &str) -> Self {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("zaz.toml"), config).unwrap();

        Self { temp }
    }

    /// A project whose `slow.env` is a named pipe, so a load blocks until [`Project::feed`].
    fn with_fifo() -> Self {
        let project = Self::new(CONFIG);
        let status = Command::new("mkfifo")
            .arg(project.path().join("slow.env"))
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo is required for these tests");

        project
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }

    fn config(&self) -> PathBuf {
        self.path().join("zaz.toml")
    }

    fn socket(&self) -> PathBuf {
        self.path().join("daemon.sock")
    }

    /// `zaz` with this project's config, socket, and log file, ready for a subcommand.
    fn zaz(&self, subcommand: &[&str]) -> Command {
        let mut cmd = Command::new(zaz_bin());
        cmd.arg("--config")
            .arg(self.config())
            .arg("--socket")
            .arg(self.socket())
            .arg("--log-file")
            .arg(self.path().join("zaz.log"))
            .args(subcommand)
            .current_dir(self.path());
        cmd
    }

    fn run(&self, subcommand: &[&str]) -> Output {
        self.zaz(subcommand).output().expect("run zaz")
    }

    fn spawn(&self, subcommand: &[&str]) -> Child {
        self.zaz(subcommand)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn zaz")
    }

    /// Let the load that is blocked on the pipe read `text`.
    fn feed(&self, text: &'static str) {
        let path = self.path().join("slow.env");

        thread::spawn(move || {
            let _ = std::fs::write(path, text);
        });
    }

    fn status(&self) -> String {
        stdout_string(&self.run(&["status"]))
    }

    /// Poll `zaz status` until its output contains `needle`.
    fn wait_status(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;

        loop {
            let status = self.status();
            if status.contains(needle) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "status never contained {needle:?}; last status: {status}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&self) {
        let _ = self.run(&["stop"]);
        wait_for_socket_gone(&self.socket(), Duration::from_secs(5));
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_exit(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;

    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the process did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn signal(child: &Child, name: &str) {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn start_keeps_waiting_past_the_hang_budget_while_the_daemon_reports_starting() {
    let project = Project::with_fifo();
    let start = project.spawn(&["start"]);

    let status = project.wait_status("Daemon Status: Starting", Duration::from_secs(10));
    assert!(!status.contains("Load error"), "{status}");

    // The hang budget is ten seconds of unanswered requests. A daemon that keeps answering
    // `Starting` outlasts it.
    thread::sleep(Duration::from_secs(11));
    assert!(project.status().contains("Daemon Status: Starting"));

    project.feed("A=1\n");
    let output = wait_exit(start, Duration::from_secs(15));

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert!(stdout_string(&output).contains("daemon started"));
    assert!(
        stderr_string(&output).contains("waiting for the daemon to finish loading"),
        "start should print a progress line while it waits: {}",
        stderr_string(&output)
    );
    assert!(project.status().contains("Daemon Status: Running"));
}

#[test]
fn start_prints_the_coded_error_and_shuts_down_when_a_source_fails() {
    let project = Project::new(CONFIG);

    let output = project.run(&["start"]);
    let stderr = stderr_string(&output);

    assert_ne!(output.status.code(), Some(0));
    assert!(stderr.contains("env_file_missing"), "{stderr}");
    assert!(
        stderr.contains("daemon failed to load its configuration"),
        "{stderr}"
    );
    assert!(
        wait_for_socket_gone(&project.socket(), Duration::from_secs(5)),
        "a daemon whose first load failed has nothing useful running"
    );
}

#[test]
fn a_second_start_waits_on_a_starting_daemon_and_never_shuts_it_down() {
    let project = Project::with_fifo();
    let first = project.spawn(&["start"]);
    project.wait_status("Daemon Status: Starting", Duration::from_secs(10));

    let second = project.spawn(&["start"]);
    thread::sleep(Duration::from_millis(1500));

    project.feed("A=1\n");
    let first = wait_exit(first, Duration::from_secs(15));
    let second = wait_exit(second, Duration::from_secs(15));

    assert_eq!(first.status.code(), Some(0), "{}", stderr_string(&first));
    assert_eq!(second.status.code(), Some(0), "{}", stderr_string(&second));
    assert!(stdout_string(&second).contains("daemon already running"));
    assert!(project.status().contains("Daemon Status: Running"));
}

#[test]
fn start_on_a_daemon_with_a_load_error_says_how_to_recover_and_leaves_it_up() {
    let project = Project::new(CONFIG);
    let mut daemon = project.spawn(&["daemon", "--quiet"]);

    let status = project.wait_status("Load error:", Duration::from_secs(10));
    assert!(status.contains("Daemon Status: Starting"), "{status}");
    assert!(status.contains("env_file_missing"), "{status}");

    let output = project.run(&["start"]);
    let stderr = stderr_string(&output);
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr.contains("zaz reload"), "{stderr}");
    assert!(stderr.contains("zaz stop"), "{stderr}");

    assert!(
        project.status().contains("Load error:"),
        "a daemon the pre-check did not spawn is never shut down"
    );
    assert!(daemon.try_wait().unwrap().is_none());

    // Fix the source and an edited group, then reload. Recovery runs the whole constructor.
    std::fs::write(project.path().join("slow.env"), "A=1\n").unwrap();
    std::fs::write(project.config(), FIXED_CONFIG).unwrap();

    let reload = project.run(&["reload"]);
    assert_eq!(reload.status.code(), Some(0), "{}", stderr_string(&reload));

    let status = project.status();
    assert!(status.contains("Daemon Status: Running"), "{status}");
    assert!(!status.contains("Load error:"), "{status}");
    assert!(
        status.contains("extra"),
        "recovery should pick up the edited group: {status}"
    );

    project.stop();
    let _ = daemon.wait();
}

#[test]
fn a_zaz_toml_typo_leaves_the_daemon_up_and_a_fix_plus_reload_recovers_it() {
    let project = Project::new("[[group\nname = ");
    let mut daemon = project.spawn(&["daemon", "--quiet"]);

    let status = project.wait_status("Load error:", Duration::from_secs(10));
    assert!(status.contains("config_parse_error"), "{status}");

    let restart = project.run(&["restart"]);
    assert_ne!(restart.status.code(), Some(0));
    assert!(
        stderr_string(&restart).contains("first_load_failed"),
        "{}",
        stderr_string(&restart)
    );

    std::fs::write(project.path().join("slow.env"), "A=1\n").unwrap();
    std::fs::write(project.config(), CONFIG).unwrap();

    let reload = project.run(&["reload"]);
    assert_eq!(reload.status.code(), Some(0), "{}", stderr_string(&reload));
    assert!(project.status().contains("Daemon Status: Running"));

    project.stop();
    let _ = daemon.wait();
}

#[test]
fn ctrl_c_during_the_wait_shuts_down_the_daemon_start_spawned() {
    let project = Project::with_fifo();
    let start = project.spawn(&["start"]);
    project.wait_status("Daemon Status: Starting", Duration::from_secs(10));

    signal(&start, "INT");
    let output = wait_exit(start, Duration::from_secs(15));

    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr_string(&output).contains("interrupted"),
        "{}",
        stderr_string(&output)
    );
    assert!(
        wait_for_socket_gone(&project.socket(), Duration::from_secs(10)),
        "the daemon start spawned should be shut down"
    );
}

#[test]
fn requests_during_starting_answer_or_say_why_not() {
    let project = Project::with_fifo();
    let start = project.spawn(&["start"]);
    project.wait_status("Daemon Status: Starting", Duration::from_secs(10));

    let restart = project.run(&["restart"]);
    assert_ne!(restart.status.code(), Some(0));
    assert!(
        stderr_string(&restart).contains("daemon_starting"),
        "{}",
        stderr_string(&restart)
    );

    let restart_group = project.run(&["restart", "app"]);
    assert!(stderr_string(&restart_group).contains("daemon_starting"));

    project.feed("A=1\n");
    let output = wait_exit(start, Duration::from_secs(15));
    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
}

#[test]
fn overlapping_reloads_during_starting_share_one_extra_evaluation() {
    let project = Project::with_fifo();
    let start = project.spawn(&["start"]);
    project.wait_status("Daemon Status: Starting", Duration::from_secs(10));

    let first = project.spawn(&["reload"]);
    let second = project.spawn(&["reload"]);
    let third = project.spawn(&["reload"]);
    thread::sleep(Duration::from_millis(800));

    // The startup load finishes first, then one more load serves all three reloads.
    project.feed("A=1\n");
    let started = wait_exit(start, Duration::from_secs(15));
    assert_eq!(
        started.status.code(),
        Some(0),
        "{}",
        stderr_string(&started)
    );

    thread::sleep(Duration::from_millis(500));
    project.feed("A=2\n");

    for reload in [first, second, third] {
        let output = wait_exit(reload, Duration::from_secs(15));
        assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
        assert!(
            stdout_string(&output).contains("config reloaded: 0 added, 0 removed, 1 modified"),
            "{}",
            stdout_string(&output)
        );
    }

    // Both loads were consumed, so nothing is left reading the pipe.
    let sixth = project.spawn(&["status"]);
    let output = wait_exit(sixth, Duration::from_secs(5));
    assert!(stdout_string(&output).contains("Daemon Status: Running"));
}

#[test]
fn shutdown_answers_every_waiting_reload() {
    let project = Project::with_fifo();
    let start = project.spawn(&["start"]);
    project.wait_status("Daemon Status: Starting", Duration::from_secs(10));

    let waiting = project.spawn(&["reload"]);
    let also_waiting = project.spawn(&["reload"]);
    thread::sleep(Duration::from_millis(800));

    let stop = project.run(&["stop"]);
    assert_eq!(stop.status.code(), Some(0), "{}", stderr_string(&stop));

    for reload in [waiting, also_waiting] {
        let output = wait_exit(reload, Duration::from_secs(10));
        assert_ne!(output.status.code(), Some(0));
        assert!(
            stderr_string(&output).contains("shutting_down"),
            "{}",
            stderr_string(&output)
        );
    }

    let _ = wait_exit(start, Duration::from_secs(15));
    assert!(
        wait_for_socket_gone(&project.socket(), Duration::from_secs(10)),
        "the daemon should exit even though its evaluation is still blocked"
    );
}

const INITIALIZE_REQUEST: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","#,
    r#""params":{"protocolVersion":"2025-06-18","capabilities":{},"#,
    r#""clientInfo":{"name":"zaz-test","version":"0"}}}"#,
    "\n",
);

const INITIALIZED_NOTIFICATION: &str = concat!(
    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    "\n",
);

const STATUS_REQUEST: &str = concat!(
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","#,
    r#""params":{"name":"zaz_status","arguments":{}}}"#,
    "\n",
);

#[test]
fn mcp_autostart_survives_a_zaz_toml_typo_and_a_fix_plus_reload_recovers() {
    use std::io::{BufRead, BufReader};

    let project = Project::new("[[group\nname = ");

    let mut mcp = project
        .zaz(&["mcp", "--autostart"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zaz mcp");
    {
        let mut stdin = mcp.stdin.take().unwrap();
        stdin.write_all(INITIALIZE_REQUEST.as_bytes()).unwrap();
        stdin
            .write_all(INITIALIZED_NOTIFICATION.as_bytes())
            .unwrap();
        stdin.write_all(STATUS_REQUEST.as_bytes()).unwrap();
    }

    let stdout = mcp.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut response = None;
    while Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
            if value.get("id").and_then(Value::as_u64) == Some(2) {
                response = Some(value);
                break;
            }
        }
    }

    let _ = wait_exit(mcp, Duration::from_secs(10));
    let response = response.expect("zaz_status should answer even though the config is broken");
    let serialized = response.to_string();

    assert!(
        !response["result"]["isError"].as_bool().unwrap_or(false),
        "{serialized}"
    );
    assert!(serialized.contains("config_parse_error"), "{serialized}");
    assert!(serialized.contains("starting"), "{serialized}");

    // The daemon the MCP session started is still up for a fix and a reload.
    assert!(project.status().contains("Load error:"));

    std::fs::write(project.path().join("slow.env"), "A=1\n").unwrap();
    std::fs::write(project.config(), CONFIG).unwrap();
    let reload = project.run(&["reload"]);
    assert_eq!(reload.status.code(), Some(0), "{}", stderr_string(&reload));
    assert!(project.status().contains("Daemon Status: Running"));
}

#[test]
fn zaz_task_exits_nonzero_with_the_coded_error_when_a_source_fails() {
    let project = Project::new(CONFIG);

    let output = run_zaz(
        project.path(),
        &["--config", project.config().to_str().unwrap(), "task"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_string(&output).contains("env_file_missing"),
        "{}",
        stderr_string(&output)
    );
}

#[test]
fn zaz_task_waits_for_a_slow_first_evaluation() {
    let project = Project::with_fifo();
    let task = project.spawn(&["task"]);

    thread::sleep(Duration::from_millis(1500));
    project.feed("A=1\n");

    let output = wait_exit(task, Duration::from_secs(15));
    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
}
