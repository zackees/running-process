# Async API parity

This is the checked-in parity inventory for #850 and its completion follow-up
#875. It records, for every public operation of every process surface this
library ships, whether an async equivalent exists and which tests prove it.

The source of truth is [`docs/async_api_parity.toml`](async_api_parity.toml).
The table below is generated from it. Everything else on this page is prose that
explains how to read and change that manifest.

## How the manifest is enforced

`ci.parity_manifest` runs as part of `./lint` and the preflight lint job. It:

1. **Discovers** the public members of each tracked surface directly from the
   source — `ast` for the Python classes, an indentation scanner for the Rust
   `impl` blocks. A new public sync method with no manifest row fails the gate,
   and a row naming a member that no longer exists fails it too. That is the
   drift guard a hand-maintained markdown table could never be.
2. **Verifies the evidence** on every row marked `implemented`: each of the four
   columns must either name a test function that exists in this tree, or carry
   an `n/a: <rationale>` marker.
3. **Reports the RED set** — every row still marked `planned`, with the issue
   that owns it.

A slice shows its RED state before the change and its GREEN state after with:

```bash
uv run --no-sync python -m ci.parity_manifest --strict   # RED: every planned row fails
uv run --no-sync python -m ci.parity_manifest            # GREEN: gate passes
```

`--strict` is what makes the manifest a failing test for missing async
operations without painting CI red for work that is honestly still queued. When
the last row flips, `require_no_planned` in the manifest is set to `true` and
`--strict` becomes the permanent default.

After editing the manifest, regenerate this page:

```bash
uv run --no-sync python -m ci.parity_manifest --write
```

## The compatibility gates

The parity manifest answers "does async have this yet?". Three sibling gates
answer the question #875 cares about more — "did adding async break the sync
API?" — and all four run in `./lint` and the preflight lint job.

| Gate | Artefact | Fails when |
| --- | --- | --- |
| `ci.parity_manifest` | `docs/async_api_parity.toml` | a public member has no row, a row names a member that no longer exists, or a cited test does not exist |
| `ci.api_snapshot` | `docs/api_snapshot_python.txt`, `docs/api_snapshot_rust.txt` | the public sync surface changes — a renamed method, a reordered or renamed parameter, a changed default, a dropped export |
| `ci.sync_test_audit` | `docs/sync_test_baseline.txt` | a baselined synchronous test disappears |
| `ci.async_compliance_guard` | `platform_compliance_baseline.toml` | a raw platform API is used outside the blessed capability layer |

Regenerating any of them is one command, so an intentional change costs one
reviewable diff:

```bash
uv run --no-sync python -m ci.api_snapshot --write
uv run --no-sync python -m ci.sync_test_audit --write
uv run --no-sync python -m ci.parity_manifest --write
```

### Why a test baseline as well as an API snapshot

The API snapshot catches a changed signature. It cannot catch the quieter
failure: the signature stays, the behaviour regresses, and the test that would
have noticed was deleted along the way. `ci.sync_test_audit` is a one-way
ratchet on removal — adding tests never requires an update, so the baseline
stays a record of coverage rather than churn nobody reads.

The audit counts `#[test]` and `def test_*` only. `#[tokio::test]` and
`async def` are excluded on purpose: an audit that counted async tests as sync
coverage would report health while sync tests were being replaced by them,
which is precisely the substitution #875 asks us to rule out.

### Downstream fixtures

`tests/test_downstream_fixtures.py` runs FastLED-shaped consumer code in a
*fresh* interpreter: legacy imports, synchronous calls, and an event-loop policy
that raises if anything constructs a loop. A second fixture exercises sync and
async in one process in both orders, plus sync work from inside a running loop.

That file also carries a control test that splices a deliberate loop creation
into the fixture and requires it to fail. The first version of the loop
assertion inspected the policy's stored loop after the fact and never fired at
all — `new_event_loop()` builds a loop without installing it. Without the
control, "no loop was created" and "the check is broken" look identical.

## Compatibility rules

