//! Process environment baselines.
//!
//! Windows exposes the logged-in user's machine + user environment through
//! `CreateEnvironmentBlock`. Unix has no OS API that reconstructs a login
//! environment, so the baseline is rebuilt from the user's identity:
//! `getpwuid_r` supplies `USER`/`LOGNAME`/`HOME`/`SHELL`, `PATH` gets the
//! platform's login default, and locale/timezone/tmpdir variables are
//! carried over from the current process when present.

#[cfg(windows)]
use std::ffi::c_void;
use std::ffi::OsString;
use std::io;

/// Return the logged-in user's baseline environment.
///
/// On Windows this is freshly constructed from machine and user settings via
/// `CreateEnvironmentBlock` and therefore excludes variables that exist only
/// in the current process. On Unix it is reconstructed from the user's
/// identity (`getpwuid_r`): `USER`, `LOGNAME`, `HOME`, `SHELL`, a platform
/// default `PATH`, plus locale (`LANG`, `LC_*`), `TZ`, and `TMPDIR` carried
/// over from the current process when set. Non-Windows targets without a
/// resolvable passwd entry fall back to the current process environment.
pub fn user_baseline_environment() -> io::Result<Vec<(OsString, OsString)>> {
    #[cfg(windows)]
    {
        let block = user_baseline_environment_block()?;
        Ok(parse_windows_environment_block(&block))
    }
    #[cfg(unix)]
    {
        Ok(unix_login_baseline_environment().unwrap_or_else(|| std::env::vars_os().collect()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        Ok(std::env::vars_os().collect())
    }
}

/// Materialize a string environment for backends whose native API accepts
/// either an inherited environment (`None`) or one complete replacement
/// block (`Some`). Ordered explicit entries are applied after the selected
/// base and win ties, case-insensitively on Windows.
#[cfg(any(feature = "daemon", test))]
pub(crate) fn materialize_environment(
    policy: crate::EnvironmentPolicy,
    explicit: &[(String, String)],
) -> io::Result<Option<Vec<(String, String)>>> {
    if policy == crate::EnvironmentPolicy::Inherit && explicit.is_empty() {
        return Ok(None);
    }

    let mut output: Vec<(String, String)> = match policy {
        crate::EnvironmentPolicy::Inherit => std::env::vars().collect(),
        crate::EnvironmentPolicy::UserBaseline => user_baseline_environment()?
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect(),
        crate::EnvironmentPolicy::Clear => Vec::new(),
        crate::EnvironmentPolicy::Auto => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Auto environment policy must be resolved before materialization",
            ));
        }
    };

    for (key, value) in explicit {
        #[cfg(windows)]
        let existing = output
            .iter_mut()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key));
        #[cfg(not(windows))]
        let existing = output.iter_mut().find(|(candidate, _)| candidate == key);
        if let Some((existing_key, existing_value)) = existing {
            *existing_key = key.clone();
            *existing_value = value.clone();
        } else {
            output.push((key.clone(), value.clone()));
        }
    }
    Ok(Some(output))
}

#[cfg(test)]
mod materialize_tests {
    use super::*;

