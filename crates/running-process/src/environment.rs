//! Process environment baselines.
//!
//! Windows exposes the logged-in user's machine + user environment through
//! `CreateEnvironmentBlock`. Unix has no equivalent stable OS API, so the
//! conservative fallback is a snapshot of the current process environment.

#[cfg(windows)]
use std::ffi::c_void;
use std::ffi::OsString;
use std::io;

/// Return the logged-in user's baseline environment.
///
/// On Windows this is freshly constructed from machine and user settings and
/// therefore excludes variables that exist only in the current process. On
/// Unix this currently falls back to the current process environment.
pub fn user_baseline_environment() -> io::Result<Vec<(OsString, OsString)>> {
    #[cfg(windows)]
    {
        let block = user_baseline_environment_block()?;
        Ok(parse_windows_environment_block(&block))
    }
    #[cfg(not(windows))]
    {
        Ok(std::env::vars_os().collect())
    }
}

/// Return a CreateProcessW-compatible Unicode user environment block.
///
/// The returned buffer is sorted and double-NUL terminated by Windows. It is
/// useful to callers that own a manual `CreateProcessW` path.
#[cfg(windows)]
pub fn user_baseline_environment_block() -> io::Result<Vec<u16>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut raw_block: *mut c_void = std::ptr::null_mut();
    let created = unsafe { CreateEnvironmentBlock(&mut raw_block, token, 0) };
    let create_error = if created == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        CloseHandle(token);
    }
    if let Some(error) = create_error {
        return Err(error);
    }

    let copied = unsafe { copy_windows_environment_block(raw_block.cast::<u16>()) };
    unsafe {
        DestroyEnvironmentBlock(raw_block);
    }
    Ok(copied)
}

#[cfg(windows)]
unsafe fn copy_windows_environment_block(cursor: *const u16) -> Vec<u16> {
    let mut len = 0usize;
    loop {
        if *cursor.add(len) == 0 && *cursor.add(len + 1) == 0 {
            len += 2;
            break;
        }
        len += 1;
    }
    std::slice::from_raw_parts(cursor, len).to_vec()
}

#[cfg(windows)]
fn parse_windows_environment_block(block: &[u16]) -> Vec<(OsString, OsString)> {
    use std::os::windows::ffi::OsStringExt;

    let mut env = Vec::new();
    let mut offset = 0usize;
    while offset < block.len() && block[offset] != 0 {
        let Some(relative_end) = block[offset..].iter().position(|value| *value == 0) else {
            break;
        };
        let end = offset + relative_end;
        let entry = &block[offset..end];
        // Drive-current-directory pseudo variables have the shape
        // `=C:=C:\path`; skip index zero so their second '=' is the
        // key/value separator.
        if let Some(separator) = entry
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, value)| (*value == b'=' as u16).then_some(index))
        {
            let key = OsString::from_wide(&entry[..separator]);
            let value = OsString::from_wide(&entry[separator + 1..]);
            env.push((key, value));
        }
        offset = end + 1;
    }
    env
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[test]
    fn parser_preserves_drive_current_directory_entries() {
        let block: Vec<u16> = OsStr::new("=C:=C:\\work")
            .encode_wide()
            .chain(std::iter::once(0))
            .chain(OsStr::new("Path=C:\\Windows").encode_wide())
            .chain(std::iter::once(0))
            .chain(std::iter::once(0))
            .collect();
        assert_eq!(
            parse_windows_environment_block(&block),
            vec![
                (OsString::from("=C:"), OsString::from("C:\\work")),
                (OsString::from("Path"), OsString::from("C:\\Windows")),
            ]
        );
    }

    #[test]
    fn live_user_baseline_is_double_nul_terminated() {
        let block = user_baseline_environment_block().unwrap();
        assert!(block.len() >= 2);
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
    }
}
