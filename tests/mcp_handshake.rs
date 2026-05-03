use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn zaz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zaz")
}

const INITIALIZE_REQUEST: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","#,
    r#""params":{"protocolVersion":"2025-06-18","capabilities":{},"#,
    r#""clientInfo":{"name":"zaz-test","version":"0"}}}"#,
    "\n",
);

#[test]
fn mcp_initialize_handshake_completes() {
    let mut child = Command::new(zaz_bin())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn zaz mcp");

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(INITIALIZE_REQUEST.as_bytes())
            .expect("write initialize request");
        // Drop stdin to signal EOF; the rmcp server exits its read loop.
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read first stdout line from zaz mcp");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().expect("poll zaz mcp child") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                child.kill().ok();
                panic!("zaz mcp did not exit within 5s after stdin close");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    let response: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|err| panic!("stdout line is not JSON ({err}): {line:?}"));
    let result = response
        .get("result")
        .unwrap_or_else(|| panic!("response missing `result`: {response}"));

    let server_info = result.get("serverInfo").expect("result.serverInfo present");
    assert_eq!(
        server_info.get("name").and_then(Value::as_str),
        Some("zaz-mcp")
    );
    assert!(
        server_info.get("version").and_then(Value::as_str).is_some(),
        "serverInfo.version should be a string"
    );
    assert!(
        result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .is_some(),
        "result.protocolVersion should be a string"
    );
}
