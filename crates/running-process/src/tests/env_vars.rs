use super::*;

/// A serialised guard: these tests mutate the real process environment, and
/// two of them setting the same variable at once would read each other's value.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_var<T>(name: &str, value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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

const PROBE: &str = "RUNNING_PROCESS_ENV_VARS_PROBE";

/// An owned switch is on only for a spelling we recognise. This is the
/// direction that matters: a typo in a switch we define must not enable it.
#[test]
fn an_owned_flag_is_on_only_for_a_recognised_affirmative() {
    for on in ["1", "true", "TRUE", " True ", "yes", "on"] {
        assert!(
            with_var(PROBE, Some(on), || flag_owned(PROBE)),
            "{on:?} must turn an owned switch on"
        );
    }
    for off in ["", "0", "false", "no", "off", "OFF", " 0 "] {
        assert!(
            !with_var(PROBE, Some(off), || flag_owned(PROBE)),
            "{off:?} must leave an owned switch off"
        );
    }
    for unknown in ["maybe", "2", "y", "enabled", "tru"] {
        assert!(
            !with_var(PROBE, Some(unknown), || flag_owned(PROBE)),
            "{unknown:?} is not a spelling we defined, so the switch stays off"
        );
    }
    assert!(!with_var(PROBE, None, || flag_owned(PROBE)), "unset is off");
}

/// A foreign switch is off only for a spelling we recognise as falsy. The
/// unknown case flips, because we do not own what the writer may have meant.
#[test]
fn a_foreign_flag_is_off_only_for_a_recognised_negative() {
    for off in ["", "0", "false", "no", "off", " OFF "] {
        assert!(
            !with_var(PROBE, Some(off), || flag_foreign(PROBE)),
            "{off:?} must turn a foreign switch off"
        );
    }
    for on in ["1", "true", "yes", "on", "maybe", "2", "enabled"] {
        assert!(
            with_var(PROBE, Some(on), || flag_foreign(PROBE)),
            "{on:?} is not a falsy spelling, so the switch reads as on"
        );
    }
}

/// Absence is not a value. Reading unset as "on" would make every process
/// claim every marker it does not carry.
#[test]
fn an_unset_foreign_flag_is_off() {
    assert!(!with_var(PROBE, None, || flag_foreign(PROBE)));
}

/// The two semantics disagree exactly where they are meant to, and nowhere
/// else. If this ever passes trivially the distinction has collapsed.
#[test]
fn the_two_flag_semantics_differ_only_on_unrecognised_values() {
    for agreed in ["1", "true", "yes", "on", "0", "false", "no", "off", ""] {
        assert_eq!(
            with_var(PROBE, Some(agreed), || flag_owned(PROBE)),
            with_var(PROBE, Some(agreed), || flag_foreign(PROBE)),
            "{agreed:?} is a recognised spelling; both semantics must agree"
        );
    }
    for disputed in ["maybe", "2", "enabled"] {
        assert!(
            !with_var(PROBE, Some(disputed), || flag_owned(PROBE)),
            "owned: unknown is off"
        );
        assert!(
            with_var(PROBE, Some(disputed), || flag_foreign(PROBE)),
            "foreign: unknown is on"
        );
    }
}

/// An exact-value guard honours one spelling and refuses every other,
/// including plausible ones -- that refusal is the point.
#[test]
fn an_exact_value_guard_refuses_plausible_misspellings() {
    assert!(with_var(BROKER_ALLOW_PRIVILEGED.name, Some("1"), || {
        BROKER_ALLOW_PRIVILEGED.is_set()
    }));
    for refused in ["true", "yes", "on", "TRUE", " 1 "] {
        assert!(
            !with_var(BROKER_ALLOW_PRIVILEGED.name, Some(refused), || {
                BROKER_ALLOW_PRIVILEGED.is_set()
            }),
            "{refused:?} must not open a privilege guard"
        );
    }
}

/// Scanning another process's environment applies the same foreign rule as
/// reading our own, so a marker means the same thing seen from either side.
#[test]
fn scanning_a_value_agrees_with_reading_the_variable() {
    for value in ["1", "true", "maybe", "0", "false", "off", ""] {
        assert_eq!(
            value_is_affirmative_foreign(value),
            with_var(PROBE, Some(value), || flag_foreign(PROBE)),
            "{value:?} must read the same scanned as it does read directly"
        );
    }
}

