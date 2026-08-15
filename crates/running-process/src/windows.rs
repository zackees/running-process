use std::process::Child;
use std::sync::mpsc::Sender;

use crate::observer::ObserverEvent;
use running_process_platform_internal::platform::process::DescendantEvent;

pub(crate) use running_process_platform_internal::platform::process::WindowsJobHandle;

pub(crate) fn assign_child_to_windows_kill_on_close_job_impl(
    child: &Child,
    address_space_limit_bytes: Option<u64>,
) -> Result<WindowsJobHandle, std::io::Error> {
    running_process_platform_internal::platform::process::assign_child_to_windows_job(
        child,
        child.id(),
        address_space_limit_bytes,
        None,
    )
}

/// Adapt the product observer channel to the host-neutral Job/IOCP callback.
/// All Win32 containment and completion-port ownership lives behind the
/// platform process facade.
pub(crate) fn assign_child_to_windows_kill_on_close_job_with_observer_impl(
    child: &Child,
    descendant_sink: Option<Sender<ObserverEvent>>,
    process_watch: Option<std::sync::Arc<crate::observer::ProcessWatchEmitter>>,
    direct_pid: u32,
    address_space_limit_bytes: Option<u64>,
) -> Result<WindowsJobHandle, std::io::Error> {
    crate::rp_rust_debug_scope!("running_process::assign_child_to_windows_kill_on_close_job");
    let emit = (descendant_sink.is_some() || process_watch.is_some()).then(|| {
        Box::new(move |event| {
            let (kind, pid) = match event {
                DescendantEvent::Started(pid) => {
                    (crate::observer::ObserverEventKind::DescendantStarted, pid)
                }
                DescendantEvent::Exited(pid) => {
                    (crate::observer::ObserverEventKind::DescendantExited, pid)
                }
            };
            if let Some(sink) = descendant_sink.as_ref() {
                let _ = sink.send(ObserverEvent::new_now(
                    crate::observer::EventCategory::Process,
                    kind,
                    pid,
                ));
            }
            if let Some(watch) = process_watch.as_ref() {
                watch.emit_inferred(pid, matches!(event, DescendantEvent::Started(_)));
            }
        }) as Box<dyn Fn(DescendantEvent) + Send>
    });
    running_process_platform_internal::platform::process::assign_child_to_windows_job(
        child,
        direct_pid,
        address_space_limit_bytes,
        emit,
    )
}

#[cfg(test)]
pub(crate) fn windows_priority_flags(nice: Option<i32>) -> u32 {
    const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;

    match nice {
        Some(value) if value >= 15 => IDLE_PRIORITY_CLASS,
        Some(value) if value >= 1 => BELOW_NORMAL_PRIORITY_CLASS,
        Some(value) if value <= -15 => HIGH_PRIORITY_CLASS,
        Some(value) if value <= -1 => ABOVE_NORMAL_PRIORITY_CLASS,
        _ => 0,
    }
}

/// Compute the full Windows `creation_flags` for a spawned child.
///
/// The default hides consoles only for console-less parents and never
/// overrides an explicit caller console policy. Priority and process-group
/// flags are additive.
#[cfg(test)]
pub(crate) fn windows_creation_flags(
    creationflags: Option<u32>,
    create_process_group: bool,
    nice: Option<i32>,
    parent_has_console: bool,
) -> u32 {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let caller = creationflags.unwrap_or(0);
    let group = if create_process_group {
        CREATE_NEW_PROCESS_GROUP
    } else {
        0
    };
    let caller_has_console_opinion =
        caller & (CREATE_NO_WINDOW | CREATE_NEW_CONSOLE | DETACHED_PROCESS) != 0;
    let no_window = if caller_has_console_opinion || parent_has_console {
        0
    } else {
        CREATE_NO_WINDOW
    };

    caller | group | no_window | windows_priority_flags(nice)
}
