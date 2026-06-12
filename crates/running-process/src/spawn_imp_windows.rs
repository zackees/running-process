use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle as StdOwnedHandle, RawHandle};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use winapi::shared::minwindef::{BOOL, DWORD, FALSE, TRUE};
use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
use winapi::um::handleapi::{CloseHandle, DuplicateHandle, INVALID_HANDLE_VALUE};
use winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject};
use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
use winapi::um::namedpipeapi::CreateNamedPipeW;
use winapi::um::processenv::GetStdHandle;
use winapi::um::processthreadsapi::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetCurrentProcessId,
    GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
};
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, INFINITE,
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT, STARTF_USESTDHANDLES, STARTUPINFOEXW, STD_ERROR_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, WAIT_OBJECT_0,
};
use winapi::um::winnt::{
    JobObjectExtendedLimitInformation, DUPLICATE_SAME_ACCESS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GENERIC_READ, GENERIC_WRITE, HANDLE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

// PROC_THREAD_ATTRIBUTE_HANDLE_LIST = ProcThreadAttributeValue(2, FALSE,
// TRUE, FALSE) = 0x00020002.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x00020002;
const STILL_ACTIVE: u32 = 259;

pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    pub fn as_raw(&self) -> HANDLE {
        self.0
    }

    /// Take the raw handle without dropping it (caller owns it now).
    pub fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

// ── Typed pipe handles ──────────────────────────────────────────────────────
//
// The bug fixed in issue #115 was: an `OwnedHandle` produced by `CreatePipe`
// (synchronous-only) was passed to `ChildStdout::from(...)` (which uses
// overlapped/alertable I/O via `alertable_io_internal`). The mismatch
// silently drops every write after the first.
//
// The fix below makes that mismatch a compile error:
//
// - `OverlappedHandle` — a `HANDLE` *known* to support overlapped I/O.
//   Construction is restricted to [`create_pipe_pair`], which creates the
//   parent end via `CreateNamedPipeW(... FILE_FLAG_OVERLAPPED ...)`.
//   `ChildStdin` / `ChildStdout` / `ChildStderr` can ONLY be built from
//   an `OverlappedHandle`.
//
// - `SyncHandle` — a `HANDLE` *known* to be opened without
//   `FILE_FLAG_OVERLAPPED`. Safe to give a spawned child as a stdio slot.
//
// Both wrap a raw [`OwnedHandle`] (so they own & close it on Drop) and
// expose no public conversion that would let a caller bypass the
// invariant.

/// A handle known to support overlapped (asynchronous) I/O.
///
/// The only constructor is [`create_pipe_pair`]; the only consumers are
/// [`OverlappedHandle::into_child_stdin`] / `..._stdout` / `..._stderr`.
/// This guarantees that any `ChildStdin` / `ChildStdout` / `ChildStderr`
/// we hand back to a caller wraps a handle that Rust stdlib's
/// `alertable_io_internal` reader can actually drive without silently
/// dropping writes (issue #115).
pub struct OverlappedHandle(OwnedHandle);

impl OverlappedHandle {
    pub fn into_child_stdin(self) -> ChildStdin {
        let raw = self.0.into_raw() as RawHandle;
        let owned = unsafe { StdOwnedHandle::from_raw_handle(raw) };
        ChildStdin::from(owned)
    }

    pub fn into_child_stdout(self) -> ChildStdout {
        let raw = self.0.into_raw() as RawHandle;
        let owned = unsafe { StdOwnedHandle::from_raw_handle(raw) };
        ChildStdout::from(owned)
    }

    pub fn into_child_stderr(self) -> ChildStderr {
        let raw = self.0.into_raw() as RawHandle;
        let owned = unsafe { StdOwnedHandle::from_raw_handle(raw) };
        ChildStderr::from(owned)
    }
}

/// A handle known to be opened **without** `FILE_FLAG_OVERLAPPED`.
///
/// Suitable for the child end of a pipe: the child uses synchronous
/// `ReadFile` / `WriteFile` via plain `println!` / `stdin.read_line(..)`.
/// Passing a `SyncHandle` to anything that expects overlapped I/O
/// (Rust's `ChildStdin` / `ChildStdout` / `ChildStderr`, for instance)
/// is impossible by construction — there is no public conversion.
pub struct SyncHandle(OwnedHandle);

impl SyncHandle {
    /// Drop the `SyncHandle` claim and recover the raw owner.
    ///
    /// Used internally when the child-side handle is about to be passed
    /// to `CreateProcessW` via `STARTUPINFOEXW.hStdInput` / `hStdOutput`
    /// / `hStdError` — at that point we no longer need the type-level
    /// proof, only the inheritable raw `HANDLE`.
    fn into_owned(self) -> OwnedHandle {
        self.0
    }
}

/// Direction parameter for [`create_pipe_pair`].
#[derive(Clone, Copy)]
enum PipeDir {
    /// stdin: parent writes, child reads.
    ParentWritesChildReads,
    /// stdout/stderr: child writes, parent reads.
    ChildWritesParentReads,
}

fn open_nul(write: bool) -> io::Result<OwnedHandle> {
    let path: Vec<u16> = OsStr::new("NUL")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD;
    sa.bInheritHandle = TRUE as BOOL;
    let access = if write { GENERIC_WRITE } else { GENERIC_READ };
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &mut sa as *mut SECURITY_ATTRIBUTES,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(h))
}

