# Rust ↔ Python Boundary Design

This document defines how Rust and Python communicate in this codebase without deadlocking, and why each pattern was chosen.

## The core problem

Rust owns background threads (PTY reader, exit watcher, relay worker). Python owns callbacks, the GIL, and the public API. When these two runtimes try to coordinate through shared locks or direct calls, deadlocks happen:

```
BAD: Rust reader thread → grab mutex → call Python callback → needs GIL
     Python main thread → holds GIL → calls Rust method → needs same mutex
     → deadlock
```

## The core solution

**Never call Python from a Rust hot loop.** Decouple production from consumption.

```
Rust thread → produce event → write to shared state → continue
Python      → poll result or read shared state → act on it
```

No GIL acquired from Rust threads. No Rust locks held while calling Python.

## Pattern 1: Atomic flags (`Arc<AtomicBool>`)

For simple on/off control state shared between Rust and Python.

### When to use

- `is_enabled` / `stop_requested` / `cancelled` / `echo_on`
- One side sets it, the other side checks it
- No compound state — just a boolean switch

### How it works

Both sides hold a clone of `Arc<AtomicBool>`. Reads and writes are CPU-level atomic operations — no mutex, no GIL, effectively non-blocking.

```
Python:  flag.store(true)    ← flip the switch
Rust:    flag.load()         ← glance at it, act accordingly
```

### Current instances

| Flag | Python sets | Rust reads | Location |
|------|-----------|------------|----------|
| `idle_timeout_enabled` | `SignalBool.value = False` | `IdleDetectorCore.enabled.load()` in wait loop | `NativeSignalBool.value` |
| `echo` | `proc.set_echo(True)` | Reader thread checks per chunk | `NativePtyProcess.echo` |
| `terminal_input_relay_stop` | `stop_terminal_input_relay()` | Relay worker checks per iteration | `NativePtyProcess.terminal_input_relay_stop` |
| `terminal_input_relay_active` | Relay worker sets on exit | Python reads status | `NativePtyProcess.terminal_input_relay_active` |

### Why not a mutex

A mutex for one boolean introduces: lock acquisition, contention, lock ordering risks, temptation to hold it too long. An atomic avoids all of that. It is the right primitive for a single flag.

### What atomics do NOT solve

- Compound state transitions (multiple related fields)
- Swapping structured objects (use `Arc<Mutex<Option<Arc<T>>>>`)
- Queues or ordered events
- "Check then act" sequences that must be atomic

## Pattern 2: Arc-shared core structs

For Rust-internal state that background threads and the Python-facing layer both need.

### When to use

- A Rust struct has methods called from both a background thread and from Python-facing `#[pymethods]`
- The struct uses `Mutex` + `Condvar` internally (already thread-safe)
- Python should not hold a reference into the struct during Rust-side waits

### How it works

Extract the state into a plain Rust struct (not `#[pyclass]`). Wrap it in `Arc`. The `#[pyclass]` holds `Arc<Core>`. Background threads get their own `Arc::clone`.

```
#[pyclass]
struct NativeIdleDetector {
    core: Arc<IdleDetectorCore>,   ← Python-facing wrapper
}

struct NativePtyProcess {
    idle_detector: Arc<Mutex<Option<Arc<IdleDetectorCore>>>>,  ← reader thread access
}

// Reader thread (no GIL):
if let Some(detector) = idle_detector.lock().as_ref() {
    detector.record_output(data);  ← pure Rust, no Python
}

// Python:
let result = py.allow_threads(|| detector.core.wait(timeout));  ← GIL released
```

### Current instances

| Core struct | PyClass wrapper | Background consumer | Shared via |
|-------------|-----------------|--------------------|----|
| `IdleDetectorCore` | `NativeIdleDetector` | PTY reader thread | `Arc<Mutex<Option<Arc<IdleDetectorCore>>>>` |

### The attach/detach pattern

When a background thread needs temporary access to a core struct (e.g. idle detector only exists during `wait_for_idle`):

```python
proc.attach_idle_detector(detector)   # store Arc in reader-visible slot
result = proc.wait_for_idle(...)      # block in Rust
proc.detach_idle_detector()           # clear the slot
```

