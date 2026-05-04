//! Integration tests for `zaz mcp` flag wiring.
//!
//! Cover the new behavior:
//! - explicit `--socket` overrides reach the MCP server (handshake completes)
//! - default behavior surfaces `DaemonNotRunning` when no daemon is up
//! - `--autostart` brings a daemon online before tool calls are serviced

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

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

const TOOLS_CALL_STATUS_REQUEST: &str = concat!(
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","#,
    r#""params":{"name":"zaz_status","arguments":{}}}"#,
    "\n",
);

fn spawn_mcp(args: &[&str], cwd: &Path) -> Child {
    Command::new(zaz_bin())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn zaz mcp")
}

fn read_response_with_id(child_stdout: impl Read, id: u64, deadline: Instant) -> Option<Value> {
    let mut reader = BufReader::new(child_stdout);
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return Some(value);
        }
    }
    None
}

fn await_child_exit(mut child: Child, label: &str) {
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
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn shutdown_daemon(socket_path: &Path) {
    let _ = Command::new(zaz_bin())
        .args(["--socket"])
        .arg(socket_path)
        .arg("stop")
        .output();
}

fn unique_socket_path(temp: &TempDir, label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    temp.path().join(format!("{label}-{nanos}.sock"))
}

#[test]
fn mcp_initialize_with_explicit_socket_completes() {
    // Outside any project tree: no zaz.toml/json anywhere upward. The
    // explicit --socket override must short-circuit socket resolution so the
    // initialize handshake still completes.
    let temp = TempDir::new().unwrap();
    let cwd = temp.path().join("outside");
    std::fs::create_dir_all(&cwd).unwrap();

    let socket = unique_socket_path(&temp, "mcp-init");
    let socket_str = socket.to_string_lossy().into_owned();

    let mut child = spawn_mcp(&["--socket", &socket_str, "mcp"], &cwd);

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(INITIALIZE_REQUEST.as_bytes())
            .expect("write initialize request");
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let response = read_response_with_id(stdout, 1, Instant::now() + Duration::from_secs(5))
        .expect("did not receive initialize response within 5s");

    let server_info = response
        .pointer("/result/serverInfo")
        .unwrap_or_else(|| panic!("response missing result.serverInfo: {response}"));
    assert_eq!(
        server_info.get("name").and_then(Value::as_str),
        Some("zaz-mcp")
    );

    await_child_exit(child, "zaz mcp (--socket)");
}

#[test]
fn mcp_without_autostart_surfaces_daemon_not_running() {
    // Project dir with a config but no daemon running. The handshake should
    // succeed; the tools/call zaz_status should report that the daemon is
    // not running.
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("zaz.toml"), "[settings]\n").unwrap();

    // Pin the socket path so we know exactly what we're checking against.
    let socket = unique_socket_path(&temp, "mcp-noauto");
    let socket_str = socket.to_string_lossy().into_owned();

    let mut child = spawn_mcp(&["--socket", &socket_str, "mcp"], &project);

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(INITIALIZE_REQUEST.as_bytes()).unwrap();
        stdin
            .write_all(INITIALIZED_NOTIFICATION.as_bytes())
            .unwrap();
        stdin
            .write_all(TOOLS_CALL_STATUS_REQUEST.as_bytes())
            .unwrap();
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let response = read_response_with_id(stdout, 2, Instant::now() + Duration::from_secs(5))
        .expect("did not receive tools/call response within 5s");

    // The error may surface as either a JSON-RPC error or a result with
    // isError. Either way, the message must mention the actionable hint.
    let serialized = response.to_string();
    assert!(
        serialized.contains("daemon is not running"),
        "expected 'daemon is not running' in response, got: {serialized}"
    );
    assert!(
        serialized.contains(&socket_str),
        "expected socket path in response, got: {serialized}"
    );

    await_child_exit(child, "zaz mcp (no --autostart)");
}

#[test]
fn mcp_autostart_brings_daemon_online() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("zaz.toml"), "[settings]\n").unwrap();

    let socket = unique_socket_path(&temp, "mcp-autostart");
    let socket_str = socket.to_string_lossy().into_owned();
    // Pin the log file to a writable temp path so the derived
    // `*.daemon-output.log` for the spawned daemon also lands in tmp,
    // mirroring the `zaz start` lifecycle tests.
    let log_file = temp.path().join("zaz.log");
    let log_str = log_file.to_string_lossy().into_owned();

    let mut child = spawn_mcp(
        &[
            "--log-file",
            &log_str,
            "--socket",
            &socket_str,
            "mcp",
            "--autostart",
        ],
        &project,
    );

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(INITIALIZE_REQUEST.as_bytes()).unwrap();
        stdin
            .write_all(INITIALIZED_NOTIFICATION.as_bytes())
            .unwrap();
        stdin
            .write_all(TOOLS_CALL_STATUS_REQUEST.as_bytes())
            .unwrap();
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let response = read_response_with_id(stdout, 2, Instant::now() + Duration::from_secs(15));

    // Always tear down whatever daemon got spawned, even if assertions below
    // fail, so tests don't leak processes.
    let response = match response {
        Some(r) => r,
        None => {
            child.kill().ok();
            shutdown_daemon(&socket);
            panic!("did not receive tools/call response within 15s");
        }
    };

    await_child_exit(child, "zaz mcp (--autostart)");
    let teardown_socket = socket.clone();
    let _guard = scopeguard_drop(move || shutdown_daemon(&teardown_socket));

    // The Status response is wrapped in result.content as MCP tool output.
    let serialized = response.to_string();
    assert!(
        !serialized.contains("daemon is not running"),
        "autostart should have started a daemon; got: {serialized}"
    );
    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("autostart response missing result: {serialized}"));
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        !is_error,
        "tools/call zaz_status reported isError; full response: {serialized}"
    );
}

/// Minimal RAII helper so we don't pull in the `scopeguard` crate just for
/// one test cleanup.
fn scopeguard_drop<F: FnOnce()>(f: F) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}
