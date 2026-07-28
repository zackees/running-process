//! Setting the host console/terminal window icon (#577).
//!
//! # Capability-reported, never assumed
//!
//! Most terminals do not let a running program change their window icon, and
//! they do not say so — the API call succeeds and nothing happens. Windows
//! Terminal is the case that matters most: `GetConsoleWindow` returns a real
//! handle to a hidden pseudo-console window, so `WM_SETICON` succeeds against
//! a window nobody can see.
//!
//! A function that returns `Ok(())` there would be worse than one that fails:
//! the caller would ship a feature that silently does nothing on the default
//! terminal of every recent Windows install. So support is *probed* and
//! reported, and [`set_host_icon`] refuses rather than pretending.
//!
//! # What is supported
//!
//! Classic Windows console (`conhost.exe`) only, for now. Everything else
//! reports [`IconSupport::Unsupported`] with a reason. Linux/X11 is a
//! plausible later addition; macOS Terminal.app and iTerm2, Windows Terminal,
//! Wayland compositors, and most modern emulators deliberately reserve the
//! window decoration to themselves, and no in-process API changes that.

use std::path::PathBuf;

/// Where an icon comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconSource {
    /// Icon file on disk: `.ico` on Windows.
    Path(PathBuf),
}

/// Whether this process can set its host window's icon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconSupport {
    /// The host window accepts an icon.
    Available,
    /// It does not, and this is why.
    ///
    /// The reason is carried so a caller can log something an operator can
    /// act on, rather than a bare boolean that invites retrying forever.
    Unsupported {
        /// Human-readable explanation.
        reason: &'static str,
    },
}

impl IconSupport {
    /// Whether the icon can actually be set.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// The reason support is absent, if it is.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Unsupported { reason } => Some(reason),
        }
    }
}