/// The table is the inventory an embedder reads, so it must be findable and
/// must not say the same thing twice.
#[test]
fn declarations_are_sorted_and_unique() {
    let names: Vec<&str> = DECLARED.iter().map(|var| var.name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "declarations must stay alphabetical");
    let mut unique = sorted.clone();
    unique.dedup();
    assert_eq!(unique, sorted, "a variable must be declared once");
}

/// Every declaration says what happens when the variable is unset and what it
/// is for. A blank field makes the inventory useless to the reader it exists
/// for.
#[test]
fn declarations_are_documented() {
    for var in DECLARED {
        assert!(!var.name.is_empty());
        assert!(
            !var.default.is_empty(),
            "{} has no documented default",
            var.name
        );
        assert!(!var.summary.is_empty(), "{} has no summary", var.name);
        assert!(
            var.name.starts_with("RUNNING_PROCESS_"),
            "{} is not one of ours",
            var.name
        );
    }
}

/// The check that keeps the inventory true: every `RUNNING_PROCESS_*` literal
/// in the crate's sources is declared here.
///
/// Without this, the table is a snapshot that silently rots the first time
/// someone adds a variable -- which is the state this module was written to
/// end. It reads the sources rather than the built binary because a string
/// literal is what a future author will write.
#[test]
fn declaration_table_covers_every_variable() {
    let declared: std::collections::BTreeSet<&str> = DECLARED.iter().map(|var| var.name).collect();
    let mut undeclared: std::collections::BTreeSet<String> = Default::default();

    for path in source_files() {
        let text = std::fs::read_to_string(&path).expect("read source file");
        for found in literal_env_names(&text) {
            if !declared.contains(found.as_str()) {
                undeclared.insert(format!("{found} (in {})", path.display()));
            }
        }
    }

    assert!(
        undeclared.is_empty(),
        "these variables are read but not declared in DECLARED:\n  {}",
        undeclared.into_iter().collect::<Vec<_>>().join("\n  ")
    );
}

/// Names this crate uses for something other than reading an environment
/// variable, and so has nothing to declare for.
const NOT_ENVIRONMENT_READS: &[&str] = &[
    // Test fixtures that assert on environment *materialization* -- they are
    // written into a child's environment, never read from ours.
    "RUNNING_PROCESS_TEST_CLIENT_ONLY_ENV",
    "RUNNING_PROCESS_TEST_DAEMON_ONLY_ENV",
    "RUNNING_PROCESS_MATERIALIZE_CANARY",
    "RUNNING_PROCESS_BASELINE_CANARY",
    // This module's own probe variable.
    "RUNNING_PROCESS_ENV_VARS_PROBE",
];

const PREFIX: &str = "RUNNING_PROCESS_";

fn literal_env_names(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    let opening = format!("\"{PREFIX}");
    while let Some(start) = text[index..].find(&opening) {
        let open = index + start + 1;
        let Some(len) = bytes[open..].iter().position(|byte| *byte == b'"') else {
            break;
        };
        let name = &text[open..open + len];
        // The bare prefix appears in `starts_with` assertions and in string
        // building; it names no variable, so it is not one to declare.
        let is_a_name = name.len() > PREFIX.len()
            && name
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
        if is_a_name && !NOT_ENVIRONMENT_READS.contains(&name) {
            found.push(name.to_owned());
        }
        index = open + len;
    }
    found
}

fn source_files() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    assert!(!files.is_empty(), "the crate must have sources to scan");
    files
}

/// A declaration says what happens when the variable is unset; the parser
/// decides what actually happens. They must be the same thing.
///
/// This test exists because they were not: `BROKER_OWNED_BIND` is documented as
/// on by default and was first declared `ForeignFlag`, whose unset case is off.
/// Nothing else in the suite would have noticed -- the switch is only exercised
/// when a test sets it -- so the default would have silently inverted.
#[test]
fn an_unset_flag_matches_its_declared_default() {
    for var in DECLARED {
        let declared_on = match var.kind {
            EnvKind::OwnedFlag | EnvKind::ForeignFlag | EnvKind::OptOutFlag => {
                describes_an_enabled_default(var.default)
            }
            EnvKind::ExactValue(_) => describes_an_enabled_default(var.default),
            _ => continue,
        };
        let actually_on = with_var(var.name, None, || var.is_set());
        assert_eq!(
            actually_on,
            declared_on,
            "{}: declared default {:?} says {}, but reading it unset gives {}",
            var.name,
            var.default,
            if declared_on { "on" } else { "off" },
            actually_on
        );
    }
}

