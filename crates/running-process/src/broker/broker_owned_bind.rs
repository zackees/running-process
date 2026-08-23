//! Broker-owned listener handoff policy.
//!
//! Unix descriptor inheritance and validation are platform mechanics and are
//! intentionally hidden behind [`crate::platform::ipc`]. This module owns the
//! product environment contract and whether the broker elects to use it.

/// Environment variable naming the listener inherited by a daemon.
pub const INHERITED_LISTENER_FD_ENV: &str = "RUNNING_PROCESS_BROKER_LISTENER_FD";

/// Escape hatch for broker-owned bind. Set to `0` to use spawn-then-probe.
pub const LAUNCHER_OPT_IN_ENV: &str = "RUNNING_PROCESS_BROKER_OWNED_BIND";

/// Whether the launcher should bind the endpoint itself.
pub fn launcher_opt_in() -> bool {
    crate::env_vars::BROKER_OWNED_BIND.is_set()
}

/// Whether this platform can hand a bound listener to a spawned daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// The broker can bind and pass the listener.
    Supported,
    /// It cannot, and this is why.
    Unsupported { reason: &'static str },
}

impl Support {
    /// Whether broker-owned bind can be used here.
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Report the capability selected by the platform facade.
pub fn support() -> Support {
    if InheritableListener::supported() {
        Support::Supported
    } else {
        Support::Unsupported {
            reason: "a Windows named-pipe listener is a single instance that becomes the connection on accept, so there is no bound listener object to hand to a child; the spawn-then-probe path applies instead",
        }
    }
}

/// Broker-facing opaque listener-inheritance operation.
///
/// This preserves the established broker API while the selected platform
/// implementation owns descriptor/handle details.
pub struct InheritableListener {
    inner: crate::platform::ipc::InheritedListener,
}

impl InheritableListener {
    /// Bind a listener that can be passed to a spawned daemon.
    pub fn bind(endpoint: &str) -> std::io::Result<Self> {
        let endpoint = crate::platform::ipc::Endpoint::new(endpoint.to_owned())?;
        crate::platform::ipc::InheritedListener::bind(&endpoint).map(|inner| Self { inner })
    }

    /// Configure the child's inherited-listener environment contract.
    pub fn prepare(&self, command: &mut std::process::Command) -> std::io::Result<()> {
        self.inner.prepare(command, INHERITED_LISTENER_FD_ENV)
    }

    /// Release endpoint-name cleanup once the child owns the listener.
    pub fn disown_endpoint(&mut self) {
        self.inner.disown_endpoint();
    }

    /// Whether the selected platform supports listener inheritance.
    pub fn supported() -> bool {
        crate::platform::ipc::InheritedListener::supported()
    }
}

/// Recover the broker listener inherited by this process, if any.
pub fn recover_from_env() -> std::io::Result<Option<crate::platform::ipc::Listener>> {
    crate::platform::ipc::InheritedListener::recover_from_env(INHERITED_LISTENER_FD_ENV)
}

#[cfg(test)]
mod tests;
