//! Live #1202 containment evidence. Deliberately opt-in: it needs a real
//! separated broker or user systemd manager and a visible cgroup v2 hierarchy,
//! neither of which is guaranteed in ordinary shells or containers. Membership
//! alone does not prove memory charging or survival after caller teardown.

#![cfg(target_os = "linux")]

use running_process::{
    spawn_daemon_request, DaemonSpawnRequest, IndependentSpawnOptions, SpawnMode,
};

struct FixtureChild(running_process::DaemonChild);

impl Drop for FixtureChild {
    fn drop(&mut self) {
        // Daemon handles intentionally do not kill on drop. Keep fixture
        // cleanup active even when reading /proc or an assertion panics.
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.0.kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Err(_) => {
                    eprintln!("fixture child cleanup could not confirm exit status");
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        eprintln!("fixture child exit was not confirmed before cleanup deadline");
    }
}

fn enabled(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

#[test]
fn unavailable_broker_does_not_fall_back_to_inherited_spawn() {
    if !enabled("RUNNING_PROCESS_LIVE_TESTS")
        || !enabled("RUNNING_PROCESS_INDEPENDENT_ABSENT_BROKER_TESTS")
    {
        eprintln!("skipping explicitly gated missing-broker fixture");
        return;
    }
    // Configure this in a dedicated test process/container. Do not mutate
    // process-wide backend selection while other live tests may be spawning.
    let socket = std::env::var_os("RUNNING_PROCESS_INDEPENDENT_BROKER")
        .expect("missing broker endpoint must be explicitly configured");
    assert!(
        !std::path::Path::new(&socket).try_exists().unwrap(),
        "this fixture requires an absent endpoint, not an existing broker"
    );
    let capability = running_process::independent_spawn_capability();
    assert!(!capability.available);
    assert_eq!(capability.backend, Some(running_process::IndependentSpawnBackend::ExternalBroker),
        "missing-endpoint coverage requires native control support; an earlier platform failure is not broker evidence");
    let directory = tempfile::tempdir().expect("private marker directory");
    let marker = directory.path().join("unexpected-launch");
    let mut request = DaemonSpawnRequest::new("/bin/sh");
    request
        .args(["-c", "printf unexpected > \"$1\"", "fixture"])
        .arg(&marker);
    let started = std::time::Instant::now();
    let result = spawn_daemon_request(
        &mut request,
        &IndependentSpawnOptions {
            mode: SpawnMode::Independent,
            readiness_timeout: std::time::Duration::from_secs(2),
            ..Default::default()
        },
    );
    match result {
        Ok(child) => {
            let _cleanup = FixtureChild(child);
            panic!("missing broker must not produce an inherited child");
        }
        Err(error) => {
            assert!(matches!(error,
            running_process::IndependentSpawnError::Launch { .. }),
            "absent endpoint must report launch failure, not unrelated platform denial: {error}")
        }
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(
        !marker.exists(),
        "rejected request must not execute its program"
    );
    // Positive control: the exact request is executable and can write its
    // marker. A broken fixture must not look like successful fallback rejection.
    let mut inherited = FixtureChild(
        spawn_daemon_request(
            &mut request,
            &IndependentSpawnOptions {
                mode: SpawnMode::Inherited,
                ..Default::default()
            },
        )
        .expect("inherited mode must not depend on the missing broker"),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = inherited.0.try_wait().expect("positive-control status") {
            assert_eq!(status, 0);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "inherited positive control deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read(&marker).expect("positive-control marker"),
        b"unexpected"
    );
}

fn unified_cgroup(pid: u32) -> String {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("read cgroup");
    text.lines()
        .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
        .expect("cgroup v2 unified path")
}

#[test]
fn independent_daemon_is_not_in_callers_cgroup_subtree() {
    if !enabled("RUNNING_PROCESS_LIVE_TESTS") || !enabled("RUNNING_PROCESS_INDEPENDENT_LIVE_TESTS")
    {
        eprintln!("skipping live independent-spawn cgroup fixture; set RUNNING_PROCESS_LIVE_TESTS=1 and RUNNING_PROCESS_INDEPENDENT_LIVE_TESTS=1");
        return;
    }

    let caller = unified_cgroup(std::process::id());
    let mut command = DaemonSpawnRequest::new("sleep");
    command.arg("30");
    let child = FixtureChild(
        spawn_daemon_request(
            &mut command,
            &IndependentSpawnOptions {
                mode: SpawnMode::Independent,
                ..Default::default()
            },
        )
        .expect("verified independent spawn"),
    );
    let target = unified_cgroup(child.0.id());

    assert_ne!(target, caller, "target must not remain in caller cgroup");
    assert!(
        !target.starts_with(&(caller.trim_end_matches('/').to_owned() + "/")),
        "target must not remain in a descendant of caller cgroup: caller={caller:?} target={target:?}"
    );
}

#[test]
fn inherited_daemon_retains_callers_cgroup() {
    if !enabled("RUNNING_PROCESS_LIVE_TESTS") {
        eprintln!("skipping live inherited-spawn fixture; set RUNNING_PROCESS_LIVE_TESTS=1");
        return;
    }
    // This baseline does not require a scheduler or broker. Session detachment
    // must not be mistaken for independent placement, including inside Docker.
    let caller = unified_cgroup(std::process::id());
    let mut request = DaemonSpawnRequest::new("sleep");
    request.arg("30");
    let child = FixtureChild(
        spawn_daemon_request(
            &mut request,
            &IndependentSpawnOptions {
                mode: SpawnMode::Inherited,
                ..Default::default()
            },
        )
        .expect("inherited daemon spawn"),
    );
    assert_eq!(unified_cgroup(child.0.id()), caller);
}

fn memory_counter(pid: u32) -> std::path::PathBuf {
    let group = unified_cgroup(pid);
    assert!(group.starts_with('/'));
    assert!(!std::path::Path::new(&group)
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir)));
    let directory = std::path::Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
    // Reject an incompatible cgroup mount/namespace mapping rather than
    // measuring an unrelated group's counter and claiming isolation.
    let members = std::fs::read_to_string(directory.join("cgroup.procs"))
        .expect("visible cgroup v2 membership");
    assert!(members.lines().any(|line| line.parse::<u32>() == Ok(pid)));
    directory.join("memory.current")
}

fn memory_bytes(path: &std::path::Path) -> i128 {
    std::fs::read_to_string(path)
        .expect("memory controller must be enabled")
        .trim()
        .parse()
        .expect("memory.current integer")
}

fn fixture_marker(directory: &std::path::Path, name: &str) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(failure) = std::fs::read_to_string(directory.join("launcher-failed")) {
            if failure.ends_with('\n') {
                let category = match failure.trim() {
                    "1" => "unsupported",
                    "2" => "permission denied",
                    "3" => "launch failed",
                    "4" => "readiness timeout",
                    "5" => "cancelled",
                    "6" => "cleanup unconfirmed",
                    _ => "invalid failure marker",
                };
                panic!("launcher reported {category} while waiting for {name}");
            }
        }
        match std::fs::read_to_string(directory.join(name)) {
            Ok(text) if text.ends_with('\n') => {
                return text.trim().parse().expect("numeric fixture marker")
            }
            Ok(_) => {} // The writer has not finished publishing its line.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot read fixture marker {name}: {error}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture marker deadline: {name}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

struct ReleaseFixture<'a>(&'a std::path::Path);

impl Drop for ReleaseFixture<'_> {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("release"), b"done");
    }
}

