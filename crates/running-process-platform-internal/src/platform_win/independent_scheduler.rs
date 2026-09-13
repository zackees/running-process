//! Bounded Task Scheduler commands for independent launch coordination.
//!
//! Registration is not launch readiness. The caller must verify the helper's
//! target identity and Job Object separation before accepting a launch. Any
//! command failure after submission is ambiguous and requires reconciliation.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Spawn the decoded target inside the already independently scheduled helper.
/// This function does not itself establish independence from any Job Object.
pub fn spawn_independent_helper_child(command: &mut Command) -> io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    command.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null())
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn()
}

/// Require interactive membership in the process token, not an impersonated
/// thread token. No credentials, identity change, or service-account fallback.
pub fn require_interactive_scheduler_token() -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Security::{CheckTokenMembership, CreateWellKnownSid,
        DuplicateToken, SecurityIdentification, TOKEN_QUERY, TOKEN_DUPLICATE, WinInteractiveSid};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken};
    let mut thread_token = std::ptr::null_mut();
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut thread_token) } != 0 {
        let _thread_token = unsafe { OwnedHandle::from_raw_handle(thread_token) };
        return Err(io::Error::new(io::ErrorKind::Unsupported, "scheduler cannot preserve thread impersonation"));
    }
    let thread_error = io::Error::last_os_error();
    if thread_error.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_NO_TOKEN as i32) {
        return Err(thread_error);
    }
    let mut primary = std::ptr::null_mut();
    // SAFETY: process pseudo-handle and valid output. Each acquired handle
    // transfers immediately into RAII ownership.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut primary) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let primary = unsafe { OwnedHandle::from_raw_handle(primary) };
    if token_integrity_rid(primary.as_raw_handle())? < 0x2000 {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "scheduler cannot preserve a below-medium integrity token"));
    }
    // A scheduler obtains a logon token, not an exact copy of this worker's
    // filtered token. Do not discard restrictions at the placement boundary.
    // This intentionally includes filtered UAC tokens until exact restriction
    // preservation is supported; callers receive Unsupported, never fallback.
    use windows_sys::Win32::Security::{TokenHasRestrictions, TokenIsAppContainer};
    for information in [TokenHasRestrictions, TokenIsAppContainer] {
        if token_dword(primary.as_raw_handle(), information)? != 0 {
            return Err(io::Error::new(io::ErrorKind::Unsupported,
                "scheduler cannot preserve this restricted or AppContainer token"));
        }
    }
    let mut duplicate = std::ptr::null_mut();
    if unsafe { DuplicateToken(primary.as_raw_handle(), SecurityIdentification, &mut duplicate) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let duplicate = unsafe { OwnedHandle::from_raw_handle(duplicate) };
    let mut sid = [0u32; 17]; // SECURITY_MAX_SID_SIZE, suitably aligned.
    let mut bytes = std::mem::size_of_val(&sid) as u32;
    if unsafe { CreateWellKnownSid(WinInteractiveSid, std::ptr::null_mut(), sid.as_mut_ptr().cast(), &mut bytes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut member = 0;
    if unsafe { CheckTokenMembership(duplicate.as_raw_handle(), sid.as_mut_ptr().cast(), &mut member) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if member == 0 { return Err(io::Error::new(io::ErrorKind::Unsupported, "independent scheduler requires an interactive user token")); }
    Ok(())
}

fn token_dword(token: windows_sys::Win32::Foundation::HANDLE,
    information: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS) -> io::Result<u32> {
    use windows_sys::Win32::Security::GetTokenInformation;
    let mut value = 0u32;
    let mut returned = 0;
    // SAFETY: callers select DWORD-valued information classes; buffer and
    // output length are valid and remain live for the synchronous call.
    if unsafe { GetTokenInformation(token, information, std::ptr::addr_of_mut!(value).cast(),
        std::mem::size_of::<u32>() as u32, &mut returned) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if returned != std::mem::size_of::<u32>() as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected token information size"));
    }
    Ok(value)
}

fn token_integrity_rid(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<u32> {
    use windows_sys::Win32::Security::{GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL};
    let mut storage = [0usize; 64];
    let capacity = std::mem::size_of_val(&storage);
    let mut returned = 0u32;
    if unsafe { GetTokenInformation(token, TokenIntegrityLevel, storage.as_mut_ptr().cast(),
        capacity as u32, &mut returned) } == 0 { return Err(io::Error::last_os_error()); }
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid token integrity label");
    if (returned as usize) < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() || returned as usize > capacity {
        return Err(invalid());
    }
    // SAFETY: storage is pointer-aligned and the returned header is complete.
    let sid = unsafe { (*storage.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    let offset = (sid as usize).checked_sub(storage.as_ptr() as usize).ok_or_else(invalid)?;
    if offset < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() || offset > returned as usize {
        return Err(invalid());
    }
    // SAFETY: pointer provenance comes from the original live allocation;
    // offset and remaining length were checked against its returned extent.
    let bytes = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>().add(offset), returned as usize - offset) };
    integrity_rid_from_sid(bytes)
}

fn integrity_rid_from_sid(bytes: &[u8]) -> io::Result<u32> {
    // Mandatory label SID is S-1-16-RID: revision1, one subauthority,
    // big-endian authority16, followed by a little-endian integrity RID.
    if bytes.len() < 12 || bytes[..8] != [1, 1, 0, 0, 0, 0, 0, 16] {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid mandatory label SID"));
    }
    Ok(u32::from_le_bytes(bytes[8..12].try_into().unwrap()))
}

/// A caller-generated opaque task name. Construction does not register it.
/// Dropping this value does not delete or stop a running task.
#[derive(Debug)]
pub struct IndependentSchedulerTask { name: String }

impl IndependentSchedulerTask {
    /// Read-only live scheduler reachability probe. Registration permission
    /// and final process placement remain launch-time checks.
    pub fn probe(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        require_interactive_scheduler_token()?;
        let task = Self::new("running-process-independent-probe".into())?;
        task.invoke(&[OsStr::new("/Query"), OsStr::new("/FO"), OsStr::new("CSV")], deadline, cancelled)
    }
    pub fn new(name: String) -> io::Result<Self> {
        if !name.starts_with("running-process-independent-") || name.len() > 200
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid independent task name"));
        }
        Ok(Self { name })
    }

    pub fn name(&self) -> &str { &self.name }

    /// Render an on-demand, non-elevated helper task for the current process's
    /// token identity. No account override, target command, or environment is
    /// accepted here.
    /// Requires an interactive token; headless/service contexts must report
    /// unavailable instead of switching identity or requesting credentials.
    pub fn render_xml(&self, helper: &Path, request: &Path,
        acknowledgement: &Path, acceptance_timeout_ms: u64) -> io::Result<String> {
        require_interactive_scheduler_token()?;
        let caller_sid = super::host::current_user_sid_text()?;
        self.render_xml_for_sid(&caller_sid, helper, request, acknowledgement, acceptance_timeout_ms)
    }

    fn render_xml_for_sid(&self, caller_sid: &str, helper: &Path, request: &Path,
        acknowledgement: &Path, acceptance_timeout_ms: u64) -> io::Result<String> {
        if !caller_sid.starts_with("S-1-") || caller_sid.len() > 184
            || !caller_sid[4..].split('-').all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid caller SID"));
        }
        if acceptance_timeout_ms == 0 || acceptance_timeout_ms > 3_600_000 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid helper acceptance timeout"));
        }
        let helper = scheduler_path(helper)?;
        let request = scheduler_path(request)?;
        let acknowledgement = scheduler_path(acknowledgement)?;
        let arguments = xml_text(&format!("\"{request}\" \"{acknowledgement}\" {acceptance_timeout_ms}"));
        Ok(format!(concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
            "<Triggers/>",
            "<Principals><Principal id=\"Caller\"><UserId>{}</UserId>",
            "<LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel>",
            "</Principal></Principals>",
            "<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>",
            "<Actions Context=\"Caller\"><Exec><Command>{}</Command><Arguments>{}</Arguments>",
            "</Exec></Actions></Task>"), caller_sid, xml_text(helper), arguments))
    }

    /// Register a caller-owned private XML artifact, never overwriting an
    /// existing task. XML must name only the helper and opaque request paths;
    /// target argv/environment must remain in a private request artifact.
    pub fn register(&self, xml: &Path, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        if !xml.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "task XML path must be absolute"));
        }
        self.invoke(&[OsStr::new("/Create"), OsStr::new("/TN"), self.name.as_ref(),
            OsStr::new("/XML"), xml.as_os_str()], deadline, cancelled)
    }

    pub fn run(&self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        self.invoke(&[OsStr::new("/Run"), OsStr::new("/TN"), self.name.as_ref()], deadline, cancelled)
    }

    /// Request scheduler termination. This alone does not prove target reap.
    pub fn end(&self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        self.invoke(&[OsStr::new("/End"), OsStr::new("/TN"), self.name.as_ref()], deadline, cancelled)
    }

    /// Delete the registration, not the running process. A nonzero exit is
    /// retained as an error rather than assumed to mean "already deleted".
    pub fn delete(&self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        self.invoke(&[OsStr::new("/Delete"), OsStr::new("/TN"), self.name.as_ref(),
            OsStr::new("/F")], deadline, cancelled)
    }

    fn invoke(&self, args: &[&OsStr], deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        check_deadline(deadline, cancelled)?;
        let mut child = Command::new(scheduler_executable()?).args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        loop {
            let result = check_deadline(deadline, cancelled).and_then(|()| child.try_wait());
            match result {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(io::Error::other(format!("Task Scheduler command failed ({status})"))),
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    // Stop only our command client. The caller must reconcile
                    // scheduler state separately, including on /Create errors.
                    if let Err(cleanup) = child.kill() {
                        // The client may have exited between polling and kill.
                        if !matches!(child.try_wait(), Ok(Some(_))) {
                            return Err(io::Error::new(error.kind(), format!(
                                "{error}; scheduler client termination unconfirmed: {cleanup}")));
                        }
                    }
                    // Windows process handles need no Unix-style zombie reap.
                    // Drop the command handle after requesting termination;
                    // never turn a bounded deadline into an unbounded wait.
                    return Err(error);
                }
            }
        }
    }
}