/// Create a pipe pair whose parent end supports overlapped I/O (so Rust's
/// `ChildStdin` / `ChildStdout` / `ChildStderr` — which use `ReadFileEx` +
/// alertable `SleepEx` via `alertable_io_internal` — can drive it without
/// silently dropping writes after the first transfer) and whose child end
/// is synchronous + inheritable.
///
/// Returns `(parent_end, child_end)` with the proof carried in the types:
/// [`OverlappedHandle`] for the parent side (only thing that can be
/// turned into a `ChildStdin/Stdout/Stderr`), [`SyncHandle`] for the
/// child side (which goes into `STARTUPINFOEXW`).
fn create_pipe_pair(dir: PipeDir) -> io::Result<(OverlappedHandle, SyncHandle)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static PIPE_COUNTER: AtomicU64 = AtomicU64::new(0);

    let counter = PIPE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = unsafe { GetCurrentProcessId() };
    let name = format!(r"\\.\pipe\running-process-{pid}-{counter}");
    let name_w: Vec<u16> = OsStr::new(&name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let (parent_open_mode, child_access) = match dir {
        PipeDir::ParentWritesChildReads => (PIPE_ACCESS_OUTBOUND, GENERIC_READ),
        PipeDir::ChildWritesParentReads => (PIPE_ACCESS_INBOUND, GENERIC_WRITE),
    };

    // Parent end: overlapped/async, not inheritable (default SA = NULL).
    let parent = unsafe {
        CreateNamedPipeW(
            name_w.as_ptr(),
            parent_open_mode | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,         // nMaxInstances: only one client
            64 * 1024, // nOutBufferSize
            64 * 1024, // nInBufferSize
            0,         // nDefaultTimeOut: use default (50 ms)
            std::ptr::null_mut(),
        )
    };
    if parent.is_null() || parent == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let parent = OverlappedHandle(OwnedHandle(parent));

    // Child end: synchronous, inheritable.
    let mut child_sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    child_sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD;
    child_sa.bInheritHandle = TRUE as BOOL;
    child_sa.lpSecurityDescriptor = std::ptr::null_mut();

    let child = unsafe {
        CreateFileW(
            name_w.as_ptr(),
            child_access,
            0, // no sharing
            &mut child_sa as *mut SECURITY_ATTRIBUTES,
            OPEN_EXISTING,
            0, // synchronous (no FILE_FLAG_OVERLAPPED)
            std::ptr::null_mut(),
        )
    };
    if child.is_null() || child == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let child = SyncHandle(OwnedHandle(child));

    Ok((parent, child))
}

