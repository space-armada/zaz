use std::path::{Path, PathBuf};

#[test]
fn every_documentation_example_parses_and_validates() {
    let examples_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("examples");

    let mut configs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&examples_root).expect("docs/examples must exist") {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join("zaz.toml");
        if candidate.exists() {
            configs.push(candidate);
        }
    }

    configs.sort();

    assert!(
        configs.len() >= 4,
        "at least four worked examples expected, found {}: {:?}",
        configs.len(),
        configs,
    );

    for config in &configs {
        let result = zaz_config::load(config);
        assert!(
            result.is_ok(),
            "{} failed to load: {:?}",
            config.display(),
            result.err(),
        );
    }
}