- Existing sync names, signatures, defaults, and import paths remain supported.
  A change to any of them needs a recorded justification under #849 and
  downstream review; the parity work is not a licence to rename.
- Async rows must resolve to native Rust futures or actor commands. Python
  executors, `asyncio.to_thread`, polling loops, and reader threads are
  forbidden, and `ci.async_compliance_guard` enforces that separately.
- An `n/a` rationale is a claim that the operation cannot meaningfully exist on
  that surface — not that nobody got to it. "Not implemented yet" is `planned`.

## Reading the table

`Status` is `implemented` when every applicable column below it names a real
test, and `planned` when async parity for that member is still outstanding. A
cell showing `-` on a planned row simply means the contract test has not been
written yet; on an implemented row every cell is filled.

<!-- BEGIN GENERATED PARITY TABLE -->

This table is generated from `docs/async_api_parity.toml` by `ci.parity_manifest`. Edit the manifest, then run `uv run --no-sync python -m ci.parity_manifest --write`.

### Rust `NativeProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `captured_combined` | planned | - | - | - | - |
| `captured_combined_bytes` | planned | - | - | - | - |
| `captured_stderr` | planned | - | - | - | - |
| `captured_stdout` | planned | - | - | - | - |
| `captured_stream_bytes` | planned | - | - | - | - |
| `clear_captured_combined` | planned | - | - | - | - |
| `clear_captured_stream` | planned | - | - | - | - |
| `close` | implemented | `sync_process_close_is_idempotent` | `async_process_close_releases_the_actor_and_is_idempotent` | `test_sync_close_is_idempotent` | `test_async_close_is_idempotent_and_releases_the_handle` |
| `close_stdin` | planned | - | - | - | - |
| `drain_combined` | planned | - | - | - | - |
| `drain_stream` | planned | - | - | - | - |
| `has_pending_combined` | planned | - | - | - | - |
| `has_pending_stream` | planned | - | - | - | - |
| `kill` | planned | - | - | - | - |
| `new` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `poll` | implemented | `sync_process_poll_reports_none_before_exit_and_a_code_after` | `async_process_poll_reports_none_before_exit_and_a_status_after` | `test_sync_poll_reports_none_while_running_and_a_code_after_exit` | `test_async_poll_reports_none_while_running_and_a_code_after_exit` |
| `read_combined` | planned | - | - | - | - |
| `read_stream` | planned | - | - | - | - |
| `returncode` | implemented | `sync_process_returncode_is_none_until_the_child_exits` | `async_process_returncode_matches_the_exit_code_the_child_chose` | `test_sync_returncode_is_none_while_the_child_runs` | `test_async_returncode_matches_the_code_the_child_chose` |
| `start` | planned | - | - | - | - |
| `terminate` | implemented | `sync_process_terminate_ends_the_child_like_kill` | `async_process_terminate_ends_the_child_like_kill` | `test_sync_terminate_ends_the_child_like_kill` | `test_async_terminate_ends_the_child_like_kill` |
| `terminate_group_soft` | implemented | `sync_terminate_group_soft_signals_a_group_the_child_owns` | `terminate_group_soft_signals_a_group_the_child_owns` | n/a: RunningProcess exposes no process-group terminate; the sync Python surface has never had one and #849 governs adding to it | `test_async_terminate_group_soft_signals_an_owned_group` |
| `wait` | planned | - | - | - | - |
| `with_observer` | planned | - | - | - | - |
| `write_stdin` | planned | - | - | - | - |
| `write_stdin_streaming` | planned | - | - | - | - |

