use std::collections::HashMap;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lean_mcp_core::models::{
    BuildResult, CodeAction, CodeActionEdit, CodeActionsResult, CompletionItem, CompletionsResult,
    DiagnosticMessage, DiagnosticsResult, FileOutline, GoalState, HoverInfo, LineProfile,
    OutlineEntry, ProofProfileResult, VerifyResult,
};
use lean_mcp_core::rate_limit::RateLimiter;

// ---------------------------------------------------------------------------
// Model serialization round-trips
// ---------------------------------------------------------------------------

fn bench_goal_state_roundtrip(c: &mut Criterion) {
    let gs = GoalState {
        line_context: "exact Nat.succ_pos n".into(),
        goals: Some(vec![
            "0 < Nat.succ n".into(),
            "∀ (m : Nat), m < n → 0 < m".into(),
        ]),
        goals_before: None,
        goals_after: None,
    };
    let json = serde_json::to_string(&gs).unwrap();

    c.bench_function("model_goal_state_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&gs)).unwrap())
    });
    c.bench_function("model_goal_state_deserialize", |b| {
        b.iter(|| serde_json::from_str::<GoalState>(black_box(&json)).unwrap())
    });
}

fn bench_diagnostics_result_roundtrip(c: &mut Criterion) {
    let dr = DiagnosticsResult {
        success: false,
        items: vec![
            DiagnosticMessage {
                severity: "error".into(),
                message: "unknown identifier 'foo'".into(),
                line: 10,
                column: 5,
            },
            DiagnosticMessage {
                severity: "warning".into(),
                message: "unused variable 'x'".into(),
                line: 15,
                column: 3,
            },
            DiagnosticMessage {
                severity: "info".into(),
                message: "declaration uses sorry".into(),
                line: 20,
                column: 1,
            },
        ],
        failed_dependencies: vec!["Mathlib.Tactic".into(), "Mathlib.Data.Nat.Basic".into()],
    };
    let json = serde_json::to_string(&dr).unwrap();

    c.bench_function("model_diagnostics_result_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&dr)).unwrap())
    });
    c.bench_function("model_diagnostics_result_deserialize", |b| {
        b.iter(|| serde_json::from_str::<DiagnosticsResult>(black_box(&json)).unwrap())
    });
}

fn bench_hover_info_roundtrip(c: &mut Criterion) {
    let hi = HoverInfo {
        symbol: "Nat.add".into(),
        info: "Nat → Nat → Nat\n\nAddition of natural numbers.".into(),
        diagnostics: vec![DiagnosticMessage {
            severity: "info".into(),
            message: "type mismatch".into(),
            line: 5,
            column: 10,
        }],
    };
    let json = serde_json::to_string(&hi).unwrap();

    c.bench_function("model_hover_info_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&hi)).unwrap())
    });
    c.bench_function("model_hover_info_deserialize", |b| {
        b.iter(|| serde_json::from_str::<HoverInfo>(black_box(&json)).unwrap())
    });
}

fn bench_completions_result_roundtrip(c: &mut Criterion) {
    let cr = CompletionsResult {
        items: (0..20)
            .map(|i| CompletionItem {
                label: format!("Nat.add_comm_{i}"),
                kind: Some("Function".into()),
                detail: Some(format!("∀ (n m : Nat), n + m = m + n (variant {i})")),
            })
            .collect(),
    };
    let json = serde_json::to_string(&cr).unwrap();

    c.bench_function("model_completions_result_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&cr)).unwrap())
    });
    c.bench_function("model_completions_result_deserialize", |b| {
        b.iter(|| serde_json::from_str::<CompletionsResult>(black_box(&json)).unwrap())
    });
}

fn bench_file_outline_roundtrip(c: &mut Criterion) {
    let fo = FileOutline {
        imports: vec![
            "Mathlib.Tactic".into(),
            "Mathlib.Data.Nat.Basic".into(),
            "Mathlib.Order.Lattice".into(),
        ],
        declarations: vec![
            OutlineEntry {
                name: "MyNamespace".into(),
                kind: "Ns".into(),
                start_line: 5,
                end_line: 100,
                type_signature: None,
                children: vec![
                    OutlineEntry {
                        name: "myTheorem".into(),
                        kind: "Thm".into(),
                        start_line: 10,
                        end_line: 20,
                        type_signature: Some("∀ (n : Nat), n + 0 = n".into()),
                        children: vec![],
                    },
                    OutlineEntry {
                        name: "myDef".into(),
                        kind: "Def".into(),
                        start_line: 25,
                        end_line: 35,
                        type_signature: Some("Nat → Nat".into()),
                        children: vec![],
                    },
                ],
            },
            OutlineEntry {
                name: "anotherDef".into(),
                kind: "Def".into(),
                start_line: 105,
                end_line: 110,
                type_signature: Some("String → Bool".into()),
                children: vec![],
            },
        ],
        total_declarations: Some(3),
    };
    let json = serde_json::to_string(&fo).unwrap();

    c.bench_function("model_file_outline_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&fo)).unwrap())
    });
    c.bench_function("model_file_outline_deserialize", |b| {
        b.iter(|| serde_json::from_str::<FileOutline>(black_box(&json)).unwrap())
    });
}

