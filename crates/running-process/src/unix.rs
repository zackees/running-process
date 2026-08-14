#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignal {
    Interrupt,
    Terminate,
    Kill,
}

fn kind(
    signal: UnixSignal,
) -> running_process_platform_internal::platform::process::UnixSignalKind {
    use running_process_platform_internal::platform::process::UnixSignalKind;
    match signal {
        UnixSignal::Interrupt => UnixSignalKind::Interrupt,
        UnixSignal::Terminate => UnixSignalKind::Terminate,
        UnixSignal::Kill => UnixSignalKind::Kill,
    }
}

pub fn unix_set_priority(pid: u32, nice: i32) -> Result<(), std::io::Error> {
    running_process_platform_internal::platform::process::unix_set_priority(pid, nice)
}
pub fn unix_signal_process(pid: u32, signal: UnixSignal) -> Result<(), std::io::Error> {
    running_process_platform_internal::platform::process::unix_signal_process(pid, kind(signal))
}
pub fn unix_signal_process_group(pid: i32, signal: UnixSignal) -> Result<(), std::io::Error> {
    running_process_platform_internal::platform::process::unix_signal_process_group(
        pid,
        kind(signal),
    )
}
pub(crate) fn unix_signal_raw(signal: UnixSignal) -> i32 {
    running_process_platform_internal::platform::process::unix_signal_raw(kind(signal))
}
