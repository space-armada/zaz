mod support;

use serde_json::Value;
use support::{run_zaz, stderr_string, stdout_string};
use tempfile::TempDir;

const CONFIG: &str = r#"
[[group]]
name = "app"
env_file = ".env"

[[group.task]]
name = "noop"
command = "true"
"#;

fn project(env: Option<&str>) -> TempDir {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("zaz.toml"), CONFIG).unwrap();

    if let Some(env) = env {
        std::fs::write(temp.path().join(".env"), env).unwrap();
    }

    temp
}

#[test]
fn check_passes_and_lists_a_missing_env_file_as_not_evaluated() {
    let temp = project(None);
    let dir = temp.path().canonicalize().unwrap();

    let output = run_zaz(&dir, &["check", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));

    let result: Value = serde_json::from_str(&stdout_string(&output)).unwrap();
    assert_eq!(result["valid"], true);
    assert_eq!(result["errors"].as_array().unwrap().len(), 0);

    let missing = result["not_evaluated"]["missing"].as_array().unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0]["field"], "group 'app' env_file");
    assert_eq!(missing[0]["path"], dir.join(".env").to_str().unwrap());

    let output = run_zaz(&dir, &["check"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout_string(&output).contains("OK"));
    assert!(stderr_string(&output).contains("not evaluated, file missing"));
}

#[test]
fn check_parses_an_existing_env_file_and_reports_the_same_code_as_load() {
    let temp = project(Some("GOOD=1\nTOKEN=hunter2 extra\n"));
    let dir = temp.path().canonicalize().unwrap();

    let output = run_zaz(&dir, &["check", "--json"]);
    assert_ne!(output.status.code(), Some(0));

    let result: Value = serde_json::from_str(&stdout_string(&output)).unwrap();
    assert_eq!(result["valid"], false);

    let error = &result["errors"][0];
    assert_eq!(error["code"], "env_file_unquoted_whitespace");
    assert!(error["hint"].as_str().is_some());

    let message = error["message"].as_str().unwrap();
    assert!(message.contains("line 2"), "{message}");
    assert!(
        message.contains(dir.join(".env").to_str().unwrap()),
        "{message}"
    );
    assert!(!message.contains("hunter2"), "{message}");
    assert!(!message.contains("zaz.toml"), "{message}");

    let Err(load_error) = zaz_daemon::Engine::new(&dir.join("zaz.toml")) else {
        panic!("load should refuse the malformed file");
    };
    assert!(
        load_error
            .to_string()
            .contains("env_file_unquoted_whitespace"),
        "{load_error}"
    );
}

#[test]
fn check_reports_an_unreadable_directory_as_an_error_not_as_missing() {
    let temp = project(None);
    let dir = temp.path().canonicalize().unwrap();
    std::fs::create_dir(dir.join(".env")).unwrap();

    let output = run_zaz(&dir, &["check", "--json"]);
    assert_ne!(output.status.code(), Some(0));

    let result: Value = serde_json::from_str(&stdout_string(&output)).unwrap();
    assert_eq!(result["errors"][0]["code"], "env_file_is_directory");
    assert_eq!(
        result["not_evaluated"]["missing"].as_array().unwrap().len(),
        0
    );
}

#[test]
fn check_does_not_open_a_named_pipe_and_lists_it_as_not_read() {
    let temp = project(None);
    let dir = temp.path().canonicalize().unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(dir.join(".env"))
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo is required for this test");

    let output = run_zaz(&dir, &["check", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));

    let result: Value = serde_json::from_str(&stdout_string(&output)).unwrap();
    assert_eq!(result["valid"], true);

    let not_read = result["not_evaluated"]["not_read"].as_array().unwrap();
    assert_eq!(not_read.len(), 1);
    assert_eq!(not_read[0]["field"], "group 'app' env_file");
    assert_eq!(not_read[0]["path"], dir.join(".env").to_str().unwrap());
    assert_eq!(not_read[0]["reason"], "not a regular file");

    let output = run_zaz(&dir, &["check"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stderr_string(&output).contains("not evaluated, not read"));
}

#[test]
fn check_reports_a_misspelled_group_env_file_with_a_hint() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("zaz.toml"),
        "[[group]]\nname = \"app\"\nenv_flie = \".env\"\n\n[[group.task]]\ncommand = \"true\"\n",
    )
    .unwrap();

    let config = temp.path().join("zaz.toml");
    let output = run_zaz(temp.path(), &["check", "--json", config.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0));

    let result: Value = serde_json::from_str(&stdout_string(&output)).unwrap();
    let error = &result["errors"][0];
    assert_eq!(error["code"], "unknown_key");
    assert_eq!(error["line"], 3);
    assert_eq!(error["hint"], "did you mean 'env_file'?");
}
