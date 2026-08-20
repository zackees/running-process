//! Public policy for setting the host console/terminal window icon (#577).
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

use running_process_platform_internal::platform::window_icon as platform_icon;

pub mod ico {
    pub use running_process_platform_internal::platform::window_icon::ico::*;
}
mod osc;

/// Where an icon comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconSource {
    /// Icon file on disk: `.ico` on Windows.
    Path(PathBuf),
    /// Raw `.ico` bytes, typically embedded in the binary with
    /// `include_bytes!` so an application ships its own icon without needing
    /// a file to exist at runtime.
    Bytes(Vec<u8>),
    /// A stock icon the OS already ships, named symbolically.
    ///
    /// Nothing to bundle and nothing to decode, which suits the cases these
    /// exist for — marking a console as a warning or an error surface.
    /// See [`StockIcon`] for the names.
    Stock(StockIcon),
}

/// A stock icon provided by the operating system.
///
/// A closed set rather than a free-form string. A name the OS does not know
/// can only fail at runtime, and a caller has no way to discover which names
/// are valid; an enum makes the answer a compile error instead. The variants
/// are the ones with a direct equivalent on every platform this could grow
/// to, so the set stays meaningful rather than becoming Windows constants
/// wearing generic names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StockIcon {
    /// The application's own default icon.
    Application,
    /// Warning: a hazard the user should notice.
    Warning,
    /// Error: something has already gone wrong.
    Error,
    /// Information: neutral notice.
    Information,
    /// Shield: an elevation or security prompt.
    Shield,
}

impl StockIcon {
    /// The symbolic name sent by the OSC 1 fallback.
    ///
    /// freedesktop icon-naming-spec names rather than this crate's variant
    /// spelling: a terminal or window manager that does anything at all with
    /// OSC 1 looks the name up in the desktop icon theme, so a bespoke name
    /// would resolve to nothing on every host.
    pub fn osc_name(self) -> &'static str {
        match self {
            Self::Application => "application-x-executable",
            Self::Warning => "dialog-warning",
            Self::Error => "dialog-error",
            Self::Information => "dialog-information",
            Self::Shield => "security-high",
        }
    }
}

/// Whether this process can set its host window's icon.
///
/// # Capability matrix
///
/// | Host | Verdict | Backend |
/// |---|---|---|
/// | Windows conhost | `Available` | `WM_SETICON` |
/// | Windows Terminal | `Degraded` | OSC 1 name only; set the profile's `icon` field for a real image |
/// | Other Windows emulators | `Degraded` | OSC 1 name only |
/// | Linux X11 | `Degraded` | OSC 1 name only (`_NET_WM_ICON` not yet implemented) |
/// | Linux Wayland | `Unsupported` | compositors do not let a client set another window's icon |
/// | macOS | `Unsupported` | the window belongs to Terminal.app / iTerm2, not to this process |
/// | No terminal | `Unsupported` | nothing to set an icon on |
///
/// An out-of-date row here is a documentation regression: callers decide
/// whether to ship an icon at all based on this table.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IconSupport {
    /// The host window accepts an icon.
    Available,
    /// The host accepts only a symbolic *name*, not an image.
    ///
    /// Reported rather than folded into `Available` because the difference is
    /// visible to the user: an OSC 1 name may be shown, ignored, or applied
    /// to something other than the window icon, and a caller told "yes" that
    /// then sees nothing change cannot tell a failure from a terminal that
    /// simply does not do icons.
    Degraded {
        /// What will actually happen, and why it is less than asked for.
        reason: &'static str,
    },
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
    /// Whether a real image icon can be set.
    ///
    /// False for [`IconSupport::Degraded`]: a caller choosing whether to embed
    /// and ship an icon file wants to know whether the file will be used, and
    /// on a degraded host it will not be.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Whether an attempt will do *something*, image or not.
    ///
    /// True for both [`IconSupport::Available`] and
    /// [`IconSupport::Degraded`] — the distinction a caller wants when
    /// deciding whether to bother calling at all, as opposed to whether to
    /// ship an image.
    pub fn is_attemptable(&self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }

    /// The reason support is absent or reduced, if it is.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Degraded { reason } | Self::Unsupported { reason } => Some(reason),
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
    /// The OS refused to build an icon from otherwise well-formed data.
    ///
    /// Removed in #720 because nothing constructed it; reinstated here
    /// because `CreateIconFromResourceEx` can fail on data this crate has
    /// already validated the shape of — the image itself may still be
    /// something the OS will not decode.
    #[error("the system refused the icon data: {0}")]
    Apply(#[source] std::io::Error),
    /// The host accepts only a symbolic name, and this source is not one.
    ///
    /// Distinct from [`IconError::Unsupported`]: the host *would* accept a
    /// stock icon, so the remedy is to pass one rather than to give up.
    #[error("this host accepts only a stock icon name, not an image file or bytes: {reason}")]
    DegradedSourceUnsupported {
        /// What the host will and will not accept.
        reason: &'static str,
    },
    /// The supplied bytes are not a usable icon.
    ///
    /// Separate from [`IconError::Load`] because the remedy differs: a bad
    /// path is fixed by pointing somewhere else, malformed bytes by fixing
    /// what was embedded.
    #[error("supplied icon data is unusable: {0}")]
    Decode(ico::IcoError),
}

