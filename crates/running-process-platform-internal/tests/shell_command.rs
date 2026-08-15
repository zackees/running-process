use std::ffi::OsStr;

#[cfg(windows)]
use running_process_platform_internal::platform::process::compat_shell_command;
use running_process_platform_internal::platform::process::shell_command;

#[cfg(unix)]
#[test]
fn shell_command_preserves_login_shell_contract_and_quoting() {
    let command_text = "printf '%s' 'alpha beta;\"gamma\"'";
    let mut command = shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("-lc"), OsStr::new(command_text)]
    );
    let output = command.output().expect("shell command should execute");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"alpha beta;\"gamma\"");
}

#[cfg(windows)]
#[test]
fn shell_command_preserves_structured_cmd_contract() {
    let command_text = "echo alpha beta ^& gamma";
    let mut command = shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("cmd.exe"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            OsStr::new("/D"),
            OsStr::new("/S"),
            OsStr::new("/C"),
            OsStr::new(command_text)
        ]
    );
    let output = command.output().expect("shell command should execute");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"alpha beta & gamma\r\n");
}

#[cfg(windows)]
#[test]
fn compat_shell_command_preserves_raw_cmd_quoting_contract() {
    let command_text = "if \"alpha beta\"==\"alpha beta\" (echo shell-ok)";
    let mut command = compat_shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("cmd"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            OsStr::new("/D /S /C \""),
            OsStr::new(command_text),
            OsStr::new("\"")
        ]
    );
    let output = command.output().expect("shell command should execute");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"shell-ok\r\n");
}
