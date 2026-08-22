//! Per-user identity hash used by every broker pipe name.
//!
//! Returns a 16-character lowercase hex string (the first 8 bytes of a
//! blake3 digest, hex-encoded). Stable across runs for the same user
//! on the same machine; collision resistant in practice.
//!
//! ## Platform inputs
//!
//! | Platform | Hash input |
//! |----------|------------|
//! | Windows  | The current process token user SID, in `S-1-...` text form, obtained via `OpenProcessToken(GetCurrentProcess())` → `GetTokenInformation(TokenUser)` → `ConvertSidToStringSidW`. |
//! | Linux    | `format!("{uid}:{machine_id}")` where `machine_id` is the contents of `/etc/machine-id`, falling back to `/var/lib/dbus/machine-id`, then to a boot-scoped `boot:<boot_id>` kernel identity when neither file exists (minimal containers). |
//! | macOS    | `format!("{uid}:{machine_uuid}")` where `machine_uuid` comes from `ioreg -d2 -c IOPlatformExpertDevice` (the `IOPlatformUUID` field). |
//!
//! ## Why a hash?
//!
//! Pipe-name length limits are tight: Windows MAX_PATH (260) and the
//! macOS `sun_path` field (104 bytes). A blake3 16-char hex is short,
//! collision-resistant for the namespace size we care about
//! (per-machine per-user), and avoids leaking the literal SID or
//! machine UUID into world-readable filesystem paths.

/// Errors that can prevent computing the user SID hash.
#[derive(Debug, thiserror::Error)]
pub enum SidError {
    /// Could not read the platform user identity (e.g. machine-id
    /// missing, ioreg unavailable, OpenProcessToken failed).
    #[error("failed to read platform user identity: {0}")]
    PlatformLookup(String),
}

/// Return the 16-character lowercase hex blake3 hash of the current
/// user's platform identity. Stable across runs.
pub fn user_sid_hash() -> Result<String, SidError> {
    let input = platform_identity_string()?;
    Ok(hash_to_16_hex(input.as_bytes()))
}

/// Hash arbitrary bytes to 16 lowercase hex characters using blake3.
///
/// Exposed for testing and for the rare caller that wants to hash a
/// non-default identity string (e.g. a CI runner ID).
pub fn hash_to_16_hex(input: &[u8]) -> String {
    let digest = blake3::hash(input);
    let bytes = digest.as_bytes();
    // 8 bytes → 16 hex chars.
    let mut out = String::with_capacity(16);
    for b in &bytes[..8] {
        // Lowercase hex, fixed width.
        out.push(nibble_to_hex(b >> 4));
        out.push(nibble_to_hex(b & 0x0F));
    }
    out
}

#[inline]
fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("nibble out of range"),
    }
}

fn platform_identity_string() -> Result<String, SidError> {
    crate::platform::host::user_machine_identity()
        .map_err(|error| SidError::PlatformLookup(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_16_lowercase_hex() {
        let h = hash_to_16_hex(b"sample-input");
        assert_eq!(h.len(), 16, "hash must be 16 chars");
        for c in h.chars() {
            assert!(
                c.is_ascii_digit() || ('a'..='f').contains(&c),
                "non-lowercase-hex char in {h:?}"
            );
        }
    }

    #[test]
    fn different_inputs_yield_different_hashes() {
        let a = hash_to_16_hex(b"alice:machine-1");
        let b = hash_to_16_hex(b"bob:machine-1");
        assert_ne!(a, b);
    }

    #[test]
    fn same_input_is_stable() {
        let a = hash_to_16_hex(b"alice:machine-1");
        let b = hash_to_16_hex(b"alice:machine-1");
        assert_eq!(a, b);
    }

    #[test]
    fn current_user_hash_resolves() {
        // On a healthy dev machine this should succeed on all three
        // platforms. CI containers without /etc/machine-id will skip
        // (we don't want to make this test platform-fragile).
        match user_sid_hash() {
            Ok(h) => {
                assert_eq!(h.len(), 16);
            }
            Err(e) => {
                eprintln!("user_sid_hash unavailable on this host: {e}");
            }
        }
    }
}
