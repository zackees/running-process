//! v2 broker pipe-name derivation (slice 3b of #483).
//!
//! Mirrors [`super::names`] for the v2 broker, using a different
//! namespace prefix (`rpb-v2-` instead of `rpb-v1-`) so a v1 and v2
//! broker for the same program can coexist on one machine. Per #470's
//! coexistence table, v2 ships as a parallel stack alongside v1 during
//! rollout; matching the v1 module's API shape lets later slices and
//! downstream consumers (zccache, etc.) port surfaces one at a time
//! without learning a new naming model.
//!
//! Slice 3b exposes only [`v2_program_pipe`] — the per-program pipe
//! name a v2 broker binds and a v2 client dials. Subsequent slices add
//! more names (private/shared/explicit-instance counterparts, the
//! broker↔daemon transport name) as they are needed.

use crate::broker::lifecycle::names::{validate_service_name, PipePathError};

/// Compile-time prefix for every v2 broker pipe. Counterpart of the
/// frozen v1 `PIPE_PREFIX = "rpb-v1"`. Encodes the v2 envelope version
/// so v1 and v2 brokers can bind simultaneously without colliding.
const PIPE_PREFIX_V2: &str = "rpb-v2";

/// Compute the v2 per-program pipe name.
///
/// Returns `"rpb-v2-{program}-{sid_hash}-{pipe_idx}"` after validating
/// `program` against the same `[a-z0-9-]{1,64}` rule as v1 service
/// names (case-only collisions are rejected for the same Windows
/// named-pipe reason documented on v1's [`validate_service_name`]) and
/// `sid_hash` for non-emptiness + 16-char hex shape.
///
/// `pipe_idx` is included so a v2 broker can bind multiple acceptor
/// pipes (`-0`, `-1`, ...) for fanout, mirroring the v1 pattern
/// `rpb-v1-<program>-<sid_hash>-<pipe_idx>` documented in #470.
///
/// This slice returns just the canonical name string. Wrapping that
/// into a platform-neutral `PipePath` (Windows `\\.\pipe\…` vs Unix
/// socket file under the broker shadow dir) lands in slice 3c when the
/// v2 binary actually starts binding.
pub fn v2_program_pipe(
    program: &str,
    sid_hash: &str,
    pipe_idx: u32,
) -> Result<String, PipePathError> {
    validate_service_name(program)?;
    validate_sid_hash(sid_hash)?;
    Ok(format!("{PIPE_PREFIX_V2}-{program}-{sid_hash}-{pipe_idx}"))
}

/// Validate that `sid_hash` is exactly 16 lowercase hex characters —
/// the same shape produced by [`super::sid::user_sid_hash`] /
/// [`super::sid::hash_to_16_hex`].
fn validate_sid_hash(sid_hash: &str) -> Result<(), PipePathError> {
    if sid_hash.is_empty() {
        return Err(PipePathError::InvalidName {
            name: sid_hash.into(),
            reason: "sid_hash must be at least 1 character",
        });
    }
    if sid_hash.len() != 16 {
        return Err(PipePathError::InvalidName {
            name: sid_hash.into(),
            reason: "sid_hash must be exactly 16 hex characters",
        });
    }
    for c in sid_hash.chars() {
        if !c.is_ascii_hexdigit() || c.is_ascii_uppercase() {
            return Err(PipePathError::InvalidName {
                name: sid_hash.into(),
                reason: "sid_hash must be lowercase hex digits",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SID: &str = "deadbeefcafef00d";

    #[test]
    fn v2_program_pipe_happy_path() {
        let name = v2_program_pipe("zccache", VALID_SID, 0)
            .expect("valid inputs produce a v2 pipe name");
        assert_eq!(name, "rpb-v2-zccache-deadbeefcafef00d-0");
    }

    #[test]
    fn v2_program_pipe_distinct_pipe_idx_distinct_names() {
        let name_0 = v2_program_pipe("zccache", VALID_SID, 0).expect("idx=0 valid");
        let name_7 = v2_program_pipe("zccache", VALID_SID, 7).expect("idx=7 valid");
        assert_ne!(name_0, name_7);
        assert!(name_7.ends_with("-7"));
    }

    #[test]
    fn v2_program_pipe_rejects_invalid_program() {
        // Empty program name.
        assert!(matches!(
            v2_program_pipe("", VALID_SID, 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Uppercase (case-only collision risk on Windows).
        assert!(matches!(
            v2_program_pipe("Zccache", VALID_SID, 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // 65 characters (over the v1-derived length cap).
        let too_long = "a".repeat(65);
        assert!(matches!(
            v2_program_pipe(&too_long, VALID_SID, 0),
            Err(PipePathError::InvalidName { .. })
        ));
    }

    #[test]
    fn v2_program_pipe_rejects_invalid_sid_hash() {
        // Empty sid_hash.
        assert!(matches!(
            v2_program_pipe("zccache", "", 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Wrong length (15 chars).
        assert!(matches!(
            v2_program_pipe("zccache", "deadbeefcafef00", 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Non-hex character.
        assert!(matches!(
            v2_program_pipe("zccache", "deadbeefcafef00g", 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Uppercase hex (not the canonical shape).
        assert!(matches!(
            v2_program_pipe("zccache", "DEADBEEFCAFEF00D", 0),
            Err(PipePathError::InvalidName { .. })
        ));
    }
}
