#![cfg(feature = "async-process")]

//! Contract coverage for the long-lived kernel-substrate process session.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::time::Duration;

use running_process::{
    AsyncProcessBuilder, AsyncProcessSessionEvent, AsyncProcessSessionOptions, ProcessError,
    StreamKind,
};

fn fixture_program() -> OsString {
    testbin("testbin-stdio-scripted")
}

fn testbin(name: &str) -> OsString {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
        .into_os_string()
}

fn options() -> AsyncProcessSessionOptions {
    AsyncProcessSessionOptions {
        max_queued_chunks: 4,
        max_chunk_bytes: 64,
        post_exit_grace: Some(Duration::from_millis(25)),
        kill_on_drop: true,
    }
}

#[tokio::test]
async fn session_pumps_first_output_before_the_direct_child_exits() {
    let mut session = AsyncProcessBuilder::new(fixture_program())
        .arg("out:first")
        .arg("sleep-ms:1000")
        .session(options());
    session.start().await.expect("start session");

    let event = tokio::time::timeout(Duration::from_millis(500), session.next_output())
        .await
        .expect("first output arrives before exit")
        .expect("session output is still open");
    assert!(matches!(
        event,
        AsyncProcessSessionEvent::Chunk(chunk)
            if chunk.stream == StreamKind::Stdout && chunk.bytes == b"first"
    ));
    assert!(session.poll().await.expect("poll").is_none());
    session.kill().await.expect("kill fixture");
    let _ = session.wait().await.expect("reap fixture");
}

#[tokio::test]
async fn session_rejects_zero_and_overflow_bounds_before_spawn() {
    let mut zero_queue =
        AsyncProcessBuilder::new("definitely-not-spawned").session(AsyncProcessSessionOptions {
            max_queued_chunks: 0,
            ..options()
        });
    assert!(matches!(
        zero_queue.start().await,
        Err(ProcessError::Io(error)) if error.kind() == ErrorKind::InvalidInput
    ));

    let mut zero_chunk =
        AsyncProcessBuilder::new("definitely-not-spawned").session(AsyncProcessSessionOptions {
            max_chunk_bytes: 0,
            ..options()
        });
    assert!(matches!(
        zero_chunk.start().await,
        Err(ProcessError::Io(error)) if error.kind() == ErrorKind::InvalidInput
    ));

    let mut overflowing =
        AsyncProcessBuilder::new("definitely-not-spawned").session(AsyncProcessSessionOptions {
            max_queued_chunks: usize::MAX,
            max_chunk_bytes: 1,
            ..options()
        });
    assert!(matches!(
        overflowing.start().await,
        Err(ProcessError::Io(error)) if error.kind() == ErrorKind::InvalidInput
    ));

    let mut tokio_overflow =
        AsyncProcessBuilder::new("definitely-not-spawned").session(AsyncProcessSessionOptions {
            max_queued_chunks: (usize::MAX >> 3) + 1,
            max_chunk_bytes: 1,
            ..options()
        });
    assert!(matches!(
        tokio_overflow.start().await,
        Err(ProcessError::Io(error)) if error.kind() == ErrorKind::InvalidInput
    ));
}

#[tokio::test]
async fn session_waits_for_direct_exit_even_when_a_full_output_queue_blocks_eof() {
    let mut session = AsyncProcessBuilder::new(fixture_program())
        .arg("out:queued")
        .session(AsyncProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 64,
            post_exit_grace: Some(Duration::from_secs(2)),
            kill_on_drop: true,
        });
    session.start().await.expect("start session");

    // The chunk fills the only slot; its EOF event cannot be delivered until
    // the receiver catches up. Direct-child reaping must not depend on that.
    let status = tokio::time::timeout(Duration::from_millis(500), session.wait())
        .await
        .expect("direct wait is independent of a full output queue")
        .expect("direct child reaped");
    assert!(status.success());
}

