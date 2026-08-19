# SESSION relay copy-avoidance evaluation (#949)

Status: **evaluation complete; Linux production path validated**. The measured
Linux `splice(2)` win is implemented by
[follow-up #1030](https://github.com/zackees/running-process/issues/1030), while
Windows and macOS keep the current buffered relay.

This result does not question Soldr's daemon/broker architecture. Soldr's stable
singleton broker successfully routes compile sessions to matching daemon
generations, including when the running broker and current CLI come from
different Soldr images. This evaluation asks a narrower question: can that
already-successful broker relay the established post-Hello SESSION byte stream
with materially less CPU or more throughput without changing its protocol,
routing, or lifecycle?

## Decision

The predeclared adoption gate was at least 20% more aggregate throughput **or**
30% less broker CPU/GiB at 16 sessions, with no more than 5% small-frame P99 or
real warm-build regression.

### Linux: go for `splice(2)`

The decisive release confirmation used five interleaved current/splice trials,
16 sessions, 8 KiB application chunks, and 64 MiB per session/direction. The
broker relay ran in a separate child process while the parent sampled its RSS,
so neither client/daemon work nor measurement work is included in broker CPU.

| Workload | Current | `splice` | Throughput change | Current CPU/GiB | `splice` CPU/GiB | CPU change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| stdout (3-trial median) | 600.958 MiB/s | 617.840 MiB/s | +2.8% | 895.428 ms | 719.686 ms | -19.6% |
| full duplex (5-trial median) | 353.978 MiB/s | 325.536 MiB/s | -8.0% | 2,089.430 ms | 1,113.226 ms | **-46.7%** |

The full-duplex broker CPU result clears the 30% gate. In the same five
interleaved confirmation trials, 8 KiB ping-pong P99 improved from 13,350 us to
8,826 us (-33.9%), rather than trading CPU for tail latency. Host wall time was
variable under WSL2, which is why topology order was interleaved and the
isolated broker CPU/GiB plus latency condition—not one throughput sample—drives
the go decision.

The 64 KiB buffered control is a Linux no-go. Its three-trial stdout median was
595.588 MiB/s and 1,129.816 ms CPU/GiB, versus 600.958 MiB/s and 895.428 ms for
current. Full-duplex throughput also fell from 739.380 to 501.729 MiB/s without
a CPU threshold win.

#### Production confirmation (#1030)

The evidence example's `splice` topology now calls the production
`relay_local_socket_session` function rather than carrying a private prototype.
Five new interleaved production trials strengthened the original result:

| Workload | Current | Production splice | Throughput change | Current CPU/GiB | Splice CPU/GiB | CPU change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| full duplex | 559.368 MiB/s | 746.197 MiB/s | +33.4% | 1,032.331 ms | 531.928 ms | **-48.5%** |

Median 8 KiB ping-pong P99 improved from 3,224 us to 1,529 us (-52.6%). A
64-session reader stalled for 250 ms remained byte-exact and bounded at
3,940,352 bytes of broker RSS. Both client-initiated and daemon-initiated
disconnect cases complete within the harness cleanup bound.

The production change was also cross-validated through a local Soldr build that
embedded each `running-process` revision. Soldr's successful stable singleton
broker remained PID 19655 throughout the comparison; it was not restarted or
replaced. When the local CLI image changed, that broker routed work to the
matching baseline and splice daemon generations behind it, whose PIDs then
remained stable for their respective five-trial series.

With `CARGO_BUILD_JOBS=2` and `SOLDR_JOBS=2`, five real warm
`soldr cargo build -p soldr-cli --bin soldr` trials took 26.973, 10.330, 9.931,
9.022, and 13.472 seconds on the baseline, versus 27.460, 9.727, 9.694, 10.247,
and 12.350 seconds with production splice. The medians were 10.330 and 10.247
seconds respectively, a 0.8% improvement and therefore no warm-build
regression. One-time image/cache priming runs were excluded; two priming
attempts were also discarded after compiler processes were killed under
concurrent container memory pressure. Those failures were build-resource
events, not broker replacement, routing, or relay failures.

### Windows: no-go for 64 KiB buffers

Five interleaved confirmation trials showed a bulk win but failed the complete
latency gate:

| Workload | Current | Tuned 64 KiB | Throughput change | Current CPU/GiB | Tuned CPU/GiB | CPU change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| stdout | 535.617 MiB/s | 645.103 MiB/s | **+20.4%** | 4,468.750 ms | 3,671.875 ms | -17.8% |
| full duplex (3-trial median) | 679.077 MiB/s | 744.180 MiB/s | +9.6% | 4,171.875 ms | 3,460.938 ms | -17.0% |

Median ping-pong P99 increased from 948 us to 1,086 us (+14.6%), beyond the 5%
budget. A follow-up was briefly filed from the earlier three-trial result, then
closed transparently after corrected five-trial confirmation invalidated the
go decision ([#1029](https://github.com/zackees/running-process/issues/1029)).

### Windows/macOS shared memory: no-go

Shared memory is technically feasible, but it is not an equivalent low-risk
optimization. It adds a new wire/lifecycle protocol, Windows fails the simpler
candidate's latency gate, and there is no local macOS performance evidence to
justify that complexity.

## What was measured

The checked-in `session_relay_evidence` example uses real `interprocess` local
sockets. `current`, `tuned-64k`, and `splice` each use two socket hops; `direct`
is a one-hop ceiling only and is not an eligible replacement because SESSION
traffic must remain broker-fronted.

Every synthetic SESSION sends its identity in an opening record and verifies a
deterministic byte pattern independently in each direction. This detects loss,
duplication, reordering, partial-write errors, and cross-session routing errors.
The matrix covers:

- stdout-heavy transfer;
- simultaneous full-duplex transfer;
- 8 KiB request/response ping-pong for P50/P99 latency;
- a reader stalled for 250 ms to exercise backpressure;
- abrupt peer disconnect and bounded relay cleanup.

The primary matrix runs 1, 16, and 64 sessions with 8 KiB application chunks.
The harness also supports 64 KiB chunks and custom session/byte counts. A
separate 64 MiB/session dataset contains the repeated adoption-gate trials. All
workloads have a 120-second outer bound. RAII endpoint/child/sampler guards,
`kill_on_drop`, and joined task sets ensure that timeout or cancellation does
not leave benchmark workers or endpoints behind.

The broker is a dedicated worker process for every relayed topology. The parent
samples broker RSS by child PID; the sampler does not execute inside the broker
or contaminate broker CPU. Result records report elapsed time, throughput,
broker process CPU, broker CPU/GiB, broker RSS, Unix voluntary+involuntary
context switches, P50/P99, and teardown status. Windows context switches are
`NA`: `GetProcessTimes` supplies CPU time but not context-switch totals, and
fabricating zero would be misleading. Direct topology has no broker, so its
broker metrics are zero/NA.

All primary matrix records were byte-exact and completed graceful teardown.
Linux `splice` additionally validates directional EOF using raw socket
half-close. `interprocess`'s portable `AsyncWrite::shutdown` is a no-op for the
current/tuned local-socket abstraction, so those paths validate whole-peer EOF
rather than claiming directional half-close coverage. The deliberate Windows
abrupt-disconnect case required the two-second forced harness/daemon-task
cleanup and records `graceful_teardown=false`; that accurately exposes the
existing named-pipe full-proxy behavior rather than leaking a task or turning it
into a false pass.

Raw evidence is checked in as the [platform matrix](evidence/session-relay-platform-matrix.csv),
[evaluation duration trials](evidence/session-relay-duration-trials.csv),
[production trials](evidence/session-relay-production-trials.csv), and
[semantics cases](evidence/session-relay-semantics.csv). The evaluation duration
file labels the initial controls separately from the interleaved confirmation
series used for the original go/no-go decision.

## Reproduction commands

Windows release matrix:

```powershell
soldr cargo run -p running-process --example session_relay_evidence --features daemon --release -- --topology all
```

Linux release matrix through the repository's persistent Docker entrypoint
(`bosn` was not installed on the evidence host):

```powershell
uv run --no-sync python ci/dev_docker.py cargo run -p running-process --example session_relay_evidence --features daemon --release -- --topology all
```

One duration-controlled input (repeat/interleave current and candidate):

```powershell
uv run --no-sync python ci/dev_docker.py cargo run -p running-process --example session_relay_evidence --features daemon --release -- --topology current --sessions 16 --workload full-duplex --bytes-mib 64 --chunk-kib 8
uv run --no-sync python ci/dev_docker.py cargo run -p running-process --example session_relay_evidence --features daemon --release -- --topology splice --sessions 16 --workload full-duplex --bytes-mib 64 --chunk-kib 8
```

Linux syscall proof (the dev image is ephemeral, so installing `strace` does
not modify the host or repository):

```powershell
uv run --no-sync python ci/dev_docker.py -- sh -lc "apt-get update -qq && apt-get install -y -qq strace >/dev/null && strace -f -c -e trace=read,write,splice,shutdown,close target/release/examples/session_relay_evidence --smoke --topology splice"
```

The evaluation prototype trace recorded 32 `splice` calls. After #1030 wired
the evidence topology to the production function, the same smoke trace recorded
42 `splice` calls (two nonblocking retries), 71 reads, 18 writes, four
shutdowns, and 55 closes across the complete client/broker/daemon harness. The
broker's bulk relay is the only path in that harness that calls `splice`, which
confirms the production selection rather than an example-local copy. The
hardware-counter attempt was:

```powershell
uv run --no-sync python ci/dev_docker.py -- sh -lc "apt-get update -qq && apt-get install -y -qq linux-perf >/dev/null && perf stat -e task-clock,cycles,instructions,context-switches target/release/examples/session_relay_evidence --smoke --topology splice"
```

The Docker/WSL2 host denied `perf_event_open` (`No permission to enable
task-clock event`). The isolated broker CPU/GiB and context-switch counters
therefore provide the CPU profile; the report does not claim hardware-counter
or page-reference proof. Linux also documents that `SPLICE_F_MOVE` is a hint,
so CPU-per-byte results—not the API name—drive the decision.

## Hosts and platform matrix

- **Toolchain:** running-process 4.10.2, Soldr CLI 0.9.1, Rust 1.95.0. The
  already-running stable singleton broker was Soldr 0.9.0 while the CLI was
  0.9.1; Soldr successfully kept work flowing through the singleton and routed
  it to a matching daemon generation behind the broker.
- **Windows:** Windows 10 Pro 10.0.19045, AMD Ryzen 7 3700X, 16 logical CPUs,
  native named pipes. The 64-session tuned cases remained bounded; the largest
  corrected primary-matrix broker RSS was 30,629,888 bytes.
- **Linux:** Debian Bookworm container on WSL2 kernel
  `6.18.33.2-microsoft-standard-WSL2`, AMD Ryzen 7 3700X, four container CPUs,
  Unix-domain sockets. The `splice` worker uses two fixed nonblocking pipes per
  proxied session; its largest corrected primary-matrix broker RSS was 3,616,768
  bytes at 64 sessions.
- **macOS:** no local performance run is claimed. The available local
  `dockur/macos` workflow correctly refused this Windows 10 host because nested
  virtualization is unavailable. The checked-in fallback remains the existing
  portable buffered relay; normal macOS CI builds/tests it, but CI fallback
  preservation is not represented as zero-copy benchmark evidence.

Follow-up #1030 changes the shipped Linux broker relay and adds a feature-gated
`interprocess` dependency to the platform-internal crate; it does not change the
SESSION wire, routing, or broker/daemon lifecycle. Its real Soldr cross-repo
validation found a 10.247-second median warm build versus 10.330 seconds on the
immediately preceding baseline (0.8% faster), within the predeclared 5% budget.
The stable singleton broker stayed running while it selected each image's
matching daemon generation behind it.

## Shared-memory feasibility details

On Windows, `CreateFileMapping` plus `MapViewOfFile` can share page-backed
mappings by inherited/duplicated handles or names. A production design needs two
bounded rings per session, atomic producer/consumer indices, separate waitable
events, owner-SID DACLs, unpredictable names or handle passing, explicit version
negotiation, and last-handle/crash cleanup. Named mappings can already exist
with a different size and remain alive until every view and handle closes.

On macOS, `shm_open` plus `ftruncate`/`mmap` provides owner/group mode-based
objects. A robust design still needs two rings, atomics, wakeup primitives,
descriptor/name exchange, unlink-on-open and crash recovery, peer-UID checks,
and protocol/version fallback.

Most importantly, compiler stdout/stderr still originates in OS pipes and the
client still consumes a socket/pipe API. A shared-memory relay would copy bytes
between those endpoints and ring buffers while adding synchronization and attack
surface; it avoids copies only inside part of the broker path, not end-to-end.
That cost is unjustified by the current evidence.

## Follow-up disposition

- Linux `splice`: **go**, implemented by
  [#1030](https://github.com/zackees/running-process/issues/1030).
- Linux 64 KiB buffered relay: **no-go** because it misses both bulk thresholds.
- Windows 64 KiB buffered relay: **no-go** because P99 exceeds the latency
  budget; the premature follow-up #1029 was closed after corrected confirmation.
- Windows/macOS shared-memory reference routing: **no-go** at present because
  the complexity and protocol/security surface are not justified by evidence.
