//! Windows host facts, directories, user identity, resources, and autostart.

use std::io;

/// A privileged system identity this process may be running as.
///
/// The variants name the *answer*, not the mechanism: what a caller does with
/// "this process is the machine's system account" does not change with how the
/// host was asked. `None` from [`current_process_privilege`] means an ordinary
/// user, which is the only case most callers care to distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedIdentity {
    /// Unix effective UID 0.
    UnixRoot,
    /// Windows LocalSystem account (`S-1-5-18`).
    WindowsLocalSystem,
}

impl std::fmt::Display for PrivilegedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnixRoot => f.write_str("root (effective uid 0)"),
            Self::WindowsLocalSystem => f.write_str("Windows LocalSystem (S-1-5-18)"),
        }
    }
}

/// The privileged identity this process is running as, if any.
///
/// Windows has no single number to compare, so this reads the process token's
/// user SID and matches it against LocalSystem's well-known value.
pub fn current_process_privilege() -> io::Result<Option<PrivilegedIdentity>> {
    let sid = current_user_sid_bytes()?;
    Ok(is_local_system_sid(&sid).then_some(PrivilegedIdentity::WindowsLocalSystem))
}

fn current_user_sid_bytes() -> io::Result<Vec<u8>> {
    use std::ptr;
    use winapi::shared::winerror::ERROR_INSUFFICIENT_BUFFER;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::{GetLengthSid, GetTokenInformation, IsValidSid};
    use winapi::um::winnt::{TokenUser, HANDLE, TOKEN_QUERY, TOKEN_USER};

    // SAFETY: this follows the standard Windows token query pattern:
    // open the current process token, ask for the required TOKEN_USER
    // buffer size, then copy the SID bytes out while the buffer is
    // still alive.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::other(format!(
                "OpenProcessToken failed (GetLastError={})",
                GetLastError()
            )));
        }
        let token = TokenHandle(token);

        let mut required_size = 0_u32;
        let ok = GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required_size);
        let last = GetLastError();
        if ok != 0 || last != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::other(format!(
                "GetTokenInformation size query failed (ok={ok}, GetLastError={last})"
            )));
        }
        if required_size == 0 {
            return Err(io::Error::other(
                "GetTokenInformation reported 0 required bytes",
            ));
        }

        let mut buf = vec![0_u8; required_size as usize];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            required_size,
            &mut required_size,
        ) == 0
        {
            return Err(io::Error::other(format!(
                "GetTokenInformation real query failed (GetLastError={})",
                GetLastError()
            )));
        }

        let token_user: *const TOKEN_USER = buf.as_ptr().cast();
        let sid = (*token_user).User.Sid;
        if sid.is_null() {
            return Err(io::Error::other("TOKEN_USER returned a null SID pointer"));
        }
        if IsValidSid(sid) == 0 {
            return Err(io::Error::other("IsValidSid returned false"));
        }

        let len = GetLengthSid(sid) as usize;
        if len == 0 || len > 1024 {
            return Err(io::Error::other(format!(
                "GetLengthSid returned implausible length {len}"
            )));
        }
        Ok(std::slice::from_raw_parts(sid as *const u8, len).to_vec())
    }
}

struct TokenHandle(winapi::um::winnt::HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

fn is_local_system_sid(sid: &[u8]) -> bool {
    const LOCAL_SYSTEM_SID: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    sid == LOCAL_SYSTEM_SID
}


/// A stable identity for this user on this machine.
///
/// The current user's SID already scopes both, so nothing else is mixed in.
/// The `windows-sid:` prefix keeps this distinguishable from the
/// `uid:machine` shape other hosts produce, as defence against an accidental
/// cross-platform collision. Callers hash this; they do not parse it.
pub fn user_machine_identity() -> io::Result<String> {
    use std::ptr;
    use winapi::shared::winerror::ERROR_INSUFFICIENT_BUFFER;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::GetTokenInformation;
    use winapi::um::winnt::{TokenUser, HANDLE, TOKEN_QUERY, TOKEN_USER};

    // SAFETY: the chain of Windows API calls below follows the documented
    // pattern for retrieving the current process user SID. Every allocated
    // buffer is freed before returning, and we never expose raw pointers to
    // safe Rust.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::other(format!(
                "OpenProcessToken failed (GetLastError={})",
                GetLastError()
            )));
        }
        let close_token = TokenHandle(token);

        // First call: query required buffer size.
        let mut required_size: u32 = 0;
        let ok = GetTokenInformation(
            close_token.0,
            TokenUser,
            ptr::null_mut(),
            0,
            &mut required_size,
        );
        // We expect this to fail with ERROR_INSUFFICIENT_BUFFER.
        let last = GetLastError();
        if ok != 0 || last != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::other(format!(
                "GetTokenInformation size query failed (ok={ok}, GetLastError={last})"
            )));
        }
        if required_size == 0 {
            return Err(io::Error::other(
                "GetTokenInformation reported 0 required bytes",
            ));
        }

        let mut buf: Vec<u8> = vec![0u8; required_size as usize];
        if GetTokenInformation(
            close_token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            required_size,
            &mut required_size,
        ) == 0
        {
            return Err(io::Error::other(format!(
                "GetTokenInformation real query failed (GetLastError={})",
                GetLastError()
            )));
        }

        // The buffer starts with a TOKEN_USER struct whose `User.Sid` points
        // into the same allocation.
        let token_user: *const TOKEN_USER = buf.as_ptr().cast();
        let sid_ptr = (*token_user).User.Sid;
        if sid_ptr.is_null() {
            return Err(io::Error::other("TOKEN_USER returned a null SID pointer"));
        }
        sid_to_identity(sid_ptr)
    }
}

/// Format a SID as `windows-sid:<hex>`.
///
/// `ConvertSidToStringSidW` would give the textual `S-1-...` form, but winapi
/// gates it behind the `sddl` feature. The raw bytes are exactly what that
/// call formats, and an identity only has to be stable per user per machine,
/// which both spellings satisfy.
///
/// # Safety
/// `sid` must point at a valid SID for the lifetime of this call.
unsafe fn sid_to_identity(sid: winapi::um::winnt::PSID) -> io::Result<String> {
    use winapi::um::securitybaseapi::{GetLengthSid, IsValidSid};

    if IsValidSid(sid) == 0 {
        return Err(io::Error::other("IsValidSid returned false"));
    }
    let len = GetLengthSid(sid) as usize;
    if len == 0 || len > 1024 {
        return Err(io::Error::other(format!(
            "GetLengthSid returned implausible length {len}"
        )));
    }
    let slice = std::slice::from_raw_parts(sid as *const u8, len);
    let mut hex = String::with_capacity(len * 2);
    for b in slice {
        hex.push(nibble_to_hex(b >> 4));
        hex.push(nibble_to_hex(b & 0x0F));
    }
    Ok(format!("windows-sid:{hex}"))
}

fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("nibble out of range"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_system_sid_is_detected() {
        assert!(is_local_system_sid(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]));
        assert!(!is_local_system_sid(&[
            1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0
        ]));
    }
}
