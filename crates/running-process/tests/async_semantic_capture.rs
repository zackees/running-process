#![cfg(feature = "async-process")]

use std::ffi::OsString;

use running_process::{AsyncProcessBuilder, AsyncStdio};

fn fixture_program() -> OsString {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    dir.join(format!(
        "testbin-stdio-scripted{}",
        std::env::consts::EXE_SUFFIX
    ))
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
    let output = AsyncProcessBuilder::new(fixture_program())
        .arg("exit:7")
        .capture()
        .await
        .expect("capture succeeds even when the child exits nonzero");
    assert_eq!(output.status.code(), Some(7));
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
