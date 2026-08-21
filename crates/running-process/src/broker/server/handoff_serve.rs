//! Serve-side wiring of the handle-passing handoff into the production
//! broker accept loop (#387).
//!
//! When a Hello negotiation issues a one-time handoff token (the client
//! advertised [`CAP_HANDLE_PASSING`] and `Negotiated.handle_passed_token`
//! is non-empty) and the broker was configured with a backend handoff
//! endpoint, this module runs the platform handoff after the `Negotiated`
//! reply has been written:
//!
//! 1. dial the configured backend handoff endpoint,
//! 2. transfer the still-open client connection — `DuplicateHandle` into
//!    the verified backend process on Windows, `sendmsg(SCM_RIGHTS)` over
//!    the handoff connection on Unix — paired with the one-time token,
//! 3. send the [`HandoffOffer`](crate::broker::protocol::HandoffOffer)
//!    frame and wait for the backend [`HandoffAck`] through
//!    [`WireHandoffDelivery`], bounded by the
//!    [`HandoffAckRegistry`] ACK deadline, and
//! 4. on acceptance, relay the handoff-ready EVENT frame
//!    ([`handoff_ready_frame`]) to the waiting client on the connection
//!    that carried Hello.
//!
//! A failure before delivery, or an explicit backend rejection, writes
//! **nothing** to the client so the caller can proxy the accepted stream.
//! A missing or malformed ACK after delivery is ownership-ambiguous:
//! [`try_complete_negotiated_handoff`] reports that the caller must relinquish
//! the original stream instead of proxying it concurrently.
//!
//! # Token lifecycle
//!
//! The production [`HelloRouter`](super::hello_router::HelloRouter) builds
//! one ephemeral [`HelloHandler`](super::hello_handler::HelloHandler) per
//! request, so the token store that issued `handle_passed_token` is gone
//! by the time the reply reaches the accept loop. This module re-seeds a
//! connection-local [`HandoffTokenStore`]/[`HandoffAckRegistry`] pair with
//! the exact issued token bytes; the orchestrators then enforce the same
//! exactly-once consumption, revocation-on-failure, and ACK-deadline
//! semantics they were tested with.
//!
//! # Handle-leak contract
//!
//! On Windows, a failure after `DuplicateHandle` succeeded leaks the
//! duplicated handle in the backend process until that process exits
//! ([`WindowsHandoffFallback::leaked_backend_handle`](super::handoff::WindowsHandoffFallback));
//! on Unix the broker keeps ownership of its descriptor, but a duplicate
//! that already reached the backend cannot be reclaimed
//! ([`UnixHandoffFallback::fd_reached_backend`](super::handoff::UnixHandoffFallback)).
//! Both are logged honestly here instead of pretending cleanup happened.

use std::sync::Mutex;
use std::time::Instant;

use prost::Message;

use crate::broker::capabilities::CAP_HANDLE_PASSING;
use crate::broker::client::connect_ipc_stream;
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, write_frame, HandoffAck, HelloReply, Negotiated,
};

use super::backend_registry::BackendRegistry;
use super::handoff::{
    handoff_ready_frame, HandoffAckRegistry, HandoffToken, HandoffTokenStore,
    PendingHandoffBackend, WireHandoffDelivery, HANDOFF_TOKEN_BYTES,
};
use super::instance::BrokerInstanceKey;

/// Broker-side inputs shared by every handoff attempted from one serve loop.
pub struct ServeHandoffContext<'a> {
    /// Backend handoff endpoint the broker dials to deliver the connection.
    pub handoff_endpoint: &'a str,
    /// Service name registered for Hello negotiation.
    pub service_name: &'a str,
    /// Backend version registered for Hello negotiation.
    pub service_version: &'a str,
    /// Broker instance key used for registry lookups.
    pub instance: &'a BrokerInstanceKey,
    /// Live backend registry holding the verified backend handle.
    pub registry: &'a Mutex<BackendRegistry>,
}

