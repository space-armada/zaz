//! Microbenchmarks for glob compilation and path matching.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::path::PathBuf;
use zaz_watch::PatternSet;

fn patterns() -> (Vec<String>, Vec<String>) {
    let include = vec![
        "**/*.rs".to_string(),
        "**/*.go".to_string(),
        "**/*.ts".to_string(),
        "**/*.tsx".to_string(),
        "**/*.md".to_string(),
        "Cargo.toml".to_string(),
    ];
    let exclude = vec![
        "**/vendor/**".to_string(),
        "**/node_modules/**".to_string(),
        "**/target/**".to_string(),
    ];
    (include, exclude)
}

/// A mix of matching and excluded paths so the matcher exercises both the
/// include and exclude sets rather than short-circuiting on one branch.
fn paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("src/main.rs"),
        PathBuf::from("crates/zaz-watch/src/glob.rs"),
        PathBuf::from("web/app/index.tsx"),
        PathBuf::from("backend/server.go"),
        PathBuf::from("docs/guide.md"),
        PathBuf::from("Cargo.toml"),
        PathBuf::from("vendor/pkg/mod.go"),
        PathBuf::from("web/node_modules/react/index.ts"),
        PathBuf::from("target/debug/build.rs"),
        PathBuf::from("README.txt"),
    ]
}

fn bench_glob(c: &mut Criterion) {
    let (include, exclude) = patterns();

    c.bench_function("compile", |b| {
        b.iter(|| PatternSet::new(black_box(&include), black_box(&exclude)).unwrap())
    });

    let set = PatternSet::new(&include, &exclude).unwrap();
    let paths = paths();
    c.bench_function("matches", |b| {
        b.iter(|| {
            let mut hits = 0usize;
            for path in &paths {
                if set.matches(black_box(path)) {
                    hits += 1;
                }
            }
            hits
        })
    });
}

criterion_group!(benches, bench_glob);
criterion_main!(benches);
