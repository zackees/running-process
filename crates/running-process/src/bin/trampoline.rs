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

fn set_process_name(exe: &Path) {
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !stem.is_empty() {
        running_process_platform_internal::platform::process::set_process_name(&stem);
    }
}

fn run() -> i32 {
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

    // The platform owns console/window mechanics; the trampoline only owns
    // its sidecar and launch policy.
    running_process_platform_internal::platform::process::configure_trampoline_command(&mut cmd);

    // 9. Spawn, wait, and exit with child's status code.
    match cmd.status() {
        Ok(status) => {
            running_process_platform_internal::platform::process::trampoline_exit_code(status)
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
