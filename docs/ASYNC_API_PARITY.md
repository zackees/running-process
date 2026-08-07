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
| `close` | planned | `sync_process_close_is_idempotent` | `async_process_close_releases_the_actor_and_is_idempotent` | - | - |
| `close_stdin` | planned | - | - | - | - |
| `drain_combined` | planned | - | - | - | - |
| `drain_stream` | planned | - | - | - | - |
| `has_pending_combined` | planned | - | - | - | - |
| `has_pending_stream` | planned | - | - | - | - |
| `kill` | planned | - | - | - | - |
| `new` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `poll` | planned | `sync_process_poll_reports_none_before_exit_and_a_code_after` | `async_process_poll_reports_none_before_exit_and_a_status_after` | - | - |
| `read_combined` | planned | - | - | - | - |
| `read_stream` | planned | - | - | - | - |
| `returncode` | planned | `sync_process_returncode_is_none_until_the_child_exits` | `async_process_returncode_matches_the_exit_code_the_child_chose` | - | - |
| `start` | planned | - | - | - | - |
| `terminate` | planned | `sync_process_terminate_ends_the_child_like_kill` | `async_process_terminate_ends_the_child_like_kill` | - | - |
| `terminate_group_soft` | planned | `sync_terminate_group_soft_signals_a_group_the_child_owns` | `terminate_group_soft_signals_a_group_the_child_owns` | - | - |
| `wait` | planned | - | - | - | - |
| `with_observer` | planned | - | - | - | - |
| `write_stdin` | planned | - | - | - | - |
| `write_stdin_streaming` | planned | - | - | - | - |

### Rust `NativePtyProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `attach_idle_detector` | planned | `sync_pty_idle_detector_attaches_and_detaches` | `async_pty_idle_detector_attaches_detaches_and_reports_an_outcome` | - | - |
| `close_impl` | planned | - | - | - | - |
| `close_nonblocking` | planned | `sync_pty_close_nonblocking_is_safe_before_start` | `async_pty_close_nonblocking_is_safe_before_start` | - | - |
| `detach_idle_detector` | planned | `sync_pty_idle_detector_attaches_and_detaches` | `async_pty_idle_detector_attaches_detaches_and_reports_an_outcome` | - | - |
| `echo_enabled` | planned | `sync_pty_echo_state_round_trips` | `async_pty_echo_state_round_trips` | - | - |
| `kill_impl` | planned | - | - | - | - |
| `kill_tree_impl` | planned | `sync_pty_tree_termination_is_accepted_after_start` | `async_pty_tree_termination_dispatches_through_the_island` | - | - |
| `mark_reader_closed` | planned | `sync_pty_store_returncode_and_mark_reader_closed_are_observable` | `async_pty_store_returncode_and_mark_reader_closed_are_observable` | - | - |
| `new` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `pty_control_churn_bytes_total` | planned | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | - | - |
| `pty_input_bytes_total` | planned | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | - | - |
| `pty_newline_events_total` | planned | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | - | - |
| `pty_output_bytes_total` | planned | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | - | - |
| `pty_submit_events_total` | planned | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | - | - |
| `read_chunk_impl` | planned | - | - | - | - |
| `record_input_metrics` | planned | `sync_pty_metrics_track_recorded_input` | `async_pty_metrics_track_recorded_input_like_the_sync_counters` | - | - |
| `request_terminal_input_relay_stop` | planned | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | - | - |
| `resize_impl` | planned | - | - | - | - |
| `respond_to_queries_impl` | planned | `sync_pty_respond_to_queries_without_a_query_is_a_noop` | `async_pty_respond_to_queries_without_a_query_is_a_noop` | - | - |
| `send_interrupt_impl` | planned | `sync_pty_send_interrupt_before_start_errors` | `async_pty_send_interrupt_before_start_errors` | - | - |
| `set_echo` | planned | `sync_pty_echo_state_round_trips` | `async_pty_echo_state_round_trips` | - | - |
| `start_impl` | planned | - | - | - | - |
| `start_terminal_input_relay_impl` | planned | `sync_pty_start_terminal_input_relay_requires_a_running_pty` | `async_pty_start_terminal_input_relay_requires_a_running_pty` | - | - |
| `stop_terminal_input_relay_impl` | planned | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | - | - |
| `store_returncode` | planned | `sync_pty_store_returncode_and_mark_reader_closed_are_observable` | `async_pty_store_returncode_and_mark_reader_closed_are_observable` | - | - |
| `terminal_input_relay_active` | planned | `sync_pty_relay_is_inactive_until_started_and_stops_cleanly` | `async_pty_relay_is_inactive_until_started_and_stops_cleanly` | - | - |
| `terminate_impl` | planned | - | - | - | - |
| `terminate_tree_impl` | planned | `sync_pty_tree_termination_is_accepted_after_start` | `async_pty_tree_termination_dispatches_through_the_island` | - | - |
| `wait_and_drain_impl` | planned | `sync_pty_wait_and_drain_agrees_with_wait_on_an_exited_child` | `async_pty_wait_and_drain_agrees_with_wait_on_an_exited_child` | - | - |
| `wait_for_reader_closed_impl` | planned | `sync_pty_wait_for_reader_closed_reports_a_bounded_timeout` | `async_pty_wait_for_reader_closed_reports_a_bounded_timeout` | - | - |
| `wait_impl` | planned | - | - | - | - |
| `write_impl` | planned | - | - | - | - |

### Python `RunningProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `captured_output_bytes` | planned | - | - | - | - |
| `checkpoint` | planned | - | - | - | - |
| `close` | planned | - | - | - | - |
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
| `poll` | planned | - | - | - | - |
| `proc` | planned | - | - | - | - |
| `pseudo_terminal` | planned | - | - | - | - |
| `returncode` | planned | - | - | - | - |
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
| `terminate` | planned | - | - | - | - |
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
| `output_bytes` | planned | - | - | - | - |
| `output_text` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `poll` | planned | - | - | - | - |
| `read` | planned | - | - | - | - |
| `read_non_blocking` | planned | - | - | - | - |
| `read_text` | planned | - | - | - | - |
| `resize` | planned | - | - | - | - |
| `send_interrupt` | planned | - | - | - | - |
| `start` | planned | - | - | - | - |
| `start_terminal_input_relay` | planned | - | - | - | - |
| `stop_terminal_input_relay` | planned | - | - | - | - |
| `submit` | planned | - | - | - | - |
| `terminal_input_relay_active` | planned | - | - | - | - |
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
| `kill_process_tree` | planned | - | - | - | - |
| `launch_detached` | planned | - | - | - | - |
| `subprocess_run` | planned | - | - | - | - |
| `terminate_process_tree` | planned | - | - | - | - |

<!-- END GENERATED PARITY TABLE -->
