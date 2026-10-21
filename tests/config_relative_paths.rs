mod support;

use serde_json::Value;
use std::time::{Duration, Instant};
use support::{run_zaz, stderr_string, stdout_string, StartedDaemon};
use tempfile::TempDir;

const MINIMAL_CONFIG: &str = r#"
[[group]]
name = "tasks"

[[group.task]]
name = "noop"
command = "true"
"#;

#[test]
fn check_reports_an_absolute_path_for_relative_config_paths() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    std::fs::write(dir.join("zaz.toml"), MINIMAL_CONFIG).unwrap();

    for config in ["zaz.toml", "./zaz.toml"] {
        let output = run_zaz(&dir, &["--config", config, "check", "--json"]);
        let stderr = stderr_string(&output);
        assert_eq!(output.status.code(), Some(0), "{config}: {stderr}");

        let result: Value = serde_json::from_str(&stdout_string(&output)).unwrap();
        let path = result["path"].as_str().unwrap();
        assert_eq!(path, dir.join("zaz.toml").to_str().unwrap(), "{config}");
    }
}

#[test]
fn relative_working_dir_resolves_against_the_config_directory() {
    let temp = TempDir::new().unwrap();
    let cfg_dir = temp.path().join("project");
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(cfg_dir.join("svc/inner")).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();

    let config_path = cfg_dir.join("zaz.toml");
    std::fs::write(
        &config_path,
        r#"
[[group]]
name = "grouped"
working_dir = "svc"

[[group.task]]
name = "inherits"
command = "pwd > inherits.txt"

[[group.task]]
name = "overrides"
command = "pwd > overrides.txt"
working_dir = "svc/inner"
"#,
    )
    .unwrap();

    let socket = temp.path().join("zaz.sock");
    let _daemon = StartedDaemon::launch(&elsewhere, &config_path, &socket);

    let expected = [
        ("svc/inherits.txt", "svc"),
        ("svc/inner/overrides.txt", "svc/inner"),
    ];

    let deadline = Instant::now() + Duration::from_secs(10);
    for (file, suffix) in expected {
        let path = cfg_dir.join(file);
        while !path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }

        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} was not written: {e}", path.display()));
        assert!(
            contents.trim_end().ends_with(suffix),
            "{file}: ran in {contents}"
        );
    }
}
