# Supervised services and the non-blocking client

`running_process_service` (Python) and
`running_process::broker::backend_sdk::{ServiceServer, PollingFrameClient}`
(Rust) package the direct-endpoint recipe in [`INTEGRATE.md`](INTEGRATE.md)
into a complete small-service kit, with no broker:

| Property | How |
|---|---|
| Single instance | The service binds a per-user endpoint derived from its name (`bind_singleton`); a second instance gets `ServiceAlreadyRunning`. A stale socket from a crashed instance is reclaimed. |
| Discovery + health | The service writes an identity sidecar; clients run the `BackendHandle` nonce probe against it on connect and every `health_interval` (default 5 s). |
| Restart | A client whose service is absent or fails a probe runs the spec's `command` detached (`spawn::spawn_daemon`), under an exclusive per-service lock file, at most 3 times per 30 s by default (`spawn_budget`). |
| Never blocks | Every client call touches only an in-memory queue. `send` refuses at once (`ServiceUnavailable` / `QueueFull`); `try_recv` returns the next reply or `None`. |
| Correlation | `send` returns the request id; each `Reply` carries it. Replies may arrive out of order (the service answers on worker threads). A request in flight when the connection drops comes back as a `Reply` with `error` set. |

The wire is the frozen v1 frame lane under the spec's payload protocol: a
registered consumer id (`0x7000..=0x7EFF`, see the registry) or private use
(`0xF000..=0xFFFF`). Payloads are opaque bytes.

## Python

```python
from running_process_service import ServiceSpec, ServiceClient, serve

spec = ServiceSpec(name="my-svc", version="1.0.0", payload_protocol=0xF0B7)

# service process
serve(spec, lambda payload: b"pong:" + payload, workers=4)

# client: from a timer or any loop that must not wait
client = ServiceClient.connect(spec.with_command([sys.executable, "svc.py"]))
try:
    rid = client.send(b"ping")
except (ServiceUnavailable, QueueFull):
    pass                       # drop; resend on a later tick
reply = client.try_recv()      # Reply(request_id, payload, error) | None
```

`ServiceServer(spec)` + `recv(timeout)` / `request.reply(bytes)` is the
lower-level form of `serve` for services that reply later.

`import running_process_service` loads only the native extension. It does not
import the `running_process` package, so it is cheap enough to load inside
another program's UI thread.

When an external supervisor (systemd, launchd, a Windows service) owns the
service, either leave `command` unset so clients never spawn, or make
`command` ask that supervisor to start it (`systemctl --user start my-svc`),
so there is still one supervisor.

## Paths

| | Linux | macOS | Windows |
|---|---|---|---|
| Endpoint | socket under `$XDG_RUNTIME_DIR/running-process/broker-v2/` | hashed socket under `$TMPDIR/.rp-<uid>-broker-v2/` | per-user named pipe |
| Identity file, spawn lock | `$XDG_RUNTIME_DIR/running-process/services/` | `~/Library/Application Support/running-process/services/` | `%LOCALAPPDATA%\running-process\services\` |

`runtime_dir` overrides the state directory and salts the endpoint name, so
independent instances (tests) can run side by side.
