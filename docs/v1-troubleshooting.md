# v1 Troubleshooting

This guide lists operator-facing broker failure modes and the matching first
checks.

## Broker Not Running

Symptoms:

- client falls back to direct daemon mode
- `Hello` connection fails
- `readyz` cannot connect

Checks:

1. Confirm `RUNNING_PROCESS_DISABLE` is not `1`.
2. Run `running-process-broker-v1 healthz`.
3. Check the broker pipe path in [v1 pipe naming](v1-pipe-naming.md).
4. Inspect the lifecycle log for `SPAWN_ATTEMPT`, `HELLO_REFUSED`, and
   `SECURITY_VIOLATION`.

## Service Unknown

Symptoms:

- `Refused.code` is `ERROR_SERVICE_UNKNOWN`
- `status --json` has no entry for the service

Checks:

1. Verify the `.servicedef` file exists in the platform service directory.
2. Verify the parent directory is current-user-only.
3. Verify `service_name` matches `[a-z0-9-]{1,64}`.
4. Run `config --effective --json` for the broker instance.

## Version Refused

Symptoms:

- `Refused.code` is `ERROR_VERSION_BLOCKED`
- the requested backend never starts

Checks:

1. Compare `wanted_version` with `ServiceDefinition.min_version`.
2. Check `version_allow_list`.
3. Confirm the backend binary exists under `per_version_binary_dir`.

## Stale Manifests

Symptoms:

- cleanup reports a live daemon that is no longer running
- restored CI cache points at an old boot id
- backend pipe paths in diagnostics no longer exist

Checks:

1. Compare manifest `boot_id` with the current boot id.
2. Run `running-process-cleanup verify`.
3. For GitHub Actions cache restore, run verification after cache restore and
   before starting consumers.

## Spawn Budget Exhausted

Symptoms:

- `Refused.code` is `ERROR_RATE_LIMITED`
- `retry_after_ms` is nonzero
- lifecycle log contains repeated `DIED_CRASH` or spawn failures

Checks:

1. Inspect backend stderr and lifecycle logs.
2. Verify the backend binary hash matches the manifest.
3. Wait for `retry_after_ms` before retrying.

## Pipe Permission Failure

Symptoms:

- `Refused.code` is `ERROR_PEER_REJECTED`
- lifecycle log contains `SECURITY_VIOLATION`

Checks:

1. Confirm client and broker run as the same intended user.
2. Verify Unix socket parent mode is `0700`.
3. Verify Windows named pipe ACL grants the current user.
4. Check for uppercase or non-canonical service names.

## GHA Cache Restore Issues

Symptoms:

- restored manifests reference another boot
- cleanup sees missing runtime paths
- service starts from stale cache state

Checks:

1. Run `running-process-cleanup verify --gha` after restore.
2. Drop manifests whose `boot_id` differs from the current runner.
3. Keep cache roots and manifest registry in the same cache key family.

## Network Filesystem Refusal

Symptoms:

- broker refuses to start
- spawn lock acquisition reports unsupported filesystem

Checks:

1. Move lock and runtime directories to a local filesystem.
2. Keep NFS, SMB, CIFS, FAT32, and exFAT outside broker-managed lock roots.

## Emergency Disable

Set:

```text
RUNNING_PROCESS_DISABLE=1
```

Then rerun the workload. If the workload succeeds in direct mode, collect a
diagnostic bundle before filing the broker issue.
