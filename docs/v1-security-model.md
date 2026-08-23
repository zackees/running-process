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

The former `backend_lifecycle/verify_pid.rs` entry was retired by #969. Its
twenty-one `unsafe` tokens were the three ways a host answers "is that still
the process I meant" -- a Linux pidfd, a macOS `kqueue`/`EVFILT_PROC`
subscription, and a Windows process handle -- plus the executable-path probe
and the signal and terminate calls. All now execute inside the audited
platform process facade, under the same conditions.

Two properties carried over deliberately, because the check is only worth as
much as they are. The handle is what pins identity: a PID can be reissued
between two questions, so each host takes a reference the kernel will not let
it reuse, and the broker asks that reference rather than the number. And the
executable comparison stays a comparison of paths as this host spells them --
case-folded and verbatim-prefix-stripped on Windows, exact elsewhere -- rather
than becoming an inode or file-index identity, which would newly accept a hard
link or a bind mount as the same image. The broker still keeps which process
to verify and what to do when verification fails; the stored executable path
and its BLAKE3 hash are still both checked before a backend is accepted.

The former `fs_health.rs` entry was retired by #973. Its two `unsafe` sites
zero-initialized a `libc::statvfs` struct and called `libc::statvfs(3)` on the
daemon data directory path; both now execute inside the audited platform
resources facade, under the same conditions -- the path is broker-owned and
never peer-supplied, the struct is stack-local, and only the inode counters are
read out. The broker keeps where to probe and how to present the result.

The same facade also owns the exhaustion classifiers that
`server/fd_pressure.rs` and `daemon/emergency_reserve.rs` used to spell out
inline as raw errno and Win32 comparisons. Neither file carried inventoried
`unsafe`, but both keyed security-relevant backpressure on numbers whose
meaning differed per host; a caller now asks which wall it hit and decides what
to do about it.

The former `host_identity.rs` entry was retired by #973. Its thirteen `unsafe`
tokens read the five facts a cache manifest records about the host that wrote
it: the machine name, a reboot-surviving machine id, a per-boot id, the
filesystem device, and the process namespaces. All five now execute inside the
audited platform host facade.