#[tokio::test]
async fn session_slow_consumer_preserves_saturated_stdout_and_stderr() {
    let stdout = vec![b'a'; 4096];
    let stderr = vec![b'b'; 4096];
    let mut session = AsyncProcessBuilder::new(fixture_program())
        .arg(format!("outhex:{}", hex(&stdout)))
        .arg(format!("errhex:{}", hex(&stderr)))
        .session(AsyncProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 31,
            post_exit_grace: Some(Duration::from_millis(25)),
            kill_on_drop: true,
        });
    session.start().await.expect("start saturated session");
    tokio::time::sleep(Duration::from_millis(40)).await;

    let mut captured_stdout = Vec::new();
    let mut captured_stderr = Vec::new();
    let mut abandoned = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(3), session.next_output())
        .await
        .expect("bounded queue makes progress once consumed")
    {
        if let AsyncProcessSessionEvent::Chunk(chunk) = &event {
            assert!(chunk.bytes.len() <= 31);
            match chunk.stream {
                StreamKind::Stdout => captured_stdout.extend_from_slice(&chunk.bytes),
                StreamKind::Stderr => captured_stderr.extend_from_slice(&chunk.bytes),
            }
        }
        abandoned |= matches!(event, AsyncProcessSessionEvent::StreamAbandoned(_));
    }
    assert_eq!(captured_stdout, stdout);
    assert_eq!(captured_stderr, stderr);
    assert!(!abandoned, "queued-but-readable output is never abandoned");
    assert!(session
        .wait()
        .await
        .expect("reap saturated child")
        .success());
}

#[tokio::test]
async fn session_kill_and_wait_remain_responsive_while_stdin_is_blocked() {
    let mut session = AsyncProcessBuilder::new(testbin("testbin-slow-stdin-reader"))
        .arg("--sleep-ms")
        .arg("200")
        .session(AsyncProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 128 * 1024,
            post_exit_grace: Some(Duration::from_millis(25)),
            kill_on_drop: true,
        });
    session.start().await.expect("start slow stdin child");

    let write = session.write_stdin(vec![0xA5; 128 * 1024]);
    let kill = session.kill();
    let wait = session.wait();
    let (write, kill, wait) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(write, kill, wait)
    })
    .await
    .expect("stdin write cannot block kill or direct wait");
    assert!(kill.is_ok(), "kill result: {kill:?}");
    assert!(wait.is_ok(), "wait result: {wait:?}");
    assert!(
        write.is_ok()
            || matches!(write, Err(ProcessError::Io(error)) if error.kind() == ErrorKind::BrokenPipe)
    );
}

#[tokio::test]
async fn session_cpu_sample_is_identity_bound_and_progresses_when_supported() {
    let mut session = AsyncProcessBuilder::new(testbin("testbin-session-cpu-spinner"))
        .nice(Some(5))
        .session(options());
    session.start().await.expect("start CPU fixture");
    let before = session.cpu_time().await.expect("CPU sample request");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = session.cpu_time().await.expect("CPU sample request");
    if let (Some(before), Some(after)) = (before, after) {
        assert!(after >= before, "direct-child CPU time is monotonic");
    }
    session.kill().await.expect("kill CPU fixture");
}

