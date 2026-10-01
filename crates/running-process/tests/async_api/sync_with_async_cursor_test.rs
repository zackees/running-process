#![cfg(feature = "async-process")]

//! #850: a blocking call on a process and an async cursor over the same
//! process's output run at the same time and both finish. The blocking adapter
//! drains output on the actor runtime while the cursor awaits it there, so
//! neither may starve the other or deadlock on the shared log.

use std::time::Duration;

use running_process::{AsyncProcess, CursorRead};

const SCRIPT: &str =
    "import time; print('one', flush=True); time.sleep(0.2); print('two', flush=True)";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_output_call_and_an_async_cursor_read_the_same_process_together() {
    let mut process = AsyncProcess::new("python").arg("-c").arg(SCRIPT);
    process.start().await.expect("start");
    let mut cursor = process.output_cursor().expect("cursor");

    let reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        loop {
            match cursor.read_next_async().await {
                CursorRead::Record(record) => bytes.extend(record.bytes),
                CursorRead::Gap { from, to } => panic!("unexpected gap {from}..{to}"),
                CursorRead::Eof => return bytes,
            }
        }
    });

    // The blocking adapter parks its thread, so it runs off the runtime's
    // workers, as a sync caller in an async program would.
    let blocking = std::thread::spawn(move || process.output_blocking());
    let output = tokio::task::spawn_blocking(move || blocking.join().expect("blocking thread"))
        .await
        .expect("join blocking")
        .expect("output_blocking");

    let seen = tokio::time::timeout(Duration::from_secs(30), reader)
        .await
        .expect("cursor reached EOF")
        .expect("cursor task");

    assert_eq!(output.exit_code, 0);
    let text = String::from_utf8_lossy(&seen).replace("\r\n", "\n");
    assert!(
        text.contains("one\n") && text.contains("two\n"),
        "cursor saw {text:?}"
    );
    assert!(output.stdout.starts_with(b"one"));
}
