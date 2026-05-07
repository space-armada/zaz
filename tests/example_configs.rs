use std::path::{Path, PathBuf};

fn collect_zaz_tomls(root: &Path) -> Vec<PathBuf> {
    let mut configs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(root).unwrap_or_else(|err| {
        panic!("{} must exist: {}", root.display(), err);
    }) {
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
    configs
}

fn assert_all_load(configs: &[PathBuf]) {
    for config in configs {
        let result = zaz_config::load(config);
        assert!(
            result.is_ok(),
            "{} failed to load: {:?}",
            config.display(),
            result.err(),
        );
    }
}

#[test]
fn every_documentation_example_parses_and_validates() {
    let examples_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("examples");

    let configs = collect_zaz_tomls(&examples_root);

    assert!(
        configs.len() >= 4,
        "at least four worked examples expected, found {}: {:?}",
        configs.len(),
        configs,
    );

    assert_all_load(&configs);
}

#[test]
fn every_migration_example_parses_and_validates() {
    let migration_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("migration");

    let configs = collect_zaz_tomls(&migration_root);

    assert!(
        !configs.is_empty(),
        "at least one migration worked example expected under {}",
        migration_root.display(),
    );

    assert_all_load(&configs);
}
