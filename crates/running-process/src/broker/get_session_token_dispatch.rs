//! `GetSessionToken` RPC dispatch (zackees/soldr#2361 Phase 2, #2363).
//!
//! Mirrors [`super::get_http_endpoint_dispatch`]'s shape exactly: this
//! slice exposes the typed request/response handler over
//! [`super::server::session_token::SessionTokenAuthority`] so a later
//! slice can wire it into the broker's control-channel connection loop.
//! See `broker_v2_control.proto`'s `GetSessionTokenRequest` doc comment
//! for why this lives on the v2 control channel rather than the frozen
//! v1 `Hello`/`Negotiated` envelope.

use prost::Message;

use super::protocol_v2::{GetSessionTokenRequest, GetSessionTokenResponse};
use super::server::session_token::SessionTokenAuthority;

/// Errors from [`decode_request_and_dispatch`].
#[derive(Debug, thiserror::Error)]
pub enum GetSessionTokenDispatchError {
    /// The incoming frame body did not decode as `GetSessionTokenRequest`.
    #[error("decode GetSessionTokenRequest: {0}")]
    Decode(#[from] prost::DecodeError),

    /// Encoding the response failed.
    #[error("encode GetSessionTokenResponse: {0}")]
    Encode(#[from] prost::EncodeError),
}

/// Decode an incoming `GetSessionTokenRequest` frame body and produce a
/// serialized `GetSessionTokenResponse` body the connection loop can write
/// back via `protocol::write_frame`.
///
/// Read-only lookup against `authority` -- this never mints or registers a
/// token itself. Minting happens once, at daemon launch
/// (`HelloRouter::launch_backend` calling `SessionTokenAuthority::register_daemon`);
/// this RPC only answers "what is it right now" for a caller that already
/// knows the daemon exists (e.g. from a prior `Hello` negotiation).
pub fn decode_request_and_dispatch(
    request_body: &[u8],
    authority: &SessionTokenAuthority,
) -> Result<Vec<u8>, GetSessionTokenDispatchError> {
    let request = GetSessionTokenRequest::decode(request_body)?;
    let response = match authority.composed_token_for(&request.daemon_id) {
        Some(session_token) => GetSessionTokenResponse {
            found: true,
            session_token,
        },
        None => GetSessionTokenResponse {
            found: false,
            session_token: Vec::new(),
        },
    };
    let mut body = Vec::with_capacity(response.encoded_len());
    response.encode(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::server::session_token::TokenHalf;

    fn authority_with_one_daemon(daemon_id: &str) -> SessionTokenAuthority {
        let mut authority = SessionTokenAuthority::with_broker_token(TokenHalf::from_bytes(
            [0xAB; crate::broker::server::session_token::SESSION_TOKEN_HALF_BYTES],
        ));
        authority
            .register_daemon(daemon_id.to_string())
            .expect("register_daemon");
        authority
    }

    fn encode_request(daemon_id: &str) -> Vec<u8> {
        let req = GetSessionTokenRequest {
            daemon_id: daemon_id.to_string(),
        };
        let mut body = Vec::with_capacity(req.encoded_len());
        req.encode(&mut body).expect("encode request");
        body
    }

    #[test]
    fn dispatch_returns_composed_token_for_a_registered_daemon() {
        let authority = authority_with_one_daemon("zccache");
        let body = encode_request("zccache");

        let resp_body = decode_request_and_dispatch(&body, &authority).expect("dispatch succeeds");
        let resp = GetSessionTokenResponse::decode(resp_body.as_slice()).expect("decode response");

        assert!(resp.found);
        assert_eq!(
            resp.session_token,
            authority.composed_token_for("zccache").unwrap()
        );
    }

    #[test]
    fn dispatch_reports_not_found_for_an_unregistered_daemon() {
        let authority = authority_with_one_daemon("zccache");
        let body = encode_request("some-other-daemon");

        let resp_body = decode_request_and_dispatch(&body, &authority).expect("dispatch succeeds");
        let resp = GetSessionTokenResponse::decode(resp_body.as_slice()).expect("decode response");

        assert!(!resp.found);
        assert!(resp.session_token.is_empty());
    }

    #[test]
    fn dispatch_rejects_malformed_request_body() {
        let authority = authority_with_one_daemon("zccache");
        let err = decode_request_and_dispatch(&[0xFF; 4], &authority)
            .expect_err("malformed request body should be rejected");
        match err {
            GetSessionTokenDispatchError::Decode(_) => {}
            other => panic!("expected Decode error, got: {other:?}"),
        }
    }
}
