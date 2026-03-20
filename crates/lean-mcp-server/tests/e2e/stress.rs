//! Stress tests for the MCP server with latency distribution reporting.
//!
//! All tests are `#[ignore]`d so they do not run in normal CI.
//! Run with:
//!   cargo test --test e2e -- stress --ignored
//! Or a specific scenario:
//!   cargo test --test e2e -- stress::concurrent_tool_calls --ignored

use crate::helpers::McpTestClient;
use serde_json::json;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Latency statistics
// ---------------------------------------------------------------------------

/// Collects duration samples and reports percentile statistics.
struct LatencyStats {
    samples: Vec<Duration>,
}

impl LatencyStats {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    fn record(&mut self, d: Duration) {
        self.samples.push(d);
    }

    fn p50(&self) -> Duration {
        self.percentile(50)
    }

    fn p95(&self) -> Duration {
        self.percentile(95)
    }

    fn p99(&self) -> Duration {
        self.percentile(99)
    }

    fn max(&self) -> Duration {
        *self.samples.iter().max().unwrap_or(&Duration::ZERO)
    }

    fn min(&self) -> Duration {
        *self.samples.iter().min().unwrap_or(&Duration::ZERO)
    }

    fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let total: Duration = self.samples.iter().sum();
        total / self.samples.len() as u32
    }

    fn throughput(&self, total_duration: Duration) -> f64 {
        if total_duration.is_zero() {
            return 0.0;
        }
        self.samples.len() as f64 / total_duration.as_secs_f64()
    }

    fn percentile(&self, p: usize) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort();
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let idx = (p * sorted.len() / 100).min(sorted.len() - 1);
        sorted[idx]
    }

    fn report(&self, name: &str, total_duration: Duration) {
        eprintln!();
        eprintln!("=== {name} ===");
        eprintln!("  Calls:      {}", self.samples.len());
        eprintln!("  Duration:   {:.3}s", total_duration.as_secs_f64());
        eprintln!(
            "  Throughput: {:.1} calls/sec",
            self.throughput(total_duration)
        );
        eprintln!("  min:  {:?}", self.min());
        eprintln!("  mean: {:?}", self.mean());
        eprintln!("  p50:  {:?}", self.p50());
        eprintln!("  p95:  {:?}", self.p95());
        eprintln!("  p99:  {:?}", self.p99());
        eprintln!("  max:  {:?}", self.max());
    }
}

// ---------------------------------------------------------------------------
// Stress test scenarios
// ---------------------------------------------------------------------------

/// Measure cold-start time: spawn server, initialize, and get tools/list.
///
/// Reports the time from process spawn to first successful tools/list response.
#[tokio::test]
#[ignore]
async fn cold_start_time() {
    const ITERATIONS: usize = 5;
    let mut stats = LatencyStats::new();

    let overall_start = Instant::now();
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let mut client = McpTestClient::spawn().await;
        client.initialize().await;
        let _tools = client.list_tools().await;
        let elapsed = start.elapsed();
        stats.record(elapsed);
        client.shutdown().await;
    }
    let overall = overall_start.elapsed();

    stats.report("Cold Start (spawn + initialize + tools/list)", overall);
}

/// Sequential baseline: send tool calls one at a time on a single server.
///
/// Uses `lean_task_result` with a nonexistent task ID for fast in-memory
/// error responses, measuring pure request-response overhead.
#[tokio::test]
#[ignore]
async fn sequential_baseline() {
    const NUM_CALLS: usize = 100;

    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let mut stats = LatencyStats::new();
    let overall_start = Instant::now();

    for i in 0..NUM_CALLS {
        let start = Instant::now();
        let _response = client
            .call_tool(
                "lean_task_result",
                json!({ "task_id": format!("nonexistent-{i}") }),
            )
            .await;
        stats.record(start.elapsed());
    }

    let overall = overall_start.elapsed();
    stats.report(
        &format!("Sequential Baseline ({NUM_CALLS} lean_task_result calls)"),
        overall,
    );

    client.shutdown().await;
}

/// Concurrent tool calls: spawn multiple server instances and fire calls
/// in parallel to measure concurrent throughput.
///
/// Each concurrent task gets its own server process to avoid protocol
/// multiplexing issues (MCP stdio is single-threaded per connection).
#[tokio::test]
#[ignore]
async fn concurrent_tool_calls() {
    const CONCURRENCY: usize = 10;
    const CALLS_PER_CLIENT: usize = 10;

    // Spawn all servers concurrently.
    let mut handles = Vec::new();
    let overall_start = Instant::now();

    for client_idx in 0..CONCURRENCY {
        let handle = tokio::spawn(async move {
            let mut client = McpTestClient::spawn().await;
            client.initialize().await;

            let mut latencies = Vec::new();
            for call_idx in 0..CALLS_PER_CLIENT {
                let start = Instant::now();
                let _response = client
                    .call_tool(
                        "lean_task_result",
                        json!({ "task_id": format!("stress-{client_idx}-{call_idx}") }),
                    )
                    .await;
                latencies.push(start.elapsed());
            }

            client.shutdown().await;
            latencies
        });
        handles.push(handle);
    }

    // Gather all results.
    let mut stats = LatencyStats::new();
    for handle in handles {
        let latencies = handle.await.expect("task panicked");
        for d in latencies {
            stats.record(d);
        }
    }
    let overall = overall_start.elapsed();

    stats.report(
        &format!("Concurrent Tool Calls ({CONCURRENCY} clients x {CALLS_PER_CLIENT} calls each)"),
        overall,
    );
}

