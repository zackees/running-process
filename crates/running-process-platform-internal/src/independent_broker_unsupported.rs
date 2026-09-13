//! External broker serving is currently implemented only for Linux.
pub fn run(_endpoint: &str, _cancelled: &std::sync::atomic::AtomicBool) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

pub fn spawn(
    _spec: &crate::platform::independent_spawn::LaunchSpec,
    _endpoint: &str,
    _timeout: std::time::Duration,
    _cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<crate::IndependentChild> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}
