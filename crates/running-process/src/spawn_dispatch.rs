//! Canonical policy dispatch; OS placement and payload handling stay private.
use crate::{IndependentBackend, SpawnLifetime, SpawnMode, SpawnOptions};
use running_process_platform_internal::platform::independent_spawn::{
    spawn, spawn_inherited, IndependentChild, InheritedChild, LaunchSpec,
};
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

enum Child {
    Inherited(InheritedChild),
    Independent(IndependentChild),
}

/// A reuse-safe process control handle. The reported mode is actual enforcement.
///
/// On Unix, a directly spawned child's terminal status remains waitable until
/// this handle is dropped, preserving its process-group identity for control.
/// The application must leave child reaping to this handle: do not reap its PID
/// with an external waiter or install automatic SIGCHLD reaping while it is held.
pub struct SpawnHandle {
    child: Child,
    lifetime: SpawnLifetime,
}

/// Exit observation is not necessarily a parent-child exit status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnExit {
    /// Available for a directly spawned child; scheduler-owned exit observation
    /// deliberately does not invent a numeric exit status.
    pub code: Option<i32>,
}

impl SpawnHandle {
    pub fn id(&self) -> u32 {
        match &self.child {
            Child::Inherited(c) => c.id(),
            Child::Independent(c) => c.id(),
        }
    }
    pub fn actual_mode(&self) -> SpawnMode {
        match &self.child {
            Child::Inherited(_) => SpawnMode::Inherited,
            Child::Independent(_) => SpawnMode::Independent,
        }
    }
    pub fn is_alive(&mut self) -> io::Result<bool> {
        match &mut self.child {
            Child::Inherited(c) => c.try_wait().map(|exit| exit.is_none()),
            Child::Independent(c) => Ok(c.is_alive()),
        }
    }
    pub fn stop(&mut self, timeout: Duration) -> io::Result<()> {
        match &mut self.child {
            Child::Inherited(c) => c.stop(timeout),
            Child::Independent(c) => c.stop(timeout),
        }
    }
    pub fn wait(&mut self, timeout: Duration, cancelled: &AtomicBool) -> io::Result<SpawnExit> {
        match &mut self.child {
            Child::Inherited(c) => c
                .wait(timeout, cancelled)
                .map(|code| SpawnExit { code: Some(code) }),
            Child::Independent(c) => {
                c.wait(timeout, cancelled)?;
                Ok(SpawnExit { code: None })
            }
        }
    }
}
impl Drop for SpawnHandle {
    fn drop(&mut self) {
        if self.lifetime == SpawnLifetime::KillOnDrop {
            let _ = self.stop(Duration::from_secs(2));
        }
    }
}

/// Spawn with explicit resource placement, lifetime, payload and deadline.
/// This operation never silently falls back or lazily starts a broker.
pub fn spawn_with_options(
    spec: &LaunchSpec,
    options: &SpawnOptions,
    cancelled: &AtomicBool,
) -> io::Result<SpawnHandle> {
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::from(io::ErrorKind::Interrupted));
    }
    if options.timeout.is_zero() || options.timeout > Duration::from_secs(30) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let child = match (options.mode, &options.backend) {
        (SpawnMode::Inherited, None) => Child::Inherited(spawn_inherited(
            spec,
            options.lifetime == SpawnLifetime::Detached,
            options.timeout,
            cancelled,
        )?),
        (SpawnMode::Inherited, Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Inherited cannot select an independent backend",
            ))
        }
        (SpawnMode::Independent, Some(IndependentBackend::NativeScheduler { launcher })) => {
            Child::Independent(spawn(spec, launcher, options.timeout, cancelled)?)
        }
        (SpawnMode::Independent, None) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Independent requires an explicit launch authority",
            ))
        }
        (SpawnMode::Independent, Some(IndependentBackend::ExternalBroker { .. })) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "external broker launch is not implemented yet",
            ))
        }
    };
    Ok(SpawnHandle {
        child,
        lifetime: options.lifetime,
    })
}
