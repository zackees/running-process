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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlatformHandoffStage {
    Transfer,
    Deliver,
    AwaitAck,
    Acknowledge,
}

fn run_platform_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &crate::platform::ipc::Stream,
    issued: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut WireHandoffDelivery<crate::platform::ipc::Stream>,
) -> bool {
    use super::handoff::HandoffDelivery;

    // Windows needs the verified process id; Unix transfers over the already
    // connected control stream and ignores it. Zero is not a valid backend
    // process id, so the Windows facade rejects a vanished registry entry
    // through its existing process-open failure path.
    let backend_pid = {
        let registry = ctx
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry
            .get_any_build(ctx.instance, ctx.service_name, ctx.service_version)
            .map(|backend| backend.daemon_process.pid)
            .unwrap_or_default()
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
            let backend_may_adopt = error.may_have_reached_backend();
            acks.abandon(tokens, &issued);
            let ownership = if backend_may_adopt {
                "; a connection copy may already have reached the backend"
            } else {
                ""
            };
            log_handoff_fallback(&format!(
                "abandoned at {:?} stage: {error}{ownership}",
                PlatformHandoffStage::Transfer
            ));
            return fallback_requires_relinquish(
                PlatformHandoffStage::Transfer,
                backend_may_adopt,
                false,
            );
        }
    };
    if let Err(error) = delivery.deliver_attachment(attachment, &issued) {
        let backend_may_adopt = attachment.backend_may_adopt_before_offer();
        acks.abandon(tokens, &issued);
        log_handoff_fallback(&format!(
            "abandoned at {:?} stage: {error}; a transferred connection copy remains in the \
             backend",
            PlatformHandoffStage::Deliver
        ));
        return fallback_requires_relinquish(
            PlatformHandoffStage::Deliver,
            backend_may_adopt,
            false,
        );
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
                "abandoned at {:?} stage: {error}; a transferred connection copy remains in the \
                 backend",
                PlatformHandoffStage::AwaitAck
            ));
            return fallback_requires_relinquish(
                PlatformHandoffStage::AwaitAck,
                true,
                explicitly_rejected,
            );
        }
    };
    if let Err(error) = acks.acknowledge(tokens, &issued, acknowledged_at) {
        tokens.revoke(&issued);
        log_handoff_fallback(&format!(
            "abandoned at {:?} stage: {error}; the transferred connection remains owned by the \
             backend",
            PlatformHandoffStage::Acknowledge
        ));
    }
    true
}

fn fallback_requires_relinquish(
    stage: PlatformHandoffStage,
    backend_may_adopt: bool,
    explicitly_rejected: bool,
) -> bool {
    match stage {
        PlatformHandoffStage::Transfer | PlatformHandoffStage::Deliver => backend_may_adopt,
        PlatformHandoffStage::AwaitAck => !explicitly_rejected,
        PlatformHandoffStage::Acknowledge => true,
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
    fn explicit_rejection_is_safe_but_an_unobserved_ack_is_ambiguous() {
        assert!(!fallback_requires_relinquish(
            PlatformHandoffStage::Transfer,
            false,
            false
        ));
        assert!(fallback_requires_relinquish(
            PlatformHandoffStage::Transfer,
            true,
            false
        ));
        assert!(!fallback_requires_relinquish(
            PlatformHandoffStage::Deliver,
            false,
            false,
        ));
        assert!(fallback_requires_relinquish(
            PlatformHandoffStage::Deliver,
            true,
            false,
        ));
        assert!(!fallback_requires_relinquish(
            PlatformHandoffStage::AwaitAck,
            true,
            true
        ));
        assert!(fallback_requires_relinquish(
            PlatformHandoffStage::AwaitAck,
            true,
            false
        ));
        assert!(fallback_requires_relinquish(
            PlatformHandoffStage::Acknowledge,
            false,
            false
        ));
    }
}
