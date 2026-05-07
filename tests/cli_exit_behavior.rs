use serde_json::to_writer;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use zaz_daemon::{ApiRequest, ApiResponse};

fn zaz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zaz")
}

fn run_zaz(current_dir: &Path, args: &[&str]) -> Output {
    Command::new(zaz_bin())
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("failed to run zaz binary")
}

fn stdout_string(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout should be valid utf-8")
}

fn stderr_string(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr should be valid utf-8")
}

fn write_task_config(temp: &TempDir, command: &str) -> std::path::PathBuf {
    let config_path = temp.path().join("zaz.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[group]]
name = "tasks"
patterns = ["**/*.rs"]

[[group.task]]
name = "run"
command = "{command}"
"#
        ),
    )
    .unwrap();
    config_path
}

#[test]
fn daemon_help_describes_foreground_mode() {
    let temp = TempDir::new().unwrap();
    let output = run_zaz(temp.path(), &["daemon", "--help"]);
    let stdout = stdout_string(&output);

    assert!(output.status.success());
    assert!(stdout.contains("Run the daemon in the foreground"));
    assert!(stdout.contains("--quiet"));
    assert!(!stdout.contains("--detach"));
}

#[test]
fn daemon_rejects_detach_flag() {
    let temp = TempDir::new().unwrap();
    let output = run_zaz(temp.path(), &["daemon", "--detach"]);
    let stderr = stderr_string(&output);

    assert!(!output.status.success());
    assert!(stderr.contains("unexpected argument '--detach'"));
}

#[test]
fn task_returns_zero_when_all_tasks_succeed() {
    let temp = TempDir::new().unwrap();
    let config_path = write_task_config(&temp, "true");

    let output = run_zaz(
        temp.path(),
        &["--config", config_path.to_str().unwrap(), "task"],
    );

    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn task_returns_nonzero_when_any_task_fails() {
    let temp = TempDir::new().unwrap();
    let config_path = write_task_config(&temp, "false");

    let output = run_zaz(
        temp.path(),
        &["--config", config_path.to_str().unwrap(), "task"],
    );
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("one or more tasks failed"));
}

fn start_daemon(current_dir: &Path, config_path: &Path, socket_path: &Path) -> Child {
    Command::new(zaz_bin())
        .args([
            "--config",
            config_path.to_str().expect("config path should be utf-8"),
            "--socket",
            socket_path.to_str().expect("socket path should be utf-8"),
            "daemon",
            "--quiet",
        ])
        .current_dir(current_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start zaz daemon")
}

fn wait_for_daemon(current_dir: &Path, socket_path: &Path) {
    let socket = socket_path.to_str().expect("socket path should be utf-8");
    let deadline = Instant::now() + Duration::from_secs(5);

    while Instant::now() < deadline {
        let output = run_zaz(current_dir, &["--socket", socket, "status"]);
        if output.status.code() == Some(0) && stdout_string(&output).contains("Daemon Status:") {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }

    panic!("daemon did not become ready in time");
}

fn start_fake_server(
    socket_path: &Path,
    expected_request: ApiRequest,
    response: ApiResponse,
) -> thread::JoinHandle<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).expect("failed to bind fake socket");
    let socket_path = socket_path.to_path_buf();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("failed to accept connection");
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .expect("failed to clone stream for reading"),
        );
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("failed to read request line");

        let request: ApiRequest = serde_json::from_str(&line).expect("request should deserialize");
        assert_request_matches(&request, &expected_request);

        to_writer(&mut stream, &response).expect("failed to serialize response");
        stream
            .write_all(b"\n")
            .expect("failed to write response newline");

        drop(stream);
        std::fs::remove_file(&socket_path).expect("failed to remove fake socket");
    })
}

