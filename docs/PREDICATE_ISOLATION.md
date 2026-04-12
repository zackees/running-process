# Predicate Isolation — Future Design

**Status: Not needed yet.** The current event pump pattern (see `REMAINING_MIGRATION_PLAN.md`) handles callbacks safely. This doc is filed for when untrusted or long-running predicates become a requirement.

## When to revisit

- User-supplied plugin predicates from third-party code
- Scoring functions that may hang or have side effects
- Predicates that touch I/O, network, or databases
- Any callable where you can't guarantee "fast, pure, deterministic"

## Core idea

Treat Python predicates as serializable policy objects, not live callbacks. Execute them in an isolated Python worker process via IPC. Rust never calls Python inline.

```
Rust → serialize inputs → pipe → worker process → unpickle predicate → evaluate → pipe → result
```

## Callable contract

Accepted:
- Top-level functions or `@dataclass(frozen=True)` with `__call__`
- All fields must be picklable scalars (`bool`, `int`, `float`, `str`, `bytes`, `None`, tuples/lists/dicts of same)
- `__call__` must be pure, fast, side-effect-free, deterministic

Rejected:
- Lambdas, nested functions, closures
- Bound methods on mutable objects
- Objects containing locks, sockets, files, threads, queues, futures, event loops, DB handles

## Validation

Before Rust accepts any predicate:
1. Check callable is top-level importable
2. `pickle.dumps(obj)` succeeds
3. Fields are plain data only
4. Reject with explicit reason on failure

## Execution model

Copy-in / result-out. No shared live state.

1. Python creates predicate, validates it, serializes it
2. Rust stores serialized payload
3. On evaluation: Rust sends (serialized_predicate, input_args) to worker
4. Worker reconstructs, evaluates, returns plain result
5. Rust continues — no GIL, no inline Python

## State models

1. **Snapshot only** (default) — predicate is immutable after registration
2. **Snapshot with explicit update** — worker returns delta, parent adopts
3. **Shared live state** — avoid unless absolutely necessary

## Why not now

The current codebase has two callback types:
- `idle_reached(diff) -> IdleDecision` — fast enum return
- `predicate(diff, ctx) -> bool` — fast boolean return

Both are called from the Python thread that initiated `wait_for()`. The event pump pattern returns control to Python between evaluations. No Rust locks are held. No GIL is acquired from Rust. The isolation overhead (subprocess IPC, pickle round-trip) would add 1-10ms per call to a 250ms sample loop — unnecessary latency for trusted, simple predicates.

If predicates become untrusted, slow, or side-effectful, implement this pattern. Until then, the event pump is simpler and faster.

## References

- `docs/RUST_PYTHON_BOUNDARY.md` — current boundary patterns
- `docs/REMAINING_MIGRATION_PLAN.md` — event pump design for `wait_for()`
- Issue #28 — remaining migration tracking
