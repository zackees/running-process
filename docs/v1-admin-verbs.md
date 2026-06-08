# v1 Admin Verbs

Admin verbs use the broker control pipe with `Frame.payload_protocol = 0xADMIN`.
Every JSON response uses `schema_version: 1`.

The current Phase 4 binary can render every admin response locally. Pipe-backed
admin transport wires these renderers to broker state when the accept loop
lands.

## Common Envelope

```json
{
  "schema_version": 1,
  "command": "status",
  "generated_at_unix_ms": 1700000000000
}
```

Additional command-specific fields live beside the common envelope fields.

## `status --json`

Returns broker liveness and backend summary.

```json
{
  "schema_version": 1,
  "command": "status",
  "generated_at_unix_ms": 1700000000000,
  "broker_instance": "shared",
  "broker_pid": 1234,
  "uptime_seconds": 12.5,
  "accepting_hello": true,
  "connections_open": 1,
  "backends": [
    {
      "service_name": "zccache",
      "service_version": "1.11.20",
      "pid": 4321,
      "backend_pipe": "rpb-v1-deadbeefdeadbeef-be-abababababababababababababababab",
      "last_active_unix_ms": 1700000000000,
      "state": "running"
    }
  ]
}
```

## `dump --json`

Returns a full debug snapshot.

```json
{
  "schema_version": 1,
  "command": "dump",
  "generated_at_unix_ms": 1700000000000,
  "broker_instance": "shared",
  "effective_config": {},
  "backend_table": [],
  "spawn_budgets": [],
  "recent_lifecycle_events": []
}
```

## `list-instances --json`

Returns every broker instance visible to the current user.

```json
{
  "schema_version": 1,
  "command": "list-instances",
  "generated_at_unix_ms": 1700000000000,
  "instances": [
    {
      "broker_instance": "shared",
      "pipe": "rpb-v1-deadbeefdeadbeef-shared",
      "pid": 1234,
      "state": "running"
    }
  ]
}
```

## `healthz`

Returns success when the broker process is alive and can answer admin frames.
The non-JSON response body is exactly:

```text
ok
```

## `readyz`

Returns success when the broker accepts new `Hello` requests. The non-JSON
response body is exactly:

```text
ready
```

During shutdown drain, `healthz` stays healthy and `readyz` returns failure.

## `backend-health <service> --json`

Returns backend health for one service.

```json
{
  "schema_version": 1,
  "command": "backend-health",
  "generated_at_unix_ms": 1700000000000,
  "service_name": "zccache",
  "backends": [
    {
      "service_version": "1.11.20",
      "pid": 4321,
      "state": "running",
      "last_hello_unix_ms": 1700000000000,
      "last_error": null
    }
  ]
}
```

## `config --effective --json`

Returns effective broker configuration and the source of each value.

```json
{
  "schema_version": 1,
  "command": "config",
  "generated_at_unix_ms": 1700000000000,
  "values": {
    "idle_timeout_secs": {
      "value": 900,
      "source": "service-definition"
    }
  }
}
```

## `diagnose --output bundle.tar.gz`

Writes a diagnostic bundle and returns a summary.

```json
{
  "schema_version": 1,
  "command": "diagnose",
  "generated_at_unix_ms": 1700000000000,
  "output": "bundle.tar.gz",
  "files": [
    "manifest/zccache-1.11.20.json",
    "events/lifecycle.jsonl",
    "config/effective.json"
  ],
  "redactions": [
    "home",
    "secret-env",
    "acl-identities"
  ]
}
```

## `metrics`

Returns OpenMetrics text. Metric names are defined in
[v1 observability](v1-observability.md).
