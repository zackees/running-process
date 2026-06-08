#![cfg(feature = "client")]

use std::time::Duration;

use running_process::broker::server::{
    enforce_hello_latency_budget, summarize_hello_latencies, PerfGuardError, HELLO_P50_BUDGET,
    HELLO_P99_BUDGET, HELLO_PERF_SAMPLE_COUNT,
};

#[test]
fn hello_perf_budget_constants_are_frozen() {
    assert_eq!(HELLO_PERF_SAMPLE_COUNT, 10_000);
    assert_eq!(HELLO_P50_BUDGET, Duration::from_micros(200));
    assert_eq!(HELLO_P99_BUDGET, Duration::from_millis(1));
}

#[test]
fn summarize_hello_latencies_uses_nearest_rank_percentiles() {
    let samples = [
        Duration::from_micros(300),
        Duration::from_micros(100),
        Duration::from_micros(200),
        Duration::from_micros(400),
    ];

    let summary = summarize_hello_latencies(&samples).unwrap();

    assert_eq!(summary.sample_count, 4);
    assert_eq!(summary.p50, Duration::from_micros(200));
    assert_eq!(summary.p99, Duration::from_micros(400));
}

#[test]
fn hello_perf_guard_accepts_samples_inside_budget() {
    let samples = vec![Duration::from_micros(100); HELLO_PERF_SAMPLE_COUNT];

    let summary = enforce_hello_latency_budget(&samples).unwrap();

    assert_eq!(summary.p50, Duration::from_micros(100));
    assert_eq!(summary.p99, Duration::from_micros(100));
}

#[test]
fn hello_perf_guard_rejects_too_few_samples() {
    let samples = vec![Duration::from_micros(100); HELLO_PERF_SAMPLE_COUNT - 1];

    let err = enforce_hello_latency_budget(&samples).unwrap_err();

    assert_eq!(
        err,
        PerfGuardError::TooFewSamples {
            required: HELLO_PERF_SAMPLE_COUNT,
            actual: HELLO_PERF_SAMPLE_COUNT - 1
        }
    );
}

#[test]
fn hello_perf_guard_rejects_slow_p50() {
    let samples = vec![Duration::from_micros(201); HELLO_PERF_SAMPLE_COUNT];

    let err = enforce_hello_latency_budget(&samples).unwrap_err();

    assert_eq!(
        err,
        PerfGuardError::P50Exceeded {
            actual: Duration::from_micros(201),
            budget: HELLO_P50_BUDGET
        }
    );
}

#[test]
fn hello_perf_guard_rejects_slow_p99() {
    let mut samples = vec![Duration::from_micros(100); HELLO_PERF_SAMPLE_COUNT];
    let slow_start = HELLO_PERF_SAMPLE_COUNT - 101;
    for sample in &mut samples[slow_start..] {
        *sample = Duration::from_millis(2);
    }

    let err = enforce_hello_latency_budget(&samples).unwrap_err();

    assert_eq!(
        err,
        PerfGuardError::P99Exceeded {
            actual: Duration::from_millis(2),
            budget: HELLO_P99_BUDGET
        }
    );
}