/// Sustained load: send calls at a steady rate over a window and check
/// whether latency degrades over time.
///
/// Splits the samples into an early half and a late half, then reports
/// both so the operator can compare.
#[tokio::test]
#[ignore]
async fn sustained_load() {
    const TOTAL_CALLS: usize = 200;
    const TARGET_DURATION: Duration = Duration::from_secs(10);

    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let delay_per_call = TARGET_DURATION / TOTAL_CALLS as u32;

    let mut stats = LatencyStats::new();
    let mut timestamps: Vec<(Instant, Duration)> = Vec::with_capacity(TOTAL_CALLS);
    let overall_start = Instant::now();

    for i in 0..TOTAL_CALLS {
        let call_start = Instant::now();
        let _response = client
            .call_tool(
                "lean_task_result",
                json!({ "task_id": format!("sustained-{i}") }),
            )
            .await;
        let latency = call_start.elapsed();
        stats.record(latency);
        timestamps.push((call_start, latency));

        // Pace the calls to spread them over the target duration.
        if let Some(remaining) = delay_per_call.checked_sub(latency) {
            tokio::time::sleep(remaining).await;
        }
    }

    let overall = overall_start.elapsed();
    stats.report(
        &format!(
            "Sustained Load ({TOTAL_CALLS} calls over {:.0}s)",
            overall.as_secs_f64()
        ),
        overall,
    );

    // Split into early and late halves to detect degradation.
    let mid = timestamps.len() / 2;
    let mut early_stats = LatencyStats::new();
    let mut late_stats = LatencyStats::new();

    for (idx, (_ts, latency)) in timestamps.iter().enumerate() {
        if idx < mid {
            early_stats.record(*latency);
        } else {
            late_stats.record(*latency);
        }
    }

    early_stats.report("  Early Half (first 50%)", overall);
    late_stats.report("  Late Half (last 50%)", overall);

    // Warn if late-half p95 is more than 3x early-half p95.
    let early_p95 = early_stats.p95();
    let late_p95 = late_stats.p95();
    if early_p95 > Duration::ZERO && late_p95 > early_p95 * 3 {
        eprintln!(
            "  WARNING: late-half p95 ({late_p95:?}) is >3x early-half p95 ({early_p95:?}) -- possible degradation"
        );
    }

    client.shutdown().await;
}

/// Mixed workload: interleave different tool types to simulate realistic usage.
///
/// Alternates between tools/list, lean_task_result, and lean_local_search
/// (without project path, so it errors fast).
#[tokio::test]
#[ignore]
async fn mixed_workload() {
    const NUM_CALLS: usize = 90; // Divisible by 3 for clean rotation.

    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let mut list_stats = LatencyStats::new();
    let mut task_stats = LatencyStats::new();
    let mut search_stats = LatencyStats::new();

    let overall_start = Instant::now();

    for i in 0..NUM_CALLS {
        let start = Instant::now();
        match i % 3 {
            0 => {
                let _resp = client.list_tools().await;
                list_stats.record(start.elapsed());
            }
            1 => {
                let _resp = client
                    .call_tool(
                        "lean_task_result",
                        json!({ "task_id": format!("mixed-{i}") }),
                    )
                    .await;
                task_stats.record(start.elapsed());
            }
            _ => {
                let _resp = client
                    .call_tool("lean_local_search", json!({ "query": "test" }))
                    .await;
                search_stats.record(start.elapsed());
            }
        }
    }

    let overall = overall_start.elapsed();

    list_stats.report("Mixed Workload: tools/list", overall);
    task_stats.report("Mixed Workload: lean_task_result", overall);
    search_stats.report("Mixed Workload: lean_local_search", overall);

    eprintln!();
    eprintln!("=== Mixed Workload Summary ===");
    eprintln!("  Total calls: {NUM_CALLS}");
    eprintln!("  Total time:  {:.3}s", overall.as_secs_f64());
    eprintln!(
        "  Throughput:  {:.1} calls/sec",
        NUM_CALLS as f64 / overall.as_secs_f64()
    );

    client.shutdown().await;
}
