//! Host-independent core of the snapshot-based descendant monitor (#1015).
//!
//! Windows discovers a spawned child's descendants through the Job Object
//! completion port wired at spawn, which a caller attaching to an
//! already-running pid does not have. That caller polls the process table
//! instead. The polling itself is host code (`platform_win_descendants.rs`);
//! everything that decides *what the table means* lives here, free of any OS
//! call, so it is tested on every host and not only on the one that uses it.

use std::collections::{HashMap, HashSet};

use crate::platform::process::DescendantEvent;

/// One live descendant: its immediate parent and when it was created.
///
/// `created` is an opaque, monotonic-per-boot creation stamp (a `FILETIME` on
/// Windows). It is what tells two processes that reused one pid apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Descendant {
    pub parent: u32,
    pub created: u64,
}

/// Every live descendant of `root`, keyed by pid.
///
/// `table` is `(pid, parent_pid)` for the whole machine. Parent ids are
/// reused, so a matching `parent_pid` alone is not proof of ancestry: a child
/// that was created *before* its supposed parent cannot be its child, and is
/// dropped. `created_of` supplies creation stamps lazily, so a caller only
/// pays to look up the processes that survive the cheap parent-id walk. A
/// process whose stamp cannot be read (an elevated one, or one that just
/// exited) is skipped rather than guessed at: reporting a phantom descendant
/// is worse than missing one that the next poll can still find.
pub(crate) fn descendants(
    root: u32,
    root_created: u64,
    table: &[(u32, u32)],
    created_of: &mut dyn FnMut(u32) -> Option<u64>,
) -> HashMap<u32, Descendant> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, parent) in table {
        // Pid 0 is its own parent on Windows; never treat a self-edge as a child.
        if pid != 0 && pid != parent {
            children.entry(parent).or_default().push(pid);
        }
    }
    let mut found: HashMap<u32, Descendant> = HashMap::new();
    let mut visited: HashSet<u32> = HashSet::from([root]);
    let mut stack = vec![(root, root_created)];
    while let Some((parent, parent_created)) = stack.pop() {
        for &child in children.get(&parent).map_or(&[][..], Vec::as_slice) {
            if !visited.insert(child) {
                continue;
            }
            let Some(created) = created_of(child) else {
                continue;
            };
            if created < parent_created {
                continue;
            }
            found.insert(child, Descendant { parent, created });
            stack.push((child, created));
        }
    }
    found
}

/// The events that turn `previous` into `current`.
///
/// A pid present in both with a different creation stamp is a *different*
/// process that reused it, so it is an exit followed by a start, never silence.
pub(crate) fn diff(
    previous: &HashMap<u32, Descendant>,
    current: &HashMap<u32, Descendant>,
) -> Vec<DescendantEvent> {
    let mut events = Vec::new();
    for (&pid, old) in previous {
        match current.get(&pid) {
            Some(new) if new.created == old.created => {}
            _ => events.push(DescendantEvent::Exited(pid)),
        }
    }
    for (&pid, new) in current {
        match previous.get(&pid) {
            Some(old) if old.created == new.created => {}
            _ => events.push(DescendantEvent::Started {
                pid,
                parent_pid: Some(new.parent),
            }),
        }
    }
    events
}