/// Run the platform handoff for one freshly negotiated Hello connection,
/// returning whether the caller must relinquish its copy of the connection.
///
/// This result lets unified broker listeners retain the exact accepted stream
/// as an in-place proxy fallback only while that is safe. Once the offer has
/// reached the backend, a lost or late ACK cannot prove the backend did not
/// adopt it, so this conservatively returns `true`; both sides must never
/// consume the same connection.
#[must_use = "true means the backend may own the connection and the caller must relinquish it"]
pub fn try_complete_negotiated_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &mut interprocess::local_socket::Stream,
    reply: &HelloReply,
) -> bool {
    let mut stream = match clone_legacy_stream_into_platform(client_stream) {
        Ok(stream) => stream,
        Err(error) => {
            log_handoff_fallback(&format!(
                "failed to clone legacy handoff callback stream: {error}"
            ));
            return false;
        }
    };
    try_complete_negotiated_handoff_opaque(ctx, &mut stream, reply)
}

pub(crate) fn try_complete_negotiated_handoff_opaque(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &mut crate::platform::ipc::Stream,
    reply: &HelloReply,
) -> bool {
    if !try_transfer_negotiated_handoff_opaque(ctx, client_stream, reply) {
        return false;
    }
    let Some(negotiated) = negotiated_with_handoff(reply) else {
        return false;
    };
    let ack = HandoffAck {
        token: negotiated.handle_passed_token.clone(),
        accepted: true,
        error_detail: String::new(),
        correlation_id: negotiated.connection_id,
    };
    let frame = handoff_ready_frame(&ack);
    handoff_transferred_after_ready_event(write_frame(client_stream, &frame.encode_to_vec()))
}

/// Transfer the accepted client connection to the negotiated backend without
/// writing the handoff-ready event.
///
/// Async broker front doors must use this entry point and write the ready event
/// through their original async stream. A Tokio Windows named-pipe handle is
/// opened for overlapped I/O; wrapping a duplicate as a synchronous stream and
/// writing through it fails with `ERROR_INVALID_PARAMETER`.
#[must_use = "true means the backend may own the connection and the caller must relinquish it"]
pub fn try_transfer_negotiated_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &mut interprocess::local_socket::Stream,
    reply: &HelloReply,
) -> bool {
    let mut stream = match clone_legacy_stream_into_platform(client_stream) {
        Ok(stream) => stream,
        Err(error) => {
            log_handoff_fallback(&format!(
                "failed to clone legacy handoff callback stream: {error}"
            ));
            return false;
        }
    };
    try_transfer_negotiated_handoff_opaque(ctx, &mut stream, reply)
}

pub(crate) fn try_transfer_negotiated_handoff_opaque(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &mut crate::platform::ipc::Stream,
    reply: &HelloReply,
) -> bool {
    let Some(negotiated) = negotiated_with_handoff(reply) else {
        return false;
    };
    let Ok(token_bytes) =
        <[u8; HANDOFF_TOKEN_BYTES]>::try_from(negotiated.handle_passed_token.as_slice())
    else {
        return false;
    };

    // Re-seed the one-time token issued by the per-request Hello handler
    // (see the module docs) so the orchestrators own its lifecycle.
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = match tokens.issue_with_random128(now, || Ok(token_bytes)) {
        Ok(issued) => issued,
        Err(error) => {
            log_handoff_fallback(&format!("failed to re-seed issued token: {error}"));
            return false;
        }
    };
    acks.register(
        issued,
        PendingHandoffBackend::for_service(ctx.service_name),
        now,
    );

    let backend_stream = match connect_ipc_stream(ctx.handoff_endpoint) {
        Ok(stream) => stream,
        Err(error) => {
            acks.abandon(&mut tokens, &issued);
            log_handoff_fallback(&format!(
                "failed to dial backend handoff endpoint {}: {error}",
                ctx.handoff_endpoint
            ));
            return false;
        }
    };
    let io_deadline = Instant::now()
        .checked_add(acks.ack_deadline())
        .unwrap_or_else(Instant::now);
    let mut delivery = WireHandoffDelivery::new_platform_stream(
        backend_stream,
        ctx.service_name,
        negotiated.connection_id,
        io_deadline,
    );

    if !run_platform_handoff(
        ctx,
        &*client_stream,
        issued,
        &mut tokens,
        &mut acks,
        &mut delivery,
    ) {
        return false;
    }

    true
}

/// Complete a negotiated handoff for callers that do not need proxy-fallback
/// ownership information.
pub fn complete_negotiated_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &mut interprocess::local_socket::Stream,
    reply: &HelloReply,
) {
    let _ = try_complete_negotiated_handoff(ctx, client_stream, reply);
}

