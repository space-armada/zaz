//! Microbenchmarks for the config parse and validate hot paths.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use zaz_config::{parse_json, parse_toml, validate};

/// A multi-group config with tasks, services, dependencies, and variables.
///
/// The dependency edges give the validator a non-trivial graph to walk for
/// cycle detection, and the repeated groups keep the parse work representative
/// of a real project rather than a single-group toy.
const TOML_FIXTURE: &str = r#"
[settings]
shell = "bash"
debounce_ms = 200
log_format = "json"

[variables]
build_dir = "./build"
bin_dir = "./bin"

[[group]]
name = "backend"
patterns = ["**/*.go", "go.mod"]
ignore = ["**/vendor/**"]

[[group.task]]
name = "test"
command = "go test ./..."

[[group.service]]
name = "server"
command = "./server --port 8080"
signal = "SIGTERM"

[[group]]
name = "frontend"
patterns = ["**/*.ts", "**/*.tsx"]
ignore = ["**/node_modules/**"]
depends_on = ["backend"]

[[group.task]]
name = "build"
command = "npm run build"

[[group.task]]
name = "lint"
command = "npm run lint"

[[group]]
name = "docs"
patterns = ["**/*.md"]
depends_on = ["frontend"]

[[group.task]]
name = "render"
command = "mkdocs build"
"#;

const JSON_FIXTURE: &str = r#"{
    "settings": { "shell": "bash", "debounce_ms": 200, "log_format": "json" },
    "variables": { "build_dir": "./build", "bin_dir": "./bin" },
    "groups": [
        {
            "name": "backend",
            "patterns": ["**/*.go", "go.mod"],
            "ignore": ["**/vendor/**"],
            "tasks": [{ "name": "test", "command": "go test ./..." }],
            "services": [{ "name": "server", "command": "./server --port 8080", "signal": "SIGTERM" }]
        },
        {
            "name": "frontend",
            "patterns": ["**/*.ts", "**/*.tsx"],
            "ignore": ["**/node_modules/**"],
            "depends_on": ["backend"],
            "tasks": [
                { "name": "build", "command": "npm run build" },
                { "name": "lint", "command": "npm run lint" }
            ]
        },
        {
            "name": "docs",
            "patterns": ["**/*.md"],
            "depends_on": ["frontend"],
            "tasks": [{ "name": "render", "command": "mkdocs build" }]
        }
    ]
}"#;

fn bench_config(c: &mut Criterion) {
    c.bench_function("parse_toml", |b| {
        b.iter(|| parse_toml(black_box(TOML_FIXTURE)).unwrap())
    });

    c.bench_function("parse_json", |b| {
        b.iter(|| parse_json(black_box(JSON_FIXTURE)).unwrap())
    });

    let config = parse_toml(TOML_FIXTURE).unwrap();
    c.bench_function("validate", |b| {
        b.iter(|| validate(black_box(&config)).unwrap())
    });
}

criterion_group!(benches, bench_config);
criterion_main!(benches);