/// Why setting an icon failed.
#[derive(Debug, thiserror::Error)]
pub enum IconError {
    /// The host cannot accept an icon at all.
    ///
    /// Distinct from an I/O failure: retrying or supplying a different file
    /// will not help, and the caller should stop asking.
    #[error("this host cannot accept a window icon: {reason}")]
    Unsupported {
        /// Why the host is unsupported.
        reason: &'static str,
    },
    /// The icon source could not be loaded.
    #[error("cannot load icon from {path}: {source}")]
    Load {
        /// Path that failed to load.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Whether this process's host window can accept an icon.
///
/// Cheap, and safe to call before deciding whether to ship an icon at all.
pub fn host_icon_support() -> IconSupport {
    imp::host_icon_support()
}

/// Set the icon on this process's host console window.
///
/// Returns [`IconError::Unsupported`] when the host does not accept icons,
/// rather than succeeding without effect.
pub fn set_host_icon(source: &IconSource) -> Result<(), IconError> {
    set_host_icon_given(host_icon_support(), source)
}

/// [`set_host_icon`] with the support verdict supplied.
///
/// Split out so the refusal path is testable on every platform without
/// depending on whether the machine running the tests happens to have a
/// console window. A test that only exercises the refusal when the ambient
/// host is unsupported silently checks nothing everywhere else.
fn set_host_icon_given(support: IconSupport, source: &IconSource) -> Result<(), IconError> {
    match support {
        IconSupport::Available => imp::set_host_icon(source),
        IconSupport::Unsupported { reason } => Err(IconError::Unsupported { reason }),
    }
}

#[cfg(windows)]
mod imp {
    use super::{IconError, IconSource, IconSupport};
    use std::os::windows::ffi::OsStrExt as _;

    use winapi::shared::windef::{HICON, HWND};
    use winapi::um::wincon::GetConsoleWindow;
    use winapi::um::winuser::{
        GetClassNameW, LoadImageW, SendMessageW, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE,
        WM_SETICON,
    };

    /// `wParam` values for `WM_SETICON`.
    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;

    /// Window class of the classic console host.
    ///
    /// This is the discriminator that matters. Windows Terminal hosts the
    /// session in a pseudo-console whose `GetConsoleWindow` handle belongs to
    /// a hidden window of a different class — `WM_SETICON` against it
    /// succeeds and changes nothing visible.
    const CONHOST_CLASS: &str = "ConsoleWindowClass";

    fn console_window() -> Option<HWND> {
        let hwnd = unsafe { GetConsoleWindow() };
        (!hwnd.is_null()).then_some(hwnd)
    }

    fn class_name(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
    }

    pub(super) fn host_icon_support() -> IconSupport {
        let Some(hwnd) = console_window() else {
            return IconSupport::Unsupported {
                reason: "this process has no console window (detached, or output is redirected \
                         from a windowless host)",
            };
        };
        if class_name(hwnd) == CONHOST_CLASS {
            return IconSupport::Available;
        }
        IconSupport::Unsupported {
            reason: "the host is not the classic console (conhost). Windows Terminal and other \
                     modern emulators own their window decoration; setting an icon would \
                     silently do nothing",
        }
    }

    pub(super) fn set_host_icon(source: &IconSource) -> Result<(), IconError> {
        let hwnd = console_window().ok_or(IconError::Unsupported {
            reason: "the console window disappeared between the support probe and the call",
        })?;

        let IconSource::Path(path) = source;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);

        // LR_DEFAULTSIZE picks the system's preferred size from a multi-image
        // .ico rather than whichever image happens to be first.
        let icon = unsafe {
            LoadImageW(
                std::ptr::null_mut(),
                wide.as_ptr(),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            )
        } as HICON;
        if icon.is_null() {
            return Err(IconError::Load {
                path: path.clone(),
                source: std::io::Error::last_os_error(),
            });
        }

        // Both slots: the small icon is the title bar and Alt+Tab, the big one
        // is the taskbar. Setting only one leaves the other stale, which looks
        // like a partial failure to a user.
        unsafe {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL, icon as isize);
            SendMessageW(hwnd, WM_SETICON, ICON_BIG, icon as isize);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{IconError, IconSource, IconSupport};

    pub(super) fn host_icon_support() -> IconSupport {
        IconSupport::Unsupported {
            reason: "setting the host window icon is implemented on Windows conhost only",
        }
    }

    pub(super) fn set_host_icon(_source: &IconSource) -> Result<(), IconError> {
        Err(IconError::Unsupported {
            reason: "setting the host window icon is implemented on Windows conhost only",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer without panicking wherever it runs — including
    /// CI, which has no console window at all.
    #[test]
    fn support_is_reportable_everywhere() {
        let support = host_icon_support();
        // Whichever answer, it must be self-describing: an unsupported result
        // without a reason would leave a caller with nothing to log.
        match &support {
            IconSupport::Available => assert_eq!(support.reason(), None),
            IconSupport::Unsupported { reason } => {
                assert!(!reason.is_empty(), "unsupported must explain itself");
                assert_eq!(support.reason(), Some(*reason));
            }
        }
    }

    #[test]
    fn availability_and_reason_are_consistent() {
        assert!(IconSupport::Available.is_available());
        assert!(IconSupport::Available.reason().is_none());

        let no = IconSupport::Unsupported { reason: "because" };
        assert!(!no.is_available());
        assert_eq!(no.reason(), Some("because"));
    }

    /// An unsupported host must refuse rather than report success.
    ///
    /// This is the whole point of the module: a caller that gets `Ok(())`
    /// would ship a feature that silently does nothing on the default
    /// terminal of every recent Windows install.
    ///
    /// The verdict is injected rather than probed, so this runs the refusal
    /// on every platform. Probing would make the test a no-op wherever the
    /// ambient host happens to be supported.
    #[test]
    fn an_unsupported_host_refuses_instead_of_pretending() {
        let error = set_host_icon_given(
            IconSupport::Unsupported {
                reason: "test verdict",
            },
            &IconSource::Path("anything.ico".into()),
        )
        .expect_err("an unsupported host must not report success");

        match error {
            IconError::Unsupported { reason } => assert_eq!(reason, "test verdict"),
            other => panic!("expected Unsupported, got {other}"),
        }
    }

    /// The refusal must not depend on the icon existing: an unsupported host
    /// is unsupported whatever it is handed.
    #[test]
    fn refusal_precedes_loading_the_icon() {
        let error = set_host_icon_given(
            IconSupport::Unsupported { reason: "nope" },
            &IconSource::Path("definitely-does-not-exist.ico".into()),
        )
        .expect_err("must refuse");
        assert!(
            matches!(error, IconError::Unsupported { .. }),
            "a missing file must not mask the unsupported verdict; got {error}"
        );
    }

    /// A missing file must be a load error, not a silent success.
    ///
    /// Reaching the load path needs a real conhost window, which a CI runner
    /// does not have. Rather than skip invisibly, the verdict is forced to
    /// `Available` so the load path runs everywhere: with no console window
    /// `imp::set_host_icon` returns `Unsupported`, and with one it returns
    /// `Load`. Both are refusals — what must never happen is `Ok`.
    #[test]
    fn a_missing_icon_file_never_reports_success() {
        let result = set_host_icon_given(
            IconSupport::Available,
            &IconSource::Path("no-such-icon-file.ico".into()),
        );
        let error = result.expect_err("a missing file cannot produce a set icon");
        assert!(
            matches!(
                error,
                IconError::Load { .. } | IconError::Unsupported { .. }
            ),
            "expected a refusal, got {error}"
        );
    }
}