#[test]
fn independent_daemon_performs_work_after_launcher_exit() {
    caller_lifetime_fixture(false, SpawnMode::Independent);
}

#[test]
fn independent_daemon_performs_work_after_owned_cgroup_kill() {
    if !enabled("RUNNING_PROCESS_INDEPENDENT_TEARDOWN_TESTS") {
        eprintln!("skipping explicitly gated cgroup teardown fixture");
        return;
    }
    caller_lifetime_fixture(true, SpawnMode::Independent);
}

#[test]
fn inherited_daemon_is_removed_by_owned_cgroup_kill() {
    if !enabled("RUNNING_PROCESS_INDEPENDENT_TEARDOWN_TESTS") {
        eprintln!("skipping explicitly gated inherited teardown control");
        return;
    }
    caller_lifetime_fixture(true, SpawnMode::Inherited);
}

struct OwnedCallerGroup {
    path: std::path::PathBuf,
    kill: std::fs::File,
    identity: (u64, u64),
    directory: std::fs::File,
}

impl OwnedCallerGroup {
    fn create(unique: &std::ffi::OsStr) -> Self {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::OpenOptionsExt;
        let parent = std::env::var_os("RUNNING_PROCESS_TEST_CGROUP_PARENT")
            .expect("explicit delegated cgroup parent required");
        let parent = std::fs::canonicalize(parent).expect("delegated parent");
        assert!(parent.starts_with("/sys/fs/cgroup"));
        assert!(
            parent.join("cgroup.controllers").is_file(),
            "requires cgroup v2"
        );
        let path = parent.join(format!("rp-owned-{}", unique.to_string_lossy()));
        std::fs::create_dir(&path).expect("create a fresh test-owned child cgroup");
        let metadata = std::fs::symlink_metadata(&path).expect("new group identity");
        let identity = (metadata.dev(), metadata.ino());
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)
            .expect("retain newly created cgroup directory");
        let opened = directory.metadata().unwrap();
        assert_eq!(
            (opened.dev(), opened.ino()),
            identity,
            "created directory was replaced"
        );
        let anchored = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let kill = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(anchored.join("cgroup.kill"))
            .unwrap_or_else(|error| {
                let _ = std::fs::remove_dir(&path);
                panic!("owned cgroup requires cgroup.kill: {error}");
            });
        Self {
            path,
            kill,
            identity,
            directory,
        }
    }

    fn control_path(&self, name: &str) -> std::path::PathBuf {
        use std::os::fd::AsRawFd;
        assert!(matches!(name, "cgroup.events" | "cgroup.procs"));
        std::path::PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd())).join(name)
    }

    fn terminate(&mut self) {
        use std::os::unix::fs::FileExt;
        assert_eq!(
            self.kill
                .write_at(b"1", 0)
                .expect("kill only the freshly owned caller group"),
            1
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            assert!(self.same_directory(), "owned group path was replaced");
            let events = std::fs::read_to_string(self.control_path("cgroup.events")).unwrap();
            if events.lines().any(|line| line == "populated 0") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owned group must become empty"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn same_directory(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.is_dir() && (metadata.dev(), metadata.ino()) == self.identity
        })
    }
}

