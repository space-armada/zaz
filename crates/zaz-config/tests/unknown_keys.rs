//! Unknown keys are collected validation errors through every entry point.

use tempfile::TempDir;
use zaz_config::{
    load, load_unvalidated, parse_json, parse_toml, ConfigError, ValidationErrorKind,
    ValidationErrors,
};

struct LevelTest {
    name: &'static str,
    toml: &'static str,
    json: &'static str,
    table: &'static str,
}

const LEVEL_TESTS: &[LevelTest] = &[
    LevelTest {
        name: "root",
        toml: "stray = 1\n[[group]]\nname = \"g\"\npatterns = [\"*\"]\n",
        json: r#"{"stray": 1, "groups": [{"name": "g", "patterns": ["*"]}]}"#,
        table: "",
    },
    LevelTest {
        name: "settings",
        toml: "[settings]\nstray = 1\n[[group]]\nname = \"g\"\npatterns = [\"*\"]\n",
        json: r#"{"settings": {"stray": 1}, "groups": [{"name": "g", "patterns": ["*"]}]}"#,
        table: "settings",
    },
    LevelTest {
        name: "group",
        toml: "[[group]]\nname = \"g\"\npatterns = [\"*\"]\nstray = 1\n",
        json: r#"{"groups": [{"name": "g", "patterns": ["*"], "stray": 1}]}"#,
        table: "[0]",
    },
    LevelTest {
        name: "task",
        toml: "[[group]]\nname = \"g\"\n\n[[group.task]]\nname = \"t\"\ncommand = \"x\"\nstray = 1\n",
        json: r#"{"groups": [{"name": "g", "tasks": [{"name": "t", "command": "x", "stray": 1}]}]}"#,
        table: "[0].",
    },
    LevelTest {
        name: "service",
        toml: "[[group]]\nname = \"g\"\n\n[[group.service]]\nname = \"s\"\ncommand = \"x\"\nstray = 1\n",
        json: r#"{"groups": [{"name": "g", "services": [{"name": "s", "command": "x", "stray": 1}]}]}"#,
        table: "[0].",
    },
];

fn unknown_keys(result: Result<impl std::fmt::Debug, ConfigError>) -> ValidationErrors {
    match result {
        Err(ConfigError::Validation(errors)) => errors,
        other => panic!("expected validation errors, got {other:?}"),
    }
}

