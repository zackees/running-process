//! Phase 3 dumb-terminal SESSION client (soldr#2365, slice 4).
//!
//! The client end of a proxied compile session — the shape the `RUSTC_WRAPPER`
//! shim uses. It does **almost nothing**: open the session with a `SessionStart`
//! (the command from its argv/env), forward its own stdin as `Stdin` frames,
//! render inbound `Stdout`/`Stderr` frames onto its real stdout/stderr, and exit
//! with the session's code. No spawning, no daemon lifecycle, no caching logic —
//! the daemon does all of that on the far side of the broker relay.
//!
//! Transport-agnostic: it drives a [`Framed`] over any byte channel (a broker
//! connection in production; a direct daemon endpoint or an in-memory duplex in
//! tests), so it composes with the proven daemon endpoint (#923) and broker
//! relay (#924) without knowing which is on the other end.

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::Framed;

use crate::broker::protocol_v2::{session_frame, SessionFrame, SessionStart};
use crate::daemon::compile_session::SessionFrameCodec;

/// Drive one compile session as a dumb terminal over `framed`.
///
/// Sends `start`, then concurrently forwards `stdin` as `Stdin` frames (closing
/// with `StdinEof` on EOF) and renders `Stdout`/`Stderr` frames onto `stdout` /
/// `stderr`. Returns the session's exit code once the terminal `Exit` frame
/// arrives (or -1 if the stream ends without one).
///
/// # Errors
///
/// Propagates a transport or local-IO error. Never errors on stdio *content*.
pub async fn run_session_client<T, In, Out, Err>(
    framed: Framed<T, SessionFrameCodec>,
    start: SessionStart,
    mut stdin: In,
    mut stdout: Out,
    mut stderr: Err,
) -> std::io::Result<i32>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    In: tokio::io::AsyncRead + Unpin + Send + 'static,
    Out: tokio::io::AsyncWrite + Unpin,
    Err: tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = framed.split();

    sink.send(SessionFrame {
        kind: Some(session_frame::Kind::Start(start)),
    })
    .await
    .map_err(|e| std::io::Error::other(format!("send SessionStart failed: {e}")))?;

    // Forward local stdin as Stdin frames on its own task, so a compiler that
    // interleaves reads and writes never deadlocks against the output loop.
    let stdin_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => {
                    let _ = sink
                        .send(SessionFrame {
                            kind: Some(session_frame::Kind::StdinEof(true)),
                        })
                        .await;
                    break;
                }
                Ok(n) => {
                    if sink
                        .send(SessionFrame {
                            kind: Some(session_frame::Kind::Stdin(buf[..n].to_vec())),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut code = -1;
    while let Some(item) = stream.next().await {
        let frame = item.map_err(|e| std::io::Error::other(format!("session recv failed: {e}")))?;
        match frame.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.write_all(&b).await?,
            Some(session_frame::Kind::Stderr(b)) => stderr.write_all(&b).await?,
            Some(session_frame::Kind::Exit(e)) => {
                code = e.code;
                break;
            }
            // Ignore any client→daemon-only frame echoed back; not expected.
            _ => {}
        }
    }
    stdout.flush().await?;
    stderr.flush().await?;

    // The session is over; stop forwarding stdin (a compiler's stdin may never
    // EOF on its own).
    stdin_task.abort();
    Ok(code)
}

#[cfg(test)]
mod tests;
