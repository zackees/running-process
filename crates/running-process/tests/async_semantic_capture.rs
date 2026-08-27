#![cfg(feature = "async-process")]

use std::ffi::OsString;

use running_process::{AsyncProcessBuilder, AsyncStdio};

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
    let args = vec![OsString::from("-c"), OsString::from("exit 7")];
    let output = AsyncProcessBuilder::new("python")
        .args(args)
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
