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
platform-neutral backend scaffold for this path. Platform modules deliver
raw token bytes plus an opaque connection payload through `HandedOffPayload<T>`.
The helper parses the 128-bit token, consumes the matching pending token exactly
once, and classifies the payload as `Accepted` or `Rejected`.

The helper does not call `DuplicateHandle`, `sendmsg`, or `recvmsg`; those
transport details remain isolated in the Windows and Unix modules.

## Platform Transport Scaffold

`running_process::broker::server::handoff::windows` models `DuplicateHandle`
attempt inputs, result, and fallback-safe errors.
`running_process::broker::server::handoff::unix` does the same for
`SCM_RIGHTS`. The platform modules own the direct handle/file-descriptor
transfer attempt when compiled on their target OS, while unsupported,
permission-denied, and timeout-like outcomes are translated into the existing
silent reconnect fallback policy.

## Platform Mechanisms

| Platform | Mechanism |
|---|---|
| Windows | `DuplicateHandle` into the backend process. |
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
- the broker adopted an existing backend after restart

Fallback is not a protocol error.

## Adopted Existing Backends

When a broker restart discovers a live backend through the central manifest, the
new broker treats that backend as adopted. It cannot use `DuplicateHandle` or
`SCM_RIGHTS` to transfer a connection that was accepted by the old broker, so it
must use reconnect mode for that backend.

The expected negotiated reply for an adopted backend keeps `backend_pipe`
populated, where `handle_passed_token` is empty. In code this is represented by
`HandoffFallbackReason::AdoptedBackend`, and the client follows the same
baseline reconnect path as any other fallback.

## Latency Proof

`running_process::broker::server::handoff::compare_handoff_latency` compares
handoff samples against reconnect fallback samples at P50 and P99. The helper
requires handoff to be strictly faster at both percentiles, preventing Phase 6
from claiming equal or slower handle passing as a successful optimization. The
registered `handoff_latency` tests cover both the faster case and the rejection
of equal or slower handoff samples.

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