fn assert_request_matches(actual: &ApiRequest, expected: &ApiRequest) {
    match (actual, expected) {
        (ApiRequest::Shutdown, ApiRequest::Shutdown)
        | (ApiRequest::Status, ApiRequest::Status)
        | (ApiRequest::RestartAll, ApiRequest::RestartAll)
        | (ApiRequest::ReloadConfig, ApiRequest::ReloadConfig) => {}
        (
            ApiRequest::RestartGroup { name: actual },
            ApiRequest::RestartGroup { name: expected },
        ) => {
            assert_eq!(actual, expected, "restart group name should match");
        }
        _ => panic!("unexpected request: got {actual:?}, expected {expected:?}"),
    }
}

#[test]
fn restart_returns_nonzero_on_api_error() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("zaz.toml");
    let socket_path = temp.path().join("daemon.sock");

    std::fs::write(
        &config_path,
        r#"
[[group]]
name = "backend"
patterns = ["**/*.rs"]

[[group.task]]
name = "noop"
command = "true"
"#,
    )
    .unwrap();

    let mut daemon = start_daemon(temp.path(), &config_path, &socket_path);
    wait_for_daemon(temp.path(), &socket_path);

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(
        temp.path(),
        &["--socket", socket, "restart", "missing-group"],
    );
    let stderr = stderr_string(&output);

    let stop_output = run_zaz(temp.path(), &["--socket", socket, "stop"]);
    let _ = daemon.wait_timeout(Duration::from_secs(5));
    if daemon.try_wait().unwrap().is_none() {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }

    assert!(!output.status.success());
    assert!(
        stderr.contains("failed to restart group 'missing-group': group not found: missing-group")
    );
    assert!(stop_output.status.success());
}

#[test]
fn stop_returns_zero_on_shutdown_acknowledgement() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("daemon.sock");
    let server = start_fake_server(
        &socket_path,
        ApiRequest::Shutdown,
        ApiResponse::Ok {
            message: Some("shutting down".to_string()),
        },
    );

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "stop"]);
    let stdout = stdout_string(&output);

    server.join().expect("fake server thread should finish");

    assert!(output.status.success());
    assert!(stdout.contains("shutting down"));
}

#[test]
fn stop_returns_zero_when_daemon_is_not_running() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("missing.sock");
    let socket = socket_path.to_str().unwrap();

    let output = run_zaz(temp.path(), &["--socket", socket, "stop"]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("No daemon running"));
}

#[test]
fn stop_returns_nonzero_on_unexpected_response() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("daemon.sock");
    let server = start_fake_server(&socket_path, ApiRequest::Shutdown, ApiResponse::EndOfStream);

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "stop"]);
    let stderr = stderr_string(&output);

    server.join().expect("fake server thread should finish");

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("stop returned unexpected response"));
}

#[test]
fn status_returns_exit_code_3_when_daemon_is_not_running() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("missing.sock");
    let socket = socket_path.to_str().unwrap();

    let output = run_zaz(temp.path(), &["--socket", socket, "status"]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(3));
    assert!(stdout.contains("no daemon running at"));
    assert!(stdout.contains(socket));
    assert!(stdout.contains("hint: start a daemon with `zaz start`"));
}

#[test]
fn status_returns_nonzero_on_api_error_response() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("daemon.sock");
    let server = start_fake_server(
        &socket_path,
        ApiRequest::Status,
        ApiResponse::Error {
            message: "status unavailable".to_string(),
        },
    );

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "status"]);
    let stderr = stderr_string(&output);

    server.join().expect("fake server thread should finish");

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("status request failed: status unavailable"));
}

#[test]
fn status_returns_nonzero_on_unexpected_response() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("daemon.sock");
    let server = start_fake_server(&socket_path, ApiRequest::Status, ApiResponse::EndOfStream);

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "status"]);
    let stderr = stderr_string(&output);

    server.join().expect("fake server thread should finish");

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("status request returned unexpected response"));
}

