use std::ffi::OsStr;

use running_process_platform_internal::platform::process::shell_command;

#[cfg(unix)]
#[test]
fn shell_command_preserves_login_shell_contract_and_quoting() {
    let command_text = "printf '%s' 'alpha beta;\"gamma\"'";
    let mut command = shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("sh"));
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
fn shell_command_preserves_raw_cmd_quoting_contract() {
    let command_text = "echo alpha beta ^& gamma";
    let mut command = shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("cmd.exe"));
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
    assert_eq!(output.stdout, b"alpha beta & gamma \r\n");
}