/// Which window an icon operation targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconScope {
    /// This process's own host console window.
    Host,
    /// A child process's console window.
    ///
    /// Only meaningful when the child was given its own console
    /// (`CREATE_NEW_CONSOLE` on Windows). A child that inherited ours shares
    /// the same window, so targeting it changes this process's icon too —
    /// that is inherent to sharing a console, not a failure, and
    /// [`icon_support`] reports it as available because the icon really does
    /// change.
    Child {
        /// Process id of the child.
        pid: u32,
    },
}

fn platform_scope(scope: IconScope) -> platform_icon::IconScope {
    match scope {
        IconScope::Host => platform_icon::IconScope::Host,
        IconScope::Child { pid } => platform_icon::IconScope::Child { pid },
    }
}

fn platform_stock(stock: StockIcon) -> platform_icon::StockIcon {
    match stock {
        StockIcon::Application => platform_icon::StockIcon::Application,
        StockIcon::Warning => platform_icon::StockIcon::Warning,
        StockIcon::Error => platform_icon::StockIcon::Error,
        StockIcon::Information => platform_icon::StockIcon::Information,
        StockIcon::Shield => platform_icon::StockIcon::Shield,
    }
}

fn platform_source(source: &IconSource) -> platform_icon::IconSource {
    match source {
        IconSource::Path(path) => platform_icon::IconSource::Path(path.clone()),
        IconSource::Bytes(bytes) => platform_icon::IconSource::Bytes(bytes.clone()),
        IconSource::Stock(stock) => platform_icon::IconSource::Stock(platform_stock(*stock)),
    }
}

fn degraded_reason(reason: platform_icon::IconDegradedReason) -> &'static str {
    match reason {
        platform_icon::IconDegradedReason::WindowsTerminal => {
            "Windows Terminal owns its window decoration and ignores WM_SETICON. Set the `icon` field on the WT profile for a real image; a stock name can still be sent via OSC 1"
        }
        platform_icon::IconDegradedReason::NonClassicWindowsHost => {
            "the host is not the classic console (conhost). Modern emulators own their window decoration and ignore WM_SETICON; a stock name can still be sent via OSC 1"
        }
        platform_icon::IconDegradedReason::LinuxNameOnly => {
            "WINDOWID is not set, so the terminal's X window cannot be identified; a stock name can still be sent via OSC 1"
        }
    }
}

