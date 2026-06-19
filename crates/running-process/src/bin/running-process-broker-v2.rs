//! v2 broker binary scaffold (slice 3a of #483).
//!
//! Intentionally minimal: prints a version banner and exits 0. This slice
//! only proves the binary is cargo-visible so subsequent slices can add the
//! pipe acceptor (3b), Hello handler (3c), and beyond without rearranging
//! the crate layout. There is no functionality here yet.
//!
//! See [zackees/running-process#483](https://github.com/zackees/running-process/issues/483)
//! for the full feature plan and the v2 broker design at
//! [#470](https://github.com/zackees/running-process/issues/470).

fn main() {
    println!(
        "running-process-broker-v2 {} (slice 3a scaffold; see issue #483)",
        env!("CARGO_PKG_VERSION")
    );
}
