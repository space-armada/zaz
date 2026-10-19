mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;
use support::{run_zaz, stderr_string, stdout_string, wait_for_socket_gone, StartedDaemon};
use tempfile::TempDir;

/// A project directory holding a config and a `.zaz/` directory, so the derived
/// socket stays inside the temp tree instead of the user's state directory.
struct Project {
    dir: PathBuf,
    config: PathBuf,
    socket: PathBuf,
}

/// The temp directory with symlinks resolved. On macOS `TempDir` hands back a
/// `/var/...` path while zaz reports the `/private/var/...` the child process
/// resolves its working directory to, so every expected path here is built from
/// the resolved root.
fn root(temp: &TempDir) -> PathBuf {
    temp.path().canonicalize().unwrap()
}

fn project(temp: &TempDir) -> Project {
    let dir = root(temp).join("project");
    let zaz_dir = dir.join(".zaz");
    std::fs::create_dir_all(&zaz_dir).unwrap();

    let config = dir.join("zaz.toml");
    std::fs::write(
        &config,
        r#"
[[group]]
name = "tasks"
patterns = ["**/*.rs"]

[[group.task]]
name = "run"
command = "true"
"#,
    )
    .unwrap();

    Project {
        dir,
        config,
        socket: zaz_dir.join("daemon.sock"),
    }
}

fn nested_dir(project: &Project) -> PathBuf {
    let nested = project.dir.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    nested
}

fn dir_outside(temp: &TempDir) -> PathBuf {
    let outside = root(temp).join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    outside
}

fn text_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn check_validates_the_project_config_from_a_nested_directory() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);

    let output = run_zaz(&nested, &["check"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout_string(&output).starts_with(&format!("{}: ", text_of(&project.config))));
}

#[test]
fn tui_reports_an_invalid_project_config_instead_of_autostarting() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);
    std::fs::write(
        &project.config,
        r#"
[[group]]
name = "tasks"
patterns = ["**/*.rs"]

[[group.task]]
name = "run"
command = "true"

[[group.task]]
name = "run"
command = "true"
"#,
    )
    .unwrap();
    let log_file = root(&temp).join("logs/tui.log");

    let output = run_zaz(&nested, &["--log-file", &text_of(&log_file)]);

    let stderr = stderr_string(&output);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr.contains(&format!("invalid config {}: ", text_of(&project.config))),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("duplicate task name 'run'"),
        "stderr: {stderr}"
    );
    assert!(!project.socket.exists());
}

#[test]
fn task_runs_the_project_config_from_a_nested_directory() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);

    let output = run_zaz(&nested, &["task"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(!stderr_string(&output).contains("no zaz.toml or zaz.json found"));
}

#[test]
fn start_binds_the_project_socket_from_a_nested_directory() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);
    let log_file = text_of(&root(&temp).join("zaz.log"));

    let output = run_zaz(&nested, &["--log-file", &log_file, "start"]);

    // Tear down before asserting, so a failed assertion cannot leak a daemon.
    let socket = text_of(&project.socket);
    let _ = run_zaz(&nested, &["--socket", &socket, "stop"]);
    wait_for_socket_gone(&project.socket, Duration::from_secs(5));

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout_string(&output).contains("daemon started (pid "));
}

#[test]
fn daemon_addresses_the_project_from_a_nested_directory() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);

    let mut running = StartedDaemon::launch(&project.dir, &project.config, &project.socket);

    // Refusing to double-bind is only reachable once the config walk resolved
    // the project above `nested` and derived the socket already in use.
    let output = run_zaz(&nested, &["daemon"]);
    running.stop();

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_string(&output).contains(&format!(
        "daemon already running (socket {} is active)",
        text_of(&project.socket)
    )));
}

#[test]
fn check_names_the_start_dir_when_no_config_exists_above_it() {
    let temp = TempDir::new().unwrap();
    let outside = dir_outside(&temp);

    let output = run_zaz(&outside, &["check"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr_string(&output),
        format!(
            "Error: no zaz.toml or zaz.json found in {} or any parent directory\n       hint: create one, or pass --config <PATH>\n",
            text_of(&outside)
        )
    );
}
