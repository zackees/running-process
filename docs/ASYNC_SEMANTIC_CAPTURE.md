# Async semantic capture

The `async-process` and `kernel-substrate` features expose a small semantic
spawn facade for consumers that must not import Tokio, native command objects,
or platform-internal process types.

```rust
use running_process::{AsyncProcessBuilder, AsyncStdio};

let output = AsyncProcessBuilder::shell("echo ready")
    .stdin(AsyncStdio::Null)
    .stdout(AsyncStdio::Piped)
    .stderr(AsyncStdio::Piped)
    .capture()
    .await?;
assert!(output.status.success());
```

`AsyncProcessBuilder` owns direct argv or platform-selected shell construction,
working directory, replacement environment, clear-environment policy, all
three stdio policies, process grouping, and owner-death policy. `capture` and
`capture_bounded` use the existing canonical actor's concurrent drain; they
return `AsyncCapturedOutput` with the original `std::process::ExitStatus`.
Nonzero and Unix signal exits are therefore captured results, not normalized
integers. Existing `AsyncProcess::output*` and `RunOutput` remain unchanged.

Only `AsyncStdio::Piped` is captured. If stdout or stderr is configured as
`AsyncStdio::Inherit` or `AsyncStdio::Null`, the corresponding vector in
`AsyncCapturedOutput` is empty: inherited bytes belong to the parent stream and
null bytes are discarded by the operating system. This is intentional and does
not change the child's exit status.

Detached execution and independent stream readers remain intentionally outside
this one-shot facade.
