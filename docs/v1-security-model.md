# v1 Security Model

The v1 broker is a local IPC coordinator. Its trust boundary is the operating
system account and the broker isolation mode selected by the service
definition.

## Assets

The broker protects:

- backend process identity
- backend pipe names
- service-definition files
- cache manifests
- lifecycle logs
- admin verb output
- maintenance operations that affect backend handles

## Trust Boundary

The broker accepts local IPC only:

- Windows named pipes
- Unix-domain sockets

The broker does not bind TCP, UDP, localhost HTTP, or a browser-facing
transport. Network authentication and TLS are outside the v1 contract.

## Caller Authentication

The broker verifies peer credentials with platform APIs:

| Platform | Check |
|---|---|
| Windows | Named-pipe client PID, process token, and current-user SID |
| Linux | `SO_PEERCRED` |
| macOS | `LOCAL_PEERCRED` |

The `Hello.peer_pid` field is telemetry. It is never the source of authority.

## Pipe Access Control

Broker and backend pipe names include a per-user identity hash. Pipe and socket
parents are current-user-only:

- Unix directories use mode `0700`.
- Windows named pipes use an SDDL that grants access to the current user and
  required system principals only.

Backend pipe names include 128 bits of randomness. Predicting a backend pipe is
not part of the attack surface.

## Service Definitions

Service-definition directories are current-user-only. The broker refuses a
definition when the parent directory is group-writable or world-writable.

Service names are lowercase-normalized and restricted to `[a-z0-9-]{1,64}`.
This prevents case-only collisions and path delimiter injection.

## Filesystem Hardening

The broker follows these filesystem rules:

- Reject network filesystem lock directories.
- Keep temp files in the target directory for atomic replacement.
- Use `rename` plus parent `fsync` on Unix.
- Use `ReplaceFileW` on Windows.
- Use no-follow traversal for manifest and cache-root paths.
- Strip macOS quarantine xattrs from relocated backend binaries.
- Strip Windows `Zone.Identifier` alternate data streams from relocated backend
  binaries.

## Dependency Audit

The v1 release gate includes a dependency audit:

- `cargo audit --deny warnings` runs on dependency changes, pushes to `main`,
  manual dispatch, and the daily security schedule.
- Security tests reject direct HTTP, TLS, browser-facing, and network RPC
  dependencies in the `running-process` crate manifest. The broker's transport
  stays local IPC by construction; adding a network stack requires an explicit
  design issue before the dependency lands.
- New dependencies are reviewed for known advisories, network stacks, TLS
  stacks, serialization format drift, and unnecessary transitive weight.
- Dependencies used by broker parsing, IPC, manifest, service-definition,
  cleanup, handoff, and lifecycle paths are reviewed as security-sensitive.
- Vulnerable advisories block release until the advisory is resolved or a
  documented exception is approved by the maintainer.

## Unsafe Inventory

The v1 release gate includes a static broker unsafe-inventory guard. Security
tests scan `crates/running-process/src/broker/**/*.rs` for lexical `unsafe`
keyword usage and compare the per-file counts against an explicit inventory.

Every broker unsafe-site count change is security-review relevant. Adding,
removing, or moving broker `unsafe` usage requires updating the inventory and
reviewing why the platform API boundary changed.

## Isolation Modes

| Mode | Security property |
|---|---|
| `PRIVATE_BROKER` | A service receives its own broker instance. |
| `SHARED_BROKER` | First-party services share one user-scoped broker. |
| `EXPLICIT_INSTANCE` | Operators group services into named trust domains. |

Third-party services use `PRIVATE_BROKER` by default.

## Threats and Commitments

| Threat | v1 commitment |
|---|---|
| Cross-user pipe collision | Include a per-user identity hash in every broker name. |
| Pipe squatting | Use current-user-only permissions and random backend pipe suffixes. |
| Peer spoofing | Verify OS peer credentials; ignore self-reported PID as authority. |
| Service name collision | Reject uppercase and non-canonical service names. |
| Symlink traversal | Use no-follow traversal for broker-managed filesystem paths. |
| Network exposure | Expose no network listener. |
| Shared-broker blast radius | Default third-party services to private brokers. |
| Version downgrade | Enforce min-version and allow-list policy from service definitions. |

## Out of Scope

The v1 broker does not provide:

- cross-machine coordination
- TLS over IPC
- manifest signatures
- encryption at rest for metadata
- sandbox escape prevention for already-compromised same-user code

Those properties require a new design layer and are not represented as v1
broker guarantees.
