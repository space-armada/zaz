mod support;

use std::path::{Path, PathBuf};
use support::{run_zaz, stderr_string, stdout_string};
use tempfile::TempDir;

/// A project directory holding a config and a `.zaz/` directory, so the derived
/// socket stays inside the temp tree instead of the user's state directory.
struct Project {
    dir: PathBuf,
    config: PathBuf,
    socket: PathBuf,
}

/// The temp directory with symlinks resolved. On macOS `TempDir` hands back a
/// `/var/...` path while the daemon reports the `/private/var/...` it canonicalizes
/// to, so every expected path in this file is built from the resolved root.
fn root(temp: &TempDir) -> PathBuf {
    temp.path().canonicalize().unwrap()
}

fn project_with(temp: &TempDir, filename: &str, contents: &str) -> Project {
    let dir = root(temp).join("project");
    let zaz_dir = dir.join(".zaz");
    std::fs::create_dir_all(&zaz_dir).unwrap();

    let config = dir.join(filename);
    std::fs::write(&config, contents).unwrap();

    Project {
        dir,
        config,
        socket: zaz_dir.join("daemon.sock"),
    }
}

fn project(temp: &TempDir) -> Project {
    project_with(temp, "zaz.toml", "")
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
fn restart_discovers_project_socket_from_nested_directory() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);

    let output = run_zaz(&nested, &["restart"]);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains(&format!(
        "no daemon running for {}",
        text_of(&project.config)
    )));
    assert!(stderr.contains(&format!("     socket: {}", text_of(&project.socket))));
    assert!(!stderr.contains("could not resolve daemon socket"));
}

#[test]
fn restart_uses_explicit_socket_outside_any_project() {
    let temp = TempDir::new().unwrap();
    let current_dir = dir_outside(&temp);

    let explicit_socket = root(&temp).join("explicit.sock");
    let explicit_socket_string = text_of(&explicit_socket);
    let output = run_zaz(
        &current_dir,
        &["--socket", &explicit_socket_string, "restart"],
    );
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains(&format!("no daemon running at {explicit_socket_string}")));
    assert!(!stderr.contains("could not resolve daemon socket"));
}

#[test]
fn restart_errors_with_actionable_message_outside_project() {
    let temp = TempDir::new().unwrap();
    let current_dir = dir_outside(&temp);

    let output = run_zaz(&current_dir, &["restart"]);
    let stderr = stderr_string(&output);

    assert!(!output.status.success());
    assert!(stderr.contains("could not resolve daemon socket from"));
    assert!(stderr.contains(current_dir.to_string_lossy().as_ref()));
    assert!(stderr.contains("--socket <PATH>"));
}

#[test]
fn restart_discovers_json_project_when_toml_is_absent() {
    let temp = TempDir::new().unwrap();
    let project = project_with(&temp, "zaz.json", "{}");
    let nested = nested_dir(&project);

    let output = run_zaz(&nested, &["restart"]);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains(&format!(
        "no daemon running for {}",
        text_of(&project.config)
    )));
    assert!(stderr.contains(&format!("     socket: {}", text_of(&project.socket))));
    assert!(!stderr.contains("could not resolve daemon socket"));
}

#[test]
fn no_daemon_block_explains_nothing_for_an_explicit_socket() {
    let temp = TempDir::new().unwrap();
    let current_dir = dir_outside(&temp);
    let socket = text_of(&root(&temp).join("explicit.sock"));

    let output = run_zaz(&current_dir, &["--socket", &socket, "status"]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        stdout,
        format!("no daemon running at {socket}\n       hint: start one with `zaz start`\n")
    );
}

#[test]
fn no_daemon_block_shows_the_socket_for_a_config_the_operator_named() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let current_dir = dir_outside(&temp);

    let output = run_zaz(
        &current_dir,
        &["--config", &text_of(&project.config), "status"],
    );
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        stdout,
        format!(
            "no daemon running for {}\n     socket: {}\n       hint: start one with `zaz start`\n",
            text_of(&project.config),
            text_of(&project.socket)
        )
    );
}

#[test]
fn no_daemon_block_stays_quiet_about_a_config_in_the_current_directory() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);

    let output = run_zaz(&project.dir, &["status"]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        stdout,
        format!(
            "no daemon running for {}\n     socket: {}\n       hint: start one with `zaz start`\n",
            text_of(&project.config),
            text_of(&project.socket)
        )
    );
}

#[test]
fn no_daemon_block_explains_a_config_found_by_the_upward_walk() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);

    let output = run_zaz(&nested, &["status"]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        stdout,
        format!(
            "no daemon running for {}\n     socket: {}\n     config: found by searching upward from the current directory\n       hint: start one with `zaz start`\n",
            text_of(&project.config),
            text_of(&project.socket)
        )
    );
}

#[test]
fn stop_prints_the_block_without_a_hint_and_exits_zero() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);

    let output = run_zaz(&nested, &["stop"]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout,
        format!(
            "no daemon running for {}\n     socket: {}\n     config: found by searching upward from the current directory\n",
            text_of(&project.config),
            text_of(&project.socket)
        )
    );
}

#[test]
fn reload_reports_the_block_on_stderr_under_the_error_prefix() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let nested = nested_dir(&project);

    let output = run_zaz(&nested, &["reload"]);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr,
        format!(
            "Error: no daemon running for {}\n     socket: {}\n     config: found by searching upward from the current directory\n       hint: start one with `zaz start`\n",
            text_of(&project.config),
            text_of(&project.socket)
        )
    );
}
