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

/// Declare a crate's own variables with the shared mechanism; see
/// `running_process_platform_internal::declare_env_vars`. Re-exported so a
/// crate built on `running-process` can keep its own table without depending
/// on the platform layer directly.
pub use running_process_platform_internal::declare_env_vars;
pub use running_process_platform_internal::env::{
    flag_foreign, flag_opt_out, flag_owned, os_named, string_named, value_is_affirmative_foreign,
    EnvKind, EnvVar, Owner,
};
/// The variables `running-process-platform-internal` declares and reads,
/// including the ones this crate reads too. See [`all_declared`] for the
/// combined inventory.
pub use running_process_platform_internal::env_vars as platform;

/// Every environment variable read by this crate or by the platform layer it
/// builds on, once each, sorted by name.
///
/// With the `probe` feature this also lists the probe crate's own reads (its
/// crash spool and report directories, its crash-handler opt-out), because
/// such a build links that crate. The probe daemon and the symbolization
/// worker are separate processes that declare their own reads.
///
/// [`DECLARED`] lists what `running-process` itself reads. A process that
/// links `running-process` also runs `running-process-platform-internal`,
/// whose reads ([`platform::DECLARED_PLATFORM`]) include variables this crate
/// never touches directly -- `HOME`, `DISPLAY`, the ConPTY switches. An
/// embedder scrubbing a child's environment needs both, so this is the one
/// list to check.
///
/// A name read by both crates has one declaration, owned by the lower crate
/// and referred to from [`DECLARED`], so it appears here once;
/// `the_combined_inventory_is_sorted_unique_and_documented` holds that.
pub fn all_declared() -> Vec<EnvVar> {
    let mut all: Vec<EnvVar> = DECLARED
        .iter()
        .chain(platform::DECLARED_PLATFORM)
        .copied()
        .collect();
    // The probe crate sits below this one only behind its feature; its table
    // joins the inventory exactly when its code does.
    #[cfg(feature = "probe")]
    all.extend_from_slice(running_process_probe::env_vars::DECLARED_PROBE);
    all.sort_by(|left, right| left.name.cmp(right.name));
    all.dedup_by(|left, right| left.name == right.name);
    all
}

/// Declares this crate's variables and builds [`DECLARED`] from them.
///
/// An entry is either a full declaration, or `IDENT => use PATH;` for a
/// variable owned by a crate below this one: the constant is re-exported
/// under the same name and listed in [`DECLARED`] where it sorts, so each name
/// has exactly one declaration however many crates read it.
macro_rules! declare {
    (@collect [$($all:ident)*]) => {
        /// Every environment variable this crate reads.
        ///
        /// Kept in the same order as the declarations above, which
        /// `declarations_are_sorted_and_unique` holds to alphabetical so a
        /// reader can find a name without searching. [`all_declared`] adds the
        /// variables only the platform layer reads.
        pub const DECLARED: &[EnvVar] = &[$($all),*];
    };
    (@collect [$($all:ident)*] $ident:ident => use $($path:ident)::+; $($rest:tt)*) => {
        #[doc = concat!(
            "Declared by the platform layer, which reads it too: [`",
            stringify!($($path)::+),
            "`]."
        )]
        pub const $ident: EnvVar = $($path)::+;
        declare!(@collect [$($all)* $ident] $($rest)*);
    };
    (@collect [$($all:ident)*]
        $ident:ident => $name:literal, $kind:expr, $owner:expr, $default:literal, $summary:literal;
        $($rest:tt)*
    ) => {
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
        declare!(@collect [$($all)* $ident] $($rest)*);
    };
    ($($rest:tt)*) => {
        declare!(@collect [] $($rest)*);
    };
}