fn unsupported_reason(reason: platform_icon::IconUnsupportedReason) -> &'static str {
    match reason {
        platform_icon::IconUnsupportedReason::ChildHasNoConsole => {
            "that process has no console window of its own (it may share this one, have been created without a window, or have exited)"
        }
        platform_icon::IconUnsupportedReason::NoConsole => {
            "this process has no console window (detached, or output is redirected from a windowless host)"
        }
        platform_icon::IconUnsupportedReason::MacTerminalOwnsWindow => {
            "on macOS the window belongs to Terminal.app or iTerm2, not to this process; set the icon on the terminal application's own bundle"
        }
        platform_icon::IconUnsupportedReason::Wayland => {
            "Wayland compositors do not let a client change another window's icon; set it in the terminal emulator's .desktop file"
        }
        platform_icon::IconUnsupportedReason::NoBackend => {
            "no window-icon backend exists for this platform"
        }
        platform_icon::IconUnsupportedReason::LinuxChildScope => {
            "X11 cannot identify another process's terminal window; WINDOWID names only this process's own host"
        }
        platform_icon::IconUnsupportedReason::LinuxNoDisplay => {
            "no display server is attached (no DISPLAY or WAYLAND_DISPLAY), so there is no window to set an icon on"
        }
        platform_icon::IconUnsupportedReason::TargetDisappeared => {
            "the target window disappeared between the support probe and the call"
        }
        platform_icon::IconUnsupportedReason::UnknownImageFormat => {
            "the X11 backend accepts PNG data (or a .ico whose largest image is a PNG)"
        }
        platform_icon::IconUnsupportedReason::StockNeedsPixels => {
            "stock icons are theme names, not images; X11 needs pixels. Pass a PNG, or let the OSC 1 fallback send the name"
        }
        platform_icon::IconUnsupportedReason::OversizedIcon => {
            "icon is larger than 512x512; window managers scale down from far smaller"
        }
        platform_icon::IconUnsupportedReason::UnsupportedPngColorType => {
            "the X11 backend needs an RGB or RGBA PNG; convert palette or grayscale images first"
        }
        platform_icon::IconUnsupportedReason::UnsupportedPngBitDepth => {
            "the X11 backend needs an 8-bit PNG"
        }
        platform_icon::IconUnsupportedReason::UnsupportedX11VisualDepth => {
            "the X11 visual depth cannot represent the requested icon"
        }
    }
}

fn map_platform_error(error: platform_icon::IconError) -> IconError {
    match error {
        platform_icon::IconError::Unsupported(reason) => IconError::Unsupported {
            reason: unsupported_reason(reason),
        },
        platform_icon::IconError::Load { path, source } => IconError::Load { path, source },
        platform_icon::IconError::Apply(source) => IconError::Apply(source),
        platform_icon::IconError::Decode(source) => IconError::Decode(source),
    }
}

/// Whether a window can accept an icon.
///
/// Cheap, and safe to call before deciding whether to ship an icon at all.
pub fn icon_support(scope: IconScope) -> IconSupport {
    match platform_icon::icon_support(platform_scope(scope)) {
        platform_icon::IconSupport::Available => IconSupport::Available,
        platform_icon::IconSupport::Degraded(reason) => IconSupport::Degraded {
            reason: degraded_reason(reason),
        },
        platform_icon::IconSupport::Unsupported(reason) => IconSupport::Unsupported {
            reason: unsupported_reason(reason),
        },
    }
}

/// Whether this process's host window can accept an icon.
pub fn host_icon_support() -> IconSupport {
    icon_support(IconScope::Host)
}

/// Set the icon on this process's host console window.
///
/// Returns [`IconError::Unsupported`] when the host does not accept icons,
/// rather than succeeding without effect.
pub fn set_host_icon(source: &IconSource) -> Result<(), IconError> {
    set_icon(IconScope::Host, source)
}

/// Set the icon on the window named by `scope`.
///
/// Returns [`IconError::Unsupported`] when that window does not accept icons,
/// rather than succeeding without effect.
pub fn set_icon(scope: IconScope, source: &IconSource) -> Result<(), IconError> {
    set_icon_given(icon_support(scope), scope, source)
}

/// [`set_host_icon`] with the support verdict supplied.
///
/// Split out so the refusal path is testable on every platform without
/// depending on whether the machine running the tests happens to have a
/// console window. A test that only exercises the refusal when the ambient
/// host is unsupported silently checks nothing everywhere else.
#[cfg(test)]
fn set_host_icon_given(support: IconSupport, source: &IconSource) -> Result<(), IconError> {
    set_icon_given(support, IconScope::Host, source)
}

