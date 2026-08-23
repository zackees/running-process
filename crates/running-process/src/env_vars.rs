//! Every environment variable this crate reads, declared in one place.
//!
//! An environment variable is an interface. Other repositories embed this
//! crate -- soldr vendors it -- and have to reason about what it reads, which
//! until now meant grepping every call site. [`DECLARED`] is that list, and
//! `declaration_table_covers_every_variable` keeps it honest: a new
//! `RUNNING_PROCESS_*` literal anywhere in the crate fails the build unless it
//! is declared here.
//!
//! # Why booleans get two accessors rather than one
//!
//! "Is this switch on?" has two defensible answers when the value is neither
//! clearly on nor clearly off, and which one is right depends on who owns the
//! variable -- not on the call site, which is how a codebase ends up with five
//! parsers that disagree.
//!
//! - [`flag_owned`] is for switches this crate defines. Unknown means **off**.
//!   The value space is ours, so anything outside it is a typo, and a typo in
//!   `SOMETHING_DISABLE` must not disable something.
//! - [`flag_foreign`] is for values written by someone else, where absence of a
//!   recognised falsy spelling is better read as "set". Unknown means **on**.
//!   The daemon marker is this kind: a process that says it is a daemon in a
//!   spelling we did not anticipate is still a daemon, and a stray `=0` must
//!   never exempt it from reaping.
//! - [`flag_opt_out`] is for an escape hatch that is on until someone turns it
//!   off. Unset means **on**, which is the whole difference from the other two.
//!
//! Both trim and lowercase before comparing, so `" True "` and `"TRUE"` agree.
//!
//! # The table and the parser must agree
//!
//! Writing the table turned up a switch whose declared default and whose
//! parser disagreed -- `BROKER_OWNED_BIND` is documented as on by default but
//! was first declared with semantics that read unset as off. That is the class
//! of bug this module exists to end, so
//! `an_unset_flag_matches_its_declared_default` now checks the two against
//! each other for every declared flag.

use std::ffi::OsStr;

/// What kind of value a variable carries, and how it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvKind {
    /// A switch this crate defines. Unknown values are off; see [`flag_owned`].
    OwnedFlag,
    /// A switch whose value space belongs to someone else. Unknown values are
    /// on; see [`flag_foreign`].
    ForeignFlag,
    /// An escape hatch that is on unless explicitly turned off. Unset is on;
    /// see [`flag_opt_out`].
    OptOutFlag,
    /// A switch that is on for exactly one spelling and off for every other,
    /// including plausible ones. Reserved for guards where honouring a
    /// misspelling would be the dangerous direction.
    ExactValue(&'static str),
    /// A filesystem path.
    Path,
    /// Free text -- a name, scope, endpoint, or token.
    Text,
    /// A number: a count, a timeout in milliseconds, a port, a descriptor.
    Number,
}

/// Who decides what values a variable may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Defined by this crate; the value space is ours.
    Crate,
    /// Set by a supervising process or a test harness; we only read it.
    Foreign,
}

/// One environment variable this crate reads.
#[derive(Debug, Clone, Copy)]
pub struct EnvVar {
    /// The variable name as it appears in the environment.
    pub name: &'static str,
    /// What the value means and how it is parsed.
    pub kind: EnvKind,
    /// Who owns the value space.
    pub owner: Owner,
    /// What happens when the variable is unset.
    pub default: &'static str,
    /// One line an embedder can read to know whether they care.
    pub summary: &'static str,
}

impl EnvVar {
    /// Read this variable as a boolean, using the semantics it declares.
    ///
    /// # Panics
    /// If the variable is not declared as a flag. That is a programming error
    /// in this crate, caught by `an_unset_flag_matches_its_declared_default`,
    /// not something a value in the environment can cause.
    pub fn is_set(&self) -> bool {
        match self.kind {
            EnvKind::OwnedFlag => flag_owned(self.name),
            EnvKind::ForeignFlag => flag_foreign(self.name),
            EnvKind::OptOutFlag => flag_opt_out(self.name),
            EnvKind::ExactValue(expected) => {
                std::env::var_os(self.name).is_some_and(|value| value == OsStr::new(expected))
            }
            other => panic!("{} is declared as {other:?}, not a flag", self.name),
        }
    }
}

/// Spellings that turn an owned switch on. Anything else, including an
/// unrecognised value, leaves it off.
const AFFIRMATIVE: &[&str] = &["1", "true", "yes", "on"];

/// Spellings that turn a foreign switch off. Anything else, including an
/// unrecognised value, leaves it on.
const NEGATIVE: &[&str] = &["", "0", "false", "no", "off"];

/// Read a switch this crate owns: on only for a recognised affirmative.
pub fn flag_owned(name: &str) -> bool {
    match std::env::var_os(name) {
        Some(value) => AFFIRMATIVE.contains(&normalize(&value).as_str()),
        None => false,
    }
}

