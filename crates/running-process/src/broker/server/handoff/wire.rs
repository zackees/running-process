//! Production wire-frame handoff delivery over a broker↔backend control
//! connection (#354, slice 6).
//!
//! Earlier slices abstracted delivery of the `(handle value, token)` pair
//! behind [`HandoffDelivery`] because the v1
//! envelope reserved no backend→broker ACK frame. This module closes that
//! gap with two envelope messages riding the existing v1 frame layout on a
//! framed broker↔backend connection:
//!
//! - [`HandoffOffer`] (broker → backend, `FRAME_KIND_REQUEST`): the
//!   duplicated handle value (Windows; zero on Unix where the fd travels
//!   via `SCM_RIGHTS`), the 16-byte one-time token, the service name, and
//!   a correlation id.
//! - [`HandoffAck`] (backend → broker, `FRAME_KIND_RESPONSE`): the token
//!   echo, an accepted flag plus error detail, and the correlation id echo.
//!
//! Both ride `Frame.payload` under [`HANDOFF_PAYLOAD_PROTOCOL`], mirroring
//! how Hello (`0x00`), admin verbs (`0xAD01`), and endpoint probes
//! (`0xB232`) share the envelope.
//!
//! [`WireHandoffDelivery`] implements [`HandoffDelivery`] over any framed
//! `Read + Write` stream (the same local-socket framing used
//! by every other broker connection). Any malformed frame, token-echo
//! mismatch, correlation-id mismatch, refused ACK, or overdue ACK is
//! reported as a delivery error; the orchestration in
//! [`super::orchestrate`] then revokes the token and falls back to the
//! `backend_pipe` reconnect path. This module never panics on wire input.
//!
//! # Deadline enforcement
//!
//! [`WireHandoffDelivery::await_backend_ack`] performs a blocking framed
//! read. Wall-clock interruption of a backend that never writes anything
//! relies on the caller configuring a read timeout on the underlying
//! stream (e.g. `set_nonblocking` + polling, or a socket read timeout);
//! a closed/erroring stream surfaces immediately. Independently of the
//! stream, an ACK observed after `deadline` is rejected here, and the
//! [`HandoffAckRegistry`](super::HandoffAckRegistry) re-validates the
//! observation instant against the deadline registered at issuance, so a
//! slow stream can never complete an overdue handoff.

use std::io::{Read, Write};
use std::time::Instant;

use prost::Message;

use crate::broker::protocol::{
    read_frame, validate_frame_envelope, write_frame, Frame, FrameKind, FrameValidationError,
    HandoffAck, HandoffOffer, PayloadEncoding, PROTOCOL_VERSION,
};
use crate::broker::server::handoff::handoff_token::HandoffToken;
use crate::broker::server::handoff::orchestrate::{HandoffDelivery, HandoffDeliveryError};
use crate::broker::server::handoff::windows::WindowsHandleValue;

/// Payload protocol reserved for broker↔backend handoff offer/ACK frames.
///
/// Re-exported from the authoritative
/// [`registry`](crate::broker::protocol::registry), which owns every v1
/// payload-protocol ID (#375). Lives in the same envelope-multiplexing
/// space as the control plane (`0x00`), admin verbs (`0xAD01`), and
/// backend-handle endpoint probes (`0xB232`).
pub use crate::broker::protocol::registry::HANDOFF_PAYLOAD_PROTOCOL;

