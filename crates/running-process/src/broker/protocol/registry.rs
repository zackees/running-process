//! Single authoritative registry of v1 broker wire-protocol constants
//! (#375).
//!
//! Every payload-protocol ID multiplexed over the frozen v1 `Frame`
//! envelope, plus the negotiated protocol version, is defined HERE and
//! only here. Subsystem modules `pub use` these constants (the old
//! public paths remain valid re-export shims) so the public API surface
//! is unchanged while the values have exactly one definition site.
//!
//! These values are part of the FROZEN-FOREVER v1 wire contract — see
//! `proto/broker_v1_envelope.proto` and #228. Never change an existing
//! value; only append new IDs (and keep them pairwise distinct — the
//! unit test below enforces this).
//!
//! # Payload-protocol ID registry
//!
//! | ID       | Constant                                   | Purpose                                        | Payload proto message            |
//! |----------|--------------------------------------------|------------------------------------------------|----------------------------------|
//! | `0x0000` | [`CONTROL_PAYLOAD_PROTOCOL`]               | Control plane: client Hello / broker HelloReply | `Hello` / `HelloReply`           |
//! | `0xAD01` | [`ADMIN_PAYLOAD_PROTOCOL`]                 | Admin verbs over the broker control socket      | `AdminRequest` / `AdminReply`    |
//! | `0xB232` | [`BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL`]  | `BackendHandle` endpoint identity probes        | raw nonce / nonce + `DaemonProcess` |
//! | `0xD0FF` | [`HANDOFF_PAYLOAD_PROTOCOL`]               | Connection handoff offer/ACK and client relay   | `HandoffOffer` / `HandoffAck`    |

/// Negotiated v1 broker protocol version.
///
/// This is the value carried in `Frame.envelope_version` (a u32 protobuf
/// field) and in the `Hello`/`Refused`/`Negotiated` protocol-range fields
/// (`client_min_protocol`, `daemon_max_protocol`, `negotiated_protocol`,
/// ...).
///
/// NOT to be confused with [`crate::broker::FRAMING_VERSION_V1`]: that is
/// the single raw `u8` framing byte written before every length-prefixed
/// frame body on the wire. Both happen to be `1` in v1, but they are
/// conceptually distinct version axes (outer framing layout vs negotiated
/// protocol schema) and MUST stay separate named constants.
pub const PROTOCOL_VERSION: u32 = 1;

/// Outer framing byte for every v1 broker connection (re-exported here so
/// the registry names both version axes side by side).
///
/// Distinct from [`PROTOCOL_VERSION`]: this `u8` prefixes the raw wire
/// layout `[u8 framing_version][u32 LE body_length][prost body]`, while
/// `PROTOCOL_VERSION` is the negotiated protocol schema version carried
/// inside the decoded `Frame`. Both are `1` in v1 by coincidence — do not
/// collapse them. Canonical definition: [`crate::broker::FRAMING_VERSION_V1`].
pub use crate::broker::FRAMING_VERSION_V1;

/// Payload protocol for v1 control-plane frames (Hello / HelloReply).
pub const CONTROL_PAYLOAD_PROTOCOL: u32 = 0x00;

/// Payload protocol value for v1 admin request/reply frames.
pub const ADMIN_PAYLOAD_PROTOCOL: u32 = 0xAD01;

/// Payload protocol reserved for `BackendHandle` endpoint identity probes.
pub const BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL: u32 = 0xB232;

/// Payload protocol reserved for broker↔backend handoff offer/ACK frames
/// and the broker→client handoff-ready relay.
pub const HANDOFF_PAYLOAD_PROTOCOL: u32 = 0xD0FF;

#[cfg(test)]
mod tests {
    use super::{
        ADMIN_PAYLOAD_PROTOCOL, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, CONTROL_PAYLOAD_PROTOCOL,
        HANDOFF_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
    };

    /// Every registered payload-protocol ID must be pairwise distinct —
    /// they multiplex independent subsystems over one frame envelope.
    #[test]
    fn payload_protocol_ids_are_pairwise_distinct() {
        let registered: [(u32, &str); 4] = [
            (CONTROL_PAYLOAD_PROTOCOL, "CONTROL_PAYLOAD_PROTOCOL"),
            (ADMIN_PAYLOAD_PROTOCOL, "ADMIN_PAYLOAD_PROTOCOL"),
            (
                BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
                "BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL",
            ),
            (HANDOFF_PAYLOAD_PROTOCOL, "HANDOFF_PAYLOAD_PROTOCOL"),
        ];
        for (left_index, (left_id, left_name)) in registered.iter().enumerate() {
            for (right_id, right_name) in &registered[left_index + 1..] {
                assert_ne!(
                    left_id, right_id,
                    "{left_name} and {right_name} share payload-protocol id {left_id:#06X}"
                );
            }
        }
    }

    /// Frozen v1 wire values — changing any of these breaks the v1 contract.
    #[test]
    fn frozen_v1_wire_values() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(CONTROL_PAYLOAD_PROTOCOL, 0x00);
        assert_eq!(ADMIN_PAYLOAD_PROTOCOL, 0xAD01);
        assert_eq!(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, 0xB232);
        assert_eq!(HANDOFF_PAYLOAD_PROTOCOL, 0xD0FF);
        assert_eq!(u32::from(crate::broker::FRAMING_VERSION_V1), 1);
    }
}
