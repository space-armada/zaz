//! Golden-path coverage for the watcher-to-trigger-to-execution pipeline.
//!
//! A group's task is marked `on_change_only`, so it is skipped on initial
//! startup and only runs once the daemon's file watcher observes a matching
//! change. Seeing the task's marker output only after the watched file is
//! edited is proof the watcher, debounce, and execution path work together,
//! not that the task happened to run for some other reason.

mod support;

use std::path::PathBuf;
use std::time::Duration;
use support::{await_log_lines, get_logs, StartedDaemon};
use tempfile::TempDir;

fn write_config(temp: &TempDir) -> PathBuf {
    let config_path = temp.path().join("zaz.toml");
    std::fs::write(
        &config_path,
        r#"
[settings]
debounce = "20ms"

[[group]]
name = "watched"
patterns = ["**/watched.txt"]

[[group.task]]
name = "on-change"
command = "echo TRIGGERED-BY-WATCHER"
on_change_only = true
"#,
    )
    .unwrap();
    config_path
}

#[test]
fn file_change_triggers_on_change_only_task() {
    let temp = TempDir::new().unwrap();
    let config_path = write_config(&temp);
    let socket_path = temp.path().join("daemon.sock");
    std::fs::write(temp.path().join("watched.txt"), "initial\n").unwrap();

    let _guard = StartedDaemon::launch(temp.path(), &config_path, &socket_path);

    let startup_logs = get_logs(
        &socket_path,
        None,
        "*",
        None,
        Some(1024),
        Some("TRIGGERED-BY-WATCHER"),
    );
    assert_eq!(
        startup_logs.total_count.unwrap_or(startup_logs.lines.len()),
        0,
        "on_change_only task ran before any file change: {:?}",
        startup_logs.lines
    );

    std::fs::write(temp.path().join("watched.txt"), "changed\n").unwrap();

    let lines = await_log_lines(
        &socket_path,
        None,
        "*",
        "TRIGGERED-BY-WATCHER",
        1,
        Duration::from_secs(10),
    );
    assert!(
        lines
            .iter()
            .any(|l| l.content.contains("TRIGGERED-BY-WATCHER")),
        "watcher-triggered task output missing: {lines:?}"
    );
}
