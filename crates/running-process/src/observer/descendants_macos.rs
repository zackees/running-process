//! #539 slice 7 — macOS descendant-lifecycle backend.
//!
//! No-admin macOS primitive: `kqueue` + `EVFILT_PROC` with
//! `NOTE_TRACK`. The kernel automatically registers a paired kevent
//! for each `fork(2)` of a tracked process; when the child appears,
//! it surfaces as a kevent with `NOTE_CHILD` set and the child PID in
//! `ident`. `NOTE_EXIT` events surface for each tracked descendant on
//! exit. Unlike the Linux backend, this is fully event-driven — no
//! polling.
//!
//! Permission model: `NOTE_TRACK` uses the calling process's
//! credentials; it works against any process the calling user owns
//! (the LaunchedProcessTree scope by definition). No admin, no
//! entitlements, no system extensions.
//!
//! Failure modes:
//!
//! - **`NOTE_TRACKERR`**: the kernel ran out of bookkeeping space
//!   when a tracked process forked. The auto-registration is dropped,
//!   so descendants of that fork are not observed. We surface this as
//!   a debug-level note (no panic, no error event) — the consumer
//!   keeps receiving events for the surviving tracked subtree.
//! - **Hardened-runtime targets**: a target compiled with
//!   `com.apple.security.cs.disable-library-validation = false` and
//!   no `com.apple.security.get-task-allow` entitlement may refuse
//!   the implicit `task_for_pid` lookup `EVFILT_PROC` performs. Same
//!   honesty caveat as `DYLD_INSERT_LIBRARIES` injection on macOS;
//!   the consumer just sees zero descendant events for that subtree.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::mpsc::Sender;

use crate::observer::{EventCategory, ObserverEvent, ObserverEventKind};

/// Enable observation for descendants of `root_pid`. **Registers
/// kqueue + `NOTE_TRACK` synchronously** on the caller's thread —
/// critical because `NOTE_TRACK` only fires `NOTE_CHILD` for
/// *future* `fork(2)` calls. If we deferred registration to the
/// pump thread, a fast-forking root (e.g. shell scripts that
/// immediately spawn background jobs) could win the race and we'd
/// miss every descendant. The drain loop runs on the pump thread.
///
/// On registration failure (root already gone, kernel rejected the
/// kevent) this is a no-op — the consumer just receives no events.
pub(crate) fn spawn_pump(root_pid: u32, sink: Sender<ObserverEvent>) {
    // SAFETY: `kqueue()` is a leaf syscall with no pointer arguments.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return;
    }
    if register_for_tracking(kq, root_pid).is_err() {
        unsafe { libc::close(kq) };
        return;
    }
    let kq_usize = kq as usize;
    let spawn_result = std::thread::Builder::new()
        .name("rp-macos-descpump".to_string())
        .spawn(move || drain_loop(kq_usize as i32, root_pid, sink));
    if spawn_result.is_err() {
        // Failed to launch the thread — close the kq we already
        // opened so we don't leak the descriptor.
        unsafe { libc::close(kq) };
    }
}

fn drain_loop(kq: i32, root_pid: u32, sink: Sender<ObserverEvent>) {
    // Defer kqueue cleanup so an early-return below still closes the
    // fd. `Drop` impl would be heavier; an inline guard is simpler.
    let _kq_guard = scopeguard(|| unsafe {
        libc::close(kq);
    });

    let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `kevent` with NULL changelist and a non-null,
        // properly-sized eventlist is a documented no-op-on-add wait.
        let n = unsafe {
            libc::kevent(
                kq,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                std::ptr::null(), // block indefinitely
            )
        };
        if n < 0 {
            // EINTR from a signal is benign; any other error we treat
            // as terminal so the pump doesn't busy-loop.
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if n == 0 {
            continue;
        }
        let mut root_exited = false;
        for ev in &events[..n as usize] {
            let pid = ev.ident as u32;
            let fflags = ev.fflags;
            // NOTE_CHILD: a tracked process forked, and `ev.ident`
            // is the new child PID. The kernel has auto-registered
            // the child for tracking with the same fflags.
            if fflags & libc::NOTE_CHILD != 0 {
                let _ = sink.send(ObserverEvent::new_now(
                    EventCategory::Process,
                    ObserverEventKind::DescendantStarted,
                    pid,
                ));
                continue;
            }
            // NOTE_TRACKERR: kernel ran out of bookkeeping space.
            // Drop on the floor — the consumer still receives events
            // for the descendants we did successfully track.
            if fflags & libc::NOTE_TRACKERR != 0 {
                continue;
            }
            // NOTE_EXIT: a tracked descendant exited.
            if fflags & libc::NOTE_EXIT != 0 {
                if pid == root_pid {
                    // Root exited: drain remaining events and exit
                    // the loop. We do not synthesize exits for any
                    // descendants the kernel didn't tell us about —
                    // each descendant's own NOTE_EXIT will have
                    // fired (or will fire) on its own kevent slot.
                    root_exited = true;
                    continue;
                }
                let _ = sink.send(ObserverEvent::new_now(
                    EventCategory::Process,
                    ObserverEventKind::DescendantExited,
                    pid,
                ));
            }
        }
        if root_exited {
            break;
        }
    }
}