/// Duplicate a handle into an inheritable copy in the current process.
fn dup_inheritable(src: HANDLE) -> io::Result<OwnedHandle> {
    let current = unsafe { GetCurrentProcess() };
    let mut out: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            current,
            src,
            current,
            &mut out as *mut HANDLE,
            0,
            TRUE as BOOL,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(out))
}

/// One stdio slot resolved to a child-inheritable handle plus an
/// optional parent-side pipe end the caller will receive.
struct ResolvedSlot {
    /// The handle the child will see in this stdio slot. Inheritable.
    /// Generic `OwnedHandle` because the three non-pipe variants
    /// (Null/Parent/Handle) produce ordinary inheritable handles whose
    /// overlapped-ness we don't track.
    child_handle: OwnedHandle,
    /// Set only for [`super::StdioSource::Pipe`]: the parent-side end
    /// of a freshly-created pipe. Typed as [`OverlappedHandle`] so it
    /// can ONLY be turned into a `ChildStdin/Stdout/Stderr` (issue #115).
    parent_end: Option<OverlappedHandle>,
}

enum SlotDir {
    Stdin,
    Stdout,
    Stderr,
}

fn resolve_slot(slot: &super::StdioSource<'_>, dir: SlotDir) -> io::Result<ResolvedSlot> {
    match slot {
        super::StdioSource::Null => {
            let write = !matches!(dir, SlotDir::Stdin);
            Ok(ResolvedSlot {
                child_handle: open_nul(write)?,
                parent_end: None,
            })
        }
        super::StdioSource::Parent => {
            let std_handle = match dir {
                SlotDir::Stdin => STD_INPUT_HANDLE,
                SlotDir::Stdout => STD_OUTPUT_HANDLE,
                SlotDir::Stderr => STD_ERROR_HANDLE,
            };
            let src = unsafe { GetStdHandle(std_handle) };
            if src.is_null() || src == INVALID_HANDLE_VALUE {
                // No real parent handle (e.g. detached parent). Fall
                // back to NUL — child still gets a valid slot.
                let write = !matches!(dir, SlotDir::Stdin);
                return Ok(ResolvedSlot {
                    child_handle: open_nul(write)?,
                    parent_end: None,
                });
            }
            Ok(ResolvedSlot {
                child_handle: dup_inheritable(src)?,
                parent_end: None,
            })
        }
        super::StdioSource::Handle(borrowed) => {
            let raw = borrowed.as_raw_handle() as HANDLE;
            Ok(ResolvedSlot {
                child_handle: dup_inheritable(raw)?,
                parent_end: None,
            })
        }
        super::StdioSource::Pipe => {
            // Use the typed pipe constructor: parent end is provably
            // `OverlappedHandle` (the only thing that can be turned into
            // a Rust `ChildStdin/Stdout/Stderr`); child end is provably
            // `SyncHandle`. This makes the issue-#115 mismatch (sync
            // handle → ChildStdout reader using alertable_io_internal,
            // silent multi-write loss) a compile-time impossibility.
            let pipe_dir = match dir {
                SlotDir::Stdin => PipeDir::ParentWritesChildReads,
                SlotDir::Stdout | SlotDir::Stderr => PipeDir::ChildWritesParentReads,
            };
            let (parent_end, child_end) = create_pipe_pair(pipe_dir)?;
            Ok(ResolvedSlot {
                child_handle: child_end.into_owned(),
                parent_end: Some(parent_end),
            })
        }
        super::StdioSource::_Phantom(_) => unreachable!(),
    }
}

pub struct SpawnedInner {
    process: Option<OwnedHandle>,
    job: Option<OwnedHandle>,
    // Held so the watcher knows what to close after drain_timeout.
    // None after watcher has been kicked.
    _drain_keepalive: Option<Arc<()>>,
}

