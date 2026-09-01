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
    ObserverEventKind, ProcessConfig, ReadStatus, StderrMode, StdinMode,
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
const FIXTURE_STARTUP_WINDOW: Duration = Duration::from_secs(10);
const FIXTURE_TEARDOWN_WINDOW: Duration = Duration::from_secs(10);

/// Owns one `spawner` fixture and every PID it reports.
///
/// The fixture's children sleep forever, so this is deliberately created
/// immediately after constructing a `NativeProcess`.  That makes `Drop` a
/// cleanup backstop for every assertion/panic path, including a malformed or
/// incomplete fixture startup transcript.
struct OwnedTree {
    // `None` after explicit teardown releases Windows' kill-on-close Job
    // Object before verifying descendant PIDs.
    process: Option<NativeProcess>,
    known_pids: Vec<FixturePid>,
    expected_children: usize,
    cleanup_verified: bool,
}

impl OwnedTree {
    fn start(process: NativeProcess, expected_children: usize) -> Self {
        let mut tree = Self {
            process: Some(process),
            known_pids: Vec::with_capacity(expected_children + 1),
            expected_children,
            cleanup_verified: false,
        };

        // From here onward `tree` owns the process even if startup output is
        // missing or an assertion below panics.
        tree.process().start().expect("spawn the tree");
        let root_pid = tree.process().pid().expect("tree root has a pid");
        tree.remember_pid(root_pid);
        tree.read_fixture_startup(root_pid);
        tree
    }

    fn root_pid(&self) -> u32 {
        self.known_pids[0].pid
    }

    fn process(&self) -> &NativeProcess {
        self.process
            .as_ref()
            .expect("fixture process unexpectedly released")
    }

    fn remember_pid(&mut self, pid: u32) {
        assert!(
            !self.known_pids.iter().any(|known| known.pid == pid),
            "fixture reported duplicate PID {pid}"
        );
        self.known_pids.push(FixturePid::capture(pid));
    }

    fn read_fixture_startup(&mut self, root_pid: u32) {
        let deadline = Instant::now() + FIXTURE_STARTUP_WINDOW;
        let mut saw_spawner_pid = false;
        let mut saw_ready = false;

        while !saw_ready {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = match self.process().read_combined(Some(remaining)) {
                ReadStatus::Line(event) => event,
                ReadStatus::Timeout => panic!(
                    "timed out reading fixture startup (no READY within {FIXTURE_STARTUP_WINDOW:?})"
                ),
                ReadStatus::Eof => panic!("fixture output closed before READY"),
            };
            let line = String::from_utf8_lossy(&event.line);
            let line = line.trim();
            if let Some(pid) = parse_pid_line(line, "SPAWNER_PID=") {
                assert!(!saw_spawner_pid, "fixture reported SPAWNER_PID twice");
                assert_eq!(
                    pid, root_pid,
                    "fixture root PID disagrees with NativeProcess"
                );
                saw_spawner_pid = true;
            } else if let Some(pid) = parse_pid_line(line, "CHILD_PID=") {
                self.remember_pid(pid);
            } else if line == "READY" {
                saw_ready = true;
            }
        }

        assert!(saw_spawner_pid, "fixture did not report SPAWNER_PID");
        assert_eq!(
            self.known_pids.len() - 1,
            self.expected_children,
            "fixture reported unexpected CHILD_PID count"
        );
    }

