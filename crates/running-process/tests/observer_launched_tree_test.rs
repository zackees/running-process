//! Acceptance fixture for the `LaunchedProcessTree` observer (#539).
//!
//! The observer module has 30 unit tests and, until now, no integration test.
//! Those unit tests drive the capability matrix and the parsing helpers, and
//! the lifecycle ones spawn `exit 0` — a process with no children. So the
//! thing the descendant backends exist to do, notice a *grandchild*, was
//! never exercised.
//!
//! #539 asks for exactly this, per backend: "fixture: spawn-chain depth 3,
//! assert event count". All three OSes already report the process category as
//! `Supported` for this scope (`subreaper-proc-poll` on Linux,
//! `job-object-iocp` on Windows, `sysctl-proc-poll` on macOS), so this is one
//! cross-platform test rather than three per-OS ones — a backend that
//! regresses turns its own CI lane red.
//!
//! # Why presence, not an exact count
//!
//! The issue says "assert event count". A descendant poller cannot promise
//! one: Linux and macOS discover descendants by scanning `/proc` and `sysctl`,
//! so a grandchild that starts and exits between two scans is legitimately
//! never seen, and one seen late is still correct. Asserting a tally would
//! encode the poll interval into the test and fail on a loaded runner for
//! reasons unrelated to the backend.
//!
//! What is falsifiable without being flaky: the direct child's `Started` and
//! `Exited` come from the spawn path rather than from polling, so those are
//! exact; and a tree with live grandchildren must not report *zero*
//! descendants.

#![cfg(feature = "client")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use running_process::observer::TraceScope;
use running_process::{
    CommandSpec, EventCategory, NativeProcess, ObserverCapabilities, ObserverConfig,
    ObserverEventKind, ProcessConfig, StderrMode, StdinMode,
};

/// Descendant discovery is gated on the Process category, not Lifecycle.
///
/// `ObserverConfig::lifecycle()` requests only `Started`/`Exited` for the
/// direct child, and the descendant sink is never created — so a test that
/// used it would wait the full window and conclude, wrongly, that the backend
/// is broken. That is exactly what the first draft of this file did.
fn tree_observer() -> ObserverConfig {
    ObserverConfig::with_categories([EventCategory::Lifecycle, EventCategory::Process])
}

/// How long to wait for a descendant to be noticed. Generous because two of
/// the three backends poll; the test asserts what happens inside the window,
/// not how quickly.
const OBSERVE_WINDOW: Duration = Duration::from_secs(15);