/// Read a switch someone else writes: off only for a recognised negative.
///
/// Unset is still off -- absence is not a value, and reading it as "on" would
/// make every process claim every marker.
pub fn flag_foreign(name: &str) -> bool {
    match std::env::var_os(name) {
        Some(value) => !NEGATIVE.contains(&normalize(&value).as_str()),
        None => false,
    }
}

/// Read an escape hatch that is on unless turned off.
///
/// Unset is *on*, which is what separates this from [`flag_foreign`]: the
/// caller is asking whether the default behaviour still applies, and it does
/// until someone says otherwise. Every recognised falsy spelling opens the
/// hatch, so a user who reaches for `=false` or `=off` gets the fallback they
/// were plainly asking for rather than silently keeping the default.
pub fn flag_opt_out(name: &str) -> bool {
    match std::env::var_os(name) {
        Some(value) => !NEGATIVE.contains(&normalize(&value).as_str()),
        None => true,
    }
}

/// Whether a *value already in hand* reads as a foreign switch being on.
///
/// Callers that scan another process's environment block have the value
/// without being able to read it from their own environment.
pub fn value_is_affirmative_foreign(value: &str) -> bool {
    !NEGATIVE.contains(&value.trim().to_ascii_lowercase().as_str())
}

fn normalize(value: &OsStr) -> String {
    value.to_string_lossy().trim().to_ascii_lowercase()
}

macro_rules! declare {
    ($($ident:ident => $name:literal, $kind:expr, $owner:expr, $default:literal, $summary:literal;)*) => {
        $(
            #[doc = $summary]
            ///
            #[doc = concat!("Environment variable `", $name, "`. Unset: ", $default, ".")]
            pub const $ident: EnvVar = EnvVar {
                name: $name,
                kind: $kind,
                owner: $owner,
                default: $default,
                summary: $summary,
            };
        )*

        /// Every environment variable this crate reads.
        ///
        /// Kept in the same order as the declarations above, which
        /// `declarations_are_sorted_and_unique` holds to alphabetical so a
        /// reader can find a name without searching.
        pub const DECLARED: &[EnvVar] = &[$($ident),*];
    };
}