fn clone_legacy_stream_into_platform(
    stream: &interprocess::local_socket::Stream,
) -> std::io::Result<crate::platform::ipc::Stream> {
    use interprocess::TryClone as _;

    stream
        .try_clone()
        .map(running_process_platform_internal::from_legacy_ipc_stream)
}

/// Record the post-transfer notification result without changing ownership.
/// After backend ACK, returning `false` would authorize an unsafe proxy of a
/// connection the backend already owns.
fn handoff_transferred_after_ready_event<T, E: std::fmt::Display>(result: Result<T, E>) -> bool {
    if let Err(error) = result {
        log_handoff_fallback(&format!(
            "completed handoff but failed to relay handoff-ready event to client: {error}"
        ));
    }
    true
}

/// Return the negotiated reply when it carries a handoff to complete.
fn negotiated_with_handoff(reply: &HelloReply) -> Option<&Negotiated> {
    let HelloReplyResult::Negotiated(negotiated) = reply.result.as_ref()? else {
        return None;
    };
    if negotiated.server_capabilities & CAP_HANDLE_PASSING == 0
        || negotiated.handle_passed_token.is_empty()
    {
        return None;
    }
    Some(negotiated)
}

#[cfg(windows)]
fn run_platform_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &crate::platform::ipc::Stream,
    issued: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut WireHandoffDelivery<crate::platform::ipc::Stream>,
) -> bool {
    use super::handoff::{HandoffDelivery, WindowsHandoffStage};

    // The accept loop is sequential, so holding the registry lock for the
    // duration of one handoff cannot deadlock against another connection.
    let backend_pid = {
        let registry = ctx
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(backend) =
            registry.get_any_build(ctx.instance, ctx.service_name, ctx.service_version)
        else {
            acks.abandon(tokens, &issued);
            log_handoff_fallback("registered backend disappeared before handoff delivery");
            return false;
        };
        backend.daemon_process.pid
    };

    let endpoint = match crate::platform::ipc::Endpoint::new(ctx.handoff_endpoint.to_owned()) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            acks.abandon(tokens, &issued);
            log_handoff_fallback(&format!("invalid backend handoff endpoint: {error}"));
            return false;
        }
    };
    let attachment = match client_stream.transfer_to_backend(
        delivery.stream(),
        &endpoint,
        backend_pid,
        issued.as_bytes(),
    ) {
        Ok(attachment) => attachment,
        Err(error) => {
            acks.abandon(tokens, &issued);
            log_handoff_fallback(&format!(
                "abandoned at {:?} stage: {error}",
                WindowsHandoffStage::Duplicate
            ));
            return false;
        }
    };
    if let Err(error) = delivery.deliver_attachment(attachment, &issued) {
        acks.abandon(tokens, &issued);
        log_handoff_fallback(&format!(
            "abandoned at {:?} stage: {error}; duplicated connection handle leaks in backend pid \
             {backend_pid} until it exits",
            WindowsHandoffStage::Deliver
        ));
        return false;
    }

    let deadline = Instant::now()
        .checked_add(acks.ack_deadline())
        .unwrap_or_else(Instant::now);
    let acknowledged_at = match delivery.await_backend_ack(&issued, deadline) {
        Ok(observed_at) => observed_at,
        Err(error) => {
            acks.abandon(tokens, &issued);
            let explicitly_rejected = delivery.backend_explicitly_rejected();
            log_handoff_fallback(&format!(
                "abandoned at {:?} stage: {error}; duplicated connection handle leaks in backend \
                 pid {backend_pid} until it exits",
                WindowsHandoffStage::AwaitAck
            ));
            return windows_fallback_requires_relinquish(
                WindowsHandoffStage::AwaitAck,
                explicitly_rejected,
            );
        }
    };
    if let Err(error) = acks.acknowledge(tokens, &issued, acknowledged_at) {
        tokens.revoke(&issued);
        log_handoff_fallback(&format!(
            "abandoned at {:?} stage: {error}; duplicated connection handle remains owned by \
             backend pid {backend_pid}",
            WindowsHandoffStage::Acknowledge
        ));
    }
    true
}

