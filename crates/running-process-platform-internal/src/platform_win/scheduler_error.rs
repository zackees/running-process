//! Classification of the Windows scheduler control host's exit status.
use std::io;

pub(crate) fn result(code: i32, operation: &str) -> io::Result<()> {
    match code {
        0 => Ok(()),
        3 => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        4 => Err(io::Error::from(io::ErrorKind::Unsupported)),
        5 => Err(io::Error::from(io::ErrorKind::NotFound)),
        // SCHED_E_USER_NOT_LOGGED_ON: InteractiveToken cannot run without
        // the requested user's existing session. Never fall back to inheritance.
        _ if code as u32 == 0x80041320 => Err(io::Error::from(io::ErrorKind::Unsupported)),
        _ => Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Task Scheduler {operation} failed (HRESULT 0x{:08x})",
                code as u32
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_control_results_remain_distinct() {
        assert!(result(0, "run").is_ok());
        for (code, kind) in [
            (3, io::ErrorKind::PermissionDenied),
            (4, io::ErrorKind::Unsupported),
            (5, io::ErrorKind::NotFound),
            (6, io::ErrorKind::Other),
        ] {
            assert_eq!(result(code, "run").unwrap_err().kind(), kind);
        }
    }

    #[test]
    fn unavailable_interactive_session_is_unsupported() {
        // SCHED_E_USER_NOT_LOGGED_ON, returned by the COM control host.
        assert_eq!(
            result(0x80041320_u32 as i32, "run").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