declare! {
    BROKER_ALLOW_PRIVILEGED => "RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED",
        EnvKind::ExactValue("1"), Owner::Crate, "privileged startup is refused",
        "Opt out of the broker's refusal to start as root or LocalSystem.";
    BROKER_CLIENT_TIMEOUT_MS => "RUNNING_PROCESS_BROKER_CLIENT_TIMEOUT_MS",
        EnvKind::Number, Owner::Crate, "the built-in client timeout",
        "Broker client request timeout, in milliseconds.";
    BROKER_CRASH_DUMP_DIR => "RUNNING_PROCESS_BROKER_CRASH_DUMP_DIR",
        EnvKind::Path, Owner::Crate, "the standard diagnostic-artifact location",
        "Where broker crash dumps are written.";
    BROKER_HELLO_PERF_GUARD => "RUNNING_PROCESS_BROKER_HELLO_PERF_GUARD",
        EnvKind::OwnedFlag, Owner::Crate, "the guard does not run",
        "Run the broker Hello latency guard.";
    BROKER_HELLO_TIMEOUT_MS => "RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS",
        EnvKind::Number, Owner::Crate, "the built-in Hello timeout",
        "Broker Hello handshake timeout, in milliseconds.";
    BROKER_HTTP_BIND => "RUNNING_PROCESS_BROKER_HTTP_BIND",
        EnvKind::Text, Owner::Crate, "the loopback bind address",
        "Bind address for the broker HTTP aggregator.";
    BROKER_HTTP_PORT => "RUNNING_PROCESS_BROKER_HTTP_PORT",
        EnvKind::Number, Owner::Crate, "an ephemeral port",
        "Port for the broker HTTP aggregator.";
    BROKER_LISTENER_FD => "RUNNING_PROCESS_BROKER_LISTENER_FD",
        EnvKind::Number, Owner::Foreign, "the daemon binds its own endpoint",
        "Descriptor of a listening socket the broker already bound and passed.";
    BROKER_MAX_INFLIGHT_HANDLERS => "RUNNING_PROCESS_BROKER_MAX_INFLIGHT_HANDLERS",
        EnvKind::Number, Owner::Crate, "the built-in concurrency cap",
        "Maximum broker request handlers running at once.";
    BROKER_OWNED_BIND => "RUNNING_PROCESS_BROKER_OWNED_BIND",
        EnvKind::OptOutFlag, Owner::Crate, "broker-owned bind is used",
        "Escape hatch: set falsy to fall back to spawn-then-probe.";
    BROKER_V1_BACKEND_NAMESPACE => "RUNNING_PROCESS_BROKER_V1_BACKEND_NAMESPACE",
        EnvKind::Text, Owner::Foreign, "no namespace is applied",
        "Backend namespace handed to a v1 broker backend.";
    BROKER_V1_BACKEND_PIPE => "RUNNING_PROCESS_BROKER_V1_BACKEND_PIPE",
        EnvKind::Text, Owner::Foreign, "the backend derives its own endpoint",
        "Endpoint a v1 broker backend should serve on.";
    BROKER_V1_INSTANCE => "RUNNING_PROCESS_BROKER_V1_INSTANCE",
        EnvKind::Text, Owner::Foreign, "the default instance",
        "Instance identifier for a v1 broker backend.";
    BROKER_V1_SERVICE_NAME => "RUNNING_PROCESS_BROKER_V1_SERVICE_NAME",
        EnvKind::Text, Owner::Foreign, "the backend supplies its own name",
        "Service name a v1 broker backend registers under.";
    BROKER_V1_SERVICE_VERSION => "RUNNING_PROCESS_BROKER_V1_SERVICE_VERSION",
        EnvKind::Text, Owner::Foreign, "the backend supplies its own version",
        "Service version a v1 broker backend reports.";
    BROKER_V1_SESSION_TOKEN => "RUNNING_PROCESS_BROKER_V1_SESSION_TOKEN",
        EnvKind::Text, Owner::Foreign, "no session token is presented",
        "Session token a v1 broker backend presents to the broker.";
    BROKER_V1_SOCKET => "RUNNING_PROCESS_BROKER_V1_SOCKET",
        EnvKind::Text, Owner::Foreign, "the standard broker endpoint",
        "Broker endpoint a v1 backend dials.";
    BROKER_V1_TRACEPARENT => "RUNNING_PROCESS_BROKER_V1_TRACEPARENT",
        EnvKind::Text, Owner::Foreign, "no trace context is propagated",
        "W3C traceparent propagated into a v1 broker backend.";
    BROKER_V1_TRACESTATE => "RUNNING_PROCESS_BROKER_V1_TRACESTATE",
        EnvKind::Text, Owner::Foreign, "no trace state is propagated",
        "W3C tracestate propagated into a v1 broker backend.";
    CHILD_PID_LOG_PATH => "RUNNING_PROCESS_CHILD_PID_LOG_PATH",
        EnvKind::Path, Owner::Foreign, "spawned child PIDs are not logged",
        "Append each spawned child PID to this file (test harness seam).";
    CLIENT_CONNECT_TIMEOUT_MS => "RUNNING_PROCESS_CLIENT_CONNECT_TIMEOUT_MS",
        EnvKind::Number, Owner::Crate, "the built-in connect timeout",
        "Daemon client connect timeout, in milliseconds.";
    CLIENT_RPC_TIMEOUT_MS => "RUNNING_PROCESS_CLIENT_RPC_TIMEOUT_MS",
        EnvKind::Number, Owner::Crate, "the built-in RPC timeout",
        "Daemon client RPC timeout, in milliseconds.";
    DAEMON_SCOPE => "RUNNING_PROCESS_DAEMON_SCOPE",
        EnvKind::Text, Owner::Crate, "the user-wide scope",
        "Daemon scope selector; `dev` gives a CWD-scoped daemon for tests.";
    DAEMON_SHADOWED => "RUNNING_PROCESS_DAEMON_SHADOWED",
        EnvKind::OwnedFlag, Owner::Crate, "a dev-build daemon relocates itself",
        "Marks a daemon already running from its shadow copy.";
    DISABLE => "RUNNING_PROCESS_DISABLE",
        EnvKind::ExactValue("1"), Owner::Crate, "the broker is used",
        "Canonical escape hatch: bypass the broker entirely.";
    FAKE_BACKEND => "RUNNING_PROCESS_FAKE_BACKEND",
        EnvKind::Path, Owner::Foreign, "backends are reached through the broker",
        "TEST-ONLY: dial this endpoint directly, skipping broker negotiation.";
    IS_DAEMON => "RUNNING_PROCESS_IS_DAEMON",
        EnvKind::ForeignFlag, Owner::Crate, "the process is not a daemon",
        "Marks a process spawned as a daemon, for originator reaping.";
    KILL_DRAIN_TIMEOUT_MS => "RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS",
        EnvKind::Number, Owner::Crate, "two seconds",
        "How long `kill()` waits for output capture to drain, in milliseconds.";
    MANIFEST_DIR => "RUNNING_PROCESS_MANIFEST_DIR",
        EnvKind::Path, Owner::Foreign, "the standard manifest location",
        "Where broker cache manifests are read and written.";
    NO_TRACKING => "RUNNING_PROCESS_NO_TRACKING",
        EnvKind::OwnedFlag, Owner::Crate, "processes are tracked",
        "Disable daemon IPC and process tracking.";
    ORIGINATOR => "RUNNING_PROCESS_ORIGINATOR",
        EnvKind::Text, Owner::Foreign, "the originator is inferred",
        "Identifies the process that originated a spawn tree.";
    SERVICE_DEF_DIR => "RUNNING_PROCESS_SERVICE_DEF_DIR",
        EnvKind::Path, Owner::Foreign, "the standard service-definition location",
        "Where service definitions are read from.";
}

#[cfg(test)]
#[path = "tests/env_vars.rs"]
mod tests;
