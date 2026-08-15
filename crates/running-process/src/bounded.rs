//! Bounded synchronous command execution helpers.

use super::*;

/// Run a command to completion while concurrently draining stdout and stderr.
///
/// The helper forces capture on regardless of `config.capture`, returns raw
/// stdout/stderr bytes, and kills the child before returning
/// [`ProcessError::Timeout`] when `timeout` elapses.
pub fn run_command(
    mut config: ProcessConfig,
    timeout: Option<Duration>,
) -> Result<RunOutput, ProcessError> {
    config.capture = true;
    let process = NativeProcess::new(config);
    process.start()?;

    let exit_code = match process.wait(timeout) {
        Ok(code) => code,
        Err(ProcessError::Timeout) => {
            match process.kill() {
                Ok(()) | Err(ProcessError::NotRunning) => {}
                Err(error) => return Err(error),
            }
            return Err(ProcessError::Timeout);
        }
        Err(error) => return Err(error),
    };

    Ok(RunOutput {
        stdout: process.captured_stdout_raw(),
        stderr: process.captured_stderr_raw(),
        exit_code,
    })
}

struct BoundedRunCleanup<'a> {
    process: &'a NativeProcess,
    armed: bool,
}

impl BoundedRunCleanup<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BoundedRunCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        // Error paths must not strand either the process tree or its capture
        // readers. Cancel first so even a failing/redundant kill cannot leave
        // threads blocked on pipes inherited by an escaped descendant.
        self.process.cancel_capture_io();
        let _ = self.process.poll();
        if self.process.returncode().is_none() {
            let _ = self.process.kill();
        } else {
            self.process.finish_capture_drain();
        }
        let _ = self
            .process
            .wait_for_capture_readers_with_deadline(kill_drain_deadline());
    }
}

fn run_native_process_bounded(
    process: NativeProcess,
    timeout: Option<Duration>,
    output_limit: usize,
) -> Result<RunOutput, ProcessError> {
    process.start()?;
    let mut cleanup = BoundedRunCleanup {
        process: &process,
        armed: true,
    };
    let started = Instant::now();

    let exit_code = loop {
        if process.shared.capture_overflowed.load(Ordering::Acquire) {
            return Err(ProcessError::OutputLimitExceeded {
                limit: output_limit,
            });
        }
        if let Some(code) = process.poll()? {
            process.finish_capture_drain();
            break code;
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return Err(ProcessError::Timeout);
        }
        thread::sleep(Duration::from_millis(5));
    };

    if !process.wait_for_capture_readers_with_deadline(kill_drain_deadline()) {
        return Err(ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "capture readers did not stop after process exit",
        )));
    }
    if process.shared.capture_overflowed.load(Ordering::Acquire) {
        return Err(ProcessError::OutputLimitExceeded {
            limit: output_limit,
        });
    }

    let output = RunOutput {
        stdout: process.captured_stdout_raw(),
        stderr: process.captured_stderr_raw(),
        exit_code,
    };
    cleanup.disarm();
    Ok(output)
}

/// Run a command with an aggregate stdout/stderr capture limit.
///
/// Once `output_limit` bytes have been retained, further output is drained
/// without allocation, the contained process is terminated, and
/// [`ProcessError::OutputLimitExceeded`] is returned. Timeout and overflow
/// paths wait for the cancelable capture readers to actually exit before
/// returning, including when a descendant escaped the process group while
/// retaining a pipe.
pub fn run_command_bounded(
    mut config: ProcessConfig,
    timeout: Option<Duration>,
    output_limit: usize,
) -> Result<RunOutput, ProcessError> {
    config.capture = true;
    config.create_process_group = true;
    let process = NativeProcess::new_with_capture_limit(config, output_limit);
    run_native_process_bounded(process, timeout, output_limit)
}

/// Run an existing [`std::process::Command`] with bounded capture.
///
/// Unlike [`run_command_bounded`], this entrypoint preserves non-UTF-8
/// program paths, arguments, environment keys/values, and every other command
/// setting exactly. Running-process still owns containment, console policy,
/// timeout cleanup, and stdout/stderr capture.
pub fn run_std_command_bounded(
    command: Command,
    timeout: Option<Duration>,
    output_limit: usize,
) -> Result<RunOutput, ProcessError> {
    let config = ProcessConfig {
        // The command override is consumed before this placeholder can be
        // inspected. Keeping ProcessConfig internal policy in one shape avoids
        // a second process-launch implementation.
        command: CommandSpec::Argv(vec!["running-process-command-override".to_string()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: None,
        create_process_group: true,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new_with_command_capture_limit(command, config, output_limit);
    run_native_process_bounded(process, timeout, output_limit)
}
