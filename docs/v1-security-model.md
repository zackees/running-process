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

The v1 release gate treats dependency changes as part of security review.
The current direct runtime dependency inventory is published in
`docs/v1-dependency-surface.md`. Security tests compare that inventory with
`crates/running-process/Cargo.toml`, so dependency additions must update the
review record in the same PR.

### Direct Dependency Review Policy

Every new or materially changed runtime direct dependency in
`crates/running-process/Cargo.toml` must be reviewed before merge. Runtime
direct dependencies include entries under `[dependencies]` and target-specific
runtime dependency sections.

Reviewers check:

- Known advisories with `cargo audit --deny warnings`.
- Whether the dependency or its enabled features add HTTP, TLS, WebSocket,
  browser-facing, remote RPC, or other network client/server capability.
- Whether a smaller existing dependency or standard-library code covers the use
  case.
- Whether the dependency affects broker parsing, IPC, manifest,
  service-definition, cleanup, handoff, or lifecycle paths. Those paths are
  security-sensitive.
- Transitive dependency weight and serialization format drift.

A dependency added only for trivial formatting, parsing, path handling, or
command glue should be rejected unless the design issue records why local code
would be less safe.

### Local IPC And No-Network Commitment

The broker's v1 transport is Windows named pipes and Unix-domain sockets only.
The broker must not bind TCP, UDP, localhost HTTP, browser-facing transports, or
remote RPC endpoints, and it must not add a dependency path that does so for
broker operation.

The `running-process` crate must not add direct dependencies whose purpose is
HTTP, TLS, WebSocket, browser-facing transport, or network RPC. It also must not
enable transitive features that create network listeners or clients for broker
operation. Adding a network-capable dependency or feature requires a design
issue that updates this security model before merge.

Security tests enforce the current forbidden direct-dependency list for the
`running-process` crate manifest.

### Cargo Audit Schedule

`.github/workflows/security-audit.yml` runs
`cargo audit --deny warnings`:

- On pull requests touching `.github/workflows/security-audit.yml`,
  `Cargo.lock`, any `Cargo.toml`, or this security model.
- On pushes to `main` touching the same paths.
- Daily through the scheduled security-audit workflow.
- By manual `workflow_dispatch`.

The scheduled run is the backstop for newly disclosed advisories when no
repository files have changed.

### Exception Process

Known-vulnerable dependencies, denied audit warnings, and dependency-policy
violations block release by default. An exception must be documented in a
GitHub issue before merge and approved by the maintainer.

The exception record must include:

- The dependency, version, advisory or policy violation, and affected broker
  path.
- Why no safer dependency or local implementation is suitable.
- Whether the exception affects the local-IPC/no-network commitment.
- The mitigation, owner, and expiration or review date.
- Any required update to tests, workflow configuration, or this document so the
  exception stays visible and narrow.

An exception does not silently weaken the no-network commitment. Any exception
that adds network capability to broker operation requires a new security-model
revision before the dependency lands.

## Unsafe Inventory

The v1 release gate includes a static broker unsafe-inventory guard. Security
tests scan `crates/running-process/src/broker/**/*.rs` for lexical `unsafe`
keyword usage and compare the per-file counts against an explicit inventory.

Every broker unsafe-site count change is security-review relevant. Adding,
removing, or moving broker `unsafe` usage requires updating the inventory and
reviewing why the platform API boundary changed.

The `backend_lifecycle/verify_pid.rs` inventory includes the Windows process
path identity probe. That probe opens the target process with limited query
rights, calls `QueryFullProcessImageNameW`, and closes the OS handle before the
stored daemon executable path and SHA-256 are accepted.

The `fs_health.rs` inventory covers the Unix inode-pressure probe (#390): two
`unsafe` sites zero-initialize a `libc::statvfs` struct and call
`libc::statvfs(3)` on the daemon data directory path. The path is a
broker-owned constant (never peer-supplied), the struct is stack-local, and
only the inode counters are read out.

## Fuzz Campaign And Reviewer Signoff

The v1 release gate requires one-hour fuzz campaign evidence for every
`cargo-fuzz` target plus explicit security reviewer signoff. The required
artifact format is published in `docs/v1-fuzz-campaign-signoff.md`, and
security tests compare its target matrix with `crates/running-process/fuzz`.

#241 cannot close until that artifact records successful release-candidate fuzz
runs, audit and regression evidence, and an approved reviewer decision.

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
