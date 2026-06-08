# v1 Handoff Optimization

The v1 broker normally returns a backend pipe name and lets the client connect
directly. The handoff optimization shortens that path by transferring an
already-open backend connection handle when the platform supports it.

## Baseline Path

```text
client -> broker: Hello
broker -> client: Negotiated { backend_pipe }
client -> backend: connect(backend_pipe)
```

This path is always supported and remains the fallback.

## Optimized Path

```text
client -> broker: Hello
broker -> backend: open or reuse backend pipe
broker -> client: Negotiated { handle_passed_token }
client: adopts transferred handle
```

The `backend_pipe` field remains present for diagnostics and fallback.

## Backend Acceptance Helper

`running_process::broker::backend_lib::accept_handed_off` is the
platform-neutral backend scaffold for this path. Platform modules will deliver
raw token bytes plus an opaque connection payload through `HandedOffPayload<T>`.
The helper parses the 128-bit token, consumes the matching pending token exactly
once, and classifies the payload as `Accepted` or `Rejected`.

The helper does not call `DuplicateHandle`, `sendmsg`, or `recvmsg`; those
transport details remain isolated for the later Windows and Unix modules.

## Platform Mechanisms

| Platform | Mechanism |
|---|---|
| Windows | `DuplicateHandle` into the client process. |
| Linux | `SCM_RIGHTS` over Unix-domain socket. |
| macOS | `SCM_RIGHTS` over Unix-domain socket. |

## Fallback Triggers

The broker returns the baseline `backend_pipe` path when:

- peer credentials cannot be mapped to a process handle
- handle duplication fails
- Unix socket ancillary data transfer fails
- service policy disables handoff
- client capabilities do not include handoff support
- broker is under resource pressure

Fallback is not a protocol error.

## Capability Bits

Handoff is negotiated through additive capability bits in `Hello` and
`Negotiated`. A client that does not advertise handoff support receives the
baseline response.

## Tuning

Operators tune handoff by service definition labels and broker config:

| Setting | Effect |
|---|---|
| `handoff.enabled` | Enables or disables the optimization. |
| `handoff.max_attempts_per_minute` | Bounds failed handoff work. |
| `handoff.disable_under_fd_pressure` | Forces baseline path during FD pressure. |

Metric and event names are stable. See [v1 observability](v1-observability.md).
