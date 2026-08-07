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
| `close` | planned | - | - | - | - |
| `close_stdin` | planned | - | - | - | - |
| `drain_combined` | planned | - | - | - | - |
| `drain_stream` | planned | - | - | - | - |
| `has_pending_combined` | planned | - | - | - | - |
| `has_pending_stream` | planned | - | - | - | - |
| `kill` | planned | - | - | - | - |
| `new` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `poll` | planned | - | - | - | - |
| `read_combined` | planned | - | - | - | - |
| `read_stream` | planned | - | - | - | - |
| `returncode` | planned | - | - | - | - |
| `start` | planned | - | - | - | - |
| `terminate` | planned | - | - | - | - |
| `terminate_group_soft` | planned | - | - | - | - |
| `wait` | planned | - | - | - | - |
| `with_observer` | planned | - | - | - | - |
| `write_stdin` | planned | - | - | - | - |
| `write_stdin_streaming` | planned | - | - | - | - |

### Rust `NativePtyProcess`

| Member | Status | Rust sync | Rust async | Python sync | Python async |
| --- | --- | --- | --- | --- | --- |
| `attach_idle_detector` | planned | - | - | - | - |
| `close_impl` | planned | - | - | - | - |
| `close_nonblocking` | planned | - | - | - | - |
| `detach_idle_detector` | planned | - | - | - | - |
| `echo_enabled` | planned | - | - | - | - |
| `finish_unix_teardown` | planned | - | - | - | - |
| `kill_impl` | planned | - | - | - | - |
| `kill_tree_impl` | planned | - | - | - | - |
| `mark_reader_closed` | planned | - | - | - | - |
| `new` | planned | - | - | - | - |
| `pid` | planned | - | - | - | - |
| `pty_control_churn_bytes_total` | planned | - | - | - | - |
| `pty_input_bytes_total` | planned | - | - | - | - |
| `pty_newline_events_total` | planned | - | - | - | - |
| `pty_output_bytes_total` | planned | - | - | - | - |
| `pty_submit_events_total` | planned | - | - | - | - |
| `read_chunk_impl` | planned | - | - | - | - |
| `record_input_metrics` | planned | - | - | - | - |
| `request_terminal_input_relay_stop` | planned | - | - | - | - |
| `resize_impl` | planned | - | - | - | - |
| `respond_to_queries_impl` | planned | - | - | - | - |
| `send_interrupt_impl` | planned | - | - | - | - |
| `set_echo` | planned | - | - | - | - |
| `start_impl` | planned | - | - | - | - |
| `start_terminal_input_relay_impl` | planned | - | - | - | - |
| `stop_terminal_input_relay_impl` | planned | - | - | - | - |
| `store_returncode` | planned | - | - | - | - |
| `terminal_input_relay_active` | planned | - | - | - | - |
| `terminate_impl` | planned | - | - | - | - |
| `terminate_tree_impl` | planned | - | - | - | - |
| `wait_and_drain_impl` | planned | - | - | - | - |
| `wait_for_reader_closed_impl` | planned | - | - | - | - |
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
