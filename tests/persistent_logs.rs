//! End-to-end coverage for SQLite log persistence.
//!
//! Each test boots a real daemon with `backend = "sqlite"`, drives output
//! through normal process commands, stops the daemon, swaps the config to a
//! non-emitting variant (so the post-restart daemon does not re-emit), and
//! asserts the pre-restart lines come back through both the daemon API
//! (`ApiRequest::GetLogs` over a raw Unix socket) and the MCP `zaz_logs`
//! tool.
//!
//! `XDG_STATE_HOME` is overridden per test so the SQLite DB lands under a
//! per-test tempdir instead of the user's real state directory.

use serde_json::{json, Value};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use zaz_daemon::{ApiRequest, ApiResponse, LogLine};
use zaz_mcp::LogsReport;

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

/// Per-test XDG override dirs. `state` holds the SQLite DB, `config` holds
/// the user config that turns SQLite on.
#[derive(Clone)]
struct XdgDirs {
    state: PathBuf,
    config: PathBuf,
}

fn xdg_dirs(temp: &TempDir) -> XdgDirs {
    let state = temp.path().join("state");
    let config = temp.path().join("config");
    std::fs::create_dir_all(&state).expect("create xdg state dir");
    std::fs::create_dir_all(config.join("zaz")).expect("create xdg config/zaz dir");
    XdgDirs { state, config }
}

/// Writes the SQLite-enabled user config to `<xdg.config>/zaz/config.toml`.
fn write_user_config(xdg: &XdgDirs) {
    let path = xdg.config.join("zaz").join("config.toml");
    std::fs::write(
        path,
        r#"
[log_storage]
backend = "sqlite"

[log_storage.sqlite]
max_size = "64MB"
max_lines_per_process = 100000
"#,
    )
    .expect("write user config");
}

fn run_zaz<I, S>(current_dir: &Path, xdg: &XdgDirs, args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(zaz_bin())
        .args(args)
        .current_dir(current_dir)
        .env("XDG_STATE_HOME", &xdg.state)
        .env("XDG_CONFIG_HOME", &xdg.config)
        .output()
        .expect("failed to run zaz binary")
}

fn unique_socket_path(temp: &TempDir, label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    temp.path().join(format!("{label}-{nanos}.sock"))
}

fn write_config(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write config");
}

/// Emits `n` numbered lines under `marker`, then sleeps so the process stays
/// up. PTY mode is fine: the engine's line reader trims trailing CR/LF
/// before logging, so content assertions compare against exact strings.
fn emitter_config(marker: &str, n: usize) -> String {
    let mut echos = String::new();
    for i in 1..=n {
        echos.push_str(&format!("echo {marker}-{i:02}; "));
    }
    format!(
        r#"
[[group]]
name = "g"
patterns = []

[[group.daemon]]
name = "emitter"
command = "sh -c '{echos}sleep 600'"
"#
    )
}

/// Variant of `emitter_config` that tags some lines with the substring
/// `needle` so the search assertion has something to match.
fn emitter_with_needle_config(marker: &str, n: usize, needle_indices: &[usize]) -> String {
    let mut echos = String::new();
    for i in 1..=n {
        if needle_indices.contains(&i) {
            echos.push_str(&format!("echo {marker}-{i:02} needle; "));
        } else {
            echos.push_str(&format!("echo {marker}-{i:02}; "));
        }
    }
    format!(
        r#"
[[group]]
name = "g"
patterns = []

[[group.daemon]]
name = "emitter"
command = "sh -c '{echos}sleep 600'"
"#
    )
}

fn two_emitters_config() -> String {
    let body = |name: &str, marker: &str| {
        format!(
            r#"
[[group.daemon]]
name = "{name}"
command = "sh -c 'echo {marker}-LINE-1; echo {marker}-LINE-2; echo {marker}-LINE-3; sleep 600'"
"#
        )
    };
    format!(
        r#"
[[group]]
name = "g"
patterns = []
{a}{b}"#,
        a = body("emitter-a", "A"),
        b = body("emitter-b", "B"),
    )
}

/// Config used after restart so the new daemon does not re-emit anything.
fn quiet_config() -> &'static str {
    r#"
[[group]]
name = "g"
patterns = []

[[group.daemon]]
name = "quiet"
command = "sleep 600"
"#
}