impl SpawnedInner {
    pub fn kill(&self) -> io::Result<()> {
        if let Some(h) = self.process.as_ref() {
            let ok = unsafe { TerminateProcess(h.as_raw(), 1) };
            if ok == FALSE {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn wait(&self) -> io::Result<i32> {
        let Some(h) = self.process.as_ref() else {
            return Err(io::Error::other("child handle absent"));
        };
        wait_inner(h)
    }

    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        let Some(h) = self.process.as_ref() else {
            return Ok(None);
        };
        try_wait_inner(h)
    }

    /// Called from Drop: close the Job Object handle, which (with
    /// KILL_ON_JOB_CLOSE) terminates the child and any descendants
    /// still in the job.
    pub fn shutdown(&mut self) {
        // Drop the Job Object handle first; KILL_ON_JOB_CLOSE will
        // terminate every assigned process.
        drop(self.job.take());
        // Then drop our process handle.
        drop(self.process.take());
    }
}

pub fn spawn_daemon(command: &mut Command, clear_env: bool) -> io::Result<super::DaemonChild> {
    let stdin = open_nul(false)?;
    let stdout = open_nul(true)?;
    let stderr = open_nul(true)?;
    let (handle, _thread, pid) = create_process_inner(
        command,
        &stdin,
        &stdout,
        &stderr,
        CreateMode::Daemon,
        clear_env,
    )?;
    Ok(super::DaemonChild {
        pid,
        handle: OwnedHandle(handle),
    })
}

pub fn spawn(
    command: &mut Command,
    stdio: super::SpawnStdio<'_>,
) -> io::Result<super::SpawnedChild> {
    let stdin_slot = resolve_slot(&stdio.stdin, SlotDir::Stdin)?;
    let stdout_slot = resolve_slot(&stdio.stdout, SlotDir::Stdout)?;
    let stderr_slot = resolve_slot(&stdio.stderr, SlotDir::Stderr)?;

    let (process, thread, pid) = create_process_inner(
        command,
        &stdin_slot.child_handle,
        &stdout_slot.child_handle,
        &stderr_slot.child_handle,
        CreateMode::Contained {
            show_console: stdio.show_console,
        },
        // Contained-mode spawn doesn't currently support env_clear via
        // an extra arg — callers using `spawn` set env via the regular
        // Command::env(...) API and inheritance follows the standard
        // CRT contract.
        false,
    )?;

    // Build the per-spawn Job Object and assign BEFORE ResumeThread so
    // the child cannot spawn grandchildren outside the job.
    let job = create_job_object()?;
    let ok = unsafe { AssignProcessToJobObject(job.as_raw(), process) };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        unsafe {
            TerminateProcess(process, 1);
            CloseHandle(thread);
            CloseHandle(process);
        }
        return Err(err);
    }

    // Now safe to start the child.
    unsafe {
        ResumeThread(thread);
        CloseHandle(thread);
    }

    // Convert the parent-side pipe ends, if any, into Rust ChildStdin
    // etc.  The kernel keeps duplicates of the child-side handles via
    // CreateProcessW, so dropping `stdin_slot.child_handle` etc.
    // below is fine. The OverlappedHandle::into_child_* methods are
    // the only conversion paths to a Rust ChildStd* — see #115.
    let stdin_pipe = stdin_slot
        .parent_end
        .map(OverlappedHandle::into_child_stdin);
    let stdout_pipe = stdout_slot
        .parent_end
        .map(OverlappedHandle::into_child_stdout);
    let stderr_pipe = stderr_slot
        .parent_end
        .map(OverlappedHandle::into_child_stderr);

    // Optional drain watcher: wait on process exit, then sleep
    // `drain_timeout`, then close our wrapper-held copies (none on
    // Windows after this point — Rust's ChildStdin/Stdout/Stderr own
    // them).  We still spawn the watcher so callers reading from the
    // pipes see EOF in bounded time after child exit; the watcher's
    // job here is purely the bounded sleep + signaling on drop.
    let drain_keepalive = if let Some(timeout) = stdio.drain_timeout {
        let process_handle = dup_inheritable(process)?;
        let keep = Arc::new(());
        let keep_watcher = Arc::clone(&keep);
        // SAFETY: process_handle is moved into the thread.  We dup it
        // so closing the outer one from shutdown() doesn't break the
        // watcher's wait.
        thread::spawn(move || {
            drain_watcher(process_handle, timeout, keep_watcher);
        });
        Some(keep)
    } else {
        None
    };

    Ok(super::SpawnedChild {
        stdin: stdin_pipe,
        stdout: stdout_pipe,
        stderr: stderr_pipe,
        pid,
        inner: SpawnedInner {
            process: Some(OwnedHandle(process)),
            job: Some(job),
            _drain_keepalive: drain_keepalive,
        },
    })
}

fn drain_watcher(process_handle: OwnedHandle, timeout: Duration, _keep: Arc<()>) {
    // Wait until the child exits.
    unsafe {
        WaitForSingleObject(process_handle.as_raw(), INFINITE);
    }
    // Give the pipes a chance to drain post-mortem.
    //
    // #199: intentional — same post-mortem drain semantic as the
    // Unix watcher. `WaitForSingleObject(INFINITE)` above gives us
    // the exit; this sleep lets the reader threads pick up the
    // last bytes the kernel still has buffered.
    thread::sleep(timeout);
    // process_handle is closed here.  The Rust ChildStdin/Stdout/Stderr
    // pipes owned by the SpawnedChild caller are not in our hands; the
    // child has exited so any reads on those pipes will EOF naturally
    // once the kernel ref-counts the write-ends to zero.
}

enum CreateMode {
    Daemon,
    Contained { show_console: bool },
}

/// Returns (process_handle, pid). For `Contained` mode the process is
/// Returns `(process_handle, thread_handle, pid)`.
///
/// For [`CreateMode::Daemon`]: `thread_handle` is `null_mut()` (already
/// closed; child is running). For [`CreateMode::Contained`]: child is
/// still suspended; the caller must assign it to a Job Object then call
/// `ResumeThread(thread_handle)` and `CloseHandle(thread_handle)`.
fn create_process_inner(
    command: &mut Command,
    stdin: &OwnedHandle,
    stdout: &OwnedHandle,
    stderr: &OwnedHandle,
    mode: CreateMode,
    clear_env: bool,
) -> io::Result<(HANDLE, HANDLE, u32)> {
    let mut cmdline = build_command_line(command.get_program(), command.get_args());

    let envs: Vec<(OsString, Option<OsString>)> = command
        .get_envs()
        .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
        .collect();
    let env_block = if envs.is_empty() && !clear_env {
        // No overrides AND no clear → let the kernel inherit the
        // parent's env block (lpEnvironment=NULL).
        None
    } else {
        // Either we have overrides, or the caller asked to clear
        // inherited env. In both cases we must build the block
        // ourselves and pass it explicitly.
        Some(build_env_block(envs, clear_env))
    };

    let cwd_w: Option<Vec<u16>> = command.get_current_dir().map(|p| {
        OsStr::new(p)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    });

    // Initialize the proc-thread attribute list.
    let mut size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
    }
    let mut attr_buf: Vec<u8> = vec![0; size];
    let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

