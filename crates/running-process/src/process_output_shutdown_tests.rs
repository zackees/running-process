use super::*;
use std::io::Write;

const FIXTURE_ENV: &str = "RUNNING_PROCESS_SESSION_SHUTDOWN_FIXTURE";
const RELEASE_ENV: &str = "RUNNING_PROCESS_SESSION_SHUTDOWN_RELEASE";
const READY_ENV: &str = "RUNNING_PROCESS_SESSION_SHUTDOWN_READY";

#[test]
fn helper() {
    let Some(mode) = std::env::var_os(FIXTURE_ENV) else {
        return;
    };
    if let Some(ready) = std::env::var_os(READY_ENV) {
        // libtest writes its startup banner before entering this helper. Closing
        // stdout earlier can make the harness exit with BrokenPipe, even when
        // the intended helper behavior never writes to stdout.
        std::fs::write(ready, b"ready").expect("publish helper readiness");
    }
    if mode == "pipe-holder" {
        let child = crate::process_runtime::runtime()
            .block_on(
                SpawnSpec::new(std::env::current_exe().expect("test executable"))
                    .arg("--exact")
                    .arg("async_process::output_shutdown_tests::helper")
                    .env(FIXTURE_ENV, "grandchild")
                    .stdin(StreamMode::Null)
                    .stdout(StreamMode::Inherit)
                    .stderr(StreamMode::Inherit)
                    .spawn(),
            )
            .expect("spawn descendant with inherited output pipes");
        drop(child);
        return;
    }
    if mode == "grandchild" {
        let release = PathBuf::from(std::env::var_os(RELEASE_ENV).expect("release marker"));
        std::fs::write(release.with_extension("ready"), b"ready").expect("ready marker");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !release.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::write(release.with_extension("done"), b"done");
        return;
    }
    if mode == "flood" {
        let bytes = [b'x'; 4096];
        for _ in 0..16384 {
            if std::io::stdout().write_all(&bytes).is_err()
                || std::io::stderr().write_all(&bytes).is_err()
            {
                break;
            }
        }
    }
    std::thread::sleep(Duration::from_secs(30));
}

async fn fixture(mode: &str) -> AsyncProcessSession {
    let directory = tempfile::tempdir().expect("private readiness directory");
    let ready = directory.path().join("ready");
    let mut session = AsyncProcessBuilder::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("async_process::output_shutdown_tests::helper")
        .env(FIXTURE_ENV, mode)
        .env(READY_ENV, ready.as_os_str())
        .session(AsyncProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 4096,
            post_exit_grace: None,
            kill_on_drop: true,
        });
    session.start().await.expect("start fixture");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("helper enters test body before output shutdown");
    session
}