struct StartedDaemon {
    current_dir: PathBuf,
    socket: PathBuf,
    xdg: XdgDirs,
    config: PathBuf,
    stopped: bool,
}

impl StartedDaemon {
    fn launch(current_dir: &Path, xdg: &XdgDirs, config: &Path, socket: &Path) -> Self {
        let log_path = current_dir.join("zaz.log");
        let args: Vec<&OsStr> = vec![
            OsStr::new("--config"),
            config.as_os_str(),
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("--log-file"),
            log_path.as_os_str(),
            OsStr::new("start"),
        ];
        let output = run_zaz(current_dir, xdg, args);
        if !output.status.success() {
            let daemon_log = current_dir.join("zaz.daemon-output.log");
            let daemon_log_contents = std::fs::read_to_string(&daemon_log).unwrap_or_else(|e| {
                format!("(no daemon-output.log at {}: {e})", daemon_log.display())
            });
            panic!(
                "zaz start exited with {:?}\nstdout: {}\nstderr: {}\ndaemon-output.log:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                daemon_log_contents,
            );
        }
        Self::wait_for_ready(current_dir, xdg, socket);
        Self {
            current_dir: current_dir.to_path_buf(),
            socket: socket.to_path_buf(),
            xdg: xdg.clone(),
            config: config.to_path_buf(),
            stopped: false,
        }
    }

    fn wait_for_ready(current_dir: &Path, xdg: &XdgDirs, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let out = run_zaz(
                current_dir,
                xdg,
                [
                    OsStr::new("--socket"),
                    socket.as_os_str(),
                    OsStr::new("status"),
                ],
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            if out.status.code() == Some(0) && stdout.contains("Daemon Status:") {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not become ready in time");
    }

    /// Stop the daemon and wait for the socket file to disappear so a
    /// subsequent `zaz start` can bind cleanly.
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        let _ = run_zaz(
            &self.current_dir,
            &self.xdg,
            [
                OsStr::new("--socket"),
                self.socket.as_os_str(),
                OsStr::new("stop"),
            ],
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !self.socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        self.stopped = true;
    }
}

impl Drop for StartedDaemon {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = run_zaz(
                &self.current_dir,
                &self.xdg,
                [
                    OsStr::new("--socket"),
                    self.socket.as_os_str(),
                    OsStr::new("stop"),
                ],
            );
        }
    }
}

/// Stop the daemon, swap the config file in place, and relaunch against the
/// same socket and config path so the SQLite hash resolves to the same DB.
fn restart_daemon(guard: StartedDaemon, new_config_body: &str) -> StartedDaemon {
    let current_dir = guard.current_dir.clone();
    let xdg = guard.xdg.clone();
    let config = guard.config.clone();
    let socket = guard.socket.clone();
    let mut guard = guard;
    guard.stop();
    drop(guard);
    write_config(&config, new_config_body);
    StartedDaemon::launch(&current_dir, &xdg, &config, &socket)
}

fn spawn_mcp(socket: &Path, cwd: &Path, xdg: &XdgDirs) -> Child {
    Command::new(zaz_bin())
        .args([
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("mcp"),
        ])
        .current_dir(cwd)
        .env("XDG_STATE_HOME", &xdg.state)
        .env("XDG_CONFIG_HOME", &xdg.config)
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

fn call_tool(socket: &Path, cwd: &Path, xdg: &XdgDirs, tool: &str, arguments: Value) -> Value {
    let mut child = spawn_mcp(socket, cwd, xdg);
    let request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
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
    await_child_exit(&mut child, &format!("zaz mcp ({tool})"));
    response
}

fn parse_logs_report(response: &Value) -> LogsReport {
    let structured = response
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("response missing result.structuredContent: {response}"));
    assert!(
        response.get("error").is_none(),
        "expected success result, got JSON-RPC error: {response}"
    );
    let is_error = response
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(!is_error, "tool result reported isError=true: {response}");
    serde_json::from_value(structured.clone())
        .unwrap_or_else(|e| panic!("structuredContent did not parse ({e}): {structured}"))
}