    let ok = unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut size) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }

    let handle_list: [HANDLE; 3] = [stdin.as_raw(), stdout.as_raw(), stderr.as_raw()];
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handle_list.as_ptr() as *mut _,
            std::mem::size_of::<[HANDLE; 3]>(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        return Err(err);
    }

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as DWORD;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin.as_raw();
    si.StartupInfo.hStdOutput = stdout.as_raw();
    si.StartupInfo.hStdError = stderr.as_raw();
    si.lpAttributeList = attr_list;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let mut flags: DWORD = EXTENDED_STARTUPINFO_PRESENT;
    match mode {
        CreateMode::Daemon => {
            // Daemons run with no visible console window and in a new
            // process group so Ctrl-C / Ctrl-Break delivered to the
            // parent's console group never reaches them.
            //
            // We intentionally do NOT add DETACHED_PROCESS. The
            // CREATE_NO_WINDOW + DETACHED_PROCESS combo is documented
            // as inconsistent by MS (both touch the same console
            // inheritance machinery): cmd.exe spawned with both
            // attaches no console, errors on its first builtin, and
            // exits immediately with no output. CREATE_NO_WINDOW alone
            // gives the child a non-visible console which is what
            // cmd-shell scripts and most console tools actually need.
            flags |= CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        }
        CreateMode::Contained { show_console } => {
            // We need to assign to a Job Object before the child runs.
            flags |= CREATE_SUSPENDED;
            // Default: no console. Caller opts in via show_console to let
            // the child inherit / allocate one (interactive subprocess).
            if !show_console {
                flags |= CREATE_NO_WINDOW;
            }
        }
    }
    if env_block.is_some() {
        flags |= CREATE_UNICODE_ENVIRONMENT;
    }

    let cwd_ptr = cwd_w
        .as_ref()
        .map(|v| v.as_ptr())
        .unwrap_or(std::ptr::null());
    let env_ptr = env_block
        .as_ref()
        .map(|v| v.as_ptr() as *mut winapi::ctypes::c_void)
        .unwrap_or(std::ptr::null_mut());

    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            TRUE as BOOL,
            flags,
            env_ptr,
            cwd_ptr,
            &mut si.StartupInfo,
            &mut pi,
        )
    };
    let err = if ok == FALSE {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        DeleteProcThreadAttributeList(attr_list);
    }
    if let Some(err) = err {
        return Err(err);
    }

    // For Contained mode we leave the child suspended and return the
    // thread handle to the caller; the caller assigns to the Job Object
    // then resumes. For Daemon mode (not CREATE_SUSPENDED) the thread is
    // already running and we just close the thread handle here.
    if matches!(mode, CreateMode::Daemon) {
        unsafe {
            CloseHandle(pi.hThread);
        }
        Ok((pi.hProcess, std::ptr::null_mut(), pi.dwProcessId))
    } else {
        Ok((pi.hProcess, pi.hThread, pi.dwProcessId))
    }
}

