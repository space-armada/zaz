//! Microbenchmarks for variable expansion and reference scanning.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::collections::HashMap;
use std::path::PathBuf;
use zaz_vars::{references, Context, Expander};

/// An input that mixes custom `${var}` references, `${zaz:*}` builtins, escapes,
/// and literal text, so both the char scanner and the builtin formatters run.
const INPUT: &str =
    "build ${build_dir} for ${target} at ${zaz:root}: files=${zaz:files} dirs=${zaz:dirs} \
     under ${zaz:prefix} with \\$literal and ${bin_dir}/run";

fn context() -> Context {
    let mut vars = HashMap::new();
    vars.insert("build_dir".to_string(), "./build".to_string());
    vars.insert("bin_dir".to_string(), "./bin".to_string());
    vars.insert("target".to_string(), "release".to_string());

    let files = vec![
        PathBuf::from("src/main.rs"),
        PathBuf::from("src/lib.rs"),
        PathBuf::from("src/cmd/run.rs"),
        PathBuf::from("src/cmd/build.rs"),
        PathBuf::from("tests/integration.rs"),
    ];

    Context::new()
        .with_variables(vars)
        .with_files(files)
        .with_root(PathBuf::from("/home/user/project"))
}

fn bench_vars(c: &mut Criterion) {
    let ctx = context();
    let expander = Expander::new(&ctx);

    c.bench_function("expand", |b| {
        b.iter(|| expander.expand(black_box(INPUT)).unwrap())
    });

    c.bench_function("references", |b| b.iter(|| references(black_box(INPUT))));
}

criterion_group!(benches, bench_vars);
criterion_main!(benches);
