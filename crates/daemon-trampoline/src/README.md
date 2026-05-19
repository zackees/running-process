# daemon-trampoline src

Source for the `daemon-trampoline` helper binary.

- `main.rs` — sole source file. Contains:
  - `detach_stdio()` — reopens stdin/stdout/stderr to the null device so
    inherited pipe handles are released before the child is spawned
    (issue #108).
  - `sidecar_path` / `Sidecar` — locates and deserializes the
    `<own-stem>.daemon.json` config next to the trampoline executable.
  - `set_process_name` — Linux `prctl`, macOS `pthread_setname_np`.
  - `run` / `main` — orchestrates the lifecycle and exits with the
    child's status code.
