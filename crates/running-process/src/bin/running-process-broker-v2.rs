//! v2 broker binary (slice 3c of #483 / #488).
//!
//! Binds the `rpb-v2-broker-v2-scaffold-<sid_hash>-0` local socket
//! (named pipe on Windows / Unix-domain socket on POSIX), accepts ONE
//! incoming connection, prints observable evidence to stdout, and
//! exits cleanly. The `program = "broker-v2-scaffold"` placeholder
//! will be replaced by a real CLI argument in a later slice; today the
//! point is just to prove the v2 binary can claim a kernel resource
//! end-to-end on every platform.
//!
//! `--no-bind` skips the bind entirely (the binary just exits 0). The
//! integration test uses it to assert the binary builds and starts on
//! every platform without needing pipe permissions.

use std::env;
use std::io::Write;
use std::process::ExitCode;

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use prost::Message;
use running_process::broker::lifecycle::names_v2::v2_program_pipe;
use running_process::broker::lifecycle::sid::user_sid_hash;
use running_process::broker::protocol::{
    hello_reply, read_frame, write_frame, Hello, HelloReply, Negotiated, ENVELOPE_VERSION,
};

/// Placeholder program name used by the slice 3c scaffold. Replaced by
/// a real CLI argument in a later slice once the v2 broker is invoked
/// in anger.
const SCAFFOLD_PROGRAM: &str = "broker-v2-scaffold";
const SCAFFOLD_PIPE_IDX: u32 = 0;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let no_bind = args.iter().any(|a| a == "--no-bind");

    println!(
        "running-process-broker-v2 {} (slice 3c; see issue #483/#488)",
        env!("CARGO_PKG_VERSION")
    );

    if no_bind {
        println!("running-process-broker-v2 --no-bind: skipping listener bind");
        return ExitCode::SUCCESS;
    }

    let sid = match user_sid_hash() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("running-process-broker-v2: user_sid_hash failed: {err}");
            return ExitCode::from(1);
        }
    };

    let pipe_name = match v2_program_pipe(SCAFFOLD_PROGRAM, &sid, SCAFFOLD_PIPE_IDX) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("running-process-broker-v2: v2_program_pipe failed: {err}");
            return ExitCode::from(1);
        }
    };

    let socket_path = match resolve_socket_path(&pipe_name) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("running-process-broker-v2: resolve_socket_path failed: {err}");
            return ExitCode::from(1);
        }
    };

    // Stale-file cleanup is Unix-only; on Windows the pipe namespace is
    // managed by the kernel and previous bindings vanish when the prior
    // process exited.
    #[cfg(unix)]
    {
        let path = std::path::Path::new(&socket_path);
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "running-process-broker-v2: create_dir_all({}) failed: {err}",
                    parent.display()
                );
                return ExitCode::from(1);
            }
        }
        let _ = std::fs::remove_file(&socket_path);
    }

    let name = match wrap_socket_name(&socket_path) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("running-process-broker-v2: wrap_socket_name failed: {err}");
            return ExitCode::from(1);
        }
    };

    let listener = match ListenerOptions::new().name(name).create_sync() {
        Ok(l) => l,
        Err(err) => {
            eprintln!("running-process-broker-v2: bind failed at {socket_path}: {err}");
            return ExitCode::from(1);
        }
    };

    println!("running-process-broker-v2 bound at {socket_path}");
    if let Err(err) = std::io::stdout().flush() {
        eprintln!("running-process-broker-v2: stdout flush failed: {err}");
    }

    let exit_code = match listener.accept() {
        Ok(mut stream) => {
            println!("running-process-broker-v2 peer connected");
            match handle_hello(&mut stream) {
                Ok(svc) => {
                    println!(
                        "running-process-broker-v2 Hello for service {svc:?} negotiated; exiting"
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("running-process-broker-v2: Hello handler failed: {err}");
                    ExitCode::from(1)
                }
            }
        }
        Err(err) => {
            eprintln!("running-process-broker-v2: accept failed: {err}");
            ExitCode::from(1)
        }
    };

    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&socket_path);
    }

    exit_code
}

/// Wrap a bare pipe name into the platform's local-socket path.
///
/// Mirrors the path scheme used by v1's private `lifecycle::names::build_pipe_path`.
/// Slice 3c repeats it inline because the scope of this slice forbids
/// new helper modules; a later slice will lift this into `names_v2`.
fn resolve_socket_path(bare_name: &str) -> Result<String, String> {
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\{bare_name}"))
    }
    #[cfg(unix)]
    {
        let dir = unix_socket_dir();
        let leaf = if cfg!(target_os = "macos") {
            // macOS sun_path is 104 bytes; hash the bare name to fit.
            let mut hash = blake3::Hasher::new();
            hash.update(bare_name.as_bytes());
            let bytes = hash.finalize();
            let mut hex = String::with_capacity(16);
            for b in bytes.as_bytes().iter().take(8) {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x}");
            }
            format!("{hex}.sock")
        } else {
            format!("{bare_name}.sock")
        };
        Ok(dir.join(leaf).to_string_lossy().into_owned())
    }
}

#[cfg(unix)]
fn unix_socket_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        let tmp = env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        tmp.join(format!(".rp-{uid}-broker-v2"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(d) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(d).join("running-process").join("broker-v2")
        } else {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/running-process-{uid}/broker-v2"))
        }
    }
}

/// Read a `Hello` frame from the peer, send back a stub `Negotiated`
/// reply, return the requested service name as evidence.
///
/// Stub semantics for slice 3d: the broker has no real service registry
/// yet, so it accepts any well-formed Hello and replies with a fixed
/// `Negotiated` carrying the v2 envelope version + the binary's package
/// version as `daemon_version`. Future slices replace this with actual
/// servicedef lookup + backend launch.
fn handle_hello<S: std::io::Read + std::io::Write>(
    stream: &mut S,
) -> Result<String, String> {
    let bytes = read_frame(stream).map_err(|e| format!("read Hello frame: {e}"))?;
    let hello = Hello::decode(bytes.as_slice()).map_err(|e| format!("decode Hello: {e}"))?;

    let reply = HelloReply {
        result: Some(hello_reply::Result::Negotiated(Negotiated {
            negotiated_protocol: ENVELOPE_VERSION as u32,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            backend_pipe: String::new(),
            warnings: Vec::new(),
            server_capabilities: 0,
            keepalive_interval_secs: 0,
            handle_passed_token: Vec::new(),
            connection_id: hello.connection_id,
        })),
    };

    let mut body = Vec::with_capacity(reply.encoded_len());
    reply
        .encode(&mut body)
        .map_err(|e| format!("encode HelloReply: {e}"))?;
    write_frame(stream, &body).map_err(|e| format!("write HelloReply frame: {e}"))?;

    Ok(hello.service_name)
}

fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        // ListenerOptions wants the bare namespaced name, not the
        // `\\.\pipe\` decorated form.
        let bare = socket_path
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(socket_path);
        bare.to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("to_ns_name: {e}"))
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("to_fs_name: {e}"))
    }
}