#[cfg(unix)]
fn run_platform_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &crate::platform::ipc::Stream,
    issued: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut WireHandoffDelivery<crate::platform::ipc::Stream>,
) -> bool {
    use std::cell::RefCell;
    use std::time::Instant;

    use super::handoff::{
        execute_unix_handoff_with_transport, HandoffDelivery, HandoffDeliveryError,
        ScmRightsAttempt, ScmRightsError, ScmRightsResult, UnixFileDescriptor, UnixHandoffAckWait,
        UnixHandoffOutcome, UnixHandoffRequest, UnixHandoffSocket, WindowsHandleValue,
    };

    use crate::platform::ipc::HandoffTransferErrorKind;

    let endpoint = match crate::platform::ipc::Endpoint::new(ctx.handoff_endpoint.to_owned()) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            acks.abandon(tokens, &issued);
            log_handoff_fallback(&format!("invalid backend handoff endpoint: {error}"));
            return false;
        }
    };
    let request = UnixHandoffRequest::new(
        UnixFileDescriptor::new(0),
        UnixHandoffSocket::new(ctx.handoff_endpoint),
        issued,
    );

    // The transport closure and the ACK wait both need the one wire
    // delivery channel; they run strictly one after the other, so a
    // RefCell resolves the shared mutable borrow safely.
    let delivery = RefCell::new(delivery);
    let transport = |attempt: &ScmRightsAttempt| -> ScmRightsResult {
        let mut delivery = delivery.borrow_mut();
        client_stream
            .transfer_to_backend(
                delivery.stream(),
                &endpoint,
                0,
                attempt.handoff_token.as_bytes(),
            )
            .map_err(|error| {
                if error.may_have_reached_backend() {
                    return ScmRightsError::PartialSend {
                        fd: -1,
                        socket: attempt.backend_socket.path.clone(),
                        sent_bytes: 1,
                        expected_bytes: attempt.handoff_token.as_bytes().len(),
                    };
                }
                match error.kind() {
                    HandoffTransferErrorKind::Unsupported => ScmRightsError::UnsupportedPlatform,
                    HandoffTransferErrorKind::PermissionDenied => {
                        ScmRightsError::PermissionDenied {
                            fd: -1,
                            socket: attempt.backend_socket.path.clone(),
                        }
                    }
                    HandoffTransferErrorKind::BackendUnavailable => {
                        ScmRightsError::BackendSocketUnavailable {
                            socket: attempt.backend_socket.path.clone(),
                        }
                    }
                    HandoffTransferErrorKind::WouldBlock => ScmRightsError::WouldBlock {
                        socket: attempt.backend_socket.path.clone(),
                    },
                    HandoffTransferErrorKind::Failed => ScmRightsError::SendFailed {
                        fd: -1,
                        socket: attempt.backend_socket.path.clone(),
                        raw_os_error: None,
                    },
                }
            })?;
        delivery
            .deliver(WindowsHandleValue::new(0), &attempt.handoff_token)
            .map_err(|error| {
                log_handoff_fallback(&format!("failed to write HandoffOffer frame: {error}"));
                ScmRightsError::PostTransferDeliveryFailed {
                    fd: attempt.fd.raw(),
                    socket: attempt.backend_socket.path.clone(),
                }
            })?;
        Ok(super::handoff::ScmRightsSuccess::new(
            attempt.fd,
            attempt.backend_socket.clone(),
            attempt.handoff_token,
        ))
    };

    struct DeliveryAckWait<'a, 'b> {
        delivery: &'a RefCell<&'b mut WireHandoffDelivery<crate::platform::ipc::Stream>>,
    }
    impl UnixHandoffAckWait for DeliveryAckWait<'_, '_> {
        fn await_backend_ack(
            &mut self,
            token: &HandoffToken,
            deadline: Instant,
        ) -> Result<Instant, HandoffDeliveryError> {
            self.delivery
                .borrow_mut()
                .await_backend_ack(token, deadline)
        }
    }
    let mut ack_wait = DeliveryAckWait {
        delivery: &delivery,
    };

    let outcome =
        execute_unix_handoff_with_transport(tokens, acks, &request, transport, &mut ack_wait);
    let explicitly_rejected = delivery.borrow().backend_explicitly_rejected();
    match outcome {
        UnixHandoffOutcome::Completed(_) => true,
        UnixHandoffOutcome::FallbackToReconnect(fallback) => {
            let reached = if fallback.fd_reached_backend {
                "; a duplicated descriptor already reached the backend and lives until it closes it"
            } else {
                ""
            };
            log_handoff_fallback(&format!(
                "abandoned at {:?} stage: {}{reached}",
                fallback.stage, fallback.detail
            ));
            unix_fallback_requires_relinquish(
                fallback.stage,
                fallback.fd_reached_backend,
                explicitly_rejected,
            )
        }
    }
}