declare! {
    GITHUB_ACTIONS => "GITHUB_ACTIONS",
        EnvKind::ForeignFlag, Owner::Foreign, "not running under GitHub Actions",
        "Set by GitHub Actions; tests wait longer for a shared runner.";
    INVOCATION_ID => "INVOCATION_ID",
        EnvKind::Text, Owner::Foreign, "not started by systemd",
        "Set by systemd for a unit invocation; identifies the launching unit.";
    LOCALAPPDATA => use running_process_platform_internal::env_vars::LOCALAPPDATA;
    PATH => use running_process_platform_internal::env_vars::PATH;
    BROKER_ALLOW_PRIVILEGED => "RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED",
        EnvKind::ExactValue("1"), Owner::Crate, "privileged startup is refused",
        "Opt out of the broker's refusal to start as root or LocalSystem.";
    BROKER_CLIENT_TIMEOUT_MS => "RUNNING_PROCESS_BROKER_CLIENT_TIMEOUT_MS",
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "the built-in client timeout",
        "Broker client request timeout, in milliseconds.";
    BROKER_CRASH_DUMP_DIR => "RUNNING_PROCESS_BROKER_CRASH_DUMP_DIR",
        EnvKind::Path, Owner::Crate, "the standard diagnostic-artifact location",
        "Where broker crash dumps are written.";
    BROKER_HELLO_PERF_GUARD => "RUNNING_PROCESS_BROKER_HELLO_PERF_GUARD",
        EnvKind::OwnedFlag, Owner::Crate, "the guard does not run",
        "Run the broker Hello latency guard.";
    BROKER_HELLO_TIMEOUT_MS => "RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS",
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "the built-in Hello timeout",
        "Broker Hello handshake timeout, in milliseconds.";
    BROKER_HTTP_BIND => "RUNNING_PROCESS_BROKER_HTTP_BIND",
        EnvKind::Text, Owner::Crate, "the loopback bind address",
        "Bind address for the broker HTTP aggregator.";
    BROKER_HTTP_PORT => "RUNNING_PROCESS_BROKER_HTTP_PORT",
        EnvKind::Number { zero_selects_default: false }, Owner::Crate, "an ephemeral port",
        "Port for the broker HTTP aggregator.";
    BROKER_LISTENER_FD => "RUNNING_PROCESS_BROKER_LISTENER_FD",
        EnvKind::Number { zero_selects_default: false }, Owner::Foreign, "the daemon binds its own endpoint",
        "Descriptor of a listening socket the broker already bound and passed.";
    BROKER_MAX_INFLIGHT_HANDLERS => "RUNNING_PROCESS_BROKER_MAX_INFLIGHT_HANDLERS",
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "the built-in concurrency cap",
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
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "the built-in connect timeout",
        "Daemon client connect timeout, in milliseconds.";
    CLIENT_RPC_TIMEOUT_MS => "RUNNING_PROCESS_CLIENT_RPC_TIMEOUT_MS",
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "the built-in RPC timeout",
        "Daemon client RPC timeout, in milliseconds.";
    DAEMON_IDENTITY_STAMP => "RUNNING_PROCESS_DAEMON_IDENTITY_STAMP",
        EnvKind::Text, Owner::Crate, "a dev-scope daemon computes it from its own executable",
        "Dev-scope daemon identity stamp, `<version>-<16 hex of the executable's blake3>`; ignored outside dev scope.";
    DAEMON_SCOPE => "RUNNING_PROCESS_DAEMON_SCOPE",
        EnvKind::Text, Owner::Crate, "the user-wide scope",
        "Daemon scope selector; `dev` gives a CWD-scoped daemon for tests.";
    DAEMON_SHADOWED => "RUNNING_PROCESS_DAEMON_SHADOWED",
        EnvKind::OwnedFlag, Owner::Crate, "a dev-build daemon relocates itself",
        "Marks a daemon already running from its shadow copy.";
    DAEMON_START_TIMEOUT_MS => "RUNNING_PROCESS_DAEMON_START_TIMEOUT_MS",
        EnvKind::Number { zero_selects_default: true }, Owner::Crate, "the built-in 750ms budget",
        "How long a client waits for a freshly spawned daemon to bind its socket, in milliseconds.";
    DISABLE => "RUNNING_PROCESS_DISABLE",
        EnvKind::ExactValue("1"), Owner::Crate, "the broker is used",
        "Canonical escape hatch: bypass the broker entirely.";
    FAKE_BACKEND => "RUNNING_PROCESS_FAKE_BACKEND",
        EnvKind::Path, Owner::Foreign, "backends are reached through the broker",
        "TEST-ONLY: dial this endpoint directly, skipping broker negotiation.";
    IS_DAEMON => "RUNNING_PROCESS_IS_DAEMON",
        EnvKind::ForeignFlag, Owner::Crate, "the process is not a daemon",
        "Marks a process spawned as a daemon, for originator reaping.";
    KILL_DRAIN_TIMEOUT_MS => use running_process_platform_internal::env_vars::KILL_DRAIN_TIMEOUT_MS;
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
    TMPDIR => use running_process_platform_internal::env_vars::TMPDIR;
    USERNAME => "USERNAME",
        EnvKind::Text, Owner::Foreign, "the endpoint is named `unknown`",
        "Windows account name, mixed into the daemon pipe name.";
    XDG_CONFIG_HOME => use running_process_platform_internal::env_vars::XDG_CONFIG_HOME;
    XDG_DATA_HOME => use running_process_platform_internal::env_vars::XDG_DATA_HOME;
    XDG_RUNTIME_DIR => use running_process_platform_internal::env_vars::XDG_RUNTIME_DIR;
}

#[cfg(test)]
#[path = "tests/env_vars.rs"]
mod tests;