fn set_icon_given(
    support: IconSupport,
    scope: IconScope,
    source: &IconSource,
) -> Result<(), IconError> {
    match support {
        IconSupport::Available => {
            platform_icon::set_icon(platform_scope(scope), &platform_source(source))
                .map_err(map_platform_error)
        }
        // Only a stock name has anything to send. A file or a byte blob would
        // mean inventing a name the caller never chose, and OSC 1 carries a
        // name rather than an image.
        IconSupport::Degraded { reason } => match source {
            IconSource::Stock(icon) => osc::emit(icon.osc_name()).map_err(IconError::Apply),
            _ => Err(IconError::DegradedSourceUnsupported { reason }),
        },
        IconSupport::Unsupported { reason } => Err(IconError::Unsupported { reason }),
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
            IconSupport::Degraded { reason } | IconSupport::Unsupported { reason } => {
                assert!(!reason.is_empty(), "a reduced verdict must explain itself");
                assert_eq!(support.reason(), Some(*reason));
            }
        }
    }

    #[test]
    fn a_degraded_host_is_attemptable_but_not_available() {
        // The distinction a caller acts on: `is_available` decides whether
        // to embed and ship an icon file, `is_attemptable` decides whether
        // to bother calling at all.
        let degraded = IconSupport::Degraded {
            reason: "name only",
        };
        assert!(!degraded.is_available());
        assert!(degraded.is_attemptable());
        assert_eq!(degraded.reason(), Some("name only"));

        assert!(IconSupport::Available.is_attemptable());
        assert!(!IconSupport::Unsupported { reason: "no" }.is_attemptable());
    }

    #[test]
    fn a_degraded_host_accepts_a_stock_name_and_refuses_an_image() {
        // OSC 1 carries a name, not an image. Accepting a file here would
        // mean inventing a name the caller never chose.
        let degraded = IconSupport::Degraded {
            reason: "name only",
        };
        let refused = set_host_icon_given(
            degraded.clone(),
            &IconSource::Path(PathBuf::from("some.ico")),
        )
        .expect_err("an image must be refused on a name-only host");
        match refused {
            IconError::DegradedSourceUnsupported { reason } => {
                assert_eq!(reason, "name only");
            }
            other => panic!("expected DegradedSourceUnsupported, got {other:?}"),
        }

        // And it is distinct from Unsupported, because the remedy differs:
        // pass a stock icon rather than give up.
        let unsupported = set_host_icon_given(
            IconSupport::Unsupported {
                reason: "none at all",
            },
            &IconSource::Stock(StockIcon::Shield),
        )
        .expect_err("an unsupported host refuses everything");
        assert!(matches!(unsupported, IconError::Unsupported { .. }));
    }

    #[test]
    fn every_stock_icon_maps_to_a_freedesktop_name() {
        // A bespoke name would resolve to nothing in any desktop icon
        // theme, which is the only place an OSC 1 name gets looked up.
        for icon in [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ] {
            let name = icon.osc_name();
            assert!(!name.is_empty(), "{icon:?} has no OSC name");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{icon:?} -> {name:?} is not a freedesktop-style name"
            );
        }
    }

    #[test]
    fn stock_names_are_distinct() {
        // Two icons sharing a name would silently show the wrong one.
        let names: std::collections::BTreeSet<&str> = [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ]
        .into_iter()
        .map(StockIcon::osc_name)
        .collect();
        assert_eq!(names.len(), 5);
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

    /// A pid that owns no console window must be refused, with a reason.
    ///
    /// Platform-neutral: off Windows the whole feature is unavailable and
    /// says so, which is a different sentence but the same contract. An
    /// earlier version asserted the Windows wording here and failed on the
    /// musl and coverage lanes — the assertion was Windows-specific while the
    /// test was not.
    #[test]
    fn a_process_with_no_console_window_is_unsupported() {
        // pid 0 is the system idle process and never owns a console window,
        // so this is stable across machines and needs no fixture.
        let support = icon_support(IconScope::Child { pid: 0 });
        assert!(!support.is_available());
        assert!(
            !support.reason().expect("must explain itself").is_empty(),
            "an unsupported result must carry a usable reason"
        );
    }

    /// Looking up our OWN pid must find the same window the host scope does.
    ///
    /// This is the deterministic test of the pid lookup: no spawning, no
    /// waiting, no session-wide state. Whenever this process has a console
    /// window, `Child { pid: self }` names that very window, so the two
    /// scopes must agree — and a broken `console_window_of_pid` makes them
    /// disagree immediately.
    ///
    /// Where there is no console window both are unsupported, which is also
    /// agreement, so the assertion holds on every machine.
    #[test]
    fn own_pid_resolves_to_the_host_console_window() {
        let host = icon_support(IconScope::Host);
        let own = icon_support(IconScope::Child {
            pid: std::process::id(),
        });
        assert_eq!(
            host.is_available(),
            own.is_available(),
            "host scope says {host:?} but our own pid says {own:?}; the pid lookup              disagrees with the direct console-window lookup"
        );
    }

    /// And the setter refuses rather than silently doing nothing.
    #[test]
    fn setting_a_childless_pid_is_an_error() {
        let error = set_icon(
            IconScope::Child { pid: 0 },
            &IconSource::Stock(StockIcon::Warning),
        )
        .expect_err("a pid with no console cannot take an icon");
        assert!(
            matches!(error, IconError::Unsupported { .. }),
            "expected Unsupported, got {error}"
        );
    }

    /// An exited process cannot be targeted either — same answer, so a caller
    /// does not have to distinguish "never had one" from "gone".
    #[test]
    fn an_implausible_pid_is_unsupported() {
        let support = icon_support(IconScope::Child { pid: u32::MAX });
        assert!(!support.is_available());
    }

    /// Host scope must keep answering exactly as before: the scope-aware
    /// entry point is a generalisation, not a behaviour change.
    #[test]
    fn host_scope_agrees_with_the_host_specific_helper() {
        assert_eq!(icon_support(IconScope::Host), host_icon_support());
    }

    #[test]
    fn scopes_are_distinguishable() {
        assert_ne!(IconScope::Host, IconScope::Child { pid: 1 });
        assert_ne!(IconScope::Child { pid: 1 }, IconScope::Child { pid: 2 });
        assert_eq!(IconScope::Child { pid: 7 }, IconScope::Child { pid: 7 });
    }

    /// A stock icon needs no data, so the only thing that can go wrong is
    /// the host — never a decode.
    ///
    /// Runs everywhere by forcing the verdict, so the enum-to-OS mapping is
    /// exercised on platforms with no console window at all.
    #[test]
    fn every_stock_icon_is_requestable() {
        for stock in [
            StockIcon::Application,
            StockIcon::Warning,
            StockIcon::Error,
            StockIcon::Information,
            StockIcon::Shield,
        ] {
            let result = set_host_icon_given(IconSupport::Available, &IconSource::Stock(stock));
            match result {
                // On a host with a real console window the icon is set.
                Ok(()) => {}
                // Without one, the refusal comes from the window lookup — not
                // from the icon, which is the point: a stock icon is never a
                // decode failure.
                Err(IconError::Unsupported { .. }) => {}
                Err(other) => panic!("{stock:?} failed for a reason other than the host: {other}"),
            }
        }
    }

    /// A stock request must never be reported as bad data.
    #[test]
    fn a_stock_icon_is_never_a_decode_error() {
        let result = set_host_icon_given(
            IconSupport::Available,
            &IconSource::Stock(StockIcon::Warning),
        );
        if let Err(error) = result {
            assert!(
                !matches!(error, IconError::Decode(_)),
                "a stock icon carries no data to decode, got {error}"
            );
        }
    }

    /// Distinct variants must not collapse onto one another.
    #[test]
    fn stock_variants_are_distinguishable() {
        assert_ne!(StockIcon::Warning, StockIcon::Error);
        assert_ne!(StockIcon::Application, StockIcon::Shield);
        assert_eq!(StockIcon::Information, StockIcon::Information);
    }

    /// Malformed bytes must be refused before the OS sees them.
    ///
    /// Runs everywhere by forcing the verdict, because the decode happens
    /// before any window is touched — so this covers the validation on
    /// platforms that have no console window at all.
    #[test]
    fn malformed_icon_bytes_are_refused() {
        let result =
            set_host_icon_given(IconSupport::Available, &IconSource::Bytes(vec![0xFF; 64]));
        let error = result.expect_err("garbage is not an icon");
        assert!(
            matches!(error, IconError::Decode(_) | IconError::Unsupported { .. }),
            "expected a refusal before the OS was handed anything, got {error}"
        );
    }

    #[test]
    fn empty_icon_bytes_are_refused() {
        let error = set_host_icon_given(IconSupport::Available, &IconSource::Bytes(Vec::new()))
            .expect_err("empty data is not an icon");
        assert!(
            matches!(error, IconError::Decode(_) | IconError::Unsupported { .. }),
            "got {error}"
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
