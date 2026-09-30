//! Product-side adapter for neutral host descendant-monitor events.

use std::sync::Arc;

use crate::observer::{
    EventCategory, ObserverEmitter, ObserverEvent, ObserverEventKind, ProcessWatchEmitter,
};
use running_process_platform_internal::platform::process::{
    start_attached_descendant_monitor, start_descendant_monitor, DescendantEvent,
    DescendantMonitorStop,
};

type Monitor =
    fn(u32, Arc<DescendantMonitorStop>, Box<dyn Fn(DescendantEvent) + Send>) -> std::io::Result<()>;

/// Monitor a child this process just spawned. On Windows its descendants
/// arrive through the Job Object completion port wired at spawn, so this adds
/// nothing there; polling as well would report every descendant twice.
pub(crate) fn start(
    root_pid: u32,
    observer: Option<&ObserverEmitter>,
    watcher: Option<&Arc<ProcessWatchEmitter>>,
) {
    start_with(root_pid, observer, watcher, start_descendant_monitor);
}

/// Monitor an already-running process this caller did not spawn (#1015).
/// There is no spawn-time Job Object to lean on, so Windows polls the process
/// table instead.
pub(crate) fn start_attached(root_pid: u32, observer: Option<&ObserverEmitter>) {
    start_with(root_pid, observer, None, start_attached_descendant_monitor);
}

fn start_with(
    root_pid: u32,
    observer: Option<&ObserverEmitter>,
    watcher: Option<&Arc<ProcessWatchEmitter>>,
    monitor: Monitor,
) {
    let observer_pump = observer.and_then(ObserverEmitter::descendant_pump);
    if observer_pump.is_none() && watcher.is_none() {
        return;
    }
    // A process watch owns the monitor lifetime when both APIs are attached;
    // dropping the independent observer subscriber must not truncate the
    // watch's launched-tree coverage.
    let stop = watcher.map_or_else(
        || Arc::clone(&observer_pump.as_ref().expect("observer checked").1),
        |watcher| watcher.descendant_stop(),
    );
    let observer_sink = observer_pump;
    let watcher = watcher.cloned();
    let failure_watcher = watcher.clone();
    let result = monitor(
        root_pid,
        stop,
        Box::new(move |event| {
            let (kind, pid, ppid) = match event {
                DescendantEvent::Started { pid, parent_pid } => {
                    (ObserverEventKind::DescendantStarted, pid, parent_pid)
                }
                DescendantEvent::Exited(pid) => (ObserverEventKind::DescendantExited, pid, None),
                DescendantEvent::Completed => {
                    if let Some(watcher) = watcher.as_ref() {
                        watcher.finish_delivery();
                    }
                    return;
                }
            };
            if let Some((sink, observer_stop)) = observer_sink.as_ref() {
                if !observer_stop.is_stopped() {
                    let _ = sink.send(ObserverEvent::new_now_with_parent(
                        EventCategory::Process,
                        kind,
                        pid,
                        ppid,
                    ));
                }
            }
            if let Some(watcher) = watcher.as_ref() {
                watcher.emit_inferred(pid, matches!(event, DescendantEvent::Started { .. }));
            }
        }),
    );
    if result.is_err() {
        if let Some(watcher) = failure_watcher.as_ref() {
            watcher.close();
        }
    }
}
