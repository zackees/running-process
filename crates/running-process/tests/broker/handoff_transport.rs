#![cfg(feature = "client")]

use std::path::PathBuf;

use running_process::broker::server::handoff::{
    DuplicateHandleAttempt, DuplicateHandleError, DuplicateHandleSuccess, HandoffAttemptDecision,
    HandoffAttemptFailure, HandoffFallbackDecision, HandoffFallbackReason, HandoffToken,
    ScmRightsAttempt, ScmRightsError, ScmRightsSuccess, UnixFileDescriptor, UnixHandoffSocket,
    WindowsHandleValue,
};

fn token(byte: u8) -> HandoffToken {
    HandoffToken::from_bytes([byte; 16])
}

fn assert_fallback_safe(fallback: HandoffFallbackDecision, expected_reason: HandoffFallbackReason) {
    assert_eq!(fallback.reason, expected_reason);
    assert!(fallback.uses_backend_reconnect());
    assert!(!fallback.sends_client_error());
}

fn assert_attempt_fallback_safe(
    decision: HandoffAttemptDecision,
    expected_reason: HandoffFallbackReason,
) {
    let HandoffAttemptDecision::FallbackToReconnect(fallback) = decision else {
        panic!("expected reconnect fallback");
    };
    assert_fallback_safe(fallback, expected_reason);
}

#[test]
fn duplicate_handle_attempt_uses_typed_inputs_and_result() {
    let handoff_token = token(0x37);
    let attempt = DuplicateHandleAttempt::new(WindowsHandleValue::new(0x51), 4242, handoff_token);

    assert_eq!(attempt.pipe_handle.get(), 0x51);
    assert_eq!(attempt.backend_pid, 4242);
    assert_eq!(attempt.handoff_token, handoff_token);

    let success = DuplicateHandleSuccess::new(WindowsHandleValue::new(0x99), 4242, handoff_token);
    assert_eq!(success.duplicated_handle.get(), 0x99);
    assert_eq!(success.backend_pid, 4242);
    assert_eq!(success.handoff_token, handoff_token);
}

#[test]
fn duplicate_handle_errors_map_to_fallback_safe_policy() {
    let unsupported = DuplicateHandleError::UnsupportedPlatform;
    assert_eq!(unsupported.attempt_failure(), None);
    assert!(unsupported.is_fallback_safe());
    assert_fallback_safe(
        unsupported.fallback_decision(),
        HandoffFallbackReason::ServicePolicyDisabled,
    );

    let permission_denied = DuplicateHandleError::PermissionDenied { backend_pid: 4242 };
    assert_eq!(
        permission_denied.attempt_failure(),
        Some(HandoffAttemptFailure::PermissionDenied)
    );
    assert!(permission_denied.is_fallback_safe());
    assert_attempt_fallback_safe(
        permission_denied.fallback_attempt_decision(),
        HandoffFallbackReason::PermissionDenied,
    );

    let cannot_open = DuplicateHandleError::CannotOpenBackend { backend_pid: 4242 };
    assert_eq!(
        cannot_open.attempt_failure(),
        Some(HandoffAttemptFailure::PermissionDenied)
    );
    assert_attempt_fallback_safe(
        cannot_open.fallback_attempt_decision(),
        HandoffFallbackReason::PermissionDenied,
    );

    let duplicate_failed = DuplicateHandleError::DuplicateFailed {
        backend_pid: 4242,
        raw_os_error: Some(5),
    };
    assert_eq!(
        duplicate_failed.attempt_failure(),
        Some(HandoffAttemptFailure::PermissionDenied)
    );
    assert_attempt_fallback_safe(
        duplicate_failed.fallback_attempt_decision(),
        HandoffFallbackReason::PermissionDenied,
    );

    let integrity = DuplicateHandleError::IntegrityMismatch { backend_pid: 4242 };
    assert_eq!(
        integrity.attempt_failure(),
        Some(HandoffAttemptFailure::IntegrityMismatch)
    );
    assert_attempt_fallback_safe(
        integrity.fallback_attempt_decision(),
        HandoffFallbackReason::IntegrityMismatch,
    );

    let timeout = DuplicateHandleError::BackendAckTimeout { backend_pid: 4242 };
    assert_eq!(
        timeout.attempt_failure(),
        Some(HandoffAttemptFailure::BackendAckTimeout)
    );
    assert_attempt_fallback_safe(
        timeout.fallback_attempt_decision(),
        HandoffFallbackReason::BackendAckTimeout,
    );
}

