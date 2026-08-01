//! Fixture: a daemon that serves the listener the broker handed it (#500).
//!
//! The unit tests for broker-owned bind prove the broker's half — it binds,
//! clears `FD_CLOEXEC`, and publishes the descriptor. They cannot prove the
//! half that matters to a client: that a *separate process*, after `exec`,
//! adopts that descriptor and answers on it. Only a real child can show that,
//! which is what this binary is for.
//!
//! Protocol, kept deliberately small so a failure points at the handover
//! rather than at the fixture:
//!
//! - Adopted a listener: accept one connection, write `SERVED`, exit `0`.
//! - No listener was passed: exit `2`.
//! - A listener was advertised but could not be adopted: exit `3`.
//! - Adopted one but serving failed: exit `4`.
//!
//! The outcomes get distinct exit codes because they fail for different
//! reasons, and a test that cannot tell them apart would pass for the wrong
//! one — "the child inherited nothing" and "the child rejected what it
//! inherited" are indistinguishable from the parent otherwise.

fn main() {
    // Non-Unix has no listener to recover; `recover_from_env` reports that
    // rather than pretending, and the exit code says which case it was.
    match running_process::broker::broker_owned_bind::recover_from_env() {
        Ok(Some(listener)) => match serve_once(listener) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("inherited-listener-daemon: serving failed: {error}");
                std::process::exit(4);
            }
        },
        Ok(None) => {
            eprintln!("inherited-listener-daemon: no listener was passed");
            std::process::exit(2);
        }
        Err(error) => {
            eprintln!("inherited-listener-daemon: listener not adoptable: {error}");
            std::process::exit(3);
        }
    }
}

/// Accept exactly one connection and answer it.
fn serve_once(
    listener: running_process::broker::brokered_backend::IpcListener,
) -> std::io::Result<()> {
    use interprocess::local_socket::traits::Listener as _;
    use std::io::Write as _;

    let mut stream = listener.accept()?;
    // The marker proves the bytes came from this process rather than from a
    // broker that happened to still be holding the socket open.
    stream.write_all(b"SERVED\n")?;
    stream.flush()?;
    Ok(())
}