#[tokio::test]
async fn full_queue_shutdown_joins_pumps_without_consumer_or_child_exit() {
    let (control, mut output) = fixture("flood").await.into_parts().expect("split");
    tokio::time::timeout(Duration::from_secs(2), async {
        while output.output.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("output queue fills");
    control.request_output_shutdown();
    let result = tokio::time::timeout(Duration::from_secs(2), output.shutdown()).await;
    let still_running = control.poll().await.expect("poll").is_none();
    control.kill().await.expect("reap fixture");
    result
        .expect("shutdown does not wait for output delivery")
        .expect("shutdown completion");
    assert!(
        still_running,
        "output shutdown must not require direct exit"
    );
    assert!(output.output.is_empty());
    assert!(output.next_output().await.is_none());
    output.shutdown().await.expect("idempotent shutdown");
}

#[tokio::test]
async fn silent_pending_read_shutdown_does_not_wait_for_direct_exit() {
    let (control, mut output) = fixture("silent").await.into_parts().expect("split");
    // Drain the test harness header until output really parks.
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(20), output.next_output()).await
    {
        assert!(event.is_some(), "fixture must still own its pipe");
    }
    control.request_output_shutdown();
    let result = tokio::time::timeout(Duration::from_secs(2), output.shutdown()).await;
    let still_running = control.poll().await.expect("poll").is_none();
    control.kill().await.expect("reap fixture");
    result
        .expect("pending read shutdown finishes")
        .expect("shutdown completion");
    assert!(still_running);
}

#[tokio::test]
async fn cancelled_shutdown_observer_retries_and_broadcasts_only_after_cleanup() {
    let (control, mut output) = fixture("silent").await.into_parts().expect("split");
    let state = output.shutdown_state.clone();
    let checkpoint = state.hold_cleanup();
    {
        let first = output.shutdown();
        tokio::pin!(first);
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut first => panic!("acknowledged while readers retained: {result:?}"),
                _ = checkpoint.entered(2) => {}
            }
        })
        .await
        .expect("both pumps reach owned-reader cleanup barrier");
        // Dropping this observer must not drop either tracked cleanup task.
    }
    {
        let retry = output.shutdown();
        let observer_one = state.wait();
        let observer_two = state.wait();
        tokio::pin!(retry, observer_one, observer_two);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        use std::future::Future as _;
        assert!(retry.as_mut().poll(&mut context).is_pending());
        assert!(observer_one.as_mut().poll(&mut context).is_pending());
        assert!(observer_two.as_mut().poll(&mut context).is_pending());
        drop(checkpoint);
        let (retried, first, second) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(retry, observer_one, observer_two)
        })
        .await
        .expect("all observers are notified after cleanup joins");
        retried.expect("retry");
        first.expect("first completion observer");
        second.expect("second completion observer");
    }
    state
        .wait()
        .await
        .expect("late observer sees retained completion");
    assert!(output.next_output().await.is_none());
    assert!(control.poll().await.expect("poll").is_none());
    control.kill().await.expect("reap fixture");
}

struct ReleaseDescendant(PathBuf);

#[tokio::test]
async fn pump_panic_still_enters_owned_reader_cleanup_before_reporting_failure() {
    let (control, mut output) = fixture("silent").await.into_parts().expect("split");
    let state = output.shutdown_state.clone();
    let checkpoint = state.hold_cleanup();
    state.inject_pump_panic();
    tokio::time::timeout(Duration::from_secs(2), checkpoint.entered(1))
        .await
        .expect("panicking pump retains its reader and enters cleanup");
    control.request_output_shutdown();
    tokio::time::timeout(Duration::from_secs(2), checkpoint.entered(2))
        .await
        .expect("both pumps enter cleanup");
    let pending = state.wait();
    tokio::pin!(pending);
    use std::future::Future as _;
    assert!(pending
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        .is_pending());
    drop(checkpoint);
    let result = tokio::time::timeout(Duration::from_secs(2), output.shutdown())
        .await
        .expect("cleanup joined after pump panic");
    control.kill().await.expect("reap fixture");
    assert!(
        matches!(result, Err(ProcessError::Io(error)) if error.to_string().contains("pump panicked"))
    );
}

impl Drop for ReleaseDescendant {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
    }
}

#[tokio::test]
async fn descendant_held_pipe_shutdown_finishes_after_direct_child_reaping() {
    let directory = tempfile::tempdir().expect("marker directory");
    let release = directory.path().join("release");
    let guard = ReleaseDescendant(release.clone());
    let mut session = AsyncProcessBuilder::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("async_process::output_shutdown_tests::helper")
        .env(FIXTURE_ENV, "pipe-holder")
        .env(RELEASE_ENV, release.as_os_str())
        .session(AsyncProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 4096,
            post_exit_grace: None,
            kill_on_drop: true,
        });
    session.start().await.expect("start pipe holder");
    assert!(tokio::time::timeout(Duration::from_secs(2), session.wait())
        .await
        .expect("direct child exits independently of its output")
        .expect("reaped")
        .success());
    tokio::time::timeout(Duration::from_secs(2), async {
        while !release.with_extension("ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("descendant owns inherited pipe");
    while let Ok(event) =
        tokio::time::timeout(Duration::from_millis(20), session.next_output()).await
    {
        assert!(event.is_some(), "descendant keeps the output open");
    }
    tokio::time::timeout(Duration::from_secs(2), session.shutdown_output())
        .await
        .expect("shutdown must not wait for descendant EOF")
        .expect("joined output cleanup");
    assert!(!release.with_extension("done").exists());
    drop(guard);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !release.with_extension("done").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("descendant survived shutdown and observed explicit release");
}
