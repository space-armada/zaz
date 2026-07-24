//! Microbenchmarks for ANSI log-line parsing and stripping.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use zaz_tui::ansi::{parse_ansi, strip_ansi};

/// A log line with several SGR sequences: colors, bold, and a reset, plus plain
/// text between them, so the parser walks both escape and literal runs.
const LINE: &str = "\x1b[1;32mINFO\x1b[0m \x1b[36mserver\x1b[0m listening on \
     \x1b[33m0.0.0.0:8080\x1b[0m after \x1b[31m3\x1b[0m retries in 42ms";

fn bench_ansi(c: &mut Criterion) {
    c.bench_function("parse_ansi", |b| b.iter(|| parse_ansi(black_box(LINE))));

    c.bench_function("strip_ansi", |b| b.iter(|| strip_ansi(black_box(LINE))));
}

criterion_group!(benches, bench_ansi);
criterion_main!(benches);
