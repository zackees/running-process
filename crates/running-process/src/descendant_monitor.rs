//! Product-side adapter for neutral host descendant-monitor events.

use crate::observer::{EventCategory, ObserverEmitter, ObserverEvent, ObserverEventKind};
use running_process_platform_internal::platform::process::{
    start_descendant_monitor, DescendantEvent,
};

pub(crate) fn start(root_pid: u32, emitter: &ObserverEmitter) {
    let Some((sink, stop)) = emitter.descendant_pump() else {
        return;
    };
    start_descendant_monitor(
        root_pid,
        stop,
        Box::new(move |event| {
            let (kind, pid) = match event {
                DescendantEvent::Started(pid) => (ObserverEventKind::DescendantStarted, pid),
                DescendantEvent::Exited(pid) => (ObserverEventKind::DescendantExited, pid),
            };
            let _ = sink.send(ObserverEvent::new_now(EventCategory::Process, kind, pid));
        }),
    );
}
