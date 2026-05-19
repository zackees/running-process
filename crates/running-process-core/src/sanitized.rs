//! `Containment::Sanitized` — spawn a child with **no orphaned inheritable
//! handles** from the parent's table. Only the stdio handles we explicitly
//! create (NUL on every platform) are passed into the child.
//!
//! Motivation: when a process tree has a pipe-redirected ancestor (e.g. a
//! Python `subprocess.Popen(stdout=PIPE)` several levels up), every
//! intermediate `CreateProcessW(bInheritHandles=TRUE)` on Windows — and every
//! `fork`+`exec` of an fd without `FD_CLOEXEC` on Unix — duplicates that
//! orphaned pipe write-end into the new child. If a daemon ends up at the
//! bottom of that chain, the original reader at the top never sees EOF.
//!
//! Sanitized spawn fixes this by:
//!
//! * **Windows**: opening NUL three times for stdin/stdout/stderr, then
//!   calling `CreateProcessW` with `STARTUPINFOEX` +
//!   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` containing **only** those three
//!   handles. The kernel ignores the "all inheritable handles" rule and
//!   duplicates exactly the listed handles into the child. Any orphaned
//!   ancestor pipe stays in the parent.
//!
//! * **Unix**: spawning with `Stdio::null()` for the three stdio slots
//!   (which Rust marks `O_CLOEXEC` internally), and a `pre_exec` closure
//!   that walks `/dev/fd` (or `/proc/self/fd`) in the forked child and
//!   closes every fd > 2 before `exec`. Equivalent to what nginx, sshd,
//!   and other production daemons do.
//!
//! Issue: <https://github.com/zackees/running-process/issues/110>.

use std::process::Command;

/// A child spawned via `ContainedProcessGroup::spawn_sanitized`.
///
/// Sanitized children always have stdin/stdout/stderr connected to the
/// platform null device — they are daemon-style processes. They are NOT
/// assigned to a `ContainedProcessGroup` Job Object on Windows and survive
/// the group being dropped, matching `Containment::Detached` semantics
/// (plus the no-orphaned-handles guarantee).
pub struct SanitizedChild {
    pid: u32,
    #[cfg(windows)]
    handle: windows::OwnedHandle,
    #[cfg(unix)]
    child: std::process::Child,
}

impl SanitizedChild {
    /// Process ID of the spawned child.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Kill the child process. Best-effort — returns the OS error if
    /// the underlying termination call fails.
    pub fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            windows::terminate(&self.handle)
        }
        #[cfg(unix)]
        {
            self.child.kill()
        }
    }

    /// Block until the child exits and return its exit code (or signal
    /// number negated on Unix when the process was terminated by signal).
    pub fn wait(&mut self) -> std::io::Result<i32> {
        #[cfg(windows)]
        {
            windows::wait(&self.handle)
        }
        #[cfg(unix)]
        {
            let status = self.child.wait()?;
            Ok(exit_code(status))
        }
    }

    /// Non-blocking variant of [`Self::wait`]. Returns `Ok(None)` while
    /// the child is still running.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        #[cfg(windows)]
        {
            windows::try_wait(&self.handle)
        }
        #[cfg(unix)]
        {
            Ok(self.child.try_wait()?.map(exit_code))
        }
    }
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(1))
}

/// Spawn `command` with sanitized handle inheritance. See the module docs
/// for the cross-platform behavior.
pub fn spawn(command: &mut Command) -> std::io::Result<SanitizedChild> {
    #[cfg(windows)]
    {
        windows::spawn(command)
    }
    #[cfg(unix)]
    {
        unix::spawn(command)
    }
}

// ── Windows implementation ──────────────────────────────────────────────────

#[cfg(windows)]
mod windows {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::OsStrExt;
    use std::process::Command;

