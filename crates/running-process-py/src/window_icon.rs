//! Python bindings for the host window icon (#577).
//!
//! Thin: the capability probe and the OS calls live in `running_process`, so
//! this only translates arguments and errors.
//!
//! # The capability has to cross the boundary too
//!
//! The whole point of the Rust API is that most terminals silently ignore an
//! icon change, so support is reported rather than assumed. A binding that
//! exposed only "set it" and swallowed the verdict would hand Python callers
//! the very trap the Rust side exists to avoid — they would call it, get no
//! exception, and ship a feature that does nothing on the default terminal of
//! every recent Windows install. So `native_window_icon_support` is exposed
//! alongside, and the setter raises rather than returning quietly.

use pyo3::exceptions::{PyOSError, PyRuntimeError};
use pyo3::prelude::*;
use running_process::window_icon::{self, IconError, IconSource};

/// Why the host cannot accept an icon, or `None` when it can.
///
/// Returning the reason rather than a bare bool is deliberate: a caller that
/// only learns "no" has nothing to log, and nothing to distinguish "this
/// terminal never allows it" from "this process has no console right now".
#[pyfunction]
pub(crate) fn native_window_icon_support() -> Option<&'static str> {
    window_icon::host_icon_support().reason()
}

/// Set the host console window's icon from a `.ico` file.
///
/// Raises `RuntimeError` when the host cannot accept an icon and `OSError`
/// when the file cannot be loaded — different problems with different
/// remedies, so they are different exception types.
#[pyfunction]
pub(crate) fn native_set_window_icon_from_path(path: &str) -> PyResult<()> {
    window_icon::set_host_icon(&IconSource::Path(path.into())).map_err(to_py_error)
}

/// Which Python exception an [`IconError`] becomes.
///
/// Split from the conversion so the mapping is testable without an
/// initialized interpreter — `PyErr::is_instance_of` needs one, and a plain
/// `cargo test` has none, which would leave this decision unverified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorKind {
    /// The terminal will never accept an icon: not retryable, and not caused
    /// by the caller's input.
    Unsupported,
    /// Something about the supplied icon failed.
    Os,
}

fn classify(error: &IconError) -> ErrorKind {
    match error {
        IconError::Unsupported { .. } => ErrorKind::Unsupported,
        _ => ErrorKind::Os,
    }
}

fn to_py_error(error: IconError) -> PyErr {
    match classify(&error) {
        ErrorKind::Unsupported => PyRuntimeError::new_err(error.to_string()),
        ErrorKind::Os => PyOSError::new_err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer wherever it runs, including a CI box with no
    /// console window at all.
    #[test]
    fn support_is_reportable_everywhere() {
        match native_window_icon_support() {
            None => {} // available
            Some(reason) => assert!(!reason.is_empty(), "a refusal must explain itself"),
        }
    }

    /// The binding must agree with the Rust API it wraps, not carry its own
    /// idea of what is supported.
    #[test]
    fn the_binding_agrees_with_the_rust_verdict() {
        let rust = window_icon::host_icon_support();
        assert_eq!(native_window_icon_support(), rust.reason());
        assert_eq!(native_window_icon_support().is_none(), rust.is_available());
    }

    /// The two failure kinds must map to different exceptions, because the
    /// remedies differ: an unsupported terminal is permanent, a bad icon is
    /// the caller's to fix.
    ///
    /// Asserted on the classification rather than the built `PyErr`, so it
    /// runs without an initialized interpreter.
    #[test]
    fn an_unsupported_host_and_a_bad_icon_map_to_different_exceptions() {
        assert_eq!(
            classify(&IconError::Unsupported { reason: "no" }),
            ErrorKind::Unsupported
        );
        assert_eq!(
            classify(&IconError::Load {
                path: "x.ico".into(),
                source: std::io::Error::other("boom"),
            }),
            ErrorKind::Os
        );
    }

    /// Whatever the host, a nonexistent file never reports success.
    #[test]
    fn a_missing_file_never_reports_success() {
        assert!(
            native_set_window_icon_from_path("no-such-icon-file.ico").is_err(),
            "a missing file cannot produce a set icon"
        );
    }
}
