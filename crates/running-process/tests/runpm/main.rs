//! `runpm` CLI integration tests (#1158).
//!
//! Boot-autostart fixture generation and `runpm.toml` batch config
//! parsing. Each former top-level `tests/*.rs` file is a module here, so
//! the category links one test executable instead of two. Test IDs are
//! `runpm::<module>::<test_name>`.
//!
//! The two modules carry different feature gates (`client` and `daemon`),
//! kept as the inner `#![cfg(...)]` attribute at the top of each file.

mod runpm_boot_autostart_fixtures;
mod runpm_toml_config;