fn mcp_logs(
    socket: &Path,
    cwd: &Path,
    xdg: &XdgDirs,
    name: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    search: Option<&str>,
) -> LogsReport {
    let mut args = json!({"name": name});
    if let Some(offset) = offset {
        args["offset"] = json!(offset);
    }
    if let Some(limit) = limit {
        args["limit"] = json!(limit);
    }
    if let Some(search) = search {
        args["search"] = json!(search);
    }
    let response = call_tool(socket, cwd, xdg, "zaz_logs", args);
    parse_logs_report(&response)
}

/// Direct daemon-API client: raw Unix socket, one newline-terminated JSON
/// request, one newline-terminated JSON response. Returns the
/// `(lines, total_count, has_more, offset)` fields of `ApiResponse::Logs`.
struct DaemonLogs {
    lines: Vec<LogLine>,
    total_count: Option<usize>,
    has_more: Option<bool>,
}

fn daemon_get_logs(
    socket: &Path,
    name: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    search: Option<&str>,
) -> DaemonLogs {
    let request = ApiRequest::GetLogs {
        name: name.to_string(),
        project: None,
        lines: None,
        offset,
        limit,
        search: search.map(|s| s.to_string()),
    };
    let mut stream = UnixStream::connect(socket).expect("connect daemon socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut payload = serde_json::to_string(&request).expect("serialize ApiRequest");
    payload.push('\n');
    stream
        .write_all(payload.as_bytes())
        .expect("write GetLogs request");
    let mut response = String::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).expect("read daemon response");
        if n == 0 {
            break;
        }
        response.push_str(std::str::from_utf8(&buf[..n]).expect("utf-8 response"));
        if response.contains('\n') {
            break;
        }
    }
    let trimmed = response.trim_end_matches('\n').to_string();
    let parsed: ApiResponse = serde_json::from_str(&trimmed)
        .unwrap_or_else(|e| panic!("parse ApiResponse failed ({e}): {trimmed}"));
    match parsed {
        ApiResponse::Logs {
            lines,
            total_count,
            has_more,
            ..
        } => DaemonLogs {
            lines,
            total_count,
            has_more,
        },
        other => panic!("expected ApiResponse::Logs, got {other:?}"),
    }
}