    use winapi::shared::minwindef::{BOOL, DWORD, FALSE, TRUE};
    use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
    use winapi::um::processthreadsapi::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    };
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
        EXTENDED_STARTUPINFO_PRESENT, INFINITE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        WAIT_OBJECT_0,
    };
    use winapi::um::winnt::{
        FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE, HANDLE,
    };

    // PROC_THREAD_ATTRIBUTE_HANDLE_LIST is not exported by winapi 0.3 — derive
    // it from the `ProcThreadAttributeValue` macro in the Windows SDK headers:
    //
    //   #define ProcThreadAttributeHandleList 2
    //   ProcThreadAttributeValue(N, Thread, Input, Additive) =
    //       (N & 0x0000FFFF)
    //     | (Thread   ? 0x00010000 : 0)
    //     | (Input    ? 0x00020000 : 0)
    //     | (Additive ? 0x00040000 : 0)
    //
    // ProcThreadAttributeHandleList = 2 with Input=TRUE → 0x00020002.
    const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x00020002;
    const STILL_ACTIVE: u32 = 259;

    pub struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        pub fn as_raw(&self) -> HANDLE {
            self.0
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

    // HANDLE is *mut c_void; OwnedHandle is the sole owner so sharing it is
    // safe.  We hand the raw pointer to Windows APIs only via &OwnedHandle.
    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}

    fn open_nul(write: bool) -> std::io::Result<OwnedHandle> {
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
            return Err(std::io::Error::last_os_error());
        }
        Ok(OwnedHandle(h))
    }

    pub fn spawn(command: &mut Command) -> std::io::Result<super::SanitizedChild> {
        // 1. Open NUL three times — fresh inheritable handles for the
        //    child's stdio slots.  These are the ONLY handles that should
        //    be passed through.
        let stdin = open_nul(false)?;
        let stdout = open_nul(true)?;
        let stderr = open_nul(true)?;

        // 2. Build the command line and (optional) env block.
        let mut cmdline = build_command_line(command.get_program(), command.get_args());

        let envs: Vec<(OsString, Option<OsString>)> = command
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        let env_block = if envs.is_empty() {
            None
        } else {
            Some(build_env_block(envs))
        };

        let cwd_w: Option<Vec<u16>> = command.get_current_dir().map(|p| {
            OsStr::new(p)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        });

        // 3. Initialize the proc-thread attribute list.
        let mut size: usize = 0;
        unsafe {
            // First call: query required size.
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        }
        // Must use vec<u8> with the queried size — the struct is opaque.
        let mut attr_buf: Vec<u8> = vec![0; size];
        let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

        let ok = unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut size) };
        if ok == FALSE {
            return Err(std::io::Error::last_os_error());
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
            let err = std::io::Error::last_os_error();
            unsafe {
                DeleteProcThreadAttributeList(attr_list);
            }
            return Err(err);
        }

        // 4. Set up STARTUPINFOEX.  The child's stdio slots are the three
        //    NUL handles.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as DWORD;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin.as_raw();
        si.StartupInfo.hStdOutput = stdout.as_raw();
        si.StartupInfo.hStdError = stderr.as_raw();
        si.lpAttributeList = attr_list;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let mut flags: DWORD = EXTENDED_STARTUPINFO_PRESENT
            | DETACHED_PROCESS
            | CREATE_NEW_PROCESS_GROUP
            | CREATE_NO_WINDOW;
        if env_block.is_some() {
            flags |= CREATE_UNICODE_ENVIRONMENT;
        }

        let cwd_ptr = cwd_w
            .as_ref()
            .map(|v| v.as_ptr())
            .unwrap_or(std::ptr::null());
        let env_ptr = env_block
            .as_ref()
            .map(|v| v.as_ptr() as *mut std::ffi::c_void)
            .unwrap_or(std::ptr::null_mut());

        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                TRUE as BOOL, // bInheritHandles
                flags,
                env_ptr,
                cwd_ptr,
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        let err = if ok == FALSE {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        unsafe {
            DeleteProcThreadAttributeList(attr_list);
        }
        if let Some(err) = err {
            return Err(err);
        }

        // We don't need the thread handle.
        unsafe {
            CloseHandle(pi.hThread);
        }

        Ok(super::SanitizedChild {
            pid: pi.dwProcessId,
            handle: OwnedHandle(pi.hProcess),
        })
    }

    pub fn terminate(handle: &OwnedHandle) -> std::io::Result<()> {
        let ok = unsafe { TerminateProcess(handle.as_raw(), 1) };
        if ok == FALSE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn wait(handle: &OwnedHandle) -> std::io::Result<i32> {
        let rc = unsafe { WaitForSingleObject(handle.as_raw(), INFINITE) };
        if rc != WAIT_OBJECT_0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut code: DWORD = 0;
        let ok = unsafe { GetExitCodeProcess(handle.as_raw(), &mut code as *mut DWORD) };
        if ok == FALSE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(code as i32)
    }

    pub fn try_wait(handle: &OwnedHandle) -> std::io::Result<Option<i32>> {
        let mut code: DWORD = 0;
        let ok = unsafe { GetExitCodeProcess(handle.as_raw(), &mut code as *mut DWORD) };
        if ok == FALSE {
            return Err(std::io::Error::last_os_error());
        }
        if code == STILL_ACTIVE {
            Ok(None)
        } else {
            Ok(Some(code as i32))
        }
    }

    fn build_command_line<'a>(program: &OsStr, args: impl Iterator<Item = &'a OsStr>) -> Vec<u16> {
        let mut s = String::new();
        s.push_str(&quote(&program.to_string_lossy()));
        for a in args {
            s.push(' ');
            s.push_str(&quote(&a.to_string_lossy()));
        }
        OsStr::new(&s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// MSVCRT argv-parsing rules for quoting a single argument.
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

    fn build_env_block(overrides: Vec<(OsString, Option<OsString>)>) -> Vec<u16> {
        use std::collections::BTreeMap;
        // Start from parent env, apply overrides.  Windows env-block keys
        // are case-insensitive but we preserve original case from the parent.
        let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
        for (k, v) in std::env::vars_os() {
            env.insert(k, v);
        }
        for (k, v) in overrides {
            match v {
                Some(val) => {
                    env.insert(k, val);
                }
                None => {
                    env.remove(&k);
                }
            }
        }
        let mut block: Vec<u16> = Vec::new();
        for (k, v) in env {
            block.extend(k.encode_wide());
            block.push(b'=' as u16);
            block.extend(v.encode_wide());
            block.push(0);
        }
        // Double-null terminator.
        block.push(0);
        block
    }
}

// ── Unix implementation ─────────────────────────────────────────────────────

#[cfg(unix)]
mod unix {
    use std::process::Command;

    pub fn spawn(command: &mut Command) -> std::io::Result<super::SanitizedChild> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        // Always run as a daemon — fully detached, no controlling tty.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        unsafe {
            command.pre_exec(|| {
                // Detach from controlling tty / process group.
                if libc::setsid() == -1 {
                    // setsid fails when we're already a session leader — not
                    // fatal for our purposes.  Keep going.
                }
                close_extra_fds();
                Ok(())
            });
        }

        let child = command.spawn()?;
        let pid = child.id();
        Ok(super::SanitizedChild { pid, child })
    }

    /// Close every open file descriptor > 2 in the calling process.
    ///
    /// Called from the forked child between `fork` and `exec`, which means:
    ///   * We MUST be async-signal-safe.  No allocator calls, no Rust I/O.
    ///   * We can call `close`, `open`, `readdir`, `getdents64` etc.
    ///
    /// Strategy:
    ///   1. Try `close_range(3, ~0, 0)` on Linux 5.9+ via direct syscall.
    ///   2. Fall back to walking `/proc/self/fd` (Linux) or `/dev/fd`
    ///      (BSD/macOS).
    ///   3. Final fallback: sysconf loop up to `_SC_OPEN_MAX`.
    unsafe fn close_extra_fds() {
        // 1. Try close_range syscall on Linux.
        #[cfg(target_os = "linux")]
        {
            // SYS_close_range = 436 on x86_64/aarch64/most arches.
            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "x86",
                target_arch = "arm",
                target_arch = "riscv64",
                target_arch = "powerpc64",
            ))]
            {
                const SYS_CLOSE_RANGE: libc::c_long = 436;
                let rc = libc::syscall(SYS_CLOSE_RANGE, 3u32, libc::c_uint::MAX, 0u32);
                if rc == 0 {
                    return;
                }
            }
        }

        // 2. Walk /dev/fd (works on Linux via /proc symlink and on macOS / BSD).
        let dir = libc::opendir(c"/dev/fd".as_ptr());
        if !dir.is_null() {
            let dir_fd = libc::dirfd(dir);
            loop {
                let ent = libc::readdir(dir);
                if ent.is_null() {
                    break;
                }
                let name_ptr = (*ent).d_name.as_ptr();
                let mut fd: libc::c_int = 0;
                let mut p = name_ptr;
                let mut ok = false;
                while *p != 0 {
                    let c = *p as u8;
                    if !c.is_ascii_digit() {
                        ok = false;
                        break;
                    }
                    fd = fd * 10 + (c - b'0') as libc::c_int;
                    p = p.add(1);
                    ok = true;
                }
                if !ok {
                    continue;
                }
                if fd > 2 && fd != dir_fd {
                    libc::close(fd);
                }
            }
            libc::closedir(dir);
            return;
        }

        // 3. Last-resort sysconf loop.
        let max = libc::sysconf(libc::_SC_OPEN_MAX);
        let max = if max < 0 { 4096 } else { max as libc::c_int };
        for fd in 3..max {
            libc::close(fd);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn sanitized_child_unix_holds_child() {
        // Smoke: just ensure the type wires up.
        let _ = std::mem::size_of::<SanitizedChild>();
    }
}