#[test]
fn scm_rights_attempt_uses_typed_inputs_and_result() {
    let handoff_token = token(0x54);
    let socket = UnixHandoffSocket::new("/tmp/running-process-handoff.sock");
    let attempt = ScmRightsAttempt::new(UnixFileDescriptor::new(17), socket.clone(), handoff_token);

    assert_eq!(attempt.fd.raw(), 17);
    assert_eq!(attempt.backend_socket, socket);
    assert_eq!(attempt.handoff_token, handoff_token);

    let success = ScmRightsSuccess::new(UnixFileDescriptor::new(18), socket.clone(), handoff_token);
    assert_eq!(success.sent_fd.raw(), 18);
    assert_eq!(success.backend_socket, socket);
    assert_eq!(success.handoff_token, handoff_token);
}

#[test]
fn scm_rights_errors_map_to_fallback_safe_policy() {
    let socket = PathBuf::from("/tmp/running-process-handoff.sock");

    let unsupported = ScmRightsError::UnsupportedPlatform;
    assert_eq!(unsupported.attempt_failure(), None);
    assert!(unsupported.is_fallback_safe());
    assert_fallback_safe(
        unsupported.fallback_decision(),
        HandoffFallbackReason::ServicePolicyDisabled,
    );

    let permission_denied = ScmRightsError::PermissionDenied {
        fd: 17,
        socket: socket.clone(),
    };
    assert_eq!(
        permission_denied.attempt_failure(),
        Some(HandoffAttemptFailure::PermissionDenied)
    );
    assert!(permission_denied.is_fallback_safe());
    assert_attempt_fallback_safe(
        permission_denied.fallback_attempt_decision(),
        HandoffFallbackReason::PermissionDenied,
    );

    let unavailable = ScmRightsError::BackendSocketUnavailable {
        socket: socket.clone(),
    };
    assert_eq!(
        unavailable.attempt_failure(),
        Some(HandoffAttemptFailure::BackendAckTimeout)
    );
    assert_attempt_fallback_safe(
        unavailable.fallback_attempt_decision(),
        HandoffFallbackReason::BackendAckTimeout,
    );

    let would_block = ScmRightsError::WouldBlock {
        socket: socket.clone(),
    };
    assert_eq!(
        would_block.attempt_failure(),
        Some(HandoffAttemptFailure::BackendAckTimeout)
    );
    assert_attempt_fallback_safe(
        would_block.fallback_attempt_decision(),
        HandoffFallbackReason::BackendAckTimeout,
    );

    let send_failed = ScmRightsError::SendFailed {
        fd: 17,
        socket: socket.clone(),
        raw_os_error: Some(11),
    };
    assert_eq!(
        send_failed.attempt_failure(),
        Some(HandoffAttemptFailure::BackendAckTimeout)
    );
    assert_attempt_fallback_safe(
        send_failed.fallback_attempt_decision(),
        HandoffFallbackReason::BackendAckTimeout,
    );

    let timeout = ScmRightsError::BackendAckTimeout { socket };
    assert_eq!(
        timeout.attempt_failure(),
        Some(HandoffAttemptFailure::BackendAckTimeout)
    );
    assert_attempt_fallback_safe(
        timeout.fallback_attempt_decision(),
        HandoffFallbackReason::BackendAckTimeout,
    );
}

#[cfg(not(unix))]
#[test]
fn scm_rights_transport_reports_unsupported_off_unix() {
    let attempt = ScmRightsAttempt::new(
        UnixFileDescriptor::new(17),
        UnixHandoffSocket::new("/tmp/running-process-handoff.sock"),
        token(0x55),
    );

    let err = running_process::broker::server::handoff::try_send_scm_rights(&attempt).unwrap_err();

    assert_eq!(err, ScmRightsError::UnsupportedPlatform);
    assert!(err.is_fallback_safe());
}