fn testbin_path(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.\n\
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

/// `spawner <count> <sleeper>` — spawns `count` sleeper grandchildren, so the
/// observed tree is test → spawner → sleepers, the depth-3 chain #539 asks for.
fn spawn_tree_config(count: usize) -> ProcessConfig {
    let spawner = testbin_path("testbin-spawner");
    let sleeper = testbin_path("testbin-sleeper");
    ProcessConfig {
        command: CommandSpec::Argv(vec![
            spawner.display().to_string(),
            count.to_string(),
            sleeper.display().to_string(),
        ]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    }
}

#[test]
fn a_spawned_tree_reports_its_direct_child_starting_and_exiting() {
    let (process, subscriber) = NativeProcess::with_observer(spawn_tree_config(2), tree_observer());
    process.start().expect("spawn the tree");
    let pid = process.pid().expect("tree root has a pid");

    // Give the grandchildren a moment to exist, then tear the tree down.
    std::thread::sleep(Duration::from_millis(500));
    process.kill().expect("kill the tree");
    let _ = process.wait(Some(OBSERVE_WINDOW));
    process.close().ok();

    let events = subscriber.drain();
    let started = events
        .iter()
        .filter(|e| matches!(e.kind, ObserverEventKind::Started))
        .count();
    let exited = events
        .iter()
        .filter(|e| matches!(e.kind, ObserverEventKind::Exited { .. }))
        .count();

    // These two come from the spawn path, not from polling, so unlike
    // descendant discovery they can be asserted exactly.
    assert_eq!(
        started, 1,
        "expected exactly one Started for the direct child, got {started} in {events:?}"
    );
    assert_eq!(
        exited, 1,
        "expected exactly one Exited for the direct child, got {exited} in {events:?}"
    );
    assert!(
        events.iter().any(|e| e.pid == pid),
        "no event carried the root pid {pid}: {events:?}"
    );
}

#[test]
fn a_tree_with_live_grandchildren_does_not_report_zero_descendants() {
    let (process, subscriber) = NativeProcess::with_observer(spawn_tree_config(3), tree_observer());
    process.start().expect("spawn the tree");

    // Poll for a descendant while the grandchildren are alive, rather than
    // sleeping a fixed time and hoping the scan already ran.
    let deadline = Instant::now() + OBSERVE_WINDOW;
    let mut saw_descendant = false;
    while Instant::now() < deadline {
        if subscriber
            .drain()
            .iter()
            .any(|e| matches!(e.kind, ObserverEventKind::DescendantStarted))
        {
            saw_descendant = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    process.kill().expect("kill the tree");
    let _ = process.wait(Some(OBSERVE_WINDOW));
    process.close().ok();

    // Deliberately "at least one" rather than "exactly three": the polling
    // backends may not have caught every grandchild inside the window, and
    // encoding the poll interval here would make this flaky on a loaded
    // runner. Zero, with three sleepers alive under the root, means descendant
    // discovery is not working at all.
    assert!(
        saw_descendant,
        "no DescendantStarted observed while three grandchildren were alive"
    );
}

#[test]
fn the_process_category_is_usable_for_this_scope() {
    // The fixtures above only mean something if the backend claims to work
    // here. If a platform ever downgrades this cell, they would start passing
    // vacuously — this is what notices.
    // `negotiate()` defaults to SystemWide, where this category genuinely is
    // Unavailable (it needs ETW / eBPF / EndpointSecurity). The scope under
    // test is the no-admin launched tree.
    let caps = ObserverCapabilities::negotiate_for_scope(TraceScope::LaunchedProcessTree);
    let support = caps.support(EventCategory::Process);
    assert!(
        !matches!(support, running_process::CapabilitySupport::Unavailable),
        "process category reports {support:?}; the spawn-chain fixtures cannot \
         mean anything if the backend is unavailable"
    );
}

/// The observer must compose with a caller-configured `std::process::Command`
/// (`with_observer_and_command`, zackees/soldr#2546): `ProcessConfig` cannot
/// express `env_remove` scrubs or non-Unicode argv/env, so callers that
/// already own a configured `Command` hand it over verbatim.
#[test]
fn an_observed_configured_command_spawns_verbatim_and_reports_lifecycle() {
    let spawner = testbin_path("testbin-spawner");
    let sleeper = testbin_path("testbin-sleeper");
    let mut command = std::process::Command::new(&spawner);
    command.arg("1").arg(sleeper.display().to_string());
    // The shaping ProcessConfig cannot express: a scrub of an inherited
    // variable plus an explicit addition, on the caller's own Command.
    command.env_remove("RP_SEAM_TEST_SCRUBBED");
    command.env("RP_SEAM_TEST_ADDED", "1");

    // The config's own command points at a binary that cannot exist. If the
    // override were not spawned verbatim, start() would fail loudly instead
    // of producing the direct child's exact lifecycle events below.
    let mut config = spawn_tree_config(1);
    config.command = CommandSpec::Argv(vec!["rp-seam-test-not-a-real-binary".to_string()]);

    let (process, subscriber) =
        NativeProcess::with_observer_and_command(command, config, tree_observer());
    process.start().expect("spawn the configured command");
    let pid = process.pid().expect("configured command has a pid");

    std::thread::sleep(Duration::from_millis(500));
    process.kill().expect("kill the tree");
    let _ = process.wait(Some(OBSERVE_WINDOW));
    process.close().ok();

    let events = subscriber.drain();
    let started = events
        .iter()
        .filter(|e| matches!(e.kind, ObserverEventKind::Started))
        .count();
    let exited = events
        .iter()
        .filter(|e| matches!(e.kind, ObserverEventKind::Exited { .. }))
        .count();
    assert_eq!(
        started, 1,
        "expected exactly one Started for the configured command, got {started} in {events:?}"
    );
    assert_eq!(
        exited, 1,
        "expected exactly one Exited for the configured command, got {exited} in {events:?}"
    );
    assert!(
        events.iter().any(|e| e.pid == pid),
        "no event carried the root pid {pid}: {events:?}"
    );
}

#[test]
fn an_adopted_pid_reports_descendants_with_parents() {
    // The post-hoc attach (observe_launched_tree) rides the same polling
    // monitors as the spawn path, which are spawn-independent only on
    // Unix — Windows discovery lives in the Job Object wired at spawn.
    if cfg!(windows) {
        return;
    }
    // Spawn WITHOUT an observer: the tree owner here manages its own
    // child, which is exactly the caller observe_launched_tree exists for.
    let process = NativeProcess::new(spawn_tree_config(3));
    process.start().expect("spawn the tree");
    let root_pid = process.pid().expect("tree root has a pid");

    let subscriber = running_process::observer::observe_launched_tree(
        root_pid,
        ObserverConfig::with_categories([EventCategory::Process]),
    );

    let deadline = Instant::now() + OBSERVE_WINDOW;
    let mut descendant_with_parent = None;
    while Instant::now() < deadline {
        if let Some(event) = subscriber
            .drain()
            .into_iter()
            .find(|e| matches!(e.kind, ObserverEventKind::DescendantStarted))
        {
            descendant_with_parent = Some(event);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    process.kill().expect("kill the tree");
    let _ = process.wait(Some(OBSERVE_WINDOW));
    process.close().ok();

    let event = descendant_with_parent
        .expect("no DescendantStarted observed on an adopted pid with three grandchildren alive");
    // running-process#1025: the Unix monitors know each descendant's
    // immediate parent. For spawner's own children that parent is the
    // observed root itself.
    assert_eq!(
        event.ppid,
        Some(root_pid),
        "descendant of the adopted root must name it as parent: {event:?}"
    );
}
