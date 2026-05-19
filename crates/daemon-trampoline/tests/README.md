# daemon-trampoline tests

Integration tests for the `daemon-trampoline` binary.

## Tests

- `stdio_detach.rs` — regression test for issue #108: the trampoline
  must release the parent's stdin/stdout/stderr before spawning the user
  command. Spawns the trampoline with `Stdio::piped()` and asserts the
  reader threads see EOF within a short timeout.
