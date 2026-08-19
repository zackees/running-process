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

// trybuild intentionally normalizes absolute input spans differently on
// Windows and Unix. Keep a snapshot for each rendering, while asserting that
// the compile-fail source itself is byte-for-byte identical across both dirs.
#[test]
fn brokered_backend_compile_fail_ui_snapshots() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let windows_fixture = manifest.join("tests/ui/brokered_backend_state_in_bind.rs");
    let unix_fixture = manifest.join("tests/ui-unix/brokered_backend_state_in_bind.rs");
    assert_eq!(
        std::fs::read(&windows_fixture).expect("read Windows trybuild fixture"),
        std::fs::read(&unix_fixture).expect("read Unix trybuild fixture"),
        "platform-specific trybuild fixtures must stay identical",
    );

    let platform_dir = if cfg!(windows) { "ui" } else { "ui-unix" };
    let pattern = format!(
        "{}/tests/{platform_dir}/brokered_backend_*.rs",
        manifest.display()
    );
    let t = trybuild::TestCases::new();
    t.compile_fail(pattern);
}
