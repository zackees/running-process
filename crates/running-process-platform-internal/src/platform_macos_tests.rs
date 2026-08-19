use std::ffi::OsStr;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

#[tokio::test]
async fn owner_death_registration_failure_aborts_spawn() {
    let mut command = tokio::process::Command::new("/usr/bin/true");
    super::configure_command_for_owner(&mut command, false, true, libc::pid_t::MAX)
        .expect("configure owner-death containment");

    match command.spawn() {
        Ok(mut child) => {
            let _ = child.kill().await;
            panic!("spawn succeeded before the owner watch was registered");
        }
        Err(error) => assert_eq!(
            error.raw_os_error(),
            Some(libc::ESRCH),
            "spawn must report the failed owner-watch registration"
        ),
    }
}

#[tokio::test]
async fn owner_death_supervisor_does_not_retain_child_stdout() {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "exec 1>&-; sleep 30"])
        .stdout(Stdio::piped());
    super::configure_command_for_owner(
        &mut command,
        false,
        true,
        unsafe { libc::getpid() },
    )
    .expect("configure owner-death containment");

    let mut child = command.spawn().expect("spawn stream-closing child");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stdout.read_to_end(&mut output))
        .await
        .expect("supervisor retained the child's closed stdout")
        .expect("read child stdout");
    assert!(output.is_empty());
    child.kill().await.expect("kill stream-closing child");
}

#[test]
fn shell_command_preserves_login_shell_contract_and_ignores_child_path() {
    let command_text = "printf '%s' 'alpha beta;\"gamma\"'";
    let mut command = super::shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("-lc"), OsStr::new(command_text)]
    );
    command
        .env_clear()
        .env("PATH", "/caller-supplied-path-override");
    let output = command
        .output()
        .expect("absolute shell command should execute independently of child PATH");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"alpha beta;\"gamma\"");
}
