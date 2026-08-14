//! Console-popup observation through the selected platform process facade.

pub use running_process_platform_internal::platform::process::ConsoleWindowInfo;

/// Monitor for new visible console windows during `duration`.
///
/// Non-Windows platforms return an empty list.
pub fn monitor_console_windows(duration: std::time::Duration) -> Vec<ConsoleWindowInfo> {
    running_process_platform_internal::platform::process::monitor_console_windows(duration)
}
