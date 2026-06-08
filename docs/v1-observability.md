# v1 Observability

The v1 broker exposes structured logs, lifecycle events, admin JSON, metrics,
trace context propagation, and diagnostic bundles.

## Structured Logging

Production logs default to JSON. Text logs are for local development.

Environment:

```text
RP_BROKER_LOG_FORMAT=json
RP_BROKER_LOG_FORMAT=text
```

Log records include:

- timestamp
- level
- broker instance
- service name
- service version
- request id
- connection id
- event kind
- error code

## Lifecycle Events

`LifecycleEvent` is the durable event format. It uses OpenTelemetry
`severity_number` and `severity_text` fields. See
[v1 lifecycle events](v1-lifecycle-events.md).

## Trace Context

Every `Frame` carries:

- `traceparent`
- `tracestate`

The broker forwards trace context from client handshake to backend lifecycle
work and admin diagnostics.

## Metrics

Metrics use OpenMetrics text and the `running_process_broker_v1_` prefix.

| Metric | Type | Labels |
|---|---|---|
| `running_process_broker_v1_hello_total` | counter | `service`, `version`, `outcome` |
| `running_process_broker_v1_hello_duration_seconds` | histogram | `service` |
| `running_process_broker_v1_active_backends` | gauge | `service` |
| `running_process_broker_v1_spawn_attempts_total` | counter | `service`, `version`, `outcome` |
| `running_process_broker_v1_spawn_budget_remaining` | gauge | `service`, `version` |
| `running_process_broker_v1_connections_open` | gauge | none |
| `running_process_broker_v1_fd_usage_ratio` | gauge | none |
| `running_process_broker_v1_uptime_seconds` | gauge | none |

Metric renames require v2.

## Diagnostic Bundle

`diagnose --output bundle.tar.gz` writes a bundle with:

- decoded lifecycle events
- central manifests
- PID and lock files
- running backend process ids
- backend executable hashes
- boot id
- broker effective config
- OS and kernel summary
- disk-free summary
- pipe namespace summary

## Redaction

Diagnostic bundles redact:

- full home directory paths, rendered as `~`
- environment variables with `KEY`, `TOKEN`, `SECRET`, or `PASS` in the name
- ACL user and group identifiers, rendered as stable hashes

Redaction happens before writing the archive.