fn scheduler_path(path: &Path) -> io::Result<&str> {
    let text = path.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "scheduler artifact path is not Unicode"))?;
    // XML cannot preserve isolated UTF-16 surrogates. Reject rather than
    // silently changing the path; target argv uses a separate binary codec.
    if !path.is_absolute() || text.ends_with(['\\', '/'])
        || text.chars().any(|character| character < ' ' || matches!(character, '"' | '\u{fffe}' | '\u{ffff}')) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid scheduler artifact path"));
    }
    Ok(text)
}

fn xml_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&apos;")
}

fn check_deadline(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "scheduler command cancelled"));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "scheduler command deadline elapsed"));
    }
    Ok(())
}

fn scheduler_executable() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    // Resolve the OS binary through the kernel API, never caller PATH or
    // a mutable SystemRoot environment variable.
    let mut buffer = vec![0u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 { return Err(io::Error::last_os_error()); }
    if length >= buffer.len() { return Err(io::Error::other("system directory path exceeds capacity")); }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length])).join("schtasks.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mandatory_label_parser_checks_structure_before_reading_rid() {
        assert_eq!(integrity_rid_from_sid(&[1,1,0,0,0,0,0,16,0,32,0,0]).unwrap(), 0x2000);
        assert!(integrity_rid_from_sid(&[1,1,0,0,0,0,0,16]).is_err());
        assert!(integrity_rid_from_sid(&[1,2,0,0,0,0,0,16,0,32,0,0]).is_err());
        assert!(integrity_rid_from_sid(&[1,1,0,0,0,0,0,5,0,32,0,0]).is_err());
    }

    #[test]
    fn definition_is_on_demand_non_elevated_and_escapes_paths() {
        let task = IndependentSchedulerTask::new("running-process-independent-123".into()).unwrap();
        let xml = task.render_xml_for_sid("S-1-5-21-123", Path::new(r"C:\Program Files\helper.exe"),
            Path::new(r"C:\private&a\request"), Path::new(r"C:\private&a\ack"), 1000).unwrap();
        assert!(xml.contains("<Triggers/>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("&quot;C:\\private&amp;a\\request&quot;"));
        assert!(!xml.contains("HighestAvailable"));
        assert!(!xml.contains("Password"));
    }

    #[test]
    fn task_names_cannot_select_other_registrations() {
        assert!(IndependentSchedulerTask::new("running-process-independent-123-abc".into()).is_ok());
        for name in ["existing-task", "running-process-independent-../other", "running-process-independent-*", "running-process-independent-\\other"] {
            assert!(IndependentSchedulerTask::new(name.into()).is_err());
        }
    }

    #[test]
    fn cancellation_and_expired_deadlines_fail_before_submission() {
        assert_eq!(check_deadline(Instant::now(), &AtomicBool::new(false)).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(check_deadline(Instant::now() + Duration::from_secs(1), &AtomicBool::new(true)).unwrap_err().kind(), io::ErrorKind::Interrupted);
    }
}
