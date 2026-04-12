# Remaining PTY Migration — Implementation Strategy

Hand this to an agent. All patterns are documented in `docs/RUST_PYTHON_BOUNDARY.md`.

## What's done

The reader thread (Rust, no GIL) already handles: echo → stdout, idle detector → `record_output()`, output accounting → `AtomicUsize`, chunk buffering → `PtyReadShared`. Python calls `wait_and_drain()` or `wait_for_idle()` and gets a result tuple back. No Python in the hot loop.

## What's left (3 items)

### 1. `expect()` → fully native (no GIL needed)

**Current:** Python loop reads chunks, appends to string buffer, calls `find_expect_match()` (already Rust), retries on miss.

**Target:** Single Rust method on `NativePtyProcess`:

```rust
fn expect(&self, py: Python<'_>, pattern: &str, is_regex: bool, timeout: Option<f64>)
    -> PyResult<(String, Option<(String, usize, usize, Vec<String>)>)>
```

Returns `(status, match_details)` where status is `"match"`, `"eof"`, or `"timeout"`.

**How:** The reader thread already pushes chunks to `PtyReadShared`. The expect method (GIL released via `py.allow_threads`) reads from the chunk queue, appends to an internal `String` buffer, runs `find_expect_match()` each iteration, and returns on match/eof/timeout. No Python callback needed — `apply_expect_action` runs in Python after the match is returned.

**Effort:** ~60 lines of Rust. Pattern 3 from boundary doc.

### 2. `wait_for()` → Rust event pump + Python dispatch

**Current:** ~400-line Python polling loop mixing expect scanning, idle sampling, callback threads, and sleep.

**Target:** Rust produces typed events, Python dispatches them:

```rust
enum WaitEvent {
    ExpectMatch { condition_index: usize, matched: String, start: usize, end: usize, groups: Vec<String> },
    IdleTriggered { idle_for_seconds: f64 },
    ProcessExit { returncode: i32 },
    Timeout,
    OutputAvailable,  // new output for Python-side callback evaluation
}

fn wait_for_next_event(&self, py: Python<'_>, /* expect patterns, idle config, timeout */)
    -> PyResult<WaitEvent>
```

Python becomes:

```python
while True:
    event = self._proc.wait_for_next_event(patterns, idle_cfg, remaining_timeout)
    match event:
        case ExpectMatch(idx, ...):
            apply_expect_action(self, conditions[idx].action, match)
            if conditions[idx].on_callback: ...  # Python callback
            return result
        case IdleTriggered(...):
            if idle_condition.on_callback: ...  # Python callback
            return result
        case ProcessExit(code):
            return result
        case Timeout:
            return result
        case OutputAvailable:
            # evaluate Callback conditions (Python threads)
            continue
```

**How:** Rust holds the expect state (search offsets, armed flags) and idle detector reference. It blocks on the chunk condvar (GIL released), scans patterns per chunk, checks idle detector, checks exit. Returns the first event. Python only runs for callback dispatch — the "sleep and check" part is gone.

**Key rule:** Rust never calls Python. It returns events. Python calls back into Rust to continue.

**Effort:** ~150 lines of Rust, ~50 lines to simplify `wait_for()` in Python. Pattern 3 from boundary doc.

### 3. POSIX terminal input relay → optional native

**Current:** Python thread does `os.read(stdin)` → `self.write()` with `termios` raw mode.

**Target:** Rust thread does `libc::read(0)` → `write_pty_input()` with `libc::tcgetattr`/`tcsetattr`.

**How:** Same pattern as the reader thread. Spawn a Rust thread, share the PTY writer handle, use `Arc<AtomicBool>` for stop signal (already exists as `terminal_input_relay_stop`). Restore terminal on stop.

**Effort:** ~80 lines of Rust (`#[cfg(unix)]`). Pattern 1 + 2 from boundary doc. Low priority — the Python version works fine.

## Execution order

1. **`expect()`** — simplest, no callbacks, immediate user benefit
2. **`wait_for()` event pump** — biggest win, replaces 400 lines of Python polling
3. **POSIX relay** — optional, marginal benefit

## Rules

- Never acquire GIL from a Rust background thread
- Never hold a Rust mutex while returning to Python
- All blocking Rust methods must use `py.allow_threads`
- Return owned data across the boundary, never references into Rust state
- Test each phase independently before starting the next
