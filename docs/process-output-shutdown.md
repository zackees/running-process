# Process-output shutdown

Coordinated with [kernal-api #13](https://github.com/zackees/kernal-api/issues/13)
and [its draft integration PR](https://github.com/zackees/kernal-api/pull/153).
This branch is an implementation prerequisite, not a release or a claim of
completed six-target runtime validation.

## Platform primitive

`PlatformOutput::shutdown` consumes one actor-owned output reader. On Linux and
macOS, dropping the readiness-based pipe releases its reader storage. On Windows,
a canceled Tokio read may still own a blocking task and buffer. The primitive
repeatedly requests handle-specific cancellation, polls that pending read to
completion, and drops the reader before returning. A cancellation request, a
direct-child exit, and dropping a read future are not cleanup acknowledgements.

The live reader exclusively owns the handle throughout this sequence. Retrying
also handles a blocking job that has not entered its syscall yet. No worker
thread identifier is targeted, and no new runtime or pipe implementation is
introduced. The owning actor must retain the shutdown future until completion;
an observer timeout must retain the outstanding storage allowance.

Microsoft explicitly documents that
[CancelIoEx can cancel synchronous I/O](https://devblogs.microsoft.com/oldnewthing/20170928-00/?p=97105).
Its [completion contract](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-cancelioex)
requires observing completion separately. The current Tokio 1.52.3 lockfile's
`io/blocking.rs` recovers its buffer when a Busy read completes. Rust 1.95 maps
native error 995 to `TimedOut`, so the implementation recognizes the exact native
code rather than treating every timeout as expected cancellation.

## Session integration

`AsyncProcessSessionControl::request_output_shutdown` synchronously latches the
request without acquiring the output consumer. It does not acknowledge cleanup.
`AsyncProcessSessionOutput::shutdown` also requests shutdown, closes and discards
the queue, and waits for the persistent result of joining both pumps. The unsplit
session offers `request_output_shutdown` and `shutdown_output` forwarding methods.
Already-returned events remain caller-owned and are outside this acknowledgement.

Each pump borrows its reader from an outer cleanup scope. Shutdown interrupts
pending reads and all event sends; their futures and buffers are dropped before
the reader shutdown primitive is awaited. An inner pump panic follows that same
cleanup path and remains an error. The actor retains actual task handles and
publishes completion only after joining both. Lifecycle errors request output
cleanup rather than returning early; actor unwind also latches the request, but
loss of the completion sender is an error, never successful acknowledgement.

Canceling an output-shutdown observer leaves the request, tasks, and completion
state alive. A retry observes the same result. At a facade mutex boundary, request
shutdown **before** acquiring the lock held by `next_output`. Ordinary output
Drop still detaches delivery and drains; direct-child kill/reap remains separate.

## Local evidence

The initial checks below ran on the shared development base `f062b52`. The scoped
change was subsequently moved onto main `0e39d9d`. On that isolated base, the
platform suite passes all 85 tests (zero ignored), and both the sync API snapshot
and parity manifest gates pass.

Session integration on the isolated base passes six focused Linux tests/helper
cases: saturated queue, silent pending read, descendant-held pipe after direct
reaping, canceled/retried observers with concurrent completion watchers, and
panic cleanup. Per-session test-only barriers retain both readers and prove
completion stays pending until cleanup is released. The panic regression was
RED when unwinding bypassed reader cleanup (`20260913T122402Z`) and GREEN after
restoring the outer cleanup boundary. The earlier API compile-RED is recorded
in `20260913T121441Z`.

The 15 existing async-session regressions pass unchanged. Strict library-test
Clippy (`--lib --profile test`, kernel-substrate only), the spawn-path guard,
and Ruff pass. The complete kernel-substrate library harness passes all 208
tests with `--test-threads=1`. Its parallel run passed 207 and failed the existing
global blocking-island permit-count assertion (one available permit rather than
two); that assertion observes the shared semaphore while other tests dispatch
work. This is not a claim that the parallel full-suite run passed.

The isolated Windows x86-64 platform test binary also cross-compiles, including
a new regression that occupies the sole blocking worker, queues a silent pipe
read, verifies shutdown remains pending, then releases the worker and requires
read completion. Native execution of this race test remains outstanding.
The isolated kernel-substrate session library also cross-compiles for Windows
x86-64; that library build does not compile or execute its unit-test harness.

Initial platform-primitive checks on the shared development base:

- Focused RED: missing shutdown method, Soldr log `20260913T120326Z`.
- Focused Linux GREEN: two tests/helper cases. The regression establishes a
  pending read on a silent live child, then requires shutdown before killing it.
- Strict Linux platform Clippy passes with `--deny warnings`.
- Windows x86-64 MSVC library and test cross-builds pass. This is not native execution.
- The sync API snapshot gate has an existing mismatch: its recorded Rust exports
  omit `spawn_contract` and `spawn_dispatch` exports already present at that shared
  development base. This change touches neither the snapshot inputs nor those exports; the
  unrelated snapshot is left unchanged. This is not a passing snapshot gate.
- Linux platform suite: 101 passed, three existing systemd-dependent tests
  ignored. The default local linker omitted the GNU build ID required by an
  existing test. Explicitly relinking the test harness with the following
  command supplies it; `readelf -n` confirms the note. No test was weakened.

```sh
SOLDR_LINKER=default soldr --no-cache cargo rustc --locked \
  -p running-process-platform-internal --no-default-features \
  --features async-process --lib --profile test -j1 \
  -- -C link-arg=-Wl,--build-id=sha1
```

Run the resulting native test harness directly after this command.

## Remaining acceptance

The broad native run [34757433276](https://github.com/zackees/running-process/actions/runs/34757433276)
at `1943831` passed all six session shutdown tests/helper cases on Windows
x86-64. Its Windows suite then stopped after 1,439 passes at the existing PTY
test `raw_ansi_bytes_flow_through_pty_to_ring_buffer`: the expected clear-screen
escape was absent and its backlog contained only a cursor-position query.
The lower-level platform tests had not run, so that run does not prove the
queued-before-syscall cancellation regression. The macOS ARM job passed.

`.github/workflows/ci-output-shutdown.yml` adds focused native jobs for all six
Linux/macOS/Windows x86-64/ARM64 combinations, independent of the PTY suite.
Reproduce its test step with:

```sh
uv run --no-project --module ci.output_shutdown_native
```

It uses the repository Cargo router and existing nextest per-test deadlines,
disables retries, and requires named PASS evidence for six session cases plus
two platform cases (three on Windows, including the queued-read race). The
fixtures are self-executing unit tests, so this narrow lane needs neither the
testbins package nor a Python extension wheel. The evidence parser has five
unit tests, including a RED/GREEN regression for Soldr's timestamped output.
The complete focused runner passes locally on Linux x86-64.

Focused six-target native acceptance remains outstanding. The coordinated
kernal-api branch now uses this acknowledgement in its native-output ledger
(`0796010`), but its guest compiler/cache workflow remains incomplete. Neither
direct-child `wait` nor ordinary output Drop is a replacement for acknowledged
shutdown; these prerequisite changes do not complete kernal-api #13.