#[cfg(unix)]
#[tokio::test]
async fn session_drop_kills_and_reaps_its_direct_child() {
    let mut session = AsyncProcessBuilder::new(testbin("testbin-sleeper")).session(options());
    session.start().await.expect("start long-lived session");
    let pid = session.pid().expect("diagnostic pid");
    drop(session);
    assert!(
        wait_until_not_running(pid, Duration::from_secs(2)),
        "kill-on-drop direct child {pid} was not reaped"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_drop_without_kill_drains_and_reaps_in_background() {
    let payload = vec![b'd'; 32 * 1024];
    let mut session = AsyncProcessBuilder::new(fixture_program())
        .arg(format!("outhex:{}", hex(&payload)))
        .arg("sleep-ms:50")
        .session(AsyncProcessSessionOptions {
            kill_on_drop: false,
            ..options()
        });
    session
        .start()
        .await
        .expect("start background-drain session");
    let pid = session.pid().expect("diagnostic pid");
    drop(session);
    assert!(
        wait_until_not_running(pid, Duration::from_secs(2)),
        "detached direct child {pid} was not drained and reaped"
    );
}

#[tokio::test]
async fn split_session_output_drop_keeps_control_reap_live() {
    // `outrep:` rather than `outhex:`: the same 32 KiB of stdout, generated by
    // the fixture instead of carried on its command line. Hex-encoded, this
    // payload is 64 KiB of argv, and Windows caps a command line at 32,767
    // characters -- the spawn failed there with `The filename or extension is
    // too long` (os error 206).
    const PAYLOAD_LEN: usize = 32 * 1024;
    let mut session = AsyncProcessBuilder::new(fixture_program())
        .arg(format!("outrep:64:{PAYLOAD_LEN}"))
        .session(AsyncProcessSessionOptions {
            max_queued_chunks: 1,
            max_chunk_bytes: 64,
            ..options()
        });
    session.start().await.expect("start split session");
    let (control, output) = session.into_parts().expect("split started session");
    drop(output);

    let status = tokio::time::timeout(Duration::from_secs(1), control.wait())
        .await
        .expect("receiver drop cannot stall direct reaping")
        .expect("direct child reaped");
    assert!(status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn split_session_control_drop_honors_terminal_cleanup() {
    let mut session = AsyncProcessBuilder::new(testbin("testbin-sleeper")).session(options());
    session.start().await.expect("start split sleeper");
    let (control, output) = session.into_parts().expect("split started session");
    let pid = control.pid();
    drop(control);

    assert!(
        wait_until_not_running(pid, Duration::from_secs(2)),
        "terminal control drop did not reap direct child {pid}"
    );
    drop(output);
}

#[cfg(unix)]
#[tokio::test]
async fn split_session_waits_while_output_is_pending_for_a_held_pipe() {
    let sleeper = testbin("testbin-sleeper");
    let mut session = AsyncProcessBuilder::new(testbin("testbin-session-pipe-holder"))
        .arg(sleeper)
        .session(AsyncProcessSessionOptions {
            // This is deliberately much longer than the fixture's 50ms
            // direct-child delay. The output future therefore stays pending
            // while the separate control lane observes the direct exit.
            post_exit_grace: Some(Duration::from_millis(500)),
            ..options()
        });
    session.start().await.expect("start split session");
    let (control, mut output) = session.into_parts().expect("split started session");

    let first = tokio::time::timeout(Duration::from_secs(1), output.next_output())
        .await
        .expect("fixture reports grandchild pid")
        .expect("output remains open");
    let AsyncProcessSessionEvent::Chunk(chunk) = first else {
        panic!("fixture reports its grandchild pid as stdout")
    };
    let text = String::from_utf8_lossy(&chunk.bytes);
    let pid = text
        .split_whitespace()
        .find_map(|word| word.strip_prefix("GRANDCHILD_PID="))
        .and_then(|pid| pid.parse::<i32>().ok())
        .expect("fixture reports grandchild pid before direct exit");

    let first_post_exit_event = {
        let pending_output = output.next_output();
        tokio::pin!(pending_output);

        let status = tokio::time::timeout(Duration::from_millis(300), control.wait())
            .await
            .expect("direct wait is independent of a pending output receive")
            .expect("direct child reaped");
        assert!(status.success());

        tokio::time::timeout(Duration::from_secs(1), &mut pending_output)
            .await
            .expect("held pipe is eventually abandoned")
    };

    let mut saw_abandoned = matches!(
        first_post_exit_event,
        Some(AsyncProcessSessionEvent::StreamAbandoned(_))
    );
    while let Some(event) = tokio::time::timeout(Duration::from_secs(1), output.next_output())
        .await
        .expect("split output drains after abandonment")
    {
        saw_abandoned |= matches!(event, AsyncProcessSessionEvent::StreamAbandoned(_));
    }
    assert!(saw_abandoned, "held pipe is explicitly abandoned");
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGKILL) },
        0,
        "cleanup sleeper"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_unbounded_post_exit_waits_for_later_eof_without_abandonment() {
    let sleeper = testbin("testbin-sleeper");
    let mut session = AsyncProcessBuilder::new(testbin("testbin-session-pipe-holder"))
        .arg(sleeper)
        .session(AsyncProcessSessionOptions {
            post_exit_grace: None,
            ..options()
        });
    session.start().await.expect("start unbounded session");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), session.wait())
            .await
            .expect("direct exit is independent of unbounded pipe EOF")
            .expect("direct child reaped")
            .success()
    );

    let mut ready_output = Vec::new();
    let pid = loop {
        let event = tokio::time::timeout(Duration::from_secs(1), session.next_output())
            .await
            .expect("fixture reports its grandchild pid")
            .expect("output remains open");
        if let AsyncProcessSessionEvent::Chunk(chunk) = event {
            ready_output.extend_from_slice(&chunk.bytes);
        }
        let text = String::from_utf8_lossy(&ready_output);
        let grandchild = text
            .split_whitespace()
            .find_map(|word| word.strip_prefix("GRANDCHILD_PID="))
            .and_then(|pid| pid.parse::<i32>().ok());
        let saw_sleeper_pid = text.split_whitespace().any(|word| word.starts_with("PID="));
        if let (Some(pid), true) = (grandchild, saw_sleeper_pid) {
            break pid;
        }
    };

    assert!(
        tokio::time::timeout(Duration::from_millis(75), session.next_output())
            .await
            .is_err(),
        "unbounded post-exit policy keeps output pending while descendant owns the pipe"
    );
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGKILL) },
        0,
        "close held pipe"
    );

    let mut saw_eof = false;
    let mut saw_abandoned = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(1), session.next_output())
        .await
        .expect("output reaches EOF after descendant closes pipe")
    {
        saw_eof |= matches!(event, AsyncProcessSessionEvent::StreamEof(_));
        saw_abandoned |= matches!(event, AsyncProcessSessionEvent::StreamAbandoned(_));
    }
    assert!(saw_eof, "closing descendant pipe reaches normal EOF");
    assert!(!saw_abandoned, "unbounded policy never abandons the stream");
}