    /// Terminate the contained tree, reap the direct child, then prove every
    /// PID emitted by the fixture is no longer running.
    fn shutdown_and_verify(&mut self) {
        let mut errors = Vec::new();
        if let Err(error) = self.process().kill() {
            errors.push(format!("kill fixture tree: {error}"));
        }
        if let Err(error) = self.process().wait(Some(FIXTURE_TEARDOWN_WINDOW)) {
            errors.push(format!("wait for fixture root: {error}"));
        }
        if let Err(error) = self.process().close() {
            errors.push(format!("close fixture owner: {error}"));
        }
        // `close()` stops capture and observer state but deliberately retains
        // the Windows Job Object. Releasing the owner here triggers that
        // Job Object's kill-on-close containment before PID verification.
        drop(self.process.take());
        assert!(errors.is_empty(), "fixture teardown errors: {errors:?}");

        let deadline = Instant::now() + FIXTURE_TEARDOWN_WINDOW;
        loop {
            let survivors: Vec<u32> = self
                .known_pids
                .iter()
                .filter(|known| known.is_running())
                .map(|known| known.pid)
                .collect();
            if survivors.is_empty() {
                self.cleanup_verified = true;
                return;
            }
            assert!(
                Instant::now() < deadline,
                "fixture PIDs still running after {FIXTURE_TEARDOWN_WINDOW:?}: {survivors:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn best_effort_cleanup(&mut self) {
        if let Some(process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait(Some(FIXTURE_TEARDOWN_WINDOW));
            let _ = process.close();
            drop(process);
        }
        let deadline = Instant::now() + FIXTURE_TEARDOWN_WINDOW;
        while Instant::now() < deadline && self.known_pids.iter().any(|known| known.is_running()) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for OwnedTree {
    fn drop(&mut self) {
        if !self.cleanup_verified {
            self.best_effort_cleanup();
        }
    }
}

fn parse_pid_line(line: &str, prefix: &str) -> Option<u32> {
    line.strip_prefix(prefix)
        .and_then(|pid| pid.trim().parse::<u32>().ok())
}

#[derive(Clone, Copy)]
struct FixturePid {
    pid: u32,
    #[cfg(target_os = "linux")]
    start_time: Option<u64>,
}

impl FixturePid {
    fn capture(pid: u32) -> Self {
        Self {
            pid,
            #[cfg(target_os = "linux")]
            start_time: linux_process_start_time(pid),
        }
    }

    fn is_running(self) -> bool {
        #[cfg(target_os = "linux")]
        {
            // A zombie has terminated even if its reaper has not collected the
            // exit status yet. The start-time match also avoids mistaking a
            // reused PID for one of this fixture's descendants.
            match linux_process_state_and_start_time(self.pid) {
                Some((state, start_time)) => state != 'Z' && Some(start_time) == self.start_time,
                None => false,
            }
        }

        #[cfg(windows)]
        {
            // Windows has no inexpensive creation-time identity helper in
            // this local fixture. A PID reuse can only make this bounded wait
            // fail conservatively; it cannot report a surviving fixture PID
            // as clean.
            windows_pid_is_running(self.pid)
        }

        #[cfg(all(unix, not(target_os = "linux")))]
        unsafe {
            // As above, a reused PID is a conservative false failure. Linux
            // has /proc start-time identity; macOS relies on its short,
            // bounded teardown window and CI coverage.
            libc::kill(self.pid as i32, 0) == 0
        }
    }
}

#[cfg(windows)]
fn windows_pid_is_running(pid: u32) -> bool {
    unsafe {
        let handle = winapi::um::processthreadsapi::OpenProcess(
            winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let queried = winapi::um::processthreadsapi::GetExitCodeProcess(handle, &mut exit_code);
        winapi::um::handleapi::CloseHandle(handle);
        queried != 0 && exit_code == winapi::um::minwinbase::STILL_ACTIVE
    }
}

#[cfg(target_os = "linux")]
fn linux_process_start_time(pid: u32) -> Option<u64> {
    linux_process_state_and_start_time(pid).map(|(_, start_time)| start_time)
}

#[cfg(target_os = "linux")]
fn linux_process_state_and_start_time(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.split_whitespace();
    let state = fields.nth(2)?.chars().next()?;
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}

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
        // On Unix `NativeProcess::kill` reaches the whole fixture only when
        // the root owns an isolated process group. Windows keeps its existing
        // per-spawn kill-on-close Job Object containment path.
        create_process_group: cfg!(unix),
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    }
}

#[test]
fn a_spawned_tree_reports_its_direct_child_starting_and_exiting() {
    let (process, subscriber) = NativeProcess::with_observer(spawn_tree_config(2), tree_observer());
    let mut tree = OwnedTree::start(process, 2);
    let pid = tree.root_pid();

    // Give the grandchildren a moment to exist, then tear the tree down.
    std::thread::sleep(Duration::from_millis(500));
    tree.shutdown_and_verify();

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
    let mut tree = OwnedTree::start(process, 3);

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

    tree.shutdown_and_verify();

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
    let mut tree = OwnedTree::start(process, 1);
    let pid = tree.root_pid();

    std::thread::sleep(Duration::from_millis(500));
    tree.shutdown_and_verify();

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
    let mut tree = OwnedTree::start(NativeProcess::new(spawn_tree_config(3)), 3);
    let root_pid = tree.root_pid();

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

    tree.shutdown_and_verify();

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