fn create_job_object() -> io::Result<OwnedHandle> {
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(err);
    }
    Ok(OwnedHandle(job))
}

pub fn terminate(handle: &OwnedHandle) -> io::Result<()> {
    let ok = unsafe { TerminateProcess(handle.as_raw(), 1) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn wait(handle: &OwnedHandle) -> io::Result<i32> {
    wait_inner(handle)
}

pub fn try_wait(handle: &OwnedHandle) -> io::Result<Option<i32>> {
    try_wait_inner(handle)
}

fn wait_inner(handle: &OwnedHandle) -> io::Result<i32> {
    let rc = unsafe { WaitForSingleObject(handle.as_raw(), INFINITE) };
    if rc != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    let mut code: DWORD = 0;
    let ok = unsafe { GetExitCodeProcess(handle.as_raw(), &mut code as *mut DWORD) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(code as i32)
}

fn try_wait_inner(handle: &OwnedHandle) -> io::Result<Option<i32>> {
    let mut code: DWORD = 0;
    let ok = unsafe { GetExitCodeProcess(handle.as_raw(), &mut code as *mut DWORD) };
    if ok == FALSE {
        return Err(io::Error::last_os_error());
    }
    if code == STILL_ACTIVE {
        Ok(None)
    } else {
        Ok(Some(code as i32))
    }
}

fn build_command_line<'a>(program: &OsStr, args: impl Iterator<Item = &'a OsStr>) -> Vec<u16> {
    let program_str = program.to_string_lossy().into_owned();
    let is_cmd = is_cmd_exe(&program_str);

    let arg_strs: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();

    let mut s = String::new();
    s.push_str(&quote(&program_str));

    // Special case for cmd.exe: when an arg of `/C` or `/K`
    // (case-insensitive) is followed by what we treat as the script,
    // do NOT apply CRT-style escaping to the script. cmd.exe parses
    // `\"` literally as backslash + quote, so the CRT's `"` → `\"`
    // escape rule corrupts paths inside the script (the redirect
    // target `"C:\path"` becomes `\"C:\path\"` which is no longer a
    // valid filename to cmd). With `/S`, cmd strips the outermost
    // pair of quotes around the whole script; everything else we
    // pass through untouched. See PR #116 for the diagnostic trail.
    let mut i = 0;
    while i < arg_strs.len() {
        let a = &arg_strs[i];
        s.push(' ');
        if is_cmd && is_cmd_script_switch(a) {
            // Emit the switch itself unquoted (it has no special chars),
            // then the remaining args concatenated with spaces, wrapped
            // in a single pair of outer quotes that `/S` will strip.
            s.push_str(a);
            let script = arg_strs[i + 1..].join(" ");
            if !script.is_empty() {
                s.push(' ');
                s.push('"');
                s.push_str(&script);
                s.push('"');
            }
            break;
        }
        s.push_str(&quote(a));
        i += 1;
    }

    OsStr::new(&s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn is_cmd_exe(program: &str) -> bool {
    // Match `cmd`, `cmd.exe`, or any path ending in those (case-insensitive).
    let lower = program.to_ascii_lowercase();
    let tail = std::path::Path::new(&lower)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(lower);
    tail == "cmd" || tail == "cmd.exe"
}

fn is_cmd_script_switch(arg: &str) -> bool {
    matches!(arg.to_ascii_lowercase().as_str(), "/c" | "/k")
}

fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let chars: Vec<char> = arg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let mut nbs = 0;
        while i < chars.len() && chars[i] == '\\' {
            nbs += 1;
            i += 1;
        }
        if i == chars.len() {
            for _ in 0..(nbs * 2) {
                out.push('\\');
            }
            break;
        } else if chars[i] == '"' {
            for _ in 0..(nbs * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
        } else {
            for _ in 0..nbs {
                out.push('\\');
            }
            out.push(chars[i]);
        }
        i += 1;
    }
    out.push('"');
    out
}

fn build_env_block(overrides: Vec<(OsString, Option<OsString>)>, clear_env: bool) -> Vec<u16> {
    use std::collections::BTreeMap;
    // Windows env var names are case-INSENSITIVE at the kernel level
    // (CreateProcessW + the env block accept any case but
    // `GetEnvironmentVariable` lookups uppercase the name). If we dedup
    // case-sensitively, an inherited "Path" and a caller override of
    // "PATH" (or vice versa) end up as TWO entries in the block; the
    // kernel picks one (whichever sorts first) and the override is
    // silently dropped.
    //
    // Use the uppercased UTF-16 form as the canonical key. Preserve
    // the original case of the most recent insert for emit.
    let upper_key = |k: &OsStr| -> Vec<u16> {
        // Simple ASCII upper-fold via OsStr→u16 chain. Windows
        // CompareStringOrdinal uses a locale-independent uppercase
        // fold; for env-var names (overwhelmingly ASCII) the simple
        // version suffices and avoids a Win32 round-trip. Non-ASCII
        // keys still dedup as long as their bytes match exactly.
        k.encode_wide()
            .map(|c| {
                if (b'a' as u16..=b'z' as u16).contains(&c) {
                    c - (b'a' as u16 - b'A' as u16)
                } else {
                    c
                }
            })
            .collect()
    };

    let mut env: BTreeMap<Vec<u16>, (OsString, OsString)> = BTreeMap::new();
    if !clear_env {
        // Default: start from the daemon's inherited env, then layer
        // overrides on top.
        for (k, v) in std::env::vars_os() {
            env.insert(upper_key(&k), (k, v));
        }
    }
    // When clear_env=true we start from an empty map; the env block
    // we hand `CreateProcessW` contains ONLY the overrides.
    for (k, v) in overrides {
        let ck = upper_key(&k);
        match v {
            Some(val) => {
                env.insert(ck, (k, val));
            }
            None => {
                env.remove(&ck);
            }
        }
    }
    let mut block: Vec<u16> = Vec::new();
    for (_ck, (k, v)) in env {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}