#[cfg(windows)]
fn windows_fallback_requires_relinquish(
    stage: super::handoff::WindowsHandoffStage,
    explicitly_rejected: bool,
) -> bool {
    use super::handoff::WindowsHandoffStage;

    match stage {
        WindowsHandoffStage::Duplicate | WindowsHandoffStage::Deliver => false,
        WindowsHandoffStage::AwaitAck => !explicitly_rejected,
        WindowsHandoffStage::Acknowledge => true,
    }
}

#[cfg(unix)]
fn unix_fallback_requires_relinquish(
    stage: super::handoff::UnixHandoffStage,
    fd_reached_backend: bool,
    explicitly_rejected: bool,
) -> bool {
    use super::handoff::UnixHandoffStage;

    match stage {
        UnixHandoffStage::Send => fd_reached_backend,
        UnixHandoffStage::AwaitAck => !explicitly_rejected,
        UnixHandoffStage::Acknowledge => true,
    }
}

/// Log one silent serve-side handoff fallback.
///
/// The broker has no tracing subscriber on the client-feature build; the
/// existing convention (lifecycle SID probing, the broker binary) is
/// stderr. Failures here are silent toward the client by contract.
fn log_handoff_fallback(detail: &str) {
    eprintln!("running-process-broker: handoff fallback: {detail}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallible_handoff_entry_point_has_a_boolean_completion_contract() {
        let _entry_point: for<'context, 'stream, 'reply> fn(
            &'context ServeHandoffContext<'context>,
            &'stream mut interprocess::local_socket::Stream,
            &'reply HelloReply,
        ) -> bool = try_complete_negotiated_handoff;
        let _transfer_entry_point: for<'context, 'stream, 'reply> fn(
            &'context ServeHandoffContext<'context>,
            &'stream mut interprocess::local_socket::Stream,
            &'reply HelloReply,
        ) -> bool = try_transfer_negotiated_handoff;
    }

    #[test]
    fn failed_ready_event_after_backend_ack_still_forbids_proxy_fallback() {
        assert!(handoff_transferred_after_ready_event::<(), _>(Err(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client disconnected")
        )));
        assert!(handoff_transferred_after_ready_event(Ok::<
            (),
            std::io::Error,
        >(())));
    }

    #[test]
    #[cfg(unix)]
    fn explicit_rejection_is_safe_but_an_unobserved_ack_is_ambiguous() {
        use super::super::handoff::UnixHandoffStage;

        assert!(!unix_fallback_requires_relinquish(
            UnixHandoffStage::Send,
            false,
            false
        ));
        assert!(unix_fallback_requires_relinquish(
            UnixHandoffStage::Send,
            true,
            false
        ));
        assert!(!unix_fallback_requires_relinquish(
            UnixHandoffStage::AwaitAck,
            true,
            true
        ));
        assert!(unix_fallback_requires_relinquish(
            UnixHandoffStage::AwaitAck,
            true,
            false
        ));
        assert!(unix_fallback_requires_relinquish(
            UnixHandoffStage::Acknowledge,
            true,
            false
        ));
    }

    #[test]
    #[cfg(windows)]
    fn explicit_rejection_is_safe_but_an_unobserved_ack_is_ambiguous() {
        use super::super::handoff::WindowsHandoffStage;

        assert!(!windows_fallback_requires_relinquish(
            WindowsHandoffStage::Duplicate,
            false
        ));
        assert!(!windows_fallback_requires_relinquish(
            WindowsHandoffStage::Deliver,
            false
        ));
        assert!(!windows_fallback_requires_relinquish(
            WindowsHandoffStage::AwaitAck,
            true
        ));
        assert!(windows_fallback_requires_relinquish(
            WindowsHandoffStage::AwaitAck,
            false
        ));
        assert!(windows_fallback_requires_relinquish(
            WindowsHandoffStage::Acknowledge,
            false
        ));
    }
}
