// Windows-specific daemon operations (DETACHED_PROCESS, named pipes)

/// Creation flags for a fully detached background process.
///
/// Do NOT combine DETACHED_PROCESS with CREATE_NO_WINDOW — they conflict.
/// DETACHED_PROCESS means "no console at all"; CREATE_NO_WINDOW means
/// "create a console but don't show a window." Using both is undefined.
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Re-spawn the current executable as a detached background process.
///
/// The spawned child receives all of `args` plus `--daemon-internal` so it
/// knows it is already running detached. The current process exits after a
/// successful spawn.
pub fn daemonize(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::windows::process::CommandExt;

    let exe = std::env::current_exe()?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(args);
    cmd.arg("--daemon-internal");
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);

    // Redirect stdio to nul so the child is completely detached.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    cmd.spawn()
        .map_err(|e| format!("failed to spawn detached daemon: {e}").to_string())?;

    // The parent exits, returning the user to the shell.
    std::process::exit(0);
}