    #[test]
    fn clear_uses_only_explicit_entries() {
        let env = materialize_environment(
            crate::EnvironmentPolicy::Clear,
            &[("CLIENT_ONLY".into(), "forwarded".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(env, vec![("CLIENT_ONLY".into(), "forwarded".into())]);
    }

    #[test]
    fn empty_inherit_uses_native_inheritance() {
        assert_eq!(
            materialize_environment(crate::EnvironmentPolicy::Inherit, &[]).unwrap(),
            None
        );
    }

    #[test]
    fn unresolved_auto_is_rejected() {
        assert!(materialize_environment(crate::EnvironmentPolicy::Auto, &[]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_entries_replace_case_insensitively() {
        let env = materialize_environment(
            crate::EnvironmentPolicy::Clear,
            &[
                ("Path".into(), "first".into()),
                ("PATH".into(), "second".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(env, vec![("PATH".into(), "second".into())]);
    }
}

/// The `PATH` a fresh login would start from. Matches `/etc/paths` order on
/// macOS and the customary `login(1)` default elsewhere.
#[cfg(unix)]
const UNIX_LOGIN_DEFAULT_PATH: &str = if cfg!(target_os = "macos") {
    "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
} else {
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
};

/// Build a clean login environment from the user's identity instead of the
/// current process environment. Returns `None` when the passwd entry cannot
/// be resolved (e.g. UID absent from NSS) so the caller can fall back.
#[cfg(unix)]
fn unix_login_baseline_environment() -> Option<Vec<(OsString, OsString)>> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;

    let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // sysconf(_SC_GETPW_R_SIZE_MAX) is allowed to return -1 ("no limit");
    // 1 KiB covers real-world passwd entries and getpwuid_r reports ERANGE
    // if it does not, in which case we grow and retry.
    let mut buf = vec![0u8; 1024];
    loop {
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut passwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        break;
    }

    let field = |ptr: *const libc::c_char| -> Option<OsString> {
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
        (!bytes.is_empty()).then(|| OsString::from_vec(bytes.to_vec()))
    };
    let name = field(passwd.pw_name)?;
    let home = field(passwd.pw_dir)?;

    let mut env: Vec<(OsString, OsString)> = vec![
        (OsString::from("USER"), name.clone()),
        (OsString::from("LOGNAME"), name),
        (OsString::from("HOME"), home),
        (
            OsString::from("PATH"),
            OsString::from(UNIX_LOGIN_DEFAULT_PATH),
        ),
    ];
    if let Some(shell) = field(passwd.pw_shell) {
        env.push((OsString::from("SHELL"), shell));
    }
    // Locale, timezone, and the per-user runtime/tmp dirs describe the login
    // *session* rather than the parent process; carry them over when present so
    // children keep rendering text and resolving paths the way the user does.
    //
    // XDG_RUNTIME_DIR (Linux) and TMPDIR (macOS) are the runtime-dir vars the
    // broker's own `resolve_socket_path` keys on to place its control + SESSION
    // sockets. They are set by the login session (PAM/logind), not by
    // getpwuid_r or profile scripts, so a reconstructed baseline can only obtain
    // them by carrying them. Dropping XDG_RUNTIME_DIR made a daemon fall back to
    // /tmp while its session-resident clients dialed $XDG_RUNTIME_DIR/… — every
    // request then missed the socket (zackees/soldr#2442). TMPDIR was already
    // carried; XDG_RUNTIME_DIR is its Linux counterpart and must be too.
    for (key, value) in std::env::vars_os() {
        let carry =
            key == "LANG" || key == "TZ" || key == "TMPDIR" || key == "XDG_RUNTIME_DIR" || {
                key.to_str().is_some_and(|k| k.starts_with("LC_"))
            };
        if carry {
            env.push((key, value));
        }
    }
    Some(env)
}

/// Return a CreateProcessW-compatible Unicode user environment block.
///
/// The returned buffer is sorted and double-NUL terminated by Windows. It is
/// useful to callers that own a manual `CreateProcessW` path.
#[cfg(windows)]
pub fn user_baseline_environment_block() -> io::Result<Vec<u16>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // CreateEnvironmentBlock documents that the token needs TOKEN_QUERY,
    // TOKEN_DUPLICATE, and TOKEN_IMPERSONATE. With TOKEN_QUERY alone the
    // call still *succeeds* but silently omits the per-user dynamic
    // variables (USERNAME, USERDOMAIN) — downstream consumers that key
    // behavior on USERNAME (e.g. soldr's daemon pipe name) then diverge
    // from processes holding the real login environment.
    let mut token = std::ptr::null_mut();
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut token,
        )
    };
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

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn login_baseline_contains_identity_and_default_path() {
        let env = user_baseline_environment().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let user = get("USER").expect("baseline must contain USER");
        assert!(!user.is_empty());
        assert_eq!(get("LOGNAME").as_ref(), Some(&user));
        let home = get("HOME").expect("baseline must contain HOME");
        assert!(!home.is_empty());
        let path = get("PATH").expect("baseline must contain PATH");
        assert!(!path.is_empty());
    }

    #[test]
    fn login_baseline_does_not_leak_arbitrary_process_vars() {
        // A variable that only exists in this process must not survive
        // into the login baseline (that's what Inherit is for).
        std::env::set_var("RUNNING_PROCESS_BASELINE_CANARY", "1");
        let env = unix_login_baseline_environment().expect("test user must have a passwd entry");
        assert!(
            !env.iter()
                .any(|(key, _)| key == "RUNNING_PROCESS_BASELINE_CANARY"),
            "process-local variables must not leak into the login baseline"
        );
        std::env::remove_var("RUNNING_PROCESS_BASELINE_CANARY");
    }

    #[test]
    fn login_baseline_carries_xdg_runtime_dir() {
        // The broker keys its socket path on XDG_RUNTIME_DIR (Linux). The
        // reconstructed login baseline must carry it so a daemon binds under the
        // same runtime dir its session-resident clients dial, instead of
        // falling back to /tmp and stranding every request (zackees/soldr#2442).
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/4242");
        let env = unix_login_baseline_environment().expect("test user must have a passwd entry");
        let carried = env
            .iter()
            .find(|(key, _)| key == "XDG_RUNTIME_DIR")
            .map(|(_, value)| value.clone());
        std::env::remove_var("XDG_RUNTIME_DIR");
        assert_eq!(
            carried.as_deref(),
            Some(std::ffi::OsStr::new("/run/user/4242")),
            "login baseline must carry XDG_RUNTIME_DIR when the session sets it"
        );
    }
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

    /// Regression: with TOKEN_QUERY-only access, CreateEnvironmentBlock
    /// succeeds but silently drops the per-user dynamic variables. The
    /// baseline must contain USERNAME (and it must match the live value
    /// when the current process has one).
    #[test]
    fn live_user_baseline_contains_username() {
        let env = user_baseline_environment().unwrap();
        let username = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("USERNAME"))
            .map(|(_, value)| value.clone());
        let username = username.expect("baseline environment must contain USERNAME");
        assert!(!username.is_empty(), "USERNAME must be non-empty");
        if let Ok(live) = std::env::var("USERNAME") {
            assert_eq!(
                username.to_string_lossy(),
                live,
                "baseline USERNAME must match the live login USERNAME"
            );
        }
    }
}
