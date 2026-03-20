//! Criterion microbenchmarks for lean-mcp-core hot paths.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

use lean_mcp_core::file_utils::{check_stale_imports, detect_lean_project, get_relative_file_path};
use lean_mcp_core::task_manager::{ItemStatus, TaskManager};

// ---------------------------------------------------------------------------
// detect_lean_project
// ---------------------------------------------------------------------------

/// Create a temp directory tree with a lakefile.lean marker at `depth` levels
/// above the deepest directory. Returns `(TempDir, deepest_path)`.
fn make_project_tree(depth: usize) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let mut current = dir.path().to_path_buf();
    for i in 0..depth {
        current = current.join(format!("d{i}"));
    }
    fs::create_dir_all(&current).unwrap();
    // Place marker at root
    fs::write(dir.path().join("lakefile.lean"), "-- lakefile").unwrap();
    (dir, current)
}

fn bench_detect_lean_project(c: &mut Criterion) {
    let mut group = c.benchmark_group("detect_lean_project");

    for depth in [1, 5, 10] {
        group.bench_function(format!("depth_{depth}"), |b| {
            b.iter_batched(
                || make_project_tree(depth),
                |(_dir, deepest)| {
                    let result = detect_lean_project(black_box(&deepest));
                    assert!(result.is_some());
                    result
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// get_relative_file_path
// ---------------------------------------------------------------------------

fn bench_get_relative_file_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_relative_file_path");

    // Absolute path under project
    group.bench_function("absolute_under_project", |b| {
        b.iter_batched(
            || {
                let dir = TempDir::new().unwrap();
                let sub = dir.path().join("src");
                fs::create_dir_all(&sub).unwrap();
                let file = sub.join("Foo.lean");
                fs::write(&file, "-- foo").unwrap();
                let file_str = file.to_string_lossy().to_string();
                let project = dir.path().to_path_buf();
                (dir, project, file_str)
            },
            |(_dir, project, file_str)| {
                let result = get_relative_file_path(black_box(&project), black_box(&file_str));
                assert!(result.is_some());
                result
            },
            BatchSize::SmallInput,
        );
    });

    // Relative path
    group.bench_function("relative_path", |b| {
        b.iter_batched(
            || {
                let dir = TempDir::new().unwrap();
                fs::write(dir.path().join("Main.lean"), "-- main").unwrap();
                let project = dir.path().to_path_buf();
                (dir, project)
            },
            |(_dir, project)| {
                let result = get_relative_file_path(black_box(&project), black_box("Main.lean"));
                assert!(result.is_some());
                result
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// check_stale_imports
// ---------------------------------------------------------------------------

/// Create a project with N imports, where `stale_count` of them have stale oleans.
fn make_stale_project(import_count: usize, stale_count: usize) -> (TempDir, String) {
    let dir = TempDir::new().unwrap();
    let build_lib = dir.path().join(".lake").join("build").join("lib");
    fs::create_dir_all(&build_lib).unwrap();

    // Build import lines
    let mut imports = Vec::new();
    for i in 0..import_count {
        let mod_dir = dir.path().join(format!("Mod{i}"));
        fs::create_dir_all(&mod_dir).unwrap();
        let mod_build = build_lib.join(format!("Mod{i}"));
        fs::create_dir_all(&mod_build).unwrap();

        if i < stale_count {
            // Stale: olean first, then source
            fs::write(mod_build.join("Impl.olean"), "olean").unwrap();
            // Ensure mtime difference
            std::thread::sleep(Duration::from_millis(10));
            fs::write(mod_dir.join("Impl.lean"), "-- impl").unwrap();
        } else {
            // Fresh: source first, then olean
            fs::write(mod_dir.join("Impl.lean"), "-- impl").unwrap();
            std::thread::sleep(Duration::from_millis(10));
            fs::write(mod_build.join("Impl.olean"), "olean").unwrap();
        }

        imports.push(format!("import Mod{i}.Impl"));
    }

    // The target file
    let content = format!("{}\n\ndef main := 0\n", imports.join("\n"));
    fs::write(dir.path().join("Main.lean"), &content).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    fs::write(build_lib.join("Main.olean"), "olean").unwrap();

    (dir, "Main.lean".to_string())
}

fn bench_check_stale_imports(c: &mut Criterion) {
    let mut group = c.benchmark_group("check_stale_imports");

    // 5 imports, none stale
    group.bench_function("5_imports_0_stale", |b| {
        b.iter_batched(
            || make_stale_project(5, 0),
            |(dir, file)| {
                let result = check_stale_imports(black_box(dir.path()), black_box(&file));
                assert!(result.is_empty());
                result
            },
            BatchSize::SmallInput,
        );
    });

    // 5 imports, 2 stale
    group.bench_function("5_imports_2_stale", |b| {
        b.iter_batched(
            || make_stale_project(5, 2),
            |(dir, file)| {
                let result = check_stale_imports(black_box(dir.path()), black_box(&file));
                assert_eq!(result.len(), 2);
                result
            },
            BatchSize::SmallInput,
        );
    });

    // 5 imports, all stale
    group.bench_function("5_imports_5_stale", |b| {
        b.iter_batched(
            || make_stale_project(5, 5),
            |(dir, file)| {
                let result = check_stale_imports(black_box(dir.path()), black_box(&file));
                assert_eq!(result.len(), 5);
                result
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// task_manager: create + poll
// ---------------------------------------------------------------------------

fn bench_task_manager_create_and_poll(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("task_manager_create_and_poll", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mgr: TaskManager<String> = TaskManager::new(black_box(Duration::from_secs(60)));
                let (id, _token) = mgr.create_task(black_box(5)).await;
                let snap = mgr.get_task(black_box(&id)).await;
                assert!(snap.is_some());
                snap
            })
        });
    });
}

// ---------------------------------------------------------------------------
// task_manager: update items
// ---------------------------------------------------------------------------

fn bench_task_manager_update_item(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("task_manager_update_item");

    group.bench_function("update_5_items", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mgr: TaskManager<String> = TaskManager::new(Duration::from_secs(60));
                let (id, _token) = mgr.create_task(5).await;

                for i in 0..5 {
                    mgr.update_item(
                        black_box(&id),
                        black_box(i),
                        ItemStatus::Completed {
                            result: format!("result_{i}"),
                        },
                    )
                    .await;
                }

                let snap = mgr.get_task(&id).await.unwrap();
                assert_eq!(snap.completed_count, 5);
                snap
            })
        });
    });

    group.bench_function("update_20_items", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mgr: TaskManager<String> = TaskManager::new(Duration::from_secs(60));
                let (id, _token) = mgr.create_task(20).await;

                for i in 0..20 {
                    mgr.update_item(
                        black_box(&id),
                        black_box(i),
                        ItemStatus::Completed {
                            result: format!("result_{i}"),
                        },
                    )
                    .await;
                }

                let snap = mgr.get_task(&id).await.unwrap();
                assert_eq!(snap.completed_count, 20);
                snap
            })
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_detect_lean_project,
    bench_get_relative_file_path,
    bench_check_stale_imports,
    bench_task_manager_create_and_poll,
    bench_task_manager_update_item,
);

criterion_main!(benches);
