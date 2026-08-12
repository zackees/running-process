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
use crate::broker::client::connect_local_socket;
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

    let backend_stream = match connect_local_socket(ctx.handoff_endpoint) {
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
    let mut delivery = WireHandoffDelivery::new_local_socket(
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

    // Relay the handoff-ready EVENT to the waiting client. The token was
    // consumed exactly once above; a failed relay write means the client
    // is gone and there is nothing further to clean up on the broker side.
    let ack = HandoffAck {
        token: token_bytes.to_vec(),
        accepted: true,
        error_detail: String::new(),
        correlation_id: negotiated.connection_id,
    };
    let frame = handoff_ready_frame(&ack);
    handoff_transferred_after_ready_event(write_frame(client_stream, &frame.encode_to_vec()))
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
    client_stream: &interprocess::local_socket::Stream,
    issued: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut WireHandoffDelivery<interprocess::local_socket::Stream>,
) -> bool {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    use super::handoff::{
        execute_verified_windows_handoff, WindowsHandleValue, WindowsHandoffOutcome,
    };

    let pipe_handle = match client_stream {
        interprocess::local_socket::Stream::NamedPipe(stream) => {
            WindowsHandleValue::new(stream.as_handle().as_raw_handle() as usize)
        }
    };

    // The accept loop is sequential, so holding the registry lock for the
    // duration of one handoff cannot deadlock against another connection.
    let registry = ctx
        .registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(backend) = registry.get_any_build(ctx.instance, ctx.service_name, ctx.service_version)
    else {
        acks.abandon(tokens, &issued);
        log_handoff_fallback("registered backend disappeared before handoff delivery");
        return false;
    };

    let outcome =
        execute_verified_windows_handoff(backend, pipe_handle, issued, tokens, acks, delivery);
    let explicitly_rejected = delivery.backend_explicitly_rejected();
    match outcome {
        WindowsHandoffOutcome::Completed(_) => true,
        WindowsHandoffOutcome::FallbackToReconnect(fallback) => {
            let leak = match fallback.leaked_backend_handle {
                Some(handle) => format!(
                    "; duplicated handle {:#x} leaks in backend pid {} until it exits",
                    handle.get(),
                    backend.daemon_process.pid
                ),
                None => String::new(),
            };
            log_handoff_fallback(&format!(
                "abandoned at {:?} stage: {}{leak}",
                fallback.stage, fallback.detail
            ));
            windows_fallback_requires_relinquish(fallback.stage, explicitly_rejected)
        }
    }
}

#[cfg(unix)]
fn run_platform_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &interprocess::local_socket::Stream,
    issued: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut WireHandoffDelivery<interprocess::local_socket::Stream>,
) -> bool {
    use std::cell::RefCell;
    use std::os::fd::{AsFd, AsRawFd};
    use std::time::Instant;

    use super::handoff::{
        execute_unix_handoff_with_transport, try_send_scm_rights_over, HandoffDelivery,
        HandoffDeliveryError, ScmRightsAttempt, ScmRightsError, ScmRightsResult,
        UnixFileDescriptor, UnixHandoffAckWait, UnixHandoffOutcome, UnixHandoffRequest,
        UnixHandoffSocket, WindowsHandleValue,
    };

    let client_fd = match client_stream {
        interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
    };
    let backend_fd = match delivery.stream() {
        interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
    };
    let request = UnixHandoffRequest::new(
        UnixFileDescriptor::new(client_fd),
        UnixHandoffSocket::new(ctx.handoff_endpoint),
        issued,
    );

    // The transport closure and the ACK wait both need the one wire
    // delivery channel; they run strictly one after the other, so a
    // RefCell resolves the shared mutable borrow safely.
    let delivery = RefCell::new(delivery);
    let transport = |attempt: &ScmRightsAttempt| -> ScmRightsResult {
        let mut delivery = delivery.borrow_mut();
        let sent = try_send_scm_rights_over(backend_fd, attempt)?;
        delivery
            .deliver(WindowsHandleValue::new(0), &attempt.handoff_token)
            .map_err(|error| {
                log_handoff_fallback(&format!("failed to write HandoffOffer frame: {error}"));
                ScmRightsError::SendFailed {
                    fd: attempt.fd.raw(),
                    socket: attempt.backend_socket.path.clone(),
                    raw_os_error: None,
                }
            })?;
        Ok(sent)
    };

    struct DeliveryAckWait<'a, 'b> {
        delivery: &'a RefCell<&'b mut WireHandoffDelivery<interprocess::local_socket::Stream>>,
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
            unix_fallback_requires_relinquish(fallback.stage, explicitly_rejected)
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
    explicitly_rejected: bool,
) -> bool {
    use super::handoff::UnixHandoffStage;

    match stage {
        UnixHandoffStage::Send => false,
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
            false
        ));
        assert!(!unix_fallback_requires_relinquish(
            UnixHandoffStage::AwaitAck,
            true
        ));
        assert!(unix_fallback_requires_relinquish(
            UnixHandoffStage::AwaitAck,
            false
        ));
        assert!(unix_fallback_requires_relinquish(
            UnixHandoffStage::Acknowledge,
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
