mod support;

use support::{run_zaz, stderr_string};
use tempfile::TempDir;

#[test]
fn restart_discovers_project_socket_from_nested_directory() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("project");
    let nested_dir = project_dir.join("a/b/c");
    let zaz_dir = project_dir.join(".zaz");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::create_dir_all(&zaz_dir).unwrap();
    std::fs::write(project_dir.join("zaz.toml"), "").unwrap();

    let output = run_zaz(&nested_dir, &["restart"]);
    let stderr = stderr_string(&output);
    let expected_socket = zaz_dir.join("daemon.sock");

    assert!(!output.status.success());
    assert!(stderr.contains("no daemon running at"));
    assert!(stderr.contains(expected_socket.to_string_lossy().as_ref()));
    assert!(!stderr.contains("could not resolve daemon socket"));
}

#[test]
fn restart_uses_explicit_socket_outside_any_project() {
    let temp = TempDir::new().unwrap();
    let current_dir = temp.path().join("outside");
    std::fs::create_dir_all(&current_dir).unwrap();

    let explicit_socket = temp.path().join("explicit.sock");
    let explicit_socket_string = explicit_socket.to_string_lossy().into_owned();
    let output = run_zaz(
        &current_dir,
        &["--socket", &explicit_socket_string, "restart"],
    );
    let stderr = stderr_string(&output);

    assert!(!output.status.success());
    assert!(stderr.contains("no daemon running at"));
    assert!(stderr.contains(&explicit_socket_string));
    assert!(!stderr.contains("could not resolve daemon socket"));
}

#[test]
fn restart_errors_with_actionable_message_outside_project() {
    let temp = TempDir::new().unwrap();
    let current_dir = temp.path().join("outside");
    std::fs::create_dir_all(&current_dir).unwrap();

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
    let project_dir = temp.path().join("project");
    let nested_dir = project_dir.join("a/b/c");
    let zaz_dir = project_dir.join(".zaz");
    let expected_socket = zaz_dir.join("daemon.sock");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::create_dir_all(&zaz_dir).unwrap();
    std::fs::write(project_dir.join("zaz.json"), "{}").unwrap();

    let output = run_zaz(&nested_dir, &["restart"]);
    let stderr = stderr_string(&output);

    assert!(!output.status.success());
    assert!(stderr.contains("no daemon running at"));
    assert!(stderr.contains(expected_socket.to_string_lossy().as_ref()));
    assert!(!stderr.contains("could not resolve daemon socket"));
}