### Rust `NativePtyProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `attach_idle_detector` | planned | `sync_pty_idle_detector_attaches_and_detaches` | `async_pty_idle_detector_attaches_detaches_and_reports_an_outcome` | - | - |
| `close_impl` | planned | - | - | - | - |
| `close_nonblocking` | implemented | `sync_pty_close_nonblocking_is_safe_before_start` | `async_pty_close_nonblocking_is_safe_before_start` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_close_nonblocking_is_safe_before_start` |
| `detach_idle_detector` | planned | `sync_pty_idle_detector_attaches_and_detaches` | `async_pty_idle_detector_attaches_detaches_and_reports_an_outcome` | - | - |
| `echo_enabled` | implemented | `sync_pty_echo_state_round_trips` | `async_pty_echo_state_round_trips` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_echo_state_round_trips` |
| `kill_impl` | planned | - | - | - | - |
| `kill_tree_impl` | implemented | `sync_pty_tree_termination_is_accepted_after_start` | `async_pty_tree_termination_dispatches_through_the_island` | n/a: PseudoTerminalProcess does not expose this PTY primitive; it is an internal detail of the sync facade's own read/wait loop | `test_async_pty_tree_termination_is_accepted_after_start` |
| `mark_reader_closed` | implemented | `sync_pty_store_returncode_and_mark_reader_closed_are_observable` | `async_pty_store_returncode_and_mark_reader_closed_are_observable` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_store_returncode_and_mark_reader_closed` |
| `new` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `pty_control_churn_bytes_total` | implemented | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_metrics_track_recorded_input` |
| `pty_input_bytes_total` | implemented | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | `test_sync_pty_output_bytes_start_at_zero` | `test_async_pty_metrics_track_recorded_input` |
| `pty_newline_events_total` | implemented | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_metrics_track_recorded_input` |
| `pty_output_bytes_total` | implemented | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | `test_sync_pty_output_bytes_start_at_zero` | `test_async_pty_metrics_track_recorded_input` |
| `pty_submit_events_total` | implemented | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_metrics_track_recorded_input` |
| `read_chunk_impl` | planned | - | - | - | - |
| `record_input_metrics` | implemented | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | `test_sync_pty_output_bytes_start_at_zero` | `test_async_pty_metrics_track_recorded_input` |
| `request_terminal_input_relay_stop` | implemented | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | `test_sync_pty_terminal_input_relay_is_inactive_until_started` | `test_async_pty_relay_is_inactive_until_started` |
| `resize_impl` | planned | - | - | - | - |
| `respond_to_queries_impl` | implemented | `sync_pty_respond_to_queries_without_a_query_is_a_noop` | `async_pty_respond_to_queries_without_a_query_is_a_noop` | n/a: PseudoTerminalProcess does not expose this PTY primitive; it is an internal detail of the sync facade's own read/wait loop | `test_async_pty_respond_to_queries_without_a_query_is_a_noop` |
| `send_interrupt_impl` | implemented | `sync_pty_send_interrupt_before_start_errors` | `async_pty_send_interrupt_before_start_errors` | `test_sync_pty_send_interrupt_before_start_is_rejected` | `test_async_pty_send_interrupt_before_start_raises` |
| `set_echo` | implemented | `sync_pty_echo_state_round_trips` | `async_pty_echo_state_round_trips` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_echo_state_round_trips` |
| `start_impl` | planned | - | - | - | - |
| `start_terminal_input_relay_impl` | implemented | `sync_pty_start_terminal_input_relay_requires_a_running_pty` | `async_pty_start_terminal_input_relay_requires_a_running_pty` | n/a: PseudoTerminalProcess does not expose this PTY primitive; it is an internal detail of the sync facade's own read/wait loop | `test_async_pty_start_relay_requires_a_running_pty` |
| `stop_terminal_input_relay_impl` | implemented | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | `test_sync_pty_terminal_input_relay_is_inactive_until_started` | `test_async_pty_relay_is_inactive_until_started` |
| `store_returncode` | implemented | `sync_pty_store_returncode_and_mark_reader_closed_are_observable` | `async_pty_store_returncode_and_mark_reader_closed_are_observable` | n/a: not exposed on PseudoTerminalProcess; the sync facade owns this bookkeeping internally rather than publishing it | `test_async_pty_store_returncode_and_mark_reader_closed` |
| `terminal_input_relay_active` | implemented | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | `test_sync_pty_terminal_input_relay_is_inactive_until_started` | `test_async_pty_relay_is_inactive_until_started` |
| `terminate_impl` | planned | - | - | - | - |
| `terminate_tree_impl` | implemented | `sync_pty_tree_termination_is_accepted_after_start` | `async_pty_tree_termination_dispatches_through_the_island` | n/a: PseudoTerminalProcess does not expose this PTY primitive; it is an internal detail of the sync facade's own read/wait loop | `test_async_pty_tree_termination_is_accepted_after_start` |
| `wait_and_drain_impl` | implemented | `sync_pty_wait_and_drain_agrees_with_wait_on_an_exited_child` | `async_pty_wait_and_drain_agrees_with_wait_on_an_exited_child` | n/a: PseudoTerminalProcess does not expose this PTY primitive; it is an internal detail of the sync facade's own read/wait loop | `test_async_pty_wait_and_drain_agrees_with_wait` |
| `wait_for_reader_closed_impl` | implemented | `sync_pty_wait_for_reader_closed_reports_a_bounded_timeout` | `async_pty_wait_for_reader_closed_reports_a_bounded_timeout` | n/a: PseudoTerminalProcess does not expose this PTY primitive; it is an internal detail of the sync facade's own read/wait loop | `test_async_pty_wait_for_reader_closed_is_bounded` |
| `wait_impl` | planned | - | - | - | - |
| `write_impl` | planned | - | - | - | - |

### Python `RunningProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `captured_output_bytes` | planned | - | - | - | - |
| `checkpoint` | planned | - | - | - | - |
| `close` | implemented | `sync_process_close_is_idempotent` | `async_process_close_releases_the_actor_and_is_idempotent` | `test_sync_close_is_idempotent` | `test_async_close_is_idempotent_and_releases_the_handle` |
| `combined_output` | planned | - | - | - | - |
| `combined_stream` | planned | - | - | - | - |
| `discard_captured_output` | planned | - | - | - | - |
| `drain_combined` | planned | - | - | - | - |
| `drain_stderr` | planned | - | - | - | - |
| `drain_stdout` | planned | - | - | - | - |
| `duration` | planned | - | - | - | - |
| `end_time` | planned | - | - | - | - |
| `exec_script` | planned | - | - | - | - |
| `exit_status` | planned | - | - | - | - |
| `expect` | planned | - | - | - | - |
| `finished` | planned | - | - | - | - |
| `get_command_str` | planned | - | - | - | - |
| `get_next_line` | planned | - | - | - | - |
| `get_next_line_non_blocking` | planned | - | - | - | - |
| `get_next_stderr_line` | planned | - | - | - | - |
| `get_next_stdout_line` | planned | - | - | - | - |
| `has_pending_output` | planned | - | - | - | - |
| `has_pending_stderr` | planned | - | - | - | - |
| `has_pending_stdout` | planned | - | - | - | - |
| `idle_timeout_enabled` | planned | - | - | - | - |
| `interactive` | planned | - | - | - | - |
| `interactive_launch_spec` | planned | - | - | - | - |
| `is_running` | planned | - | - | - | - |
| `is_runninng` | planned | - | - | - | - |
| `is_started` | planned | - | - | - | - |
| `kill` | planned | - | - | - | - |
| `line_iter` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `poll` | implemented | `sync_process_poll_reports_none_before_exit_and_a_code_after` | `async_process_poll_reports_none_before_exit_and_a_status_after` | `test_sync_poll_reports_none_while_running_and_a_code_after_exit` | `test_async_poll_reports_none_while_running_and_a_code_after_exit` |
| `proc` | planned | - | - | - | - |
| `pseudo_terminal` | planned | - | - | - | - |
| `returncode` | implemented | `sync_process_returncode_is_none_until_the_child_exits` | `async_process_returncode_matches_the_exit_code_the_child_chose` | `test_sync_returncode_is_none_while_the_child_runs` | `test_async_returncode_matches_the_code_the_child_chose` |
| `run` | planned | - | - | - | - |
| `run_streaming` | planned | - | - | - | - |
| `send_interrupt` | planned | - | - | - | - |
| `start` | planned | - | - | - | - |
| `start_time` | planned | - | - | - | - |
| `stderr` | planned | - | - | - | - |
| `stderr_stream` | planned | - | - | - | - |
| `stdout` | planned | - | - | - | - |
| `stdout_stream` | planned | - | - | - | - |
| `stream_iter` | planned | - | - | - | - |
| `submit` | planned | - | - | - | - |
| `terminate` | implemented | `sync_process_terminate_ends_the_child_like_kill` | `async_process_terminate_ends_the_child_like_kill` | `test_sync_terminate_ends_the_child_like_kill` | `test_async_terminate_ends_the_child_like_kill` |
| `wait` | planned | - | - | - | - |
| `wait_for` | planned | - | - | - | - |
| `wait_for_expect` | planned | - | - | - | - |
| `wait_for_idle` | planned | - | - | - | - |
| `write` | planned | - | - | - | - |

