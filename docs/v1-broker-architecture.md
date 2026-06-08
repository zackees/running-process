# v1 Broker Internal Architecture

This document is for contributors working on `running-process-broker-v1`.
The public architecture is summarized in
[v1 architecture overview](v1-architecture-overview.md).

## Process Model

`running-process-broker-v1` is a per-user local IPC daemon. Each broker
instance owns one trust domain:

- shared user broker
- private service broker
- explicit named instance

The broker accepts control-plane connections, validates `Hello`, coordinates
backend lifecycle, and returns direct backend pipe addresses.

## Major Components

| Component | Responsibility |
|---|---|
| Listener | Binds the platform pipe or socket for one broker instance. |
| Framing | Reads and writes `[1][u32 length][prost body]` frames. |
| Protocol | Decodes `Frame`, `Hello`, `HelloReply`, and admin payloads. |
| Service registry | Loads and validates service-definition files, re-reading on `Hello`. |
| Instance resolver | Maps service definitions to shared, private, or explicit broker instances. |
| Backend registry | Tracks verified backend handles by instance, service, and version. |
| Spawn coordinator | Serializes backend startup for one service/version. |
| Perf guard | Summarizes Hello latency samples and enforces the frozen P50/P99 budgets. |
| Lifecycle monitor | Watches process death, idle timers, and shutdown drains. |
| Manifest registry | Reads and writes central cache manifests. |
| Admin dispatcher | Implements status, dump, health, config, diagnose, and metrics verbs. |
| Event log | Appends bounded `LifecycleEvent` records. |

## Hello Path

1. Listener accepts one local IPC connection.
2. `server::connection::handle_hello_connection` reads the initial frame with
   the 64 KiB `Hello` cap.
3. Protocol layer decodes `Frame`, verifies it is a control-plane request,
   then decodes `Hello` from `Frame.payload`.
4. Peer credential check validates the OS identity.
5. Service registry resolves and revalidates the service definition.
6. Instance resolver selects the broker trust domain.
7. Backend registry returns a live backend or asks the spawn coordinator to start
   one.
8. Broker writes a response `Frame` whose payload is `HelloReply`
   (`Negotiated` or `Refused`).
9. Client disconnects and uses the backend pipe directly.

The first server slices expose this boundary as `HelloRequest` and
`handle_hello_connection`: the request contains the decoded `Hello`, the
original `Frame` metadata, and the OS-verified peer identity. The full accept
loop calls the same connection handler after binding the platform socket and
checking credentials.

## Backend Table

The backend registry is keyed by:

```text
(broker_instance, service_name, service_version)
```

Each entry stores:

- backend process id
- backend pipe endpoint
- backend executable hash
- boot id
- last activity timestamp
- spawn budget state
- lifecycle event cursor

## Concurrency Rules

- One spawn lock exists per service/version.
- The broker never holds the global backend table lock while launching a child
  process.
- Admin dump snapshots copy state into a diagnostic structure before encoding
  JSON.
- Shutdown cancellation is broadcast before listener close.

## Error Mapping

Internal errors map to stable `Refused.code` values:

| Internal condition | Wire code |
|---|---|
| Protocol range mismatch | `ERROR_VERSION_UNSUPPORTED` |
| Missing service definition | `ERROR_SERVICE_UNKNOWN` |
| Spawn failure | `ERROR_BACKEND_SPAWN_FAILED` |
| Spawn budget exhausted | `ERROR_RATE_LIMITED` |
| Shutdown in progress | `ERROR_SHUTTING_DOWN` |
| Peer credential failure | `ERROR_PEER_REJECTED` |
| Policy version floor | `ERROR_VERSION_BLOCKED` |
| Unclassified invariant failure | `ERROR_INTERNAL` |

## Contributor Rule

Code changes that affect any component above update the matching v1 document in
the same PR.
