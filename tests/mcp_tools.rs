//! Integration tests for the eight MCP tools against a running zaz daemon.
//!
//! Each test brings up a real daemon via `zaz start`, spawns `zaz mcp`,
//! drives the JSON-RPC handshake and a single `tools/call`, then asserts on
//! the structured response. The daemon is torn down by an RAII guard so a
//! failed assertion does not leak processes.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use zaz_mcp::{ConfigReport, GroupsReport, LogsReport, MutationReport, StatusReport};

fn zaz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zaz")
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

fn run_zaz(current_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(zaz_bin())
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("failed to run zaz binary")
}

fn write_test_config(temp: &TempDir) -> PathBuf {
    let config_path = temp.path().join("zaz.toml");
    std::fs::write(
        &config_path,
        r#"
[[group]]
name = "backend"
patterns = ["**/*.rs"]

[[group.task]]
name = "noop"
command = "true"

[[group.daemon]]
name = "sleeper"
command = "sleep 60"
"#,
    )
    .unwrap();
    config_path
}

fn unique_socket_path(temp: &TempDir, label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    temp.path().join(format!("{label}-{nanos}.sock"))
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
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "zaz start exited with {:?}\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        );
        Self::wait_for_ready(current_dir, socket_path);

        Self {
            current_dir,
            socket,
        }
    }

    fn wait_for_ready(current_dir: &Path, socket_path: &Path) {
        let socket = socket_path.to_str().expect("socket path should be utf-8");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = run_zaz(current_dir, &["--socket", socket, "status"]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            if output.status.code() == Some(0) && stdout.contains("Daemon Status:") {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not become ready in time");
    }
}

impl Drop for StartedDaemon<'_> {
    fn drop(&mut self) {
        let _ = run_zaz(self.current_dir, &["--socket", &self.socket, "stop"]);
    }
}

fn spawn_mcp(socket_str: &str, cwd: &Path) -> Child {
    Command::new(zaz_bin())
        .args(["--socket", socket_str, "mcp"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn zaz mcp")
}

fn read_response_with_id(child: &mut Child, id: u64, timeout: Duration) -> Value {
    let stdout = child
        .stdout
        .take()
        .expect("zaz mcp child should have piped stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => panic!("read_line failed: {e}"),
        }
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return value;
        }
    }
    panic!("did not receive JSON-RPC response with id {id} within {timeout:?}");
}

fn await_child_exit(child: &mut Child, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child
            .try_wait()
            .unwrap_or_else(|e| panic!("poll {label}: {e}"))
        {
            Some(_) => return,
            None if Instant::now() >= deadline => {
                child.kill().ok();
                panic!("{label} did not exit within 5s after stdin close");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Spawn `zaz mcp`, run the initialize/initialized/tools-call sequence for a
/// single tool, and return the JSON-RPC response value (always with id=2).
fn call_tool(socket_path: &Path, cwd: &Path, tool_name: &str, arguments: Value) -> Value {
    let socket_str = socket_path.to_string_lossy().into_owned();
    let mut child = spawn_mcp(&socket_str, cwd);

    let request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": tool_name, "arguments": arguments},
    });
    let mut request_line = serde_json::to_string(&request).expect("serialize tools/call request");
    request_line.push('\n');

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(INITIALIZE_REQUEST.as_bytes()).unwrap();
        stdin
            .write_all(INITIALIZED_NOTIFICATION.as_bytes())
            .unwrap();
        stdin.write_all(request_line.as_bytes()).unwrap();
    }

    let response = read_response_with_id(&mut child, 2, Duration::from_secs(10));
    await_child_exit(&mut child, &format!("zaz mcp ({tool_name})"));
    response
}

fn structured_content(response: &Value) -> &Value {
    response
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("response missing result.structuredContent: {response}"))
}

fn parse_structured<T: serde::de::DeserializeOwned>(response: &Value) -> T {
    let sc = structured_content(response);
    serde_json::from_value(sc.clone())
        .unwrap_or_else(|e| panic!("structuredContent did not parse ({e}): {sc}"))
}

fn assert_not_error(response: &Value) {
    assert!(
        response.get("error").is_none(),
        "expected success result, got JSON-RPC error: {response}"
    );
    let is_error = response
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(!is_error, "tool result reported isError=true: {response}");
}