#[test]
fn check_json_returns_nonzero_on_validation_error() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("zaz.toml");

    std::fs::write(
        &config_path,
        r#"
[[group]]
name = "backend"
patterns = ["**/*.rs"]

[[group.task]]
name = "broken"
command = ""
"#,
    )
    .unwrap();

    let config = config_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["check", "--json", config]);
    let stdout = stdout_string(&output);
    let stderr = stderr_string(&output);
    let payload: Value = serde_json::from_str(&stdout).expect("stdout should be valid json");

    assert!(!output.status.success());
    assert_eq!(payload["valid"], false);
    assert_eq!(payload["path"], Value::String(config.to_string()));
    assert!(payload["errors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty()));
    assert!(stderr.contains("configuration validation failed"));
}

#[test]
fn restart_returns_nonzero_on_unexpected_response() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("daemon.sock");
    let server = start_fake_server(
        &socket_path,
        ApiRequest::RestartAll,
        ApiResponse::EndOfStream,
    );

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "restart"]);
    let stderr = stderr_string(&output);

    server.join().expect("fake server thread should finish");

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("restart returned unexpected response"));
}

#[test]
fn reload_returns_nonzero_when_daemon_is_not_running() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("missing.sock");
    let socket = socket_path.to_str().unwrap();

    let output = run_zaz(temp.path(), &["--socket", socket, "reload"]);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("no daemon running at"));
    assert!(stderr.contains(socket));
    assert!(stderr.contains("hint: start a daemon with `zaz start`"));
}

#[test]
fn reload_returns_nonzero_on_api_error() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("daemon.sock");
    let server = start_fake_server(
        &socket_path,
        ApiRequest::ReloadConfig,
        ApiResponse::Error {
            message: "reload failed".to_string(),
        },
    );

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "reload"]);
    let stderr = stderr_string(&output);

    server.join().expect("fake server thread should finish");

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("reload failed: reload failed"));
}

#[test]
fn reload_returns_nonzero_on_unexpected_response() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("daemon.sock");
    let server = start_fake_server(
        &socket_path,
        ApiRequest::ReloadConfig,
        ApiResponse::EndOfStream,
    );

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "reload"]);
    let stderr = stderr_string(&output);

    server.join().expect("fake server thread should finish");

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("reload returned unexpected response"));
}

#[test]
fn task_command_does_not_start_daemons_for_daemon_only_groups() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("zaz.toml");
    let daemon_marker = temp.path().join("daemon-started");

    std::fs::write(
        &config_path,
        format!(
            r#"
[settings]
shell = "/bin/sh"

[[group]]
name = "daemon-only"
patterns = ["**/*.rs"]

[[group.daemon]]
name = "server"
command = "echo started > {}"

[[group]]
name = "tasks"
patterns = ["**/*.rs"]
depends_on = ["daemon-only"]

[[group.task]]
name = "noop"
command = "true"
"#,
            daemon_marker.display()
        ),
    )
    .unwrap();

    let config = config_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--config", config, "task"]);
    let stderr = stderr_string(&output);

    assert!(output.status.success(), "stderr: {stderr}");
    assert!(!daemon_marker.exists());
}

#[test]
fn check_pretty_returns_nonzero_on_parse_error() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("zaz.toml");

    std::fs::write(
        &config_path,
        r#"
[[group]]
name = "backend"
patterns = ["**/*.rs"
"#,
    )
    .unwrap();

    let config = config_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["check", config]);
    let stderr = stderr_string(&output);

    assert!(!output.status.success());
    assert!(stderr.contains("error"));
    assert!(stderr.contains("Found 1 error"));
    assert!(stderr.contains("configuration parse failed"));
}

fn write_lifecycle_config(temp: &TempDir, group: &str) -> std::path::PathBuf {
    let config_path = temp.path().join("zaz.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[group]]
name = "{group}"
patterns = ["**/*.rs"]

[[group.task]]
name = "noop"
command = "true"
"#
        ),
    )
    .unwrap();
    config_path
}

struct StartedDaemon<'a> {
    current_dir: &'a Path,
    socket: String,
}