/// Read a declared default line as on or off.
///
/// The defaults are prose, because an embedder reads them; this maps the two
/// shapes actually in use rather than pretending prose is a boolean. A default
/// that matches neither shape fails loudly instead of defaulting to "off",
/// which would let a mis-worded line pass the check above vacuously.
fn describes_an_enabled_default(default: &str) -> bool {
    match default {
        "broker-owned bind is used" => true,
        "privileged startup is refused"
        | "the broker is used"
        | "processes are tracked"
        | "the guard does not run"
        | "the process is not a daemon"
        | "a dev-build daemon relocates itself" => false,
        other => panic!(
            "default {other:?} is not a phrasing this check recognises; \
             add it rather than letting the assertion pass vacuously"
        ),
    }
}

/// What `0` means is a property of the variable, and every numeric variable
/// must have decided it.
///
/// This is where a real bug lived: three millisecond parsers guarded against
/// zero and two did not, so `CLIENT_CONNECT_TIMEOUT_MS=0` produced a
/// zero-duration connect timeout -- every connection failing instantly --
/// while the same `0` on the RPC timeout fell back to the default. Neither
/// behaviour was written down, so neither was wrong on purpose.
#[test]
fn zero_means_what_each_numeric_variable_declares() {
    let mut checked = 0;
    for var in DECLARED {
        let EnvKind::Number {
            zero_selects_default,
        } = var.kind
        else {
            continue;
        };
        checked += 1;
        let sentinel = std::time::Duration::from_millis(9_999);
        let read = with_var(var.name, Some("0"), || var.millis_or(sentinel));
        if zero_selects_default {
            assert_eq!(
                read, sentinel,
                "{}: zero must fall back to the caller's default",
                var.name
            );
        } else {
            assert_eq!(
                read,
                std::time::Duration::ZERO,
                "{}: zero must be honoured as zero",
                var.name
            );
        }
    }
    assert!(checked >= 8, "expected the numeric variables to be checked");
}

/// A value that is not a number is a mistake, not a smaller number. Every
/// numeric variable falls back rather than guessing.
#[test]
fn an_unparseable_number_falls_back_to_the_default() {
    let sentinel = std::time::Duration::from_millis(4_242);
    for var in DECLARED {
        if !matches!(var.kind, EnvKind::Number { .. }) {
            continue;
        }
        for junk in ["", "  ", "abc", "12ms", "-1", "1.5"] {
            assert_eq!(
                with_var(var.name, Some(junk), || var.millis_or(sentinel)),
                sentinel,
                "{}: {junk:?} is not a number and must not be read as one",
                var.name
            );
        }
    }
}

/// Whitespace around a number is a formatting accident, not a different value.
#[test]
fn a_number_is_read_through_surrounding_whitespace() {
    let sentinel = std::time::Duration::from_millis(1);
    assert_eq!(
        with_var(CLIENT_RPC_TIMEOUT_MS.name, Some(" 250 "), || {
            CLIENT_RPC_TIMEOUT_MS.millis_or(sentinel)
        }),
        std::time::Duration::from_millis(250)
    );
}

/// A path is taken as the host wrote it. Repairing invalid Unicode would point
/// somewhere else, and an empty value names no path at all.
#[test]
fn a_path_is_taken_verbatim_and_an_empty_one_is_absent() {
    assert_eq!(
        with_var(MANIFEST_DIR.name, Some("/tmp/manifests"), || MANIFEST_DIR
            .path()),
        Some(std::path::PathBuf::from("/tmp/manifests"))
    );
    assert_eq!(
        with_var(MANIFEST_DIR.name, Some(""), || MANIFEST_DIR.path()),
        None
    );
    assert_eq!(
        with_var(MANIFEST_DIR.name, None, || MANIFEST_DIR.path()),
        None
    );
}

/// Text reads the same way: set-but-empty is not a value.
#[test]
fn empty_text_is_absent() {
    assert_eq!(
        with_var(DAEMON_SCOPE.name, Some("dev"), || DAEMON_SCOPE.text()),
        Some("dev".to_owned())
    );
    assert_eq!(
        with_var(DAEMON_SCOPE.name, Some(""), || DAEMON_SCOPE.text()),
        None
    );
}