/// Poll the daemon API until the count of lines matching `search` under
/// `name` reaches `expected` or the deadline trips. Scoping by `search`
/// keeps the count off internal daemon-source lines that share the same
/// `process` field as their owning daemon entry.
fn await_log_count(
    socket: &Path,
    name: &str,
    search: &str,
    expected: usize,
    timeout: Duration,
) -> usize {
    let deadline = Instant::now() + timeout;
    let mut last = 0;
    let mut last_lines: Vec<LogLine> = Vec::new();
    while Instant::now() < deadline {
        let logs = daemon_get_logs(socket, name, None, Some(1024), Some(search));
        last = logs.total_count.unwrap_or(logs.lines.len());
        last_lines = logs.lines;
        if last >= expected {
            return last;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let observed: Vec<String> = last_lines
        .iter()
        .map(|l| {
            format!(
                "[{:?}/{:?}] {}: {}",
                l.source, l.output_kind, l.process, l.content
            )
        })
        .collect();
    panic!(
        "timed out waiting for {expected} `{name}` log lines matching {search:?}; last observed {last} within {timeout:?}\nlines:\n{}",
        observed.join("\n")
    );
}

#[test]
fn persisted_logs_survive_daemon_restart() {
    let temp = TempDir::new().unwrap();
    let xdg = xdg_dirs(&temp);
    write_user_config(&xdg);
    let config_path = temp.path().join("zaz.toml");
    let socket = unique_socket_path(&temp, "persist-restart");

    write_config(&config_path, &emitter_config("PRE-LINE", 5));
    let guard = StartedDaemon::launch(temp.path(), &xdg, &config_path, &socket);
    await_log_count(&socket, "emitter", "PRE-LINE", 5, Duration::from_secs(10));

    let guard = restart_daemon(guard, quiet_config());

    let api = daemon_get_logs(&socket, "emitter", None, Some(10), Some("PRE-LINE"));
    assert_eq!(api.total_count, Some(5), "daemon API total_count");
    assert_eq!(api.lines.len(), 5, "daemon API page size");
    assert_eq!(api.has_more, Some(false));
    let api_contents: Vec<&str> = api.lines.iter().map(|l| l.content.as_str()).collect();
    assert_eq!(
        api_contents,
        vec![
            "PRE-LINE-01",
            "PRE-LINE-02",
            "PRE-LINE-03",
            "PRE-LINE-04",
            "PRE-LINE-05",
        ],
        "daemon API contents (oldest first)"
    );

    let mcp = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "emitter",
        None,
        Some(10),
        Some("PRE-LINE"),
    );
    assert_eq!(mcp.total_count, Some(5), "mcp total_count");
    assert_eq!(mcp.entries.len(), 5, "mcp page size");
    let mcp_contents: Vec<&str> = mcp.entries.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(mcp_contents, api_contents, "mcp contents match daemon API");

    drop(guard);
}

#[test]
fn paginated_and_search_queries_against_persisted_logs() {
    let temp = TempDir::new().unwrap();
    let xdg = xdg_dirs(&temp);
    write_user_config(&xdg);
    let config_path = temp.path().join("zaz.toml");
    let socket = unique_socket_path(&temp, "persist-page");

    let needle_indices = vec![3usize, 7, 11, 15, 19];
    write_config(
        &config_path,
        &emitter_with_needle_config("LINE", 25, &needle_indices),
    );

    let guard = StartedDaemon::launch(temp.path(), &xdg, &config_path, &socket);
    await_log_count(&socket, "emitter", "LINE-", 25, Duration::from_secs(15));

    let guard = restart_daemon(guard, quiet_config());

    // First page. `search = "LINE-"` scopes assertions to the 25 emitter
    // stdout lines, skipping internal daemon-source entries the engine
    // logs under the same process name.
    let api_p0 = daemon_get_logs(&socket, "emitter", None, Some(10), Some("LINE-"));
    assert_eq!(api_p0.total_count, Some(25));
    assert_eq!(api_p0.has_more, Some(true));
    assert_eq!(api_p0.lines.len(), 10);
    let api_p0_contents: Vec<&str> = api_p0.lines.iter().map(|l| l.content.as_str()).collect();
    let expected_p0: Vec<String> = (1..=10)
        .map(|i| format_line("LINE", i, &needle_indices))
        .collect();
    let expected_p0_refs: Vec<&str> = expected_p0.iter().map(String::as_str).collect();
    assert_eq!(
        api_p0_contents, expected_p0_refs,
        "daemon API page 0 contents"
    );

    let mcp_p0 = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "emitter",
        None,
        Some(10),
        Some("LINE-"),
    );
    assert_eq!(mcp_p0.total_count, Some(25));
    assert_eq!(mcp_p0.has_more, Some(true));
    let mcp_p0_contents: Vec<&str> = mcp_p0.entries.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(mcp_p0_contents, expected_p0_refs, "mcp page 0 contents");

    // Second page.
    let api_p1 = daemon_get_logs(&socket, "emitter", Some(10), Some(10), Some("LINE-"));
    assert_eq!(api_p1.total_count, Some(25));
    assert_eq!(api_p1.has_more, Some(true));
    let api_p1_contents: Vec<&str> = api_p1.lines.iter().map(|l| l.content.as_str()).collect();
    let expected_p1: Vec<String> = (11..=20)
        .map(|i| format_line("LINE", i, &needle_indices))
        .collect();
    let expected_p1_refs: Vec<&str> = expected_p1.iter().map(String::as_str).collect();
    assert_eq!(
        api_p1_contents, expected_p1_refs,
        "daemon API page 1 contents"
    );

    let mcp_p1 = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "emitter",
        Some(10),
        Some(10),
        Some("LINE-"),
    );
    let mcp_p1_contents: Vec<&str> = mcp_p1.entries.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(mcp_p1_contents, expected_p1_refs, "mcp page 1 contents");

    // Third (partial) page.
    let api_p2 = daemon_get_logs(&socket, "emitter", Some(20), Some(10), Some("LINE-"));
    assert_eq!(api_p2.total_count, Some(25));
    assert_eq!(api_p2.has_more, Some(false));
    assert_eq!(api_p2.lines.len(), 5);

    let mcp_p2 = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "emitter",
        Some(20),
        Some(10),
        Some("LINE-"),
    );
    assert_eq!(mcp_p2.total_count, Some(25));
    assert_eq!(mcp_p2.has_more, Some(false));
    assert_eq!(mcp_p2.entries.len(), 5);

    // Search.
    let api_search = daemon_get_logs(&socket, "emitter", None, Some(50), Some("needle"));
    assert_eq!(api_search.total_count, Some(needle_indices.len()));
    assert_eq!(api_search.lines.len(), needle_indices.len());
    for line in &api_search.lines {
        assert!(
            line.content.contains("needle"),
            "daemon API search hit lacked needle: {}",
            line.content
        );
    }

    let mcp_search = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "emitter",
        None,
        Some(50),
        Some("needle"),
    );
    assert_eq!(mcp_search.total_count, Some(needle_indices.len()));
    assert_eq!(mcp_search.entries.len(), needle_indices.len());
    for entry in &mcp_search.entries {
        assert!(
            entry.content.contains("needle"),
            "mcp search hit lacked needle: {}",
            entry.content
        );
    }

    drop(guard);
}