impl<'a> StartedDaemon<'a> {
    fn launch(current_dir: &'a Path, config_path: &Path, socket_path: &Path) -> Self {
        let log_path = current_dir.join("zaz.log");
        let socket = socket_path
            .to_str()
            .expect("socket path should be utf-8")
            .to_string();
        let output = run_zaz(
            current_dir,
            &[
                "--config",
                config_path.to_str().expect("config path should be utf-8"),
                "--socket",
                &socket,
                "--log-file",
                log_path.to_str().expect("log path should be utf-8"),
                "start",
            ],
        );
        let stdout = stdout_string(&output);
        let stderr = stderr_string(&output);
        assert!(
            output.status.success(),
            "zaz start exited with {:?}\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        );
        wait_for_daemon(current_dir, socket_path);

        Self {
            current_dir,
            socket,
        }
    }
}

impl Drop for StartedDaemon<'_> {
    fn drop(&mut self) {
        let _ = run_zaz(self.current_dir, &["--socket", &self.socket, "stop"]);
    }
}

#[test]
fn start_then_status_reports_running() {
    let temp = TempDir::new().unwrap();
    let config_path = write_lifecycle_config(&temp, "backend");
    let socket_path = temp.path().join("daemon.sock");

    let _guard = StartedDaemon::launch(temp.path(), &config_path, &socket_path);

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "status"]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}");
    assert!(stdout.contains("Daemon Status:"));
    assert!(stdout.contains("backend"));
}

#[test]
fn start_then_stop_brings_status_back_to_not_running() {
    let temp = TempDir::new().unwrap();
    let config_path = write_lifecycle_config(&temp, "backend");
    let socket_path = temp.path().join("daemon.sock");

    let guard = StartedDaemon::launch(temp.path(), &config_path, &socket_path);
    let socket = socket_path.to_str().unwrap().to_string();
    drop(guard);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = run_zaz(temp.path(), &["--socket", &socket, "status"]);
        if output.status.code() == Some(3) {
            let stdout = stdout_string(&output);
            assert!(stdout.contains("no daemon running at"));
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "status did not report exit code 3 after stop; last code = {:?}",
                output.status.code()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn start_then_restart_all_succeeds() {
    let temp = TempDir::new().unwrap();
    let config_path = write_lifecycle_config(&temp, "backend");
    let socket_path = temp.path().join("daemon.sock");

    let _guard = StartedDaemon::launch(temp.path(), &config_path, &socket_path);

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "restart"]);
    let stdout = stdout_string(&output);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("restart initiated for all groups"));
}

#[test]
fn start_then_restart_named_group_succeeds() {
    let temp = TempDir::new().unwrap();
    let config_path = write_lifecycle_config(&temp, "backend");
    let socket_path = temp.path().join("daemon.sock");

    let _guard = StartedDaemon::launch(temp.path(), &config_path, &socket_path);

    let socket = socket_path.to_str().unwrap();
    let output = run_zaz(temp.path(), &["--socket", socket, "restart", "backend"]);
    let stdout = stdout_string(&output);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("restart initiated for group 'backend'"));
}

#[test]
fn start_is_idempotent_when_daemon_already_running() {
    let temp = TempDir::new().unwrap();
    let config_path = write_lifecycle_config(&temp, "backend");
    let socket_path = temp.path().join("daemon.sock");

    let _guard = StartedDaemon::launch(temp.path(), &config_path, &socket_path);

    let log_path = temp.path().join("zaz.log");
    let output = run_zaz(
        temp.path(),
        &[
            "--config",
            config_path.to_str().unwrap(),
            "--socket",
            socket_path.to_str().unwrap(),
            "--log-file",
            log_path.to_str().unwrap(),
            "start",
        ],
    );
    let stdout = stdout_string(&output);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("daemon already running"),
        "expected idempotent start message, got: {stdout}"
    );
}

trait ChildExt {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl ChildExt for Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}