The Windows boot-identity fix (#757) moved with them, unchanged in substance.
The primary path calls `RegGetValueW` for the fixed, machine-local
`PrefetchParameters\BootId` DWORD and accepts it only after validating the
return status, registry type, and exact four-byte length. If that value is
unavailable, the fallback queries the current process's documented
`ProcessTelemetryIdInformation` through `NtQueryInformationProcess`. It uses
the non-owning current-process pseudo-handle, probes the kernel-reported buffer
size, caps allocation at 1 MiB, retries once if the size grows, and reads
`BootId` only after the returned length covers that field.

What the broker keeps is the part that was never a host mechanic: what an
*absent* fact means. A host that cannot name the current boot still gets a
process-stable unique token rather than a shared constant, so loss of both OS
sources fails closed instead of accepting an identity that every process on
every such host would agree on. That decision belongs to the comparison the
identity exists to support, so it stays with the comparison -- and it is now
reachable, and tested, without any `unsafe` at all.

The former `server/connection.rs` inventory entry was retired by #971. Local
IPC peer-identity and Windows SID extraction now run behind the audited
`running-process-platform-internal` IPC facade, so the shared broker connection
path contains no native `unsafe` sites.

The former `broker_owned_bind.rs` and `server/singleton_bind.rs` entries were
retired by #971. Listener inheritance/adoption and host-specific endpoint
construction now execute inside the audited platform IPC facade, while the
broker retains the surrounding opt-in, retry, and diagnostics policy.

The former `secure_dir.rs` entry was retired by #971. The owner-private
directory mode and Windows DACL mechanics formerly covered by nine inventoried
`unsafe` tokens now execute inside the audited platform IPC facade. The broker
retains directory-placement policy and public diagnostics.

The former `server/handoff/unix.rs` and `server/handoff/windows.rs` entries
were retired by #971. The deprecated public 4.x descriptor-passing mechanics --
constructing and inspecting `msghdr` control messages and calling `sendmsg` /
`recvmsg` on Unix, and duplicating the backend handle via `DuplicateHandle` on
Windows -- now execute inside the audited `running-process-platform-internal`
IPC facade. The broker retains the legacy numeric fd/HANDLE models and the
public error and fallback policy, and reaches the mechanics only through hidden
crate-root compatibility adapters, so the shared handoff path contains no
native `unsafe` sites.

The former `server/spawn_coordinator.rs` entry was retired by #972, in two
steps. First the Windows lock-file identity probe moved out -- a
`BY_HANDLE_FILE_INFORMATION` `assume_init` and the `GetFileInformationByHandle`
behind it -- taking the count from eight to six. Then the advisory file lock
itself moved: the Unix `flock` pair and the two Windows `LockFileEx` /
`UnlockFileEx` sites with their zero-initialized `OVERLAPPED` structs. Opening
a lock file with host-appropriate permissions, taking and releasing an
exclusive lock, telling a contended lock from a failed one, and proving file
identity all now execute inside the audited platform filesystem facade, so the
backend spawn coordinator contains no native `unsafe` sites. The broker retains
the policy around them: lock lifetime, the identity re-check that detects a
replaced lock file, retry budgets, and error classification.

The former `manifest.rs` entry was retired by #972. Its single `unsafe` token
was the `ReplaceFileW` call that swaps a freshly written manifest onto the
published path. Atomic replacement now executes inside the audited platform
filesystem facade, which is also where the Unix `rename` and the parent-
directory fsync live, so one reviewed implementation covers durability on every
host. The broker retains what it always owned: the temporary file's name, the
content it writes, the SHA-256 it records, and when a replacement is
attempted.

The former `lifecycle/privilege.rs` entry was retired by #973. Its three
`unsafe` tokens were the Windows process-token query that decides whether the
broker is running as LocalSystem: `OpenProcessToken`, the two-step
`GetTokenInformation` size-then-read of `TOKEN_USER`, and the `CloseHandle` in
the owning guard's `Drop`. Asking "is this process a privileged system
identity" now executes inside the audited platform host facade, alongside the
Unix `geteuid` comparison that answers the same question. The broker retains
every part of the decision that is policy rather than fact: that privileged
startup is refused at all, the `RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED` escape
hatch for isolated test environments, and the operator-facing error.

The former `lifecycle/process_tree.rs` entry was retired by #969's remaining
scope. Its seven `unsafe` tokens installed owner-death containment for the
broker process itself: `prctl(PR_SET_PDEATHSIG)` on Linux, and on Windows the
`CreateJobObjectW` / `SetInformationJobObject` / `AssignProcessToJobObject`
sequence plus the `CloseHandle` guard for the retained job handle. All of it
now executes inside the audited platform process facade.

The containment properties are unchanged and worth restating, because they are
the reason the handle is held rather than dropped: a kill-on-close job destroys
its members when the last handle closes, so the handle is parked for the
process lifetime and never taken out. A lost race on installation leaks that
one duplicate handle deliberately -- closing it would close a handle to a job
that already contains this process, terminating the broker. Windows reporting
`ERROR_ACCESS_DENIED` on assignment still means "already inside someone else's
job", which is containment rather than a failure to contain, and is reported as
such.

The former `lifecycle/sid.rs` entry was retired by #973. Its five `unsafe`
tokens derived the per-user identity the broker hashes into its endpoint scope:
the Windows process-token query for `TOKEN_USER`, the `CloseHandle` guard, the
SID validity and length checks, the raw-slice read of the SID bytes, and the
`getuid` reads on Unix. Producing that identity now executes inside the audited
platform host facade, which owns all three shapes -- a Windows SID, a
uid-plus-machine-id on Linux, a uid-plus-platform-UUID on macOS. The broker
retains the parts that are its own: the BLAKE3 hash, its 16-hex truncation, and
what the resulting scope is used for.

The former `lifecycle/names.rs` entry was retired by #971. Its two inventoried
`unsafe` tokens were both `libc::getuid()` reads used to derive the Unix
fallback socket directory. Endpoint directory placement and the `sun_path` /
`MAX_PATH` budgets now execute inside the audited platform IPC facade, so the
broker derives a v1 endpoint address without a native call of its own. The
broker retains the naming policy: the `rpb-v1-` prefix, service and version
validation, and the SID-hash check. The equivalent `client_v2.rs` read is
unchanged and still inventoried.

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