### Python `PseudoTerminalProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `available` | planned | - | - | - | - |
| `checkpoint` | planned | - | - | - | - |
| `close` | planned | - | - | - | - |
| `discard_output` | planned | - | - | - | - |
| `drain` | planned | - | - | - | - |
| `drain_echo` | planned | - | - | - | - |
| `exit_status` | planned | - | - | - | - |
| `expect` | planned | - | - | - | - |
| `idle_timeout_enabled` | planned | - | - | - | - |
| `interrupt_and_wait` | planned | - | - | - | - |
| `is_running` | planned | - | - | - | - |
| `kill` | planned | - | - | - | - |
| `output` | planned | - | - | - | - |
| `output_bytes` | implemented | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | `test_sync_pty_output_bytes_start_at_zero` | `test_async_pty_metrics_track_recorded_input` |
| `output_text` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `poll` | planned | - | - | - | - |
| `read` | planned | - | - | - | - |
| `read_non_blocking` | planned | - | - | - | - |
| `read_text` | planned | - | - | - | - |
| `resize` | planned | - | - | - | - |
| `send_interrupt` | implemented | `sync_pty_send_interrupt_before_start_errors` | `async_pty_send_interrupt_before_start_errors` | `test_sync_pty_send_interrupt_before_start_is_rejected` | `test_async_pty_send_interrupt_before_start_raises` |
| `start` | planned | - | - | - | - |
| `start_terminal_input_relay` | planned | - | - | - | - |
| `stop_terminal_input_relay` | implemented | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | `test_sync_pty_terminal_input_relay_is_inactive_until_started` | `test_async_pty_relay_is_inactive_until_started` |
| `submit` | planned | - | - | - | - |
| `terminal_input_relay_active` | implemented | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | `test_sync_pty_terminal_input_relay_is_inactive_until_started` | `test_async_pty_relay_is_inactive_until_started` |
| `terminate` | planned | - | - | - | - |
| `wait` | planned | - | - | - | - |
| `wait_for` | planned | - | - | - | - |
| `wait_for_expect` | planned | - | - | - | - |
| `wait_for_idle` | planned | - | - | - | - |
| `write` | planned | - | - | - | - |

### Python `InteractiveProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `close` | planned | - | - | - | - |
| `exit_status` | planned | - | - | - | - |
| `kill` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `poll` | planned | - | - | - | - |
| `send_interrupt` | planned | - | - | - | - |
| `start` | planned | - | - | - | - |
| `terminate` | planned | - | - | - | - |
| `wait` | planned | - | - | - | - |

### Python module-level helpers

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `find_processes_by_originator` | planned | - | - | - | - |
| `get_process_tree_info` | planned | - | - | - | - |
| `kill_process_tree` | implemented | `sync_kill_tree_terminates_the_started_child` | `missing_pid_is_a_successful_noop_on_the_async_form` | `test_kill_process_tree_kills_parent_and_child` | `test_module_level_kill_process_tree_kills_a_real_child` |
| `launch_detached` | planned | - | - | - | - |
| `subprocess_run` | planned | - | - | - | - |
| `terminate_process_tree` | planned | - | - | - | - |

<!-- END GENERATED PARITY TABLE -->