The `Arc<Mutex<Option<...>>>` field on `NativePtyProcess` is held by the reader thread. The mutex is only locked for the pointer swap (nanoseconds), never during the hot loop. The reader thread takes a clone of the `Arc<IdleDetectorCore>` and drops the lock immediately.

## Pattern 3: Rust-side event production + Python-side result polling

For operations where Rust does continuous work and Python needs the outcome.

### When to use

- A Rust loop produces results over time (idle detection, output buffering)
- Python wants to block until a result is ready
- Python should not be in the loop

### How it works

Rust does all the work internally. Python calls a single method that releases the GIL and blocks on a Rust condvar. When the condition is met, the method returns a result tuple.

```python
# Python — one call, GIL released during wait:
idle_detected, reason, idle_for, returncode = proc.wait_for_idle(detector, timeout)
```

```rust
// Rust — entire wait happens without GIL:
fn wait_for_idle(&self, py: Python<'_>, ...) -> PyResult<(...)> {
    // attach detector to reader thread
    // spawn exit watcher (pure Rust)
    let result = py.allow_threads(|| detector.core.wait(timeout));
    // detach detector
    Ok(result)
}
```

### Current instances

| Operation | Rust method | Python surface |
|-----------|------------|----------------|
| Idle wait | `IdleDetectorCore::wait()` | `NativePtyProcess.wait_for_idle(detector, timeout)` |
| Process wait | `NativeProcess::wait()` | `NativeRunningProcess.wait(py, timeout)` |
| Chunk read | condvar wait in `read_chunk` | `NativePtyProcess.read_chunk(timeout)` |

## Pattern 4: Output queue (existing)

For streaming data from Rust threads to Python.

### How it works

The reader thread pushes chunks into `PtyReadShared` (a `Mutex<VecDeque>` + `Condvar`). Python calls `read_chunk(timeout)` which releases the GIL and waits on the condvar.

```
Reader thread: lock queue → push chunk → notify condvar → unlock
Python:        py.allow_threads(|| { lock queue → wait condvar → pop chunk })
```

This is the standard bounded-producer/consumer pattern. The GIL is never held during the condvar wait.

## Anti-patterns (do not do these)

### 1. Never call Python while holding a Rust mutex

```rust
// BAD:
let guard = shared.lock();
python_callback();  // needs GIL — if Python tries to lock `shared`, deadlock
```

### 2. Never hold the GIL while waiting on Rust

```python
# BAD (implicit):
result = native_method_that_blocks()  # if this doesn't py.allow_threads, GIL is held
```

Always use `py.allow_threads(|| ...)` for any Rust method that may block.

### 3. Never make Rust async tasks wait for Python responses

```rust
// BAD:
async fn work() {
    let event = do_rust_work().await;
    let answer = call_python(event);  // blocks async runtime on GIL
    use_answer(answer).await;
}
```

Rust should emit events and move on. If Python must respond, use a separate channel with timeouts.

### 4. Never share `Py<T>` with non-Python threads

`Py<T>` requires the GIL to access. If a Rust background thread holds `Py<T>`, it must acquire the GIL to read it — defeating the purpose. Extract the data into Rust-native types before sharing.

## Decision checklist for new cross-boundary features

1. **Is it a single boolean flag?** → `Arc<AtomicBool>`
2. **Is it a single numeric counter?** → `Arc<AtomicUsize>`
3. **Is it structured state read by background threads?** → Extract to `Arc<CoreStruct>`, wrap in `#[pyclass]`
4. **Does Python need to block for a result?** → Rust method with `py.allow_threads`, condvar inside
5. **Does Python need to stream data?** → `Mutex<VecDeque>` + `Condvar`, Python polls with `read(timeout)`
6. **Does Python need a callback during Rust work?** → **Don't.** Restructure as: Rust emits events → Python reads results after. If truly unavoidable, use a dedicated bridge thread that acquires the GIL only for the callback, holds no Rust locks during the call, and has a timeout.

## File reference

| File | Patterns used |
|------|--------------|
| `crates/running-process-py/src/lib.rs` | All four patterns |
| `crates/running-process-core/src/lib.rs` | Pattern 1 (atomic returncode), Pattern 4 (output queues) |
| `src/running_process/pty.py` | Python-side consumer of patterns 1-4 |