impl Drop for OwnedCallerGroup {
    fn drop(&mut self) {
        use std::os::unix::fs::FileExt;
        let _ = self.kill.write_at(b"1", 0);
        // No recursive removal and no parent kill: only this fresh directory.
        if self.same_directory() {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

fn caller_lifetime_fixture(teardown: bool, mode: SpawnMode) {
    if !enabled("RUNNING_PROCESS_LIVE_TESTS")
        || (mode == SpawnMode::Independent && !enabled("RUNNING_PROCESS_INDEPENDENT_LIVE_TESTS"))
    {
        eprintln!("skipping opt-in caller lifetime fixture");
        return;
    }
    let exe = std::env::current_exe().expect("test executable");
    let fixture = exe
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap()
        .join("testbin-independent-launcher");
    assert!(fixture.is_file(), "build testbins first");
    let directory = tempfile::tempdir().expect("private fixture directory");
    let mut group =
        teardown.then(|| OwnedCallerGroup::create(directory.path().file_name().unwrap()));
    let _release = ReleaseFixture(directory.path());
    let mut request = DaemonSpawnRequest::new(fixture);
    request.arg(directory.path()).arg(match mode {
        SpawnMode::Inherited => "inherited",
        SpawnMode::Independent => "independent",
    });
    // Daemon Auto environment is UserBaseline, not ambient inheritance.
    // Preserve only the explicit backend selectors needed by this nested
    // caller, otherwise a broker test could accidentally exercise systemd.
    for key in [
        "RUNNING_PROCESS_INDEPENDENT_BROKER",
        "RUNNING_PROCESS_INDEPENDENT_HELPER",
    ] {
        match std::env::var_os(key) {
            Some(value) => {
                request.env(key, value);
            }
            None => {
                request.env_remove(key);
            }
        }
    }
    let mut launcher = FixtureChild(
        spawn_daemon_request(
            &mut request,
            &IndependentSpawnOptions {
                mode: SpawnMode::Inherited,
                ..Default::default()
            },
        )
        .expect("launcher spawn"),
    );
    assert_eq!(
        fixture_marker(directory.path(), "launcher-started"),
        u64::from(launcher.0.id())
    );
    if let Some(group) = &group {
        std::fs::write(
            group.control_path("cgroup.procs"),
            launcher.0.id().to_string(),
        )
        .expect("move only fixture launcher before launch gate");
        let members = std::fs::read_to_string(group.control_path("cgroup.procs")).unwrap();
        assert_eq!(members.trim(), launcher.0.id().to_string());
    }
    std::fs::write(directory.path().join("launch"), b"go").unwrap();
    let target = fixture_marker(directory.path(), "launched");
    assert_eq!(fixture_marker(directory.path(), "started"), target);
    if let Some(group) = &mut group {
        let target_group = unified_cgroup(u32::try_from(target).unwrap());
        let target_path =
            std::path::Path::new("/sys/fs/cgroup").join(target_group.trim_start_matches('/'));
        match mode {
            SpawnMode::Independent => assert!(
                !target_path.starts_with(&group.path),
                "independent daemon must be outside kill target"
            ),
            SpawnMode::Inherited => {
                assert_eq!(
                    target_path, group.path,
                    "inherited control must share caller group"
                );
                let members = std::fs::read_to_string(group.control_path("cgroup.procs")).unwrap();
                assert!(
                    members
                        .lines()
                        .any(|line| line.parse::<u64>() == Ok(target)),
                    "control daemon must be alive inside the group before teardown"
                );
            }
        }
        group.terminate();
    } else {
        std::fs::write(directory.path().join("exit-launcher"), b"exit").unwrap();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(status) = launcher.0.try_wait().expect("launcher status") {
            if teardown {
                assert_ne!(status, 0);
            } else {
                assert_eq!(status, 0);
            }
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "launcher exit deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if teardown && mode == SpawnMode::Inherited {
        // terminate() already required populated=0 after confirming that the
        // live daemon belonged to this exact group. No PID reuse inference.
        return;
    }
    // Fresh work requested only after confirmed caller exit: an earlier
    // readiness marker or a recycled numeric PID cannot satisfy this proof.
    assert!(!directory.path().join("resident").exists());
    std::fs::write(directory.path().join("allocate"), b"go").unwrap();
    assert_eq!(
        fixture_marker(directory.path(), "resident"),
        64 * 1024 * 1024
    );
    std::fs::write(directory.path().join("release"), b"done").unwrap();
    assert_eq!(
        fixture_marker(directory.path(), "released"),
        target,
        "the surviving daemon must acknowledge allocation release"
    );
}

#[test]
fn independent_allocation_is_charged_outside_caller_group() {
    if !enabled("RUNNING_PROCESS_LIVE_TESTS")
        || !enabled("RUNNING_PROCESS_INDEPENDENT_MEMORY_TESTS")
    {
        eprintln!("skipping memory fixture; requires explicit live/memory opt-in and quiet dedicated caller/target cgroups");
        return;
    }
    let exe = std::env::current_exe().expect("test executable");
    let fixture = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test executable in profile/deps")
        .join("testbin-independent-memory-holder");
    assert!(
        fixture.is_file(),
        "build testbins before running acceptance tests"
    );
    let directory = tempfile::tempdir().expect("private fixture directory");
    let mut request = DaemonSpawnRequest::new(fixture);
    request.arg(directory.path());
    let child = FixtureChild(
        spawn_daemon_request(
            &mut request,
            &IndependentSpawnOptions {
                mode: SpawnMode::Independent,
                ..Default::default()
            },
        )
        .expect("independent memory fixture"),
    );
    let started = fixture_marker(directory.path(), "started");
    assert_eq!(
        started,
        u64::from(child.0.id()),
        "scheduler acknowledgement must identify the actual fixture"
    );
    let caller_counter = memory_counter(std::process::id());
    let target_counter = memory_counter(child.0.id());
    assert_ne!(caller_counter, target_counter);
    assert!(
        !target_counter
            .parent()
            .unwrap()
            .starts_with(caller_counter.parent().unwrap()),
        "independent allocation cannot be in a worker descendant"
    );
    // A Docker harness can name its finite outer cgroup. This is read-only:
    // never weaken memory.max or move a process to make this assertion pass.
    let outer = std::env::var_os("RUNNING_PROCESS_INDEPENDENT_OUTER_CGROUP").map(|path| {
        let path = std::fs::canonicalize(path).expect("outer cgroup path");
        let caller = std::fs::canonicalize(caller_counter.parent().unwrap()).unwrap();
        let target = std::fs::canonicalize(target_counter.parent().unwrap()).unwrap();
        assert!(
            caller.starts_with(&path) && target.starts_with(&path),
            "both worker and daemon must remain inside the supplied outer cgroup"
        );
        assert!(
            caller != path && target != path,
            "outer limit must be an ancestor of both groups"
        );
        let limit = std::fs::read_to_string(path.join("memory.max")).expect("outer memory limit");
        let limit: u64 = limit
            .trim()
            .parse()
            .expect("outer memory.max must be finite, not max");
        let counter = path.join("memory.current");
        let before = memory_bytes(&counter);
        assert!(
            i128::from(limit) - before >= 128 * 1024 * 1024,
            "fixture requires 128 MiB spare outer capacity; do not intentionally trigger OOM"
        );
        (counter, before, limit)
    });
    let caller_before = memory_bytes(&caller_counter);
    let target_before = memory_bytes(&target_counter);
    std::fs::write(directory.path().join("allocate"), b"go").expect("allocate signal");
    let resident = fixture_marker(directory.path(), "resident");
    assert_eq!(resident, 64 * 1024 * 1024);
    let target_delta = memory_bytes(&target_counter) - target_before;
    let caller_delta = memory_bytes(&caller_counter) - caller_before;
    let outer_delta = outer
        .as_ref()
        .map(|(counter, before, _)| memory_bytes(counter) - before);
    assert_eq!(
        memory_counter(std::process::id()),
        caller_counter,
        "caller must not migrate during the charging measurement"
    );
    assert_eq!(
        memory_counter(child.0.id()),
        target_counter,
        "daemon must not migrate during the charging measurement"
    );
    std::fs::write(directory.path().join("release"), b"done").expect("release memory");
    assert_eq!(
        fixture_marker(directory.path(), "released"),
        u64::from(child.0.id())
    );
    // Tolerate allocator/controller noise, but not a missing 64 MiB charge.
    assert!(
        target_delta >= 48 * 1024 * 1024,
        "target charge delta: {target_delta}"
    );
    assert!(
        caller_delta.abs() < 16 * 1024 * 1024,
        "caller charge delta: {caller_delta}; run in quiet dedicated cgroups"
    );
    if let Some(delta) = outer_delta {
        assert!(
            delta >= 48 * 1024 * 1024,
            "allocation must still charge outer cgroup: {delta}"
        );
    }
    if let Some((counter, _, original_limit)) = outer {
        let limit = std::fs::read_to_string(counter.parent().unwrap().join("memory.max")).unwrap();
        assert_eq!(
            limit.trim().parse::<u64>().unwrap(),
            original_limit,
            "independent spawning must not change the outer limit"
        );
    }
}
