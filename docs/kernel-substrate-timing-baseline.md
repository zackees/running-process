# Kernel substrate timing baseline

This is recorded Linux evidence, not a wall-clock acceptance threshold. The
resolver graph is the contract; timing makes a material build-cost change
visible for review. CI continues to upload a fresh evidence artifact on every
run.

## 2026-08-27 — isolated Linux Bosn stack

The standard substrate library check ran twice against a temporary target
directory in a Bosn-managed container pinned to
`bosn/linux@sha256:1f3c264d8a5814980b48ec125e9935f0e8dfc4feaf9c29e8c94a83f65eec2d42`.
The source mount was read-only; the evidence output mount was writable. Both
runs exited successfully with lockfile SHA-256
`d798de507e13b25099cec3692c902c63d989b4e1145905cc6e90b29480592a49`.

| Run | Exit code | Duration |
| --- | ---: | ---: |
| isolated clean | 0 | 12.905 s |
| exact repeat | 0 | 1.932 s |

The resolver graph contained 40 Linux packages. The machine-readable record
uses schema version 1 and is retained by the validation artifact; this summary
must not be used as a fixed threshold.
