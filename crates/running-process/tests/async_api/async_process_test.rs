#![cfg(feature = "async-process")]

use running_process::AsyncProcess;

#[tokio::test]
async fn public_async_process_runs_without_exposing_platform_child_types() {
    #[cfg(windows)]
    let process = AsyncProcess::new("cmd.exe")
        .arg("/C")
        .arg("echo public-async");
    #[cfg(not(windows))]
    let process = AsyncProcess::new("/bin/sh")
        .arg("-c")
        .arg("printf public-async");

    let mut process = process;
    process.start().await.expect("async process start");
    let output = process.output().await.expect("async process output");
    assert!(output.stdout.starts_with(b"public-async"));
    assert_eq!(output.exit_code, 0);
}
