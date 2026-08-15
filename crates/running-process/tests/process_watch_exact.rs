#![cfg(target_os = "linux")]

use std::time::Duration;

use running_process::{
    CommandSpec, NativeProcess, ObservationGrade, ObservationPolicy, ProcessConfig, ProcessWatch,
    ProcessWatchRead, StderrMode, StdinMode,
};

#[test]
fn exact_trace_observes_rapid_execs_and_normalizes_minus_one() {
    let exec_watch = ProcessWatch::on_exec(
        Some("true".to_owned()),
        None,
        None,
        None,
        Duration::ZERO,
        "rapid-true",
    )
    .unwrap();
    let any_exec_watch = ProcessWatch::on_exec(
        None,
        None,
        None,
        None,
        Duration::ZERO,
        "descendant-execs-only",
    )
    .unwrap();
    let exit_watch = ProcessWatch::on_exit(
        Some(-1),
        None,
        None,
        None,
        Some(1),
        Duration::ZERO,
        "minus-one",
    )
    .unwrap();
    let config = ProcessConfig {
        command: CommandSpec::Argv(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "i=0; while [ $i -lt 20 ]; do /bin/true; i=$((i+1)); done; /bin/sh -c 'exit 255'; exit 0"
                .to_owned(),
        ]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let (process, subscriber) = NativeProcess::with_process_watches(
        config,
        vec![exec_watch, any_exec_watch, exit_watch],
        ObservationPolicy::RequireExact,
    )
    .unwrap();
    let mut cursor = subscriber.cursor();
    process.start().unwrap();
    assert_eq!(process.wait(Some(Duration::from_secs(10))).unwrap(), 0);

    let matches = subscriber.snapshot();
    let execs = matches
        .iter()
        .filter(|item| item.watch.label == "rapid-true")
        .count();
    assert_eq!(execs, 20, "exact tracing lost a rapid /bin/true exec");
    assert_eq!(
        matches
            .iter()
            .filter(|item| item.watch.label == "descendant-execs-only")
            .count(),
        21,
        "the launched-tree watch must not publish the root's initial exec",
    );
    assert!(matches.iter().any(|item| item.watch.label == "minus-one"));
    assert!(matches
        .iter()
        .filter(|item| item.watch.label == "rapid-true")
        .all(|item| item.event.observation_grade == ObservationGrade::ExactTrace));

    let mut cursor_matches = 0;
    loop {
        match cursor.read_next(Some(Duration::from_secs(1))) {
            ProcessWatchRead::Match(_) => cursor_matches += 1,
            ProcessWatchRead::Loss(loss) => panic!("unexpected trace loss: {}", loss.reason),
            ProcessWatchRead::Eof => break,
            ProcessWatchRead::Gap(gap) => panic!("unexpected cursor gap: {gap:?}"),
            ProcessWatchRead::Timeout => panic!("watch cursor did not reach EOF"),
        }
    }
    assert_eq!(cursor_matches, matches.len());
}

#[test]
fn exact_trace_preserves_negative_signal_return_codes() {
    let watch = ProcessWatch::on_spawn(None, Some(1), Duration::ZERO, "enable-exact").unwrap();
    let config = ProcessConfig {
        command: CommandSpec::Argv(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "kill -KILL $$".to_owned(),
        ]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let (process, _subscriber) =
        NativeProcess::with_process_watches(config, vec![watch], ObservationPolicy::RequireExact)
            .unwrap();
    process.start().unwrap();
    assert_eq!(process.wait(Some(Duration::from_secs(10))).unwrap(), -9);
}
