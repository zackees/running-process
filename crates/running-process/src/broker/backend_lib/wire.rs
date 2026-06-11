//! Backend-side wire handling for broker handoff offers (#354, slice 6).
//!
//! Given a framed connection to the broker (the same v1 local-socket
//! framing used by every other broker connection), this module:
//!
//! 1. reads one [`HandoffOffer`] frame
//!    ([`read_handoff_offer`]),
//! 2. validates and consumes the presented one-time token through the
//!    existing [`accept_handed_off`] path, and
//! 3. replies with a [`HandoffAck`] frame echoing the token and
//!    correlation id ([`respond_to_handoff_offer`]).
//!
//! [`serve_handoff_offer`] composes all three for the common case. A
//! rejected token still produces a well-formed `HandoffAck` with
//! `accepted = false` and the rejection detail, so the broker can fall
//! back to the reconnect path immediately instead of waiting out its ACK
//! deadline.

use std::io::{Read, Write};
use std::time::Instant;

use prost::Message;

use crate::broker::backend_lib::accept_handed_off::{
    accept_handed_off, HandedOffPayload, HandoffAcceptance,
};
use crate::broker::protocol::{
    read_frame, write_frame, Frame, FrameKind, FramingError, HandoffAck, HandoffOffer,
};
use crate::broker::server::handoff::wire::{handoff_ack_frame, validate_handoff_frame};
use crate::broker::server::{HandoffToken, HandoffTokenStore};

/// Errors surfaced while reading or answering a handoff offer frame.
#[derive(Debug, thiserror::Error)]
pub enum BackendHandoffWireError {
    /// v1 framing failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The offer frame could not be decoded.
    #[error("failed to decode HandoffOffer Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// The offer payload could not be decoded.
    #[error("failed to decode HandoffOffer payload: {0}")]
    DecodePayload(prost::DecodeError),
    /// The frame did not match the handoff-offer contract.
    #[error("unexpected HandoffOffer frame: {0}")]
    UnexpectedFrame(&'static str),
}

/// Read and validate one broker→backend [`HandoffOffer`] frame.
pub fn read_handoff_offer<S: Read>(
    stream: &mut S,
) -> Result<HandoffOffer, BackendHandoffWireError> {
    let bytes = read_frame(stream)?;
    let frame = Frame::decode(bytes.as_slice()).map_err(BackendHandoffWireError::DecodeFrame)?;
    validate_handoff_frame(&frame, FrameKind::Request)
        .map_err(BackendHandoffWireError::UnexpectedFrame)?;
    let offer = HandoffOffer::decode(frame.payload.as_slice())
        .map_err(BackendHandoffWireError::DecodePayload)?;
    if frame.request_id != offer.correlation_id {
        return Err(BackendHandoffWireError::UnexpectedFrame(
            "frame request_id does not match HandoffOffer correlation_id",
        ));
    }
    Ok(offer)
}

/// Validate/consume one received offer and write the matching [`HandoffAck`].
///
/// The presented token rides the offer; `expected_token` is the token the
/// backend was told to expect (it arrived out of band, e.g. through the
/// spawn environment). On acceptance the one-time token is consumed from
/// `pending_tokens` exactly once and the ACK reports `accepted = true`; on
/// rejection the ACK carries `accepted = false` plus the rejection detail.
/// Either way the ACK echoes the offer's token bytes and correlation id.
pub fn respond_to_handoff_offer<S: Write>(
    stream: &mut S,
    pending_tokens: &mut HandoffTokenStore,
    expected_token: HandoffToken,
    offer: HandoffOffer,
    now: Instant,
) -> Result<HandoffAcceptance<HandoffOffer>, BackendHandoffWireError> {
    let presented_token = offer.token.clone();
    let correlation_id = offer.correlation_id;
    let payload = HandedOffPayload::new(expected_token, presented_token.clone(), offer);
    let acceptance = accept_handed_off(pending_tokens, payload, now);

    let ack = match &acceptance {
        HandoffAcceptance::Accepted(_) => HandoffAck {
            token: presented_token,
            accepted: true,
            error_detail: String::new(),
            correlation_id,
        },
        HandoffAcceptance::Rejected(rejected) => HandoffAck {
            token: presented_token,
            accepted: false,
            error_detail: rejected.reason.to_string(),
            correlation_id,
        },
    };
    write_handoff_ack(stream, &ack)?;
    Ok(acceptance)
}

/// Write one backend→broker [`HandoffAck`] frame.
pub fn write_handoff_ack<S: Write>(
    stream: &mut S,
    ack: &HandoffAck,
) -> Result<(), BackendHandoffWireError> {
    let frame = handoff_ack_frame(ack);
    let mut bytes = Vec::with_capacity(64);
    frame
        .encode(&mut bytes)
        .expect("prost encoding Frame into Vec cannot fail because Vec writes are infallible");
    write_frame(stream, &bytes)?;
    Ok(())
}

/// Read one offer, validate/consume the token, and reply with the ACK.
///
/// Convenience composition of [`read_handoff_offer`] and
/// [`respond_to_handoff_offer`] for backends serving one handoff per
/// control exchange.
pub fn serve_handoff_offer<S: Read + Write>(
    stream: &mut S,
    pending_tokens: &mut HandoffTokenStore,
    expected_token: HandoffToken,
    now: Instant,
) -> Result<HandoffAcceptance<HandoffOffer>, BackendHandoffWireError> {
    let offer = read_handoff_offer(stream)?;
    respond_to_handoff_offer(stream, pending_tokens, expected_token, offer, now)
}