fn format_line(marker: &str, i: usize, needle_indices: &[usize]) -> String {
    if needle_indices.contains(&i) {
        format!("{marker}-{i:02} needle")
    } else {
        format!("{marker}-{i:02}")
    }
}

#[test]
fn process_filter_against_persisted_logs() {
    let temp = TempDir::new().unwrap();
    let xdg = xdg_dirs(&temp);
    write_user_config(&xdg);
    let config_path = temp.path().join("zaz.toml");
    let socket = unique_socket_path(&temp, "persist-filter");

    write_config(&config_path, &two_emitters_config());
    let guard = StartedDaemon::launch(temp.path(), &xdg, &config_path, &socket);
    await_log_count(&socket, "emitter-a", "A-LINE-", 3, Duration::from_secs(10));
    await_log_count(&socket, "emitter-b", "B-LINE-", 3, Duration::from_secs(10));

    let guard = restart_daemon(guard, quiet_config());

    let api_a = daemon_get_logs(&socket, "emitter-a", None, Some(20), Some("A-LINE-"));
    assert_eq!(api_a.total_count, Some(3));
    let a_contents: Vec<&str> = api_a.lines.iter().map(|l| l.content.as_str()).collect();
    assert_eq!(a_contents, vec!["A-LINE-1", "A-LINE-2", "A-LINE-3"]);
    for line in &api_a.lines {
        assert_eq!(line.process, "emitter-a");
    }

    let api_b = daemon_get_logs(&socket, "emitter-b", None, Some(20), Some("B-LINE-"));
    assert_eq!(api_b.total_count, Some(3));
    let b_contents: Vec<&str> = api_b.lines.iter().map(|l| l.content.as_str()).collect();
    assert_eq!(b_contents, vec!["B-LINE-1", "B-LINE-2", "B-LINE-3"]);
    for line in &api_b.lines {
        assert_eq!(line.process, "emitter-b");
    }

    // "LINE-" matches both A-LINE-* and B-LINE-* but no daemon-source
    // text, so the cross-process query returns exactly six entries.
    let api_all = daemon_get_logs(&socket, "*", None, Some(20), Some("LINE-"));
    assert_eq!(api_all.total_count, Some(6));
    let processes: std::collections::HashSet<&str> =
        api_all.lines.iter().map(|l| l.process.as_str()).collect();
    assert!(processes.contains("emitter-a"));
    assert!(processes.contains("emitter-b"));

    let mcp_a = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "emitter-a",
        None,
        Some(20),
        Some("A-LINE-"),
    );
    assert_eq!(mcp_a.total_count, Some(3));
    let mcp_a_contents: Vec<&str> = mcp_a.entries.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(mcp_a_contents, vec!["A-LINE-1", "A-LINE-2", "A-LINE-3"]);

    let mcp_b = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "emitter-b",
        None,
        Some(20),
        Some("B-LINE-"),
    );
    assert_eq!(mcp_b.total_count, Some(3));
    let mcp_b_contents: Vec<&str> = mcp_b.entries.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(mcp_b_contents, vec!["B-LINE-1", "B-LINE-2", "B-LINE-3"]);

    let mcp_all = mcp_logs(
        &socket,
        temp.path(),
        &xdg,
        "*",
        None,
        Some(20),
        Some("LINE-"),
    );
    assert_eq!(mcp_all.total_count, Some(6));

    drop(guard);
}
