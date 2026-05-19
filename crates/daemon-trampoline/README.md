# daemon-trampoline

Minimal helper binary used by `launch_detached(...)` to spawn a user-supplied
command in the background and exit. The trampoline reads a sidecar
`<own-stem>.daemon.json` file next to its own executable for the command,
arguments, working directory, and environment to use.

## Responsibilities

- Read the sidecar JSON next to its own executable.
- Set the process name (Linux: `prctl(PR_SET_NAME)`; macOS: `pthread_setname_np`).
- Detach inherited stdin/stdout/stderr so the spawned child does not hold
  the original caller's pipe handles open after the caller exits (see issue
  #108 and `detach_stdio` in `src/main.rs`).
- Spawn the child with `process::Command`, wait for it, and exit with the
  child's status code (or `128 + signal` on Unix when killed).

## Layout

- `src/main.rs` — the entire trampoline binary.
- `tests/` — integration tests that spawn the trampoline binary with piped
  stdio and verify the detach behavior.
