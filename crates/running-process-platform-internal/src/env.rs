//! The mechanism for reading declared environment variables (#1101).
//!
//! This is *how* a variable is read -- the kinds, the owner, the typed
//! accessors and the boolean parsers -- with no variable names in it. It lives
//! here, below `running-process`, so every crate in the workspace can share
//! one parser instead of growing its own; `running_process::env_vars`
//! re-exports it and keeps the policy: which variables exist, and what each
//! means.
//!
//! # Why booleans get two accessors rather than one
//!
//! "Is this switch on?" has two defensible answers when the value is neither
//! clearly on nor clearly off, and which one is right depends on who owns the
//! variable -- not on the call site, which is how a codebase ends up with five
//! parsers that disagree.
//!
//! - [`flag_owned`] is for switches the reader defines. Unknown means **off**.
//! - [`flag_foreign`] is for values written by someone else. Unknown means
//!   **on**, so a stray `=0` cannot exempt a process from being reaped.
//! - [`flag_opt_out`] is for an escape hatch that is on until someone turns it
//!   off. Unset means **on**.
//!
//! All three trim and lowercase before comparing, so `" True "` and `"TRUE"`
//! agree.

use std::ffi::OsStr;

/// What kind of value a variable carries, and how it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvKind {
    /// A switch the reader defines. Unknown values are off; see [`flag_owned`].
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
    ///
    /// `zero_selects_default` records what `0` means for *this* variable,
    /// because it is not the same answer everywhere. A connect timeout of zero
    /// makes every connection fail instantly and is never what anyone wants,
    /// so zero falls back to the default. A drain timeout of zero means "do
    /// not wait", which is a perfectly reasonable thing to ask for, so zero is
    /// honoured. Leaving this unstated is how the two ended up parsed
    /// differently by accident.
    Number { zero_selects_default: bool },
}

/// Who decides what values a variable may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Defined by the reader; the value space is ours.
    Crate,
    /// Set by a supervising process or a test harness; we only read it.
    Foreign,
}

/// One environment variable the reader reads.
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
    /// in the reader, caught by `an_unset_flag_matches_its_declared_default`,
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

impl EnvVar {
    /// Read this variable as a count, falling back to `default`.
    ///
    /// A value that is not a number is not a smaller number: it is a mistake,
    /// and the default is a better answer than a silently-wrong one.
    pub fn count_or(&self, default: usize) -> usize {
        self.parsed::<usize>().unwrap_or(default)
    }

    /// Read this variable as a millisecond duration, falling back to `default`.
    pub fn millis_or(&self, default: std::time::Duration) -> std::time::Duration {
        self.parsed::<u64>()
            .map(std::time::Duration::from_millis)
            .unwrap_or(default)
    }

    /// Read this variable as a port number, if it names one.
    pub fn port(&self) -> Option<u16> {
        self.parsed::<u16>()
    }

    /// Whether the variable is present at all, whatever its value -- including
    /// empty or `0`.
    ///
    /// Several older switches are read this way, and narrowing them to
    /// recognised spellings would turn `=0` from "on" into "off" for anyone who
    /// already relies on it. It is a method rather than an [`EnvKind`] variant
    /// because `EnvKind` is public and exhaustive: adding a variant would break
    /// a downstream `match` in a 4.x release.
    pub fn is_present(&self) -> bool {
        std::env::var_os(self.name).is_some()
    }

    /// Read this variable as text, if it is set to anything.
    pub fn text(&self) -> Option<String> {
        std::env::var(self.name)
            .ok()
            .filter(|value| !value.is_empty())
    }

    /// Read this variable exactly as the host wrote it.
    ///
    /// Unlike [`EnvVar::path`] an empty value is still `Some`: some readers
    /// have always treated `VAR=` as set, and this keeps them doing so.
    pub fn os(&self) -> Option<std::ffi::OsString> {
        std::env::var_os(self.name)
    }

    /// Read this variable as Unicode exactly as written, empty included.
    ///
    /// `None` when unset *or* not valid Unicode, matching `std::env::var`.
    pub fn string(&self) -> Option<String> {
        std::env::var(self.name).ok()
    }

