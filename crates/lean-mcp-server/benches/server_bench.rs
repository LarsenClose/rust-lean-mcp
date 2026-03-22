//! Criterion microbenchmarks for lean-mcp-server hot paths.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use lean_mcp_server::server::AppContext;
use lean_mcp_server::tools::multi_attempt::{
    filter_diagnostics_by_line_range, prepare_edit, resolve_column, to_diagnostic_messages,
};
use lean_mcp_server::tools::search::SearchConfig;

// ---------------------------------------------------------------------------
// Helpers: diagnostic JSON builders
// ---------------------------------------------------------------------------

fn make_diagnostic(start_line: u32, end_line: u32, severity: i32, msg: &str) -> Value {
    json!({
        "range": {
            "start": {"line": start_line, "character": 0},
            "end": {"line": end_line, "character": 10}
        },
        "severity": severity,
        "message": msg
    })
}

fn make_diagnostics(count: usize) -> Vec<Value> {
    (0..count)
        .map(|i| make_diagnostic(i as u32, i as u32, (i % 4 + 1) as i32, &format!("msg {i}")))
        .collect()
}

// ---------------------------------------------------------------------------
// prepare_edit
// ---------------------------------------------------------------------------

fn bench_prepare_edit(c: &mut Criterion) {
    let mut group = c.benchmark_group("prepare_edit");

    group.bench_function("single_line", |b| {
        let line_text = "    sorry";
        b.iter(|| {
            prepare_edit(
                black_box(line_text),
                black_box(4),
                black_box("exact Nat.zero"),
                black_box(100),
                black_box(10),
            )
        });
    });

    group.bench_function("multi_line", |b| {
        let line_text = "    sorry";
        let snippet = "apply And.intro\n  exact h1\n  exact h2";
        b.iter(|| {
            prepare_edit(
                black_box(line_text),
                black_box(4),
                black_box(snippet),
                black_box(100),
                black_box(10),
            )
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// filter_diagnostics_by_line_range
// ---------------------------------------------------------------------------

fn bench_filter_diagnostics_by_line_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_diagnostics_by_line_range");

    group.bench_function("10_diags", |b| {
        let diags = make_diagnostics(10);
        b.iter(|| filter_diagnostics_by_line_range(black_box(&diags), black_box(3), black_box(7)));
    });

    group.bench_function("100_diags", |b| {
        let diags = make_diagnostics(100);
        b.iter(|| {
            filter_diagnostics_by_line_range(black_box(&diags), black_box(30), black_box(70))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// to_diagnostic_messages
// ---------------------------------------------------------------------------

fn bench_to_diagnostic_messages(c: &mut Criterion) {
    let mut group = c.benchmark_group("to_diagnostic_messages");

    group.bench_function("10_diags", |b| {
        let diags = make_diagnostics(10);
        b.iter(|| to_diagnostic_messages(black_box(&diags), 0));
    });

    group.bench_function("100_diags", |b| {
        let diags = make_diagnostics(100);
        b.iter(|| to_diagnostic_messages(black_box(&diags), 0));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// resolve_column
// ---------------------------------------------------------------------------

fn bench_resolve_column(c: &mut Criterion) {
    let mut group = c.benchmark_group("resolve_column");

    group.bench_function("auto_detect", |b| {
        let line = "    sorry";
        b.iter(|| resolve_column(black_box(line), black_box(None)));
    });

    group.bench_function("explicit_column", |b| {
        let line = "    sorry";
        b.iter(|| resolve_column(black_box(line), black_box(Some(5))));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// resolve_project_path
// ---------------------------------------------------------------------------

fn bench_resolve_project_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("resolve_project_path");

    // Fastest: explicit path always wins
    group.bench_function("explicit_path", |b| {
        let ctx = AppContext::with_options(
            Some(PathBuf::from("/explicit/lean/project")),
            SearchConfig::default(),
        );
        b.iter(|| {
            let result = ctx.resolve_project_path(black_box(None));
            assert!(result.is_ok());
            result
        });
    });

    // File detection: walk up from a file in a temp Lean project
    group.bench_function("file_detection", |b| {
        b.iter_batched(
            || {
                let dir = TempDir::new().unwrap();
                let sub = dir.path().join("src");
                fs::create_dir_all(&sub).unwrap();
                fs::write(dir.path().join("lakefile.lean"), "-- lakefile").unwrap();
                let file = sub.join("Foo.lean");
                fs::write(&file, "-- foo").unwrap();
                let file_str = file.to_string_lossy().to_string();
                let ctx = AppContext::new();
                (dir, ctx, file_str)
            },
            |(_dir, ctx, file_str)| {
                let result = ctx.resolve_project_path(Some(&file_str));
                assert!(result.is_ok());
                result
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_prepare_edit,
    bench_filter_diagnostics_by_line_range,
    bench_to_diagnostic_messages,
    bench_resolve_column,
    bench_resolve_project_path,
);

criterion_main!(benches);
