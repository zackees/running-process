#![cfg(feature = "client")]

use std::time::Duration;

use running_process::broker::server::handoff::{compare_handoff_latency, HandoffLatencyError};

fn micros(value: u64) -> Duration {
    Duration::from_micros(value)
}

#[test]
fn measured_handoff_samples_must_beat_reconnect_fallback_at_p50_and_p99() {
    let handoff = [
        micros(8),
        micros(9),
        micros(9),
        micros(10),
        micros(10),
        micros(11),
        micros(11),
        micros(12),
        micros(12),
        micros(13),
    ];
    let fallback = [
        micros(23),
        micros(24),
        micros(24),
        micros(25),
        micros(25),
        micros(26),
        micros(27),
        micros(28),
        micros(29),
        micros(30),
    ];

    let comparison = compare_handoff_latency(&handoff, &fallback).unwrap();

    assert!(comparison.proves_handoff_faster());
    assert_eq!(comparison.handoff.sample_count, handoff.len());
    assert_eq!(comparison.fallback.sample_count, fallback.len());
    assert!(comparison.p50_savings() >= micros(15));
    assert!(comparison.p99_savings() >= micros(17));
}

#[test]
fn equal_or_slower_handoff_samples_do_not_pass_the_phase6_latency_proof() {
    let fallback = [micros(10), micros(11), micros(12), micros(13)];

    assert_eq!(
        compare_handoff_latency(&[micros(10), micros(11), micros(12), micros(13)], &fallback),
        Err(HandoffLatencyError::P50NotFaster {
            handoff: micros(11),
            fallback: micros(11),
        })
    );
    assert_eq!(
        compare_handoff_latency(&[micros(9), micros(10), micros(20), micros(21)], &fallback),
        Err(HandoffLatencyError::P99NotFaster {
            handoff: micros(21),
            fallback: micros(13),
        })
    );
}

#[test]
fn latency_comparison_requires_both_handoff_and_fallback_samples() {
    assert_eq!(
        compare_handoff_latency(&[], &[micros(1)]),
        Err(HandoffLatencyError::EmptyHandoffSamples)
    );
    assert_eq!(
        compare_handoff_latency(&[micros(1)], &[]),
        Err(HandoffLatencyError::EmptyFallbackSamples)
    );
}