fn write(dir: &TempDir, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn assert_single_stray(errors: &ValidationErrors, test: &LevelTest, located: bool) {
    assert_eq!(errors.len(), 1, "{}: {errors}", test.name);

    let error = errors.iter().next().unwrap();
    let ValidationErrorKind::UnknownKey { key, table } = &error.kind else {
        panic!("{}: wrong kind: {error}", test.name);
    };

    assert_eq!(key, "stray", "{}", test.name);
    assert!(table.contains(test.table), "{}: {table}", test.name);
    assert_eq!(error.code(), "unknown_key", "{}", test.name);
    assert_eq!(error.span.is_some(), located, "{}", test.name);
}

#[test]
fn stray_key_is_reported_by_every_entry_point_at_every_level() {
    let dir = TempDir::new().unwrap();

    for test in LEVEL_TESTS {
        let toml_path = write(&dir, "zaz.toml", test.toml);
        let json_path = write(&dir, "zaz.json", test.json);

        assert_single_stray(&unknown_keys(parse_toml(test.toml)), test, true);
        assert_single_stray(&unknown_keys(parse_json(test.json)), test, false);
        assert_single_stray(&unknown_keys(load(&toml_path)), test, true);
        assert_single_stray(&unknown_keys(load(&json_path)), test, false);
        assert_single_stray(&unknown_keys(load_unvalidated(&toml_path)), test, true);
        assert_single_stray(&unknown_keys(load_unvalidated(&json_path)), test, false);
    }
}

#[test]
fn stray_key_location_is_its_line_and_column() {
    let toml =
        "[[group]]\nname = \"g\"\n\n[[group.task]]\nname = \"t\"\ncommand = \"x\"\nstray = 1\n";
    let errors = unknown_keys(parse_toml(toml));

    let span = errors.iter().next().unwrap().span.unwrap();
    assert_eq!((span.line, span.column), (7, 1));
}

#[test]
fn two_misspellings_are_both_reported_with_hints() {
    let toml = r#"
[settings]
shel = "bash"

[[group]]
name = "g"
patterns = ["*"]
depends_onn = []

[[group.service]]
name = "s"
command = "x"
"#;
    let errors = unknown_keys(parse_toml(toml));
    assert_eq!(errors.len(), 2, "{errors}");

    let found: Vec<_> = errors
        .iter()
        .map(|e| (e.hint.clone().unwrap(), e.span.unwrap().line))
        .collect();
    assert_eq!(
        found,
        vec![
            ("did you mean 'shell'?".to_string(), 3),
            ("did you mean 'depends_on'?".to_string(), 8),
        ]
    );
}

#[test]
fn unknown_keys_join_semantic_errors_in_one_report() {
    let dir = TempDir::new().unwrap();
    let toml =
        "[[group]]\nname = \"g\"\npatterns = [\"*\"]\ndepends_on = [\"missing\"]\nstray = 1\n";
    let path = write(&dir, "zaz.toml", toml);

    let errors = unknown_keys(load(&path));
    let codes: Vec<_> = errors.iter().map(|e| e.code()).collect();
    assert_eq!(codes, vec!["unknown_key", "unknown_dependency"]);
    assert_eq!(errors.iter().nth(1).unwrap().span.unwrap().line, 4);

    // The unvalidated loader skips the semantic passes.
    let errors = unknown_keys(load_unvalidated(&path));
    assert_eq!(errors.len(), 1);
}

#[test]
fn unknown_key_without_a_close_match_has_no_hint() {
    let errors = unknown_keys(parse_toml("zzzzzzzz = 1\n"));

    assert!(errors.iter().next().unwrap().hint.is_none());
}

#[test]
fn every_alias_is_still_accepted() {
    for service_key in ["service", "daemon", "daemons"] {
        let toml = format!(
            "[settings]\ndebounce_ms = 250\n\n[[group]]\nname = \"g\"\n\n\
             [[group.task]]\nname = \"t\"\ncommand = \"x\"\n\n\
             [[group.{service_key}]]\nname = \"a\"\ncommand = \"x\"\ndelay_ms = 5\n"
        );
        let config = parse_toml(&toml).unwrap_or_else(|e| panic!("{service_key}: {e}"));

        assert_eq!(config.settings.debounce_ms(), 250, "{service_key}");
        assert_eq!(config.groups[0].tasks.len(), 1, "{service_key}");
        assert_eq!(config.groups[0].services.len(), 1, "{service_key}");
        assert_eq!(
            config.groups[0].services[0].delay_ms(),
            Some(5),
            "{service_key}"
        );
    }

    for (groups_key, tasks_key, service_key) in [
        ("group", "task", "service"),
        ("groups", "tasks", "services"),
        ("groups", "tasks", "daemon"),
        ("groups", "tasks", "daemons"),
    ] {
        let json = format!(
            r#"{{"settings": {{"debounce_ms": 250}}, "{groups_key}": [{{"name": "g",
                "{tasks_key}": [{{"name": "t", "command": "x"}}],
                "{service_key}": [{{"name": "a", "command": "x", "delay_ms": 5}}]}}]}}"#
        );
        let config = parse_json(&json).unwrap_or_else(|e| panic!("{service_key}: {e}"));

        assert_eq!(config.groups[0].tasks.len(), 1, "{service_key}");
        assert_eq!(config.groups[0].services.len(), 1, "{service_key}");
    }
}

#[test]
fn a_type_error_still_aborts_before_keys_are_collected() {
    let toml = "stray = 1\n[[group]]\nname = \"g\"\n\n[[group.service]]\nname = \"s\"\ncommand = \"x\"\nsignal = \"NOPE\"\n";

    assert!(matches!(parse_toml(toml), Err(ConfigError::Toml { .. })));
}

#[test]
fn trailing_json_after_the_config_is_still_rejected() {
    assert!(matches!(parse_json("{} {}"), Err(ConfigError::Json(_))));
}
