#![cfg(feature = "async-process")]

use std::ffi::OsString;

use running_process::{AsyncProcessBuilder, AsyncStdio};

fn testbin(name: &str) -> OsString {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
        .into_os_string()
}

#[test]
fn semantic_builder_is_public_and_backend_free() {
    let _ = AsyncProcessBuilder::new("program")
        .arg("argument")
        .current_dir(".")
        .clear_env(true)
        .env("ONE", "1")
        .stdin(AsyncStdio::Null)
        .stdout(AsyncStdio::Piped)
        .stderr(AsyncStdio::Inherit)
        .create_process_group(true)
        .kill_when_owner_dies(true)
        .build();
    let _ = AsyncProcessBuilder::shell("echo semantic capture");
}

#[tokio::test]
async fn capture_preserves_a_nonzero_exit_status() {
    let output = AsyncProcessBuilder::new(testbin("testbin-stdio-scripted"))
        .arg("exit:7")
        .capture()
        .await
        .expect("capture succeeds even when the child exits nonzero");
    assert_eq!(output.status.code(), Some(7));
}

#[tokio::test]
async fn semantic_builder_clears_environment_before_explicit_overrides() {
    const INHERITED: &str = "ASYNC_SEMANTIC_CAPTURE_INHERITED";
    const EXPLICIT: &str = "ASYNC_SEMANTIC_CAPTURE_EXPLICIT";
    std::env::set_var(INHERITED, "must-not-reach-child");

    let temp = tempfile::tempdir().expect("temporary output directory");
    let output_path = temp.path().join("environment.txt");
    let output = AsyncProcessBuilder::new(testbin("testbin-env-dump"))
        .arg(output_path.as_os_str())
        .clear_env(true)
        .env(EXPLICIT, "present")
        .capture()
        .await
        .expect("clear-environment child runs");
    std::env::remove_var(INHERITED);

    assert!(output.status.success());
    let environment = std::fs::read_to_string(output_path).expect("read child environment");
    assert!(environment.contains("ASYNC_SEMANTIC_CAPTURE_EXPLICIT=present\n"));
    assert!(!environment.contains("ASYNC_SEMANTIC_CAPTURE_INHERITED="));
}

#[tokio::test]
async fn semantic_stdio_null_discards_each_captured_stream() {
    let output = AsyncProcessBuilder::new(testbin("testbin-stdio-scripted"))
        .arg("out:discarded-stdout")
        .arg("err:discarded-stderr")
        .stdin(AsyncStdio::Null)
        .stdout(AsyncStdio::Null)
        .stderr(AsyncStdio::Null)
        .capture()
        .await
        .expect("null stdio child runs");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[tokio::test]
async fn semantic_stdio_inherit_does_not_claim_to_capture_each_stream() {
    let output = AsyncProcessBuilder::new(testbin("testbin-stdio-scripted"))
        .arg("out:inherited-stdout")
        .arg("err:inherited-stderr")
        .stdout(AsyncStdio::Inherit)
        .stderr(AsyncStdio::Inherit)
        .capture()
        .await
        .expect("inherited stdio child runs");

    assert!(output.status.success());
    // Do not assert where inherited output appears: the test harness may
    // capture its own stdout/stderr. The facade must only promise emptiness.
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[tokio::test]
async fn semantic_shell_uses_the_platform_owned_convention() {
    #[cfg(windows)]
    let command = "echo semantic-shell";
    #[cfg(not(windows))]
    let command = "printf semantic-shell";

    let output = AsyncProcessBuilder::shell(command)
        .capture()
        .await
        .expect("platform shell runs");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("semantic-shell"));
}

#[cfg(unix)]
#[tokio::test]
async fn capture_preserves_a_signal_exit_status() {
    use std::os::unix::process::ExitStatusExt;

    let output = AsyncProcessBuilder::shell("kill -TERM $$")
        .capture()
        .await
        .expect("signal exit is a captured process result");
    assert_eq!(output.status.signal(), Some(libc::SIGTERM));
}
