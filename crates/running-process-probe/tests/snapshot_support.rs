//! Pins `snapshot::capture_supported()` to the support matrix callers used to
//! restate themselves (#975): a capture backend for the OS and a `framehop`
//! unwinder for the architecture.

#[test]
fn capture_supported_matches_the_backend_and_unwinder_matrix() {
    let expected = cfg!(all(
        any(windows, target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    ));
    assert_eq!(
        running_process_probe::snapshot::capture_supported(),
        expected
    );
}