    /// Read this variable as a path, if it is set to anything.
    ///
    /// Takes the value as the host wrote it: a path that is not valid Unicode
    /// is still a path, and lossily repairing it would point somewhere else.
    pub fn path(&self) -> Option<std::path::PathBuf> {
        std::env::var_os(self.name)
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// Parse the value, applying this variable's declared rule for zero.
    ///
    /// # Panics
    /// If the variable is not declared as a number. A programming error in
    /// the reader, not something the environment can cause.
    fn parsed<T>(&self) -> Option<T>
    where
        T: std::str::FromStr + Default + PartialEq,
    {
        let EnvKind::Number {
            zero_selects_default,
        } = self.kind
        else {
            panic!("{} is declared as {:?}, not a number", self.name, self.kind);
        };
        let parsed: T = std::env::var(self.name)
            .ok()?
            .trim()
            .parse()
            .ok()
            .filter(|value: &T| !(zero_selects_default && *value == T::default()))?;
        Some(parsed)
    }
}

/// Spellings that turn an owned switch on. Anything else, including an
/// unrecognised value, leaves it off.
const AFFIRMATIVE: &[&str] = &["1", "true", "yes", "on"];

/// Spellings that turn a foreign switch off. Anything else, including an
/// unrecognised value, leaves it on.
const NEGATIVE: &[&str] = &["", "0", "false", "no", "off"];

/// Read a switch the reader owns: on only for a recognised affirmative.
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

/// Read a variable whose *name* is caller-supplied data, exactly as the host
/// wrote it.
///
/// For the few readers that are handed a name rather than choosing one -- a
/// descriptor-passing key agreed with a parent, an embedder's disclosure
/// allowlist. There is nothing to declare for those: the variable belongs to
/// whoever supplied the name. Every other read goes through a declared
/// [`EnvVar`]; the `running_process_env_direct` Dylint lint rejects a string
/// literal passed here, so this cannot become a way around declaring one.
pub fn os_named(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

/// [`os_named`] as Unicode: `None` when unset *or* not valid Unicode,
/// matching `std::env::var`.
pub fn string_named(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Whether a *value already in hand* reads as a foreign switch being on.
///
/// Callers that scan another process's environment block have the value
/// without being able to read it from their own environment.
pub fn value_is_affirmative_foreign(value: &str) -> bool {
    !NEGATIVE.contains(&value.trim().to_ascii_lowercase().as_str())
}

/// Declare environment variables as `EnvVar` constants plus a table of all of
/// them, so a crate's inventory is one list rather than scattered literals.
///
/// ```ignore
/// declare_env_vars! {
///     /// The table's doc.
///     pub const TABLE;
///     HOME => "HOME", EnvKind::Path, Owner::Foreign, "unset", "Home dir.";
/// }
/// ```
#[macro_export]
macro_rules! declare_env_vars {
    (
        $(#[$table_meta:meta])*
        pub const $table:ident;
        $($ident:ident => $name:literal, $kind:expr, $owner:expr, $default:literal, $summary:literal;)*
    ) => {
        $(
            #[doc = $summary]
            ///
            #[doc = concat!("Environment variable `", $name, "`. Unset: ", $default, ".")]
            pub const $ident: $crate::env::EnvVar = $crate::env::EnvVar {
                name: $name,
                kind: $kind,
                owner: $owner,
                default: $default,
                summary: $summary,
            };
        )*

        $(#[$table_meta])*
        pub const $table: &[$crate::env::EnvVar] = &[$($ident),*];
    };
}

fn normalize(value: &OsStr) -> String {
    value.to_string_lossy().trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests mutate the real process environment, so two of them setting
    /// the same variable at once would read each other's value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_var<T>(name: &str, value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os(name);
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        let outcome = body();
        match previous {
            Some(previous) => std::env::set_var(name, previous),
            None => std::env::remove_var(name),
        }
        outcome
    }

    const PROBE: &str = "PLATFORM_INTERNAL_ENV_MECHANISM_PROBE";

    fn var(kind: EnvKind) -> EnvVar {
        EnvVar {
            name: PROBE,
            kind,
            owner: Owner::Crate,
            default: "unset",
            summary: "test probe",
        }
    }

    #[test]
    fn the_three_flag_readers_disagree_only_on_unrecognised_values_and_unset() {
        for (value, owned, foreign, opt_out) in [
            (Some("1"), true, true, true),
            (Some(" TRUE "), true, true, true),
            (Some("0"), false, false, false),
            (Some("off"), false, false, false),
            (Some("maybe"), false, true, true),
            (None, false, false, true),
        ] {
            assert_eq!(
                with_var(PROBE, value, || flag_owned(PROBE)),
                owned,
                "{value:?}"
            );
            assert_eq!(
                with_var(PROBE, value, || flag_foreign(PROBE)),
                foreign,
                "{value:?}"
            );
            assert_eq!(
                with_var(PROBE, value, || flag_opt_out(PROBE)),
                opt_out,
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_declared_flag_reads_through_its_own_semantics() {
        assert!(with_var(PROBE, Some("yes"), || var(EnvKind::OwnedFlag).is_set()));
        assert!(!with_var(PROBE, None, || var(EnvKind::OwnedFlag).is_set()));
        assert!(with_var(PROBE, None, || var(EnvKind::OptOutFlag).is_set()));
        assert!(with_var(PROBE, Some("exactly"), || {
            var(EnvKind::ExactValue("exactly")).is_set()
        }));
        assert!(!with_var(PROBE, Some("Exactly"), || {
            var(EnvKind::ExactValue("exactly")).is_set()
        }));
    }

    #[test]
    fn numbers_honour_the_declared_meaning_of_zero_and_reject_garbage() {
        let selects = var(EnvKind::Number {
            zero_selects_default: true,
        });
        let honours = var(EnvKind::Number {
            zero_selects_default: false,
        });
        assert_eq!(with_var(PROBE, Some("0"), || selects.count_or(9)), 9);
        assert_eq!(with_var(PROBE, Some("0"), || honours.count_or(9)), 0);
        assert_eq!(with_var(PROBE, Some(" 42 "), || honours.count_or(9)), 42);
        assert_eq!(with_var(PROBE, Some("many"), || honours.count_or(9)), 9);
        assert_eq!(with_var(PROBE, Some("8080"), || honours.port()), Some(8080));
        assert_eq!(
            with_var(PROBE, Some("250"), || {
                honours.millis_or(std::time::Duration::from_secs(1))
            }),
            std::time::Duration::from_millis(250)
        );
    }

    #[test]
    fn text_and_path_treat_empty_as_absent_and_keep_the_value_verbatim() {
        let text = var(EnvKind::Text);
        assert_eq!(with_var(PROBE, Some(""), || text.text()), None);
        assert_eq!(
            with_var(PROBE, Some("a b"), || text.text()),
            Some("a b".into())
        );
        assert_eq!(with_var(PROBE, Some(""), || text.path()), None);
        assert_eq!(
            with_var(PROBE, Some("/x/y"), || text.path()),
            Some(std::path::PathBuf::from("/x/y"))
        );
    }

    #[test]
    fn os_and_string_keep_an_empty_value_as_set() {
        let text = var(EnvKind::Text);
        assert_eq!(
            with_var(PROBE, Some(""), || text.os()),
            Some(std::ffi::OsString::new())
        );
        assert_eq!(
            with_var(PROBE, Some(""), || text.string()),
            Some(String::new())
        );
        assert_eq!(with_var(PROBE, None, || text.os()), None);
        assert_eq!(with_var(PROBE, None, || text.string()), None);
    }

    #[test]
    fn a_presence_switch_is_on_for_any_value_even_empty_or_zero() {
        let presence = var(EnvKind::Text);
        for value in ["", "0", "off", "1"] {
            assert!(
                with_var(PROBE, Some(value), || presence.is_present()),
                "{value:?}"
            );
        }
        assert!(!with_var(PROBE, None, || presence.is_present()));
    }

    #[test]
    #[should_panic(expected = "not a flag")]
    fn reading_a_non_flag_as_a_flag_is_a_programming_error() {
        with_var(PROBE, Some("1"), || var(EnvKind::Text).is_set());
    }

    #[test]
    fn a_value_already_in_hand_reads_like_the_foreign_flag() {
        for value in ["1", "true", "maybe"] {
            assert!(value_is_affirmative_foreign(value), "{value}");
        }
        for value in ["", "0", " OFF "] {
            assert!(!value_is_affirmative_foreign(value), "{value}");
        }
    }
}