/// Register `pid` on the kqueue for fork+exec+exit tracking. Returns
/// `Err` on registration failure (typically because the PID is
/// already gone).
fn register_for_tracking(kq: i32, pid: u32) -> std::io::Result<()> {
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = pid as libc::uintptr_t;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ENABLE;
    change.fflags = libc::NOTE_EXIT | libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_TRACK;
    change.data = 0;
    change.udata = std::ptr::null_mut::<c_void>();
    // SAFETY: changelist points to one well-initialized kevent;
    // eventlist is NULL so the call only registers, doesn't fetch.
    let r = unsafe {
        libc::kevent(
            kq,
            &change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if change.flags & libc::EV_ERROR != 0 && change.data != 0 {
        // kevent returns EV_ERROR in the changelist entry's `flags`
        // with the errno in `data` for per-change failures (e.g.
        // ESRCH for an already-dead PID).
        return Err(std::io::Error::from_raw_os_error(change.data as i32));
    }
    Ok(())
}

/// Minimal scope-guard to run a closure on drop. Inline instead of
/// pulling in the `scopeguard` crate just for this one usage.
fn scopeguard<F: FnOnce()>(f: F) -> ScopeGuard<F> {
    ScopeGuard(Some(f))
}

struct ScopeGuard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for ScopeGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_for_tracking_nonexistent_pid_errors() {
        // SAFETY: `kqueue()` returns a leaf fd.
        let kq = unsafe { libc::kqueue() };
        assert!(kq >= 0, "kqueue() must succeed");
        let err = register_for_tracking(kq, 0x7FFF_FFFE)
            .expect_err("nonexistent pid should fail registration");
        // Either ESRCH (no such process) or EINVAL is acceptable —
        // both signal the kernel rejected the registration.
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS-level errno, got: {err}"
        );
        // SAFETY: close the kqueue we just opened.
        unsafe {
            libc::close(kq);
        }
    }

    #[test]
    fn end_to_end_descendant_started_and_exited_for_spawned_chain() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
        use std::time::Duration;

        // Leading `sleep 0.2` gives the kqueue pump's NOTE_TRACK
        // registration time to land before bash starts forking the
        // background sleepers — `NOTE_TRACK` only fires `NOTE_CHILD`
        // for *future* fork() calls, so a fast-forking root would
        // race the pump and we'd see zero descendants. Real-world
        // long-lived parents (clud, daemons) hit this naturally.
        let cfg = ProcessConfig {
            command: CommandSpec::Argv(vec![
                "bash".into(),
                "-c".into(),
                "sleep 0.2; sleep 0.5 & sleep 0.5 & sleep 0.5 & wait".into(),
            ]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
        };
        let (process, subscriber) = NativeProcess::with_observer(
            cfg,
            ObserverConfig::with_categories([EventCategory::Process]),
        );
        process.start().expect("spawn bash chain");
        let _ = process
            .wait(Some(Duration::from_secs(30)))
            .expect("bash chain exits");
        process.close().ok();
        // Give the kqueue pump a beat to flush queued events after
        // root exit.
        std::thread::sleep(Duration::from_millis(200));

        let events = subscriber.drain();
        let started = events
            .iter()
            .filter(|e| {
                e.category == EventCategory::Process
                    && matches!(e.kind, ObserverEventKind::DescendantStarted)
            })
            .count();
        let exited = events
            .iter()
            .filter(|e| {
                e.category == EventCategory::Process
                    && matches!(e.kind, ObserverEventKind::DescendantExited)
            })
            .count();
        assert!(
            started >= 3,
            "expected ≥3 DescendantStarted, got {started} (all: {events:?})"
        );
        assert!(
            exited >= 3,
            "expected ≥3 DescendantExited, got {exited} (all: {events:?})"
        );
        for ev in &events {
            assert_eq!(
                ev.category,
                EventCategory::Process,
                "Lifecycle leaked into Process-only subscriber: {ev:?}"
            );
        }
    }
}