/// Drive a monitor: poll, emit the difference, wait, repeat.
///
/// `poll` returns `None` once the root is gone (or is no longer the process
/// that was being watched); the pump then reports every remaining descendant
/// as exited and completes. `wait` returns `true` when monitoring was cancelled.
pub(crate) fn pump(
    stopped: impl Fn() -> bool,
    mut poll: impl FnMut() -> Option<HashMap<u32, Descendant>>,
    emit: &dyn Fn(DescendantEvent),
    mut wait: impl FnMut() -> bool,
) {
    let mut known: HashMap<u32, Descendant> = HashMap::new();
    loop {
        if stopped() {
            emit(DescendantEvent::Completed);
            return;
        }
        let Some(current) = poll() else {
            for pid in known.into_keys() {
                emit(DescendantEvent::Exited(pid));
            }
            emit(DescendantEvent::Completed);
            return;
        };
        for event in diff(&known, &current) {
            emit(event);
        }
        known = current;
        if wait() {
            emit(DescendantEvent::Completed);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn stamps(pairs: &[(u32, u64)]) -> impl FnMut(u32) -> Option<u64> + '_ {
        move |pid| pairs.iter().find(|(p, _)| *p == pid).map(|(_, t)| *t)
    }

    #[test]
    fn the_walk_finds_children_and_grandchildren_with_their_immediate_parent() {
        let table = [(10, 1), (20, 10), (30, 20), (40, 10), (50, 999)];
        let times = [(20, 200), (30, 300), (40, 250), (50, 500)];
        let got = descendants(10, 100, &table, &mut stamps(&times));
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[&20],
            Descendant {
                parent: 10,
                created: 200
            }
        );
        assert_eq!(
            got[&30],
            Descendant {
                parent: 20,
                created: 300
            }
        );
        assert_eq!(
            got[&40],
            Descendant {
                parent: 10,
                created: 250
            }
        );
        assert!(!got.contains_key(&50), "an unrelated tree is not ours");
    }

    #[test]
    fn a_child_older_than_its_parent_is_a_reused_parent_id_and_is_rejected() {
        // pid 20 names 10 as its parent, but 20 started before this pid-10
        // process did: it belongs to whatever held pid 10 earlier.
        let table = [(20, 10), (30, 10)];
        let times = [(20, 50), (30, 150)];
        let got = descendants(10, 100, &table, &mut stamps(&times));
        assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![30]);
    }

    #[test]
    fn an_unreadable_creation_time_is_skipped_not_guessed() {
        let table = [(20, 10), (30, 10)];
        let times = [(30, 150)];
        let got = descendants(10, 100, &table, &mut stamps(&times));
        assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![30]);
    }

    #[test]
    fn creation_times_are_only_looked_up_for_processes_under_the_root() {
        let table = [(20, 10), (60, 99), (70, 99), (80, 99)];
        let asked = RefCell::new(Vec::new());
        let mut created = |pid| {
            asked.borrow_mut().push(pid);
            Some(500)
        };
        let _ = descendants(10, 100, &table, &mut created);
        assert_eq!(*asked.borrow(), vec![20]);
    }

    #[test]
    fn self_parented_and_cyclic_rows_terminate_and_never_report_the_root() {
        let table = [(0, 0), (10, 20), (20, 10), (30, 30)];
        let times = [(20, 200), (10, 100), (30, 300)];
        let got = descendants(10, 100, &table, &mut stamps(&times));
        assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![20]);
    }

    #[test]
    fn diff_reports_starts_and_exits_and_stays_silent_when_nothing_changed() {
        let d = |parent, created| Descendant { parent, created };
        let a: HashMap<_, _> = [(20, d(10, 200)), (30, d(20, 300))].into();
        let b: HashMap<_, _> = [(20, d(10, 200)), (40, d(10, 400))].into();
        assert!(diff(&a, &a).is_empty());
        let mut events = diff(&a, &b);
        events.sort_by_key(|event| format!("{event:?}"));
        assert_eq!(
            events,
            vec![
                DescendantEvent::Exited(30),
                DescendantEvent::Started {
                    pid: 40,
                    parent_pid: Some(10)
                },
            ]
        );
    }

    #[test]
    fn a_reused_pid_is_an_exit_then_a_start_never_silence() {
        let d = |parent, created| Descendant { parent, created };
        let before: HashMap<_, _> = [(20, d(10, 200))].into();
        let after: HashMap<_, _> = [(20, d(10, 900))].into();
        let events = diff(&before, &after);
        assert_eq!(events.len(), 2);
        assert!(events.contains(&DescendantEvent::Exited(20)));
        assert!(events.contains(&DescendantEvent::Started {
            pid: 20,
            parent_pid: Some(10)
        }));
    }

    #[test]
    fn the_pump_emits_each_change_once_then_exits_everything_when_the_root_goes() {
        let d = |parent, created| Descendant { parent, created };
        let mut polls = vec![
            Some(HashMap::from([(20, d(10, 200))])),
            Some(HashMap::from([(20, d(10, 200)), (30, d(20, 300))])),
            None,
        ]
        .into_iter();
        let seen = RefCell::new(Vec::new());
        pump(
            || false,
            || polls.next().flatten(),
            &|event| seen.borrow_mut().push(event),
            || false,
        );
        let seen = seen.into_inner();
        assert_eq!(
            seen.first(),
            Some(&DescendantEvent::Started {
                pid: 20,
                parent_pid: Some(10)
            })
        );
        assert!(seen.contains(&DescendantEvent::Started {
            pid: 30,
            parent_pid: Some(20)
        }));
        assert!(seen.contains(&DescendantEvent::Exited(20)));
        assert!(seen.contains(&DescendantEvent::Exited(30)));
        assert_eq!(seen.last(), Some(&DescendantEvent::Completed));
        assert_eq!(
            seen.iter()
                .filter(|e| **e == DescendantEvent::Completed)
                .count(),
            1
        );
    }

    #[test]
    fn a_cancelled_pump_completes_without_polling() {
        let seen = RefCell::new(Vec::new());
        pump(
            || true,
            || panic!("a stopped pump must not poll"),
            &|event| seen.borrow_mut().push(event),
            || panic!("a stopped pump must not wait"),
        );
        assert_eq!(seen.into_inner(), vec![DescendantEvent::Completed]);
    }
}
