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
    let stop = observer_pump.as_ref().map_or_else(
        || watcher.expect("watcher checked").descendant_stop(),
        |(_, stop)| Arc::clone(stop),
    );
    let observer_sink = observer_pump.map(|(sink, _)| sink);
    let watcher = watcher.cloned();
    start_descendant_monitor(
        root_pid,
        stop,
        Box::new(move |event| {
            let (kind, pid) = match event {
                DescendantEvent::Started(pid) => (ObserverEventKind::DescendantStarted, pid),
                DescendantEvent::Exited(pid) => (ObserverEventKind::DescendantExited, pid),
            };
            if let Some(sink) = observer_sink.as_ref() {
                let _ = sink.send(ObserverEvent::new_now(EventCategory::Process, kind, pid));
            }
            if let Some(watcher) = watcher.as_ref() {
                watcher.emit_inferred(pid, matches!(event, DescendantEvent::Started(_)));
            }
        }),
    );
}