#[cfg(unix)]
#[tokio::test]
async fn session_zero_post_exit_grace_drains_ready_output_then_abandons() {
    let sleeper = testbin("testbin-sleeper");
    let mut session = AsyncProcessBuilder::new(testbin("testbin-session-pipe-holder"))
        .arg(sleeper)
        .session(AsyncProcessSessionOptions {
            post_exit_grace: Some(Duration::ZERO),
            ..options()
        });
    session.start().await.expect("start zero-grace session");
    assert!(session.wait().await.expect("direct child exits").success());

    let mut transcript = Vec::new();
    let mut saw_abandoned = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(1), session.next_output())
        .await
        .expect("zero grace abandons only after ready output drains")
    {
        if let AsyncProcessSessionEvent::Chunk(chunk) = &event {
            transcript.extend_from_slice(&chunk.bytes);
        }
        saw_abandoned |= matches!(event, AsyncProcessSessionEvent::StreamAbandoned(_));
    }
    let text = String::from_utf8_lossy(&transcript);
    let pid = text
        .split_whitespace()
        .find_map(|word| word.strip_prefix("GRANDCHILD_PID="))
        .and_then(|pid| pid.parse::<i32>().ok())
        .expect("ready output was preserved before immediate abandonment");
    assert!(saw_abandoned, "zero grace abandons a held pipe immediately");
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGKILL) },
        0,
        "cleanup sleeper"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_abandons_a_grandchild_held_pipe_after_direct_exit() {
    let sleeper = testbin("testbin-sleeper");
    let mut session = AsyncProcessBuilder::new(testbin("testbin-session-pipe-holder"))
        .arg(sleeper)
        .session(AsyncProcessSessionOptions {
            post_exit_grace: Some(Duration::from_millis(30)),
            ..options()
        });
    session.start().await.expect("start pipe-holder session");
    assert!(session.wait().await.expect("direct child exits").success());

    let mut saw_abandoned = false;
    let mut transcript = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(1), session.next_output())
        .await
        .expect("post-exit grace expires")
    {
        if let AsyncProcessSessionEvent::Chunk(chunk) = &event {
            transcript.extend_from_slice(&chunk.bytes);
        }
        saw_abandoned |= matches!(event, AsyncProcessSessionEvent::StreamAbandoned(_));
    }
    assert!(saw_abandoned, "held inherited pipe is explicitly abandoned");
    let text = String::from_utf8_lossy(&transcript);
    let pid = text
        .split_whitespace()
        .find_map(|word| word.strip_prefix("GRANDCHILD_PID="))
        .and_then(|pid| pid.parse::<i32>().ok())
        .expect("fixture reports grandchild pid before direct exit");
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGKILL) },
        0,
        "cleanup sleeper"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_post_exit_grace_is_a_cumulative_pipe_read_budget() {
    let writer = testbin("testbin-session-drip-writer");
    let mut session = AsyncProcessBuilder::new(testbin("testbin-session-pipe-holder"))
        .arg(writer)
        .session(AsyncProcessSessionOptions {
            post_exit_grace: Some(Duration::from_millis(35)),
            ..options()
        });
    session.start().await.expect("start drip pipe-holder");
    assert!(session.wait().await.expect("direct child exits").success());

    let mut transcript = Vec::new();
    let mut abandoned = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(1), session.next_output())
        .await
        .expect("drip writer cannot extend total post-exit grace forever")
    {
        if let AsyncProcessSessionEvent::Chunk(chunk) = &event {
            transcript.extend_from_slice(&chunk.bytes);
        }
        abandoned |= matches!(event, AsyncProcessSessionEvent::StreamAbandoned(_));
    }
    assert!(abandoned, "drip writer is abandoned after cumulative grace");
    let text = String::from_utf8_lossy(&transcript);
    let pid = text
        .split_whitespace()
        .find_map(|word| word.strip_prefix("GRANDCHILD_PID="))
        .and_then(|pid| pid.parse::<i32>().ok())
        .expect("fixture reports drip-writer pid");
    let kill_result = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(
        kill_result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "cleanup drip writer"
    );
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 0x0F) as usize] as char);
    }
    text
}

#[cfg(unix)]
fn wait_until_not_running(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}
