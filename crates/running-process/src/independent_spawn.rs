//! Opt-in independent launching for persistent build/cache daemons.
//!
//! Enable the `independent-spawn` feature explicitly. Ordinary process APIs
//! remain available without it or an external launch authority. The canonical
//! [`crate::SpawnOptions`] default is inherited placement and kill-on-drop.
//!
//! # Resource placement is not lifetime
//!
//! [`crate::SpawnMode::Independent`] requires verified placement outside the
//! requester's worker cgroup subtree or restrictive Windows Job Object.
//! Enclosing user, container and machine limits still apply. It is neither
//! privilege elevation nor a container escape. `setsid`, terminal detachment,
//! and double-forking alone do not remove Linux cgroup membership.
//!
//! [`crate::SpawnLifetime::Detached`] means dropping a successfully returned
//! handle leaves the target running. `KillOnDrop` stops it on normal handle
//! destruction; it is not a promise about abrupt requester termination.
//! Dropping a handle before a launch commits is never a detached success.
//!
//! # Persistent daemon through a native scheduler
//!
//! Build the matching helper from this checkout with:
//!
//! ```text
//! soldr cargo build -p running-process --no-default-features --features independent-spawn --bin running-process-launcher
//! ```
//!
//! Provision the helper in a stable location readable by the same user. Linux
//! requires an accessible systemd user manager; Windows requires a permitted
//! same-user Task Scheduler session. No permanent startup registration is made.
//! Use native absolute paths on each OS; these illustrative paths are Linux.
//! Create the owner-private working/log directory before making this request.
//! The daemon in this example must write exactly `ready` to its marker after
//! application initialization. The marker must not already exist.
//!
//! ```no_run
//! use running_process::{
//!     spawn_with_options, IndependentBackend, SpawnLifetime, SpawnMode, SpawnOptions,
//! };
//! use running_process::independent_spawn::{LaunchSpec, Readiness};
//! use std::{io, path::PathBuf, sync::atomic::AtomicBool, time::Duration};
//!
//! # fn main() -> io::Result<()> {
//! let directory = PathBuf::from("/absolute/private/cache-daemon");
//! let marker = directory.join("ready");
//! let spec = LaunchSpec {
//!     program: "/absolute/path/cache-daemon".into(),
//!     args: vec!["--ready-file".into(), marker.clone().into_os_string()],
//!     cwd: directory.clone().into_os_string(),
//!     // Complete selected environment, not additions to inherited variables.
//!     environment: vec![("CACHE_DIRECTORY".into(), directory.clone().into_os_string())],
//!     stdout: Some(directory.join("stdout.log").into_os_string()),
//!     stderr: Some(directory.join("stderr.log").into_os_string()),
//!     readiness: Readiness::File { path: marker.into_os_string(), value: b"ready".to_vec() },
//! };
//! let options = SpawnOptions {
//!     mode: SpawnMode::Independent,
//!     lifetime: SpawnLifetime::Detached,
//!     backend: Some(IndependentBackend::NativeScheduler {
//!         launcher: "/absolute/path/running-process-launcher".into(),
//!     }),
//!     timeout: Duration::from_secs(10),
//! };
//! let cancelled = AtomicBool::new(false);
//! let handle = spawn_with_options(&spec, &options, &cancelled)?;
//! assert_eq!(handle.actual_mode(), SpawnMode::Independent);
//! // Keep the handle to stop/wait later, or detach after verified readiness.
//! drop(handle);
//! # Ok(())
//! # }
//! ```
//!
//! The application owns its daemon endpoint and existing payload protocol.
//! Independent launching does not provide singleton discovery or deduplicate
//! separate successful requests. Coordinate those through the application's
//! existing ownership mechanism; do not treat a scheduling acknowledgement as
//! application readiness. The caller owns marker cleanup between launches.
//!
//! # Docker without systemd
//!
//! Provision a broker **before** constrained workers, for example from the
//! container entrypoint: `running-process-launcher --broker /private/broker/s`.
//! Its endpoint must have an owner-private parent directory. Select
//! `IndependentBackend::ExternalBroker { endpoint: "/private/broker/s".into() }`
//! instead of `NativeScheduler` above. The broker must already be outside the
//! worker subtree, in the same cgroup/mount namespaces. It never relocates
//! itself, and the client never starts one lazily. An absent or in-worker
//! broker cannot satisfy independence. Container-wide OOM can kill either the
//! broker or target. Privileged Docker is used only by our disposable cgroup
//! test harness, not required by the public broker launch API.
//!
//! # Control, errors, and limits
//!
//! - `id()` is informational. Retain the reuse-safe handle for `stop`,
//!   `is_alive`, and bounded `wait`; do not signal a saved numeric PID later.
//! - Independent exit observation has no parent-child numeric exit status:
//!   [`crate::SpawnExit::code`] is `None`. Direct inherited children expose it.
//! - Scheduling and readiness share a nonzero budget of at most 30 seconds.
//!   Cancellation returns `Interrupted`; deadline expiry returns `TimedOut`.
//!   Runtime authority failures can return `Unsupported` or `PermissionDenied`.
//!   No independent request silently falls back to inherited placement.
//! - The payload carries argv, cwd, and the complete selected environment.
//!   Logging accepts regular files, not caller-owned pipes, sockets, or console
//!   handles. Unsupported file types fail instead of implicitly inheriting them.
//! - Linux broker sessions are bounded at 32, including live committed targets.
//!   Overload closes the excess connection. If broker control is lost, `stop`
//!   attempts to kill the pinned target and reports the error; descendant
//!   cleanup still depends on the broker remaining responsive.
//! - Native schedulers are implemented on Linux and Windows. External broker
//!   placement is implemented on Linux. Other combinations return an explicit
//!   unsupported error; inherited spawning remains available.

pub use running_process_platform_internal::platform::independent_spawn::{
    run_broker, run_launcher, spawn, IndependentChild, LaunchSpec, Readiness,
};
