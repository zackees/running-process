use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

#[derive(serde::Deserialize)]
struct Sidecar {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
}

fn sidecar_path(exe: &Path) -> PathBuf {
    // Replace extension with `.daemon.json`.
    // On Windows: foo.exe -> foo.daemon.json
    // On Unix:    foo     -> foo.daemon.json
    let stem = exe
        .file_stem()
        .expect("daemon-trampoline: cannot determine exe file stem");
    exe.with_file_name(format!("{}.daemon.json", stem.to_string_lossy()))
}

#[allow(clippy::needless_return)]
fn set_process_name(exe: &Path) {
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        // prctl(PR_SET_NAME, ...) — name truncated to 15 chars by the kernel
        let truncated: String = stem.chars().take(15).collect();
        let c_name = std::ffi::CString::new(truncated).unwrap_or_default();
        unsafe {
            libc::prctl(libc::PR_SET_NAME, c_name.as_ptr() as libc::c_ulong, 0, 0, 0);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let c_name = std::ffi::CString::new(stem).unwrap_or_default();
        unsafe {
            libc::pthread_setname_np(c_name.as_ptr());
        }
    }
}

/// Reopen stdin/stdout/stderr to the platform null device (`/dev/null` on
/// Unix, `NUL` on Windows) and close the inherited file descriptors /
/// handles. Released handles include any pipe write ends the trampoline
/// inherited from the process that invoked `launch_detached(...)`.
///
/// Without this, a grandparent process that reads the caller's stdio via
/// a pipe — e.g. Python's
/// `subprocess.Popen(["mytool"], stdout=PIPE)` where `mytool` internally
/// calls `launch_detached(...)` — never observes EOF after the immediate
/// caller exits, because the orphaned trampoline + the child it spawned
/// keep the pipe's write end alive indefinitely. See issue #108.
///
/// Runs *before* the child `Command` is spawned so the child inherits the
/// null device too. `DETACHED_PROCESS` / `CREATE_NO_WINDOW` on Windows
/// only severs the console; arbitrary inherited pipe handles survive
/// those flags and must be closed explicitly.
///
/// Best-effort — failures are silent. A failed detach is no worse than
/// the pre-fix behavior; the only way the underlying syscalls can fail
/// here is if the null device is unopenable, on which platform the pipe
/// inheritance problem could not have arisen anyway.
fn detach_stdio() {
    #[cfg(unix)]
    detach_stdio_unix();

    #[cfg(windows)]
    detach_stdio_windows();
}

#[cfg(unix)]
fn detach_stdio_unix() {
    // SAFETY: open/dup2/close are async-signal-safe and we're single-
    // threaded this early in startup. Failures are deliberately silent —
    // we cannot use stderr to report them (that's exactly what we're
    // detaching from).
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null < 0 {
            return;
        }
        let _ = libc::dup2(null, libc::STDIN_FILENO);
        let _ = libc::dup2(null, libc::STDOUT_FILENO);
        let _ = libc::dup2(null, libc::STDERR_FILENO);
        if null > libc::STDERR_FILENO {
            let _ = libc::close(null);
        }
    }
}

#[cfg(windows)]
fn detach_stdio_windows() {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::processenv::{GetStdHandle, SetStdHandle};
    use winapi::um::winbase::{STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    use winapi::um::winnt::{FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE};

    let nul: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();

    // Open a fresh NUL handle per slot so closing the previous handle
    // (which on consoles may alias siblings) doesn't invalidate the
    // others.
    for (slot, access) in [
        (STD_INPUT_HANDLE, GENERIC_READ),
        (STD_OUTPUT_HANDLE, GENERIC_WRITE),
        (STD_ERROR_HANDLE, GENERIC_WRITE),
    ] {
        // SAFETY: the std-handle slots belong to this process; no other
        // thread is touching them this early in startup.
        unsafe {
            let nul_handle = CreateFileW(
                nul.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            );
            if nul_handle.is_null() || nul_handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let old = GetStdHandle(slot);
            let _ = SetStdHandle(slot, nul_handle);
            // GetStdHandle returns NULL when no handle is set and
            // INVALID_HANDLE_VALUE on error; neither is closeable.
            if !old.is_null() && old != INVALID_HANDLE_VALUE {
                let _ = CloseHandle(old);
            }
        }
    }
}

fn run() -> i32 {
    // FIRST thing: drop any stdio handles we inherited from the process
    // that spawned this trampoline. Otherwise both the trampoline and the
    // child it's about to spawn keep the caller's pipe write ends alive,
    // and any grandparent reading those pipes hangs indefinitely after
    // the immediate caller exits. See issue #108. Must run before the
    // `process::Command` below so the child inherits NUL instead of the
    // original pipes.
    detach_stdio();

    // 1. Determine our own exe path.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("daemon-trampoline: failed to get current exe path: {e}");
            return 1;
        }
    };

    // 2. Derive sidecar path.
    let sidecar = sidecar_path(&exe);

    // 3. Read sidecar JSON.
    let json = match fs::read_to_string(&sidecar) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "daemon-trampoline: failed to read sidecar {}: {e}",
                sidecar.display()
            );
            return 1;
        }
    };

    let cfg: Sidecar = match serde_json::from_str(&json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "daemon-trampoline: failed to parse sidecar {}: {e}",
                sidecar.display()
            );
            return 1;
        }
    };

    // 4. Set process name (Linux/macOS only).
    set_process_name(&exe);

    // 5. Build the command.
    let mut cmd = process::Command::new(&cfg.command);
    cmd.args(&cfg.args);

    // 6. Environment: replace if specified, otherwise inherit.
    if let Some(ref env) = cfg.env {
        cmd.env_clear();
        cmd.envs(env);
    }

    // 7. Working directory.
    if let Some(ref cwd) = cfg.cwd {
        cmd.current_dir(cwd);
    }

    // 8. Inherit stdin/stdout/stderr (default behavior).

    // 8b. On Windows, prevent the child from creating a visible console window.
    //     The trampoline itself was spawned with DETACHED_PROCESS (no console).
    //     Without explicit flags, Windows auto-creates a visible console for
    //     console applications spawned by a consoleless parent.
    //     CREATE_NO_WINDOW (0x0800_0000) gives the child a hidden console.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // 9. Spawn, wait, and exit with child's status code.
    match cmd.status() {
        Ok(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                // If killed by signal, map to 128 + signal (Unix convention).
                if let Some(sig) = status.signal() {
                    return 128 + sig;
                }
            }
            status.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("daemon-trampoline: failed to spawn '{}': {e}", cfg.command);
            1
        }
    }
}

fn main() {
    process::exit(run());
}
