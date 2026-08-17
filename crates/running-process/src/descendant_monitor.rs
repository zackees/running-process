//! Product-side adapter for neutral host descendant-monitor events.

use std::sync::Arc;

use crate::observer::{
    EventCategory, ObserverEmitter, ObserverEvent, ObserverEventKind, ProcessWatchEmitter,
};
use running_process_platform_internal::platform::process::{
    start_descendant_monitor, DescendantEvent,
};

pub(crate) fn start(
    root_pid: u32,
    observer: Option<&ObserverEmitter>,
    watcher: Option<&Arc<ProcessWatchEmitter>>,
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
    let result = start_descendant_monitor(
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
