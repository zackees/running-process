//! Slice 33 of #500: trybuild UI assertions for the `BrokeredBackend`
//! trait shape (#497).
//!
//! Each `tests/ui/brokered_backend_*.rs` file documents one misuse
//! pattern the trait is supposed to prevent (e.g. smuggling state into
//! `bind`). trybuild compiles each file and matches the actual rustc
//! stderr against the recorded `*.stderr` snapshot.
//!
//! These tests are inherently sensitive to rustc diagnostic wording.
//! If the diagnostics change across toolchain updates, re-run with
//! `TRYBUILD=overwrite soldr cargo nextest run -p running-process --features
//! client --test brokered_backend_ui` to refresh the snapshots, then audit
//! the diff in code review.
//!
//! The harness only runs on the `client` feature (which gates the
//! `broker` module). Skipped on builds that drop the feature.

#![cfg(feature = "client")]

// rustc renders the input span two ways. Behind soldr's rustc wrapper
// (every `soldr cargo` build, local or CI), zccache's worktree path remap
// makes it print the workspace-relative `./crates/...:line:col` form on
// every host, recorded under `ui/`. Without
// the wrapper, trybuild normalizes the span to `tests/<dir>/...`, recorded
// per host under `ui-unix/`, `ui-macos/`, and `ui-windows/`. Keep a fixture
// for each rendering, while asserting that the compile-fail source itself is
// identical.
#[test]
fn brokered_backend_compile_fail_ui_snapshots() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures = ["ui", "ui-unix", "ui-macos", "ui-windows"].map(|directory| {
        manifest.join(format!(
            "tests/{directory}/brokered_backend_state_in_bind.rs"
        ))
    });
    for fixture in fixtures.iter().skip(1) {
        assert_eq!(
            std::fs::read(&fixtures[0]).expect("read primary trybuild fixture"),
            std::fs::read(fixture).expect("read platform trybuild fixture"),
            "platform-specific trybuild fixtures must stay identical",
        );
    }

    // `soldr cargo` exports the wrapper it installed; trybuild's nested cargo
    // inherits it, along with zccache's worktree path remap.
    let soldr_wrapped = std::env::var_os("SOLDR_EFFECTIVE_RUSTC_WRAPPER").is_some();
    let platform_dir = if soldr_wrapped {
        "ui"
    } else if cfg!(target_os = "macos") {
        "ui-macos"
    } else if cfg!(windows) && std::env::var_os("CI").is_some() {
        "ui-windows"
    } else if cfg!(windows) {
        "ui"
    } else {
        "ui-unix"
    };
    let pattern = format!(
        "{}/tests/{platform_dir}/brokered_backend_*.rs",
        manifest.display()
    );
    let t = trybuild::TestCases::new();
    t.compile_fail(pattern);
}