#[test]
fn mcp_zaz_status_against_running_daemon() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-status");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(&socket, temp.path(), "zaz_status", json!({}));

    assert_not_error(&response);
    let report: StatusReport = parse_structured(&response);
    let group_names: Vec<&str> = report.groups.iter().map(|g| g.name.as_str()).collect();
    assert!(
        group_names.contains(&"backend"),
        "expected `backend` in groups, got {group_names:?}"
    );
}

#[test]
fn mcp_zaz_list_groups_returns_summary() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-list");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(&socket, temp.path(), "zaz_list_groups", json!({}));

    assert_not_error(&response);
    let report: GroupsReport = parse_structured(&response);
    assert_eq!(
        report.groups.len(),
        1,
        "expected exactly one group, got {:?}",
        report.groups
    );
    let group = &report.groups[0];
    assert_eq!(group.name, "backend");
    assert_eq!(group.task_count, 1, "task_count mismatch: {group:?}");
    assert_eq!(group.daemon_count, 1, "daemon_count mismatch: {group:?}");
}

#[test]
fn mcp_zaz_logs_returns_paginated_envelope() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-logs");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(&socket, temp.path(), "zaz_logs", json!({}));

    assert_not_error(&response);
    let report: LogsReport = parse_structured(&response);
    assert_eq!(report.name, "*", "default name should resolve to wildcard");
}

#[test]
fn mcp_zaz_config_returns_parsed_config() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-config");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(&socket, temp.path(), "zaz_config", json!({}));

    assert_not_error(&response);
    let report: ConfigReport = parse_structured(&response);
    assert!(
        report.path.ends_with("zaz.toml"),
        "config path should end with zaz.toml, got {:?}",
        report.path
    );
    assert_eq!(report.groups.len(), 1);
    let group = &report.groups[0];
    assert_eq!(group.name, "backend");
    assert_eq!(group.tasks.len(), 1);
    assert_eq!(group.tasks[0].name, "noop");
    assert_eq!(group.daemons.len(), 1);
    assert_eq!(group.daemons[0].name, "sleeper");
}

#[test]
fn mcp_zaz_restart_group_succeeds() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-restart-group");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(
        &socket,
        temp.path(),
        "zaz_restart_group",
        json!({"name": "backend"}),
    );

    assert_not_error(&response);
    let report: MutationReport = parse_structured(&response);
    assert!(
        report.message.contains("backend"),
        "restart_group message should mention the group name, got: {}",
        report.message
    );
}

#[test]
fn mcp_zaz_restart_process_succeeds() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-restart-proc");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(
        &socket,
        temp.path(),
        "zaz_restart_process",
        json!({"group": "backend", "process": "sleeper"}),
    );

    assert_not_error(&response);
    let report: MutationReport = parse_structured(&response);
    assert!(
        report.message.contains("sleeper"),
        "restart_process message should mention the process name, got: {}",
        report.message
    );
}

#[test]
fn mcp_zaz_restart_all_succeeds() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-restart-all");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(&socket, temp.path(), "zaz_restart_all", json!({}));

    assert_not_error(&response);
    let report: MutationReport = parse_structured(&response);
    assert!(
        report.message.contains("all groups"),
        "restart_all message should mention all groups, got: {}",
        report.message
    );
}

#[test]
fn mcp_zaz_reload_config_succeeds() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-reload");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(&socket, temp.path(), "zaz_reload_config", json!({}));

    assert_not_error(&response);
    let report: MutationReport = parse_structured(&response);
    assert!(
        report.message.contains("config reloaded"),
        "reload_config message should mention 'config reloaded', got: {}",
        report.message
    );
}

#[test]
fn mcp_zaz_restart_group_unknown_returns_error() {
    let temp = TempDir::new().unwrap();
    let config = write_test_config(&temp);
    let socket = unique_socket_path(&temp, "mcp-restart-bad");
    let _guard = StartedDaemon::launch(temp.path(), &config, &socket);

    let response = call_tool(
        &socket,
        temp.path(),
        "zaz_restart_group",
        json!({"name": "does-not-exist"}),
    );

    let serialized = response.to_string();
    let has_jsonrpc_error = response.get("error").is_some();
    let has_tool_error = response
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        has_jsonrpc_error || has_tool_error,
        "expected an error response for unknown group, got: {serialized}"
    );
    assert!(
        serialized.contains("does-not-exist"),
        "error response should mention the unknown group name, got: {serialized}"
    );
    assert!(
        serialized.contains("restart_group"),
        "error response should mention the operation, got: {serialized}"
    );
}
