//! Bounded synchronous command execution helpers.

use super::*;

/// Optional containment policy for [`run_std_command_bounded_with_options`].
///
/// The default keeps the established bounded-run behavior. Setting
/// [`Self::kill_when_owner_dies`] asks the host to terminate the launched
/// command when the process that called the bounded runner dies:
///
/// - Linux installs `PR_SET_PDEATHSIG` in the child before `exec`, including
///   a parent-race guard.
/// - macOS installs the existing kqueue supervisor before `exec`.
/// - Windows reuses the [`NativeProcess`] per-spawn kill-on-close job; it does
///   not create a second job object.
///
/// On Linux and macOS this policy guarantees only direct-child termination.
/// Those operating systems do not provide a parent-death primitive that can
/// atomically terminate a process group, so ordinary descendants may outlive
/// their direct parent. Windows Job Object containment covers the whole job.
/// Callers that need Unix tree cleanup must use an application-level
/// supervisor or an explicit tree-containment mechanism.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BoundedRunOptions {
    /// Terminate the launched command if its bounded-run owner exits.
    ///
    /// Linux and macOS guarantee direct-child termination only; Windows
    /// terminates the Job Object tree.
    kill_when_owner_dies: bool,

    /// Semantic launch priority passed to the platform command configuration.
    ///
    /// On Unix this is the child nice value. On Windows it selects the
    /// existing creation priority-class mapping; numeric nice values are not
    /// portable priority levels across those platforms.
    nice: Option<i32>,
}

impl BoundedRunOptions {
    /// Set whether the bounded runner asks the host to terminate the direct
    /// child if its owner exits unexpectedly.
    #[must_use]
    pub fn kill_when_owner_dies(mut self, enabled: bool) -> Self {
        self.kill_when_owner_dies = enabled;
        self
    }

    /// Set the semantic launch priority for the bounded child.
    #[must_use]
    pub fn nice(mut self, nice: Option<i32>) -> Self {
        self.nice = nice;
        self
    }
}

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
    run_std_command_bounded_with_options(
        command,
        timeout,
        output_limit,
        BoundedRunOptions::default(),
    )
}

/// Run an existing [`std::process::Command`] with bounded capture and
/// explicit containment options.
///
/// Like [`run_std_command_bounded`], this preserves the supplied command
/// losslessly, forces null stdin plus separately captured stdout/stderr, and
/// keeps bounded-run's process group and cleanup behavior. The options only
/// add host-native containment; they do not add a second launch path.
pub fn run_std_command_bounded_with_options(
    command: Command,
    timeout: Option<Duration>,
    output_limit: usize,
    options: BoundedRunOptions,
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
        nice: options.nice,
        address_space_limit_bytes: None,
    };
    let process = NativeProcess::new_with_command_capture_limit(
        command,
        config,
        output_limit,
        options.kill_when_owner_dies,
    );
    run_native_process_bounded(process, timeout, output_limit)
}
