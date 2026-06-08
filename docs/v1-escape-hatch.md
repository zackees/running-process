# v1 Escape Hatch

The v1 broker escape hatch is:

```text
RUNNING_PROCESS_USE_BROKER=off
```

Participating consumers read this variable before attempting broker discovery.
When it is set to `off`, the consumer uses its direct daemon path.

## Values

| Value | Behavior |
|---|---|
| unset | Follow the rollout default for the consumer version. |
| `off` | Disable broker usage. |
| `on` | Force broker usage when the consumer supports it. |
| `auto` | Follow the rollout default explicitly. |

Unknown values are configuration errors and are logged with the consumer name.

## Intended Use

Use the escape hatch for:

- production rollback
- isolating broker defects from backend defects
- bisecting performance regressions
- CI jobs that require direct daemon mode during rollout

## Operator Examples

Unix shell:

```sh
RUNNING_PROCESS_USE_BROKER=off cargo test
```

PowerShell:

```powershell
$env:RUNNING_PROCESS_USE_BROKER = "off"
cargo test
```

GitHub Actions:

```yaml
env:
  RUNNING_PROCESS_USE_BROKER: off
```

## Deprecation Timeline

| Stage | Escape hatch state |
|---|---|
| Phase 4 through Phase 6 | Required and tested. |
| Phase 7 | Required and monitored. |
| Phase 8 | Removal PR opens after default-on is stable for the published window. |
| v1.0 | Final state recorded in release notes. |

The direct daemon path remains available until the removal stage is complete.
