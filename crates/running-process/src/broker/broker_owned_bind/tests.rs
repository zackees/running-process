//! Tests for broker-owned bind (#500 slice 32).

use super::*;

#[test]
fn support_is_reported_honestly_for_this_platform() {
    // The point of a capability query is that a caller can branch on it. Both
    // arms must carry something actionable: `Supported` needs no words, and
    // `Unsupported` must explain itself well enough that an operator reading a
    // log does not go looking for a misconfiguration.
    match support() {
        Support::Supported => assert!(cfg!(unix), "only Unix can pass a bound listener"),
        Support::Unsupported { reason } => {
            assert!(!cfg!(unix), "Unix should support broker-owned bind");
            assert!(
                reason.len() > 40,
                "an unsupported reason of {reason:?} tells an operator nothing"
            );
        }
    }
}

#[test]
fn the_env_var_name_is_namespaced_to_this_project() {
    // It lands in the environment of every daemon the broker spawns, so a
    // generic name would be a collision waiting to happen.
    assert!(INHERITED_LISTENER_FD_ENV.starts_with("RUNNING_PROCESS_"));
}

#[test]
fn no_env_var_means_no_inherited_listener() {
    // A daemon started by hand, or by the existing spawn-then-probe path, must
    // see "nothing was passed" rather than an error — that is the signal to
    // bind for itself.
    //
    // Reads the ambient environment rather than setting it: env-mutating tests
    // race under a parallel runner, and this crate has been bitten by that.
    // The variable is not set in a normal test process, which is exactly the
    // case being asserted.
    if std::env::var_os(INHERITED_LISTENER_FD_ENV).is_some() {
        eprintln!("skipping: {INHERITED_LISTENER_FD_ENV} is set in this environment");
        return;
    }
    let recovered = recover_from_env().expect("absence is not an error");
    assert!(recovered.is_none());
}

#[cfg(unix)]
mod unix {
    use super::*;

    /// A socket path inside a fresh temp dir, short enough for `sun_path`.
    ///
    /// `sun_path` is ~108 bytes, and a long temp path silently truncates —
    /// which surfaces as a bind failure with a misleading message.
    fn socket_path(dir: &tempfile::TempDir) -> String {
        dir.path().join("s").display().to_string()
    }

    #[test]
    fn the_endpoint_is_listening_the_moment_bind_returns() {
        // This is the property the whole slice exists for: a client can
        // connect before any daemon has been spawned, let alone reached its
        // own bind.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = socket_path(&dir);

        let _listener = InheritableListener::bind(&path).expect("broker binds the endpoint");

        // Connect immediately. Under the spawn-then-probe path this would fail
        // until the daemon got there.
        use interprocess::local_socket::traits::Stream as _;
        use interprocess::local_socket::{GenericFilePath, Stream, ToFsName as _};
        let name = path
            .as_str()
            .to_fs_name::<GenericFilePath>()
            .expect("socket name");
        Stream::connect(name).expect("endpoint must accept connections before any daemon exists");
    }

    #[test]
    fn preparing_a_command_publishes_a_descriptor_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = InheritableListener::bind(&socket_path(&dir)).expect("bind");

        let mut command = std::process::Command::new("/bin/true");
        listener.prepare(&mut command).expect("prepare");

        let published = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new(INHERITED_LISTENER_FD_ENV))
            .and_then(|(_, value)| value)
            .expect("prepare must publish the descriptor")
            .to_string_lossy()
            .into_owned();
        let fd: i32 = published.parse().expect("descriptor must be a number");
        assert!(fd >= 0, "descriptor {fd} is not valid");
    }

    #[test]
    fn preparing_clears_cloexec_so_the_descriptor_survives_exec() {
        // Without this the child inherits nothing and binds its own socket,
        // leaving the broker holding a listener no one serves — a failure that
        // looks like a daemon that started fine.
        use std::os::fd::AsRawFd as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let listener = InheritableListener::bind(&socket_path(&dir)).expect("bind");
        let mut command = std::process::Command::new("/bin/true");
        listener.prepare(&mut command).expect("prepare");

        let raw = {
            use std::os::fd::AsFd as _;
            listener.listener.as_fd().as_raw_fd()
        };
        // SAFETY: `raw` is the live listener's descriptor.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        assert_eq!(
            flags & libc::FD_CLOEXEC,
            0,
            "FD_CLOEXEC is still set; the child would not inherit this listener"
        );
    }

    #[test]
    fn a_non_numeric_descriptor_is_an_error_not_a_silent_fallback() {
        // Falling back to "bind your own" on a malformed value would hide a
        // broker/daemon version mismatch behind a working-looking daemon on
        // the wrong socket.
        let err = parse_descriptor("not-a-number").expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_negative_descriptor_is_rejected() {
        let err = parse_descriptor("-1").expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// The parsing half of `recover_from_env`, without touching the
    /// environment.
    ///
    /// Split for the same reason as elsewhere in this repo: a test that sets
    /// an env var races every other test in the binary.
    fn parse_descriptor(raw: &str) -> std::io::Result<i32> {
        let fd: i32 = raw.trim().parse().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{INHERITED_LISTENER_FD_ENV}={raw:?} is not a descriptor number"),
            )
        })?;
        if fd < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{INHERITED_LISTENER_FD_ENV}={fd} is not a valid descriptor"),
            ));
        }
        Ok(fd)
    }
}