/// Build the v1 frame carrying one broker→backend [`HandoffOffer`].
pub fn handoff_offer_frame(offer: &HandoffOffer) -> Frame {
    let mut payload = Vec::with_capacity(64);
    offer.encode(&mut payload).expect(
        "prost encoding HandoffOffer into Vec cannot fail because Vec writes are infallible",
    );
    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: HANDOFF_PAYLOAD_PROTOCOL,
        payload,
        request_id: offer.correlation_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

/// Build the v1 frame carrying one backend→broker [`HandoffAck`].
pub fn handoff_ack_frame(ack: &HandoffAck) -> Frame {
    let mut payload = Vec::with_capacity(64);
    ack.encode(&mut payload)
        .expect("prost encoding HandoffAck into Vec cannot fail because Vec writes are infallible");
    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Response as i32,
        payload_protocol: HANDOFF_PAYLOAD_PROTOCOL,
        payload,
        request_id: ack.correlation_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

/// Build the v1 frame relaying one completed handoff to the CLIENT (#354,
/// slice 7).
///
/// After the backend ACKs a [`HandoffOffer`], the broker relays the
/// backend's [`HandoffAck`] verbatim to the waiting client on the client's
/// original broker connection — the same socket that carried Hello and is
/// now backend-served. The relay rides the envelope as a broker→client push
/// (`FRAME_KIND_EVENT`) under [`HANDOFF_PAYLOAD_PROTOCOL`], mirroring how
/// the offer/ACK pair rides the broker↔backend control connection. The
/// client matches the relay by the one-time token echo (the only handoff
/// secret it knows); the correlation id is broker↔backend bookkeeping that
/// the client does not validate.
pub fn handoff_ready_frame(ack: &HandoffAck) -> Frame {
    let mut payload = Vec::with_capacity(64);
    ack.encode(&mut payload)
        .expect("prost encoding HandoffAck into Vec cannot fail because Vec writes are infallible");
    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Event as i32,
        payload_protocol: HANDOFF_PAYLOAD_PROTOCOL,
        payload,
        request_id: ack.correlation_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

/// Validate the envelope fields shared by both handoff frame directions.
///
/// Returns the expected-vs-actual mismatch as a static description so both
/// the broker and backend sides report wire violations uniformly.
pub fn validate_handoff_frame(frame: &Frame, expected_kind: FrameKind) -> Result<(), &'static str> {
    validate_frame_envelope(frame, expected_kind, HANDOFF_PAYLOAD_PROTOCOL).map_err(|error| {
        match error {
            FrameValidationError::EnvelopeVersion { .. } => "envelope_version is not v1",
            FrameValidationError::Kind { .. } => match expected_kind {
                FrameKind::Request => "kind is not REQUEST",
                FrameKind::Event => "kind is not EVENT",
                _ => "kind is not RESPONSE",
            },
            FrameValidationError::PayloadProtocol { .. } => "payload_protocol is not handoff",
            FrameValidationError::PayloadEncoding { .. } => "payload is compressed",
        }
    })
}

/// [`HandoffDelivery`] implementation that sends [`HandoffOffer`] frames to
/// the backend over a framed control connection and waits for the matching
/// [`HandoffAck`].
///
/// `deliver` writes one offer frame; `await_backend_ack` reads one response
/// frame and requires the token echo, the correlation id, and the accepted
/// flag to all match. Every violation is a delivery error — the
/// orchestration falls back to reconnect and revokes the token.
#[derive(Debug)]
pub struct WireHandoffDelivery<S> {
    stream: S,
    service_name: String,
    correlation_id: u64,
}

impl<S> WireHandoffDelivery<S> {
    /// Wrap a framed broker↔backend connection for one handoff.
    ///
    /// `correlation_id` ties the offer to its ACK; reuse the request or
    /// connection id of the client Hello that triggered the handoff.
    pub fn new(stream: S, service_name: impl Into<String>, correlation_id: u64) -> Self {
        Self {
            stream,
            service_name: service_name.into(),
            correlation_id,
        }
    }

    /// Return the correlation id stamped on the offer and required on the ACK.
    pub fn correlation_id(&self) -> u64 {
        self.correlation_id
    }

    /// Unwrap the underlying connection (e.g. to keep using it after a
    /// completed handoff).
    pub fn into_stream(self) -> S {
        self.stream
    }
}

impl<S: Read + Write> HandoffDelivery for WireHandoffDelivery<S> {
    fn deliver(
        &mut self,
        handle: WindowsHandleValue,
        token: &HandoffToken,
    ) -> Result<(), HandoffDeliveryError> {
        let offer = HandoffOffer {
            handle_value: handle.get() as u64,
            token: token.as_bytes().to_vec(),
            service_name: self.service_name.clone(),
            correlation_id: self.correlation_id,
        };
        let frame = handoff_offer_frame(&offer);
        let mut bytes = Vec::with_capacity(64);
        frame
            .encode(&mut bytes)
            .expect("prost encoding Frame into Vec cannot fail because Vec writes are infallible");
        write_frame(&mut self.stream, &bytes).map_err(|error| {
            HandoffDeliveryError::DeliveryFailed {
                detail: format!("failed to write HandoffOffer frame: {error}"),
            }
        })?;
        Ok(())
    }

    fn await_backend_ack(
        &mut self,
        token: &HandoffToken,
        deadline: Instant,
    ) -> Result<Instant, HandoffDeliveryError> {
        let bytes = read_frame(&mut self.stream).map_err(|error| {
            ack_not_observed(format!("failed to read HandoffAck frame: {error}"))
        })?;
        let observed_at = Instant::now();
        let frame = Frame::decode(bytes.as_slice()).map_err(|error| {
            ack_not_observed(format!("failed to decode HandoffAck Frame: {error}"))
        })?;
        validate_handoff_frame(&frame, FrameKind::Response)
            .map_err(|detail| ack_not_observed(format!("unexpected HandoffAck frame: {detail}")))?;
        if frame.request_id != self.correlation_id {
            return Err(ack_not_observed(format!(
                "HandoffAck frame request_id {} does not match correlation id {}",
                frame.request_id, self.correlation_id
            )));
        }
        let ack = HandoffAck::decode(frame.payload.as_slice()).map_err(|error| {
            ack_not_observed(format!("failed to decode HandoffAck payload: {error}"))
        })?;
        if ack.correlation_id != self.correlation_id {
            return Err(ack_not_observed(format!(
                "HandoffAck correlation id {} does not match offer correlation id {}",
                ack.correlation_id, self.correlation_id
            )));
        }
        if ack.token != token.as_bytes() {
            return Err(ack_not_observed(
                "HandoffAck token echo does not match the offered token".to_string(),
            ));
        }
        if !ack.accepted {
            return Err(ack_not_observed(format!(
                "backend refused the handoff: {}",
                if ack.error_detail.is_empty() {
                    "no detail provided"
                } else {
                    ack.error_detail.as_str()
                }
            )));
        }
        if observed_at > deadline {
            return Err(ack_not_observed(
                "backend HandoffAck arrived after the ACK deadline".to_string(),
            ));
        }
        Ok(observed_at)
    }
}

fn ack_not_observed(detail: String) -> HandoffDeliveryError {
    HandoffDeliveryError::AckNotObserved { detail }
}