fn bench_code_actions_result_roundtrip(c: &mut Criterion) {
    let car = CodeActionsResult {
        actions: vec![
            CodeAction {
                title: "Try this: simp only [Nat.add_comm, Nat.add_assoc]".into(),
                is_preferred: true,
                edits: vec![CodeActionEdit {
                    new_text: "simp only [Nat.add_comm, Nat.add_assoc]".into(),
                    start_line: 5,
                    start_column: 3,
                    end_line: 5,
                    end_column: 8,
                }],
            },
            CodeAction {
                title: "Try this: omega".into(),
                is_preferred: false,
                edits: vec![CodeActionEdit {
                    new_text: "omega".into(),
                    start_line: 5,
                    start_column: 3,
                    end_line: 5,
                    end_column: 8,
                }],
            },
        ],
    };
    let json = serde_json::to_string(&car).unwrap();

    c.bench_function("model_code_actions_result_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&car)).unwrap())
    });
    c.bench_function("model_code_actions_result_deserialize", |b| {
        b.iter(|| serde_json::from_str::<CodeActionsResult>(black_box(&json)).unwrap())
    });
}

fn bench_proof_profile_result_roundtrip(c: &mut Criterion) {
    let mut categories = HashMap::new();
    categories.insert("elaboration".into(), 42.5);
    categories.insert("type_checking".into(), 13.2);
    categories.insert("tactic".into(), 28.8);
    let ppr = ProofProfileResult {
        ms: 84.5,
        lines: vec![
            LineProfile {
                line: 10,
                ms: 42.5,
                text: "  exact h".into(),
            },
            LineProfile {
                line: 15,
                ms: 28.8,
                text: "  simp [Nat.add_comm]".into(),
            },
            LineProfile {
                line: 20,
                ms: 13.2,
                text: "  ring".into(),
            },
        ],
        categories,
    };
    let json = serde_json::to_string(&ppr).unwrap();

    c.bench_function("model_proof_profile_result_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&ppr)).unwrap())
    });
    c.bench_function("model_proof_profile_result_deserialize", |b| {
        b.iter(|| serde_json::from_str::<ProofProfileResult>(black_box(&json)).unwrap())
    });
}

fn bench_verify_result_roundtrip(c: &mut Criterion) {
    let vr = VerifyResult {
        axioms: vec![
            "propext".into(),
            "Classical.choice".into(),
            "Quot.sound".into(),
        ],
        warnings: vec![],
    };
    let json = serde_json::to_string(&vr).unwrap();

    c.bench_function("model_verify_result_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&vr)).unwrap())
    });
    c.bench_function("model_verify_result_deserialize", |b| {
        b.iter(|| serde_json::from_str::<VerifyResult>(black_box(&json)).unwrap())
    });
}

fn bench_build_result_roundtrip(c: &mut Criterion) {
    let br = BuildResult {
        success: true,
        output: "Build completed successfully.\nCompiled 42 modules.".into(),
        errors: vec![],
    };
    let json = serde_json::to_string(&br).unwrap();

    c.bench_function("model_build_result_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&br)).unwrap())
    });
    c.bench_function("model_build_result_deserialize", |b| {
        b.iter(|| serde_json::from_str::<BuildResult>(black_box(&json)).unwrap())
    });
}

// ---------------------------------------------------------------------------
// Rate limiter throughput
// ---------------------------------------------------------------------------

fn bench_rate_limiter_under_limit(c: &mut Criterion) {
    c.bench_function("rate_limiter_check_and_record_under_limit", |b| {
        b.iter(|| {
            let mut rl = RateLimiter::new();
            for _ in 0..3 {
                rl.check_and_record(black_box("leansearch"), 3, 30).unwrap();
            }
        })
    });
}

fn bench_rate_limiter_at_limit(c: &mut Criterion) {
    c.bench_function("rate_limiter_check_and_record_at_limit", |b| {
        b.iter(|| {
            let mut rl = RateLimiter::new();
            for _ in 0..10 {
                rl.check_and_record(black_box("leanfinder"), 10, 30)
                    .unwrap();
            }
            // This call should fail.
            let _ = rl.check_and_record(black_box("leanfinder"), 10, 30);
        })
    });
}

fn bench_rate_limiter_multiple_categories(c: &mut Criterion) {
    let categories = [
        "leansearch",
        "loogle",
        "leanfinder",
        "state_search",
        "hammer",
    ];
    c.bench_function("rate_limiter_multiple_categories", |b| {
        b.iter(|| {
            let mut rl = RateLimiter::new();
            for cat in &categories {
                for _ in 0..3 {
                    rl.check_and_record(black_box(cat), 6, 30).unwrap();
                }
            }
        })
    });
}

fn bench_rate_limiter_window_pruning(c: &mut Criterion) {
    use std::time::{Duration, Instant};

    c.bench_function("rate_limiter_window_pruning", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let mut rl = RateLimiter::new();
                // Fill via check_and_record then
                // age-out by using a very short window.
                for _ in 0..10 {
                    rl.check_and_record("bench", 100, 3600).unwrap();
                }
                // Now time the pruning call with a 0-second window
                // (all 10 timestamps will be pruned).
                let start = Instant::now();
                let _ = rl.check_and_record(black_box("bench"), 100, 0);
                total += start.elapsed();
            }
            total
        })
    });
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

criterion_group!(
    model_benches,
    bench_goal_state_roundtrip,
    bench_diagnostics_result_roundtrip,
    bench_hover_info_roundtrip,
    bench_completions_result_roundtrip,
    bench_file_outline_roundtrip,
    bench_code_actions_result_roundtrip,
    bench_proof_profile_result_roundtrip,
    bench_verify_result_roundtrip,
    bench_build_result_roundtrip,
);

criterion_group!(
    rate_limiter_benches,
    bench_rate_limiter_under_limit,
    bench_rate_limiter_at_limit,
    bench_rate_limiter_multiple_categories,
    bench_rate_limiter_window_pruning,
);

criterion_main!(model_benches, rate_limiter_benches);
