# klockstat — Design Document

## Purpose

`klockstat` provides X-Kernel with lock contention statistics aggregated per lock class, which the kernel uses to diagnose
hot locks and blocking paths. The statistics are read through `/proc/lock_stat` (Kconfig `KFEAT_LOCK_STAT`)
or `klockstat::dump_lock_stat()`.

## Scope

```
util/klockstat/
├── src/
│   ├── lib.rs       # counters, aggregation, formatted output
│   └── runtime.rs   # heap locks registered by initialization call site (stats feature)
util/klockstat-macros/
└── src/static_lock.rs  # static_lock! procedural macro
```

Integration points:

- `ksync::Mutex` / `ksync::RwLock`
- the `kspin::SpinLock` family (`SpinNoIrq`, etc.)

## Host Test Layout

Host unit tests live in `tests/host/` and remain private path modules in
their original parents, not separately discovered Cargo integration targets.
`tests/host/lib.rs` preserves `tests` under `cfg(test)` (four cases);
`tests/host/runtime.rs` preserves `runtime::tests` under
`cfg(all(test, feature = "stats"))` (five cases). There are no guest tests.
The runtime test still compares two lookups from the same call site; it does
not depend on a fixed source filename or line number.

Validate on a host target with `cargo test -p klockstat --lib` and
`cargo test -p klockstat --features stats --lib` (four and nine cases).
Use host-target Cargo settings without kernel `--cfg unittest`.

## Lock Class Model

Each lock class is identified by `(location, kind)`:

| Field | Meaning |
|------|------|
| `location` | Usually `file:line` |
| `kind` | `Mutex`, `RwLock`, `SpinNoIrq`, etc. |

Multiple lock instances under the same class share one set of counters.

### Registration Paths

**Static locks** — use `static_lock!`:

```rust
use ksync::{RwLock, static_lock};

static_lock! {
    static TASK_TABLE: RwLock<TaskTable> = RwLock::new(TaskTable::new());
}
```

The macro expands to:

1. a standalone `LockClassStats` static item;
2. a `linkme::distributed_slice(LOCK_CLASSES)` entry;
3. a lock static variable bound with `new_with_stats`.

**Heap locks / non-const static contexts** — `Mutex::new` / `RwLock::new` (when `stats` is enabled):

- the constructor carries `#[track_caller]`;
- on the first call, `class_for_init_site` deduplicates by call site in the runtime registry;
- Runtime classes are deduplicated by call site (`file`, `line`, `column`, and lock kind); the displayed label is `file:line`, so distinct classes may share a label.

**Untracked locks** — point to `NOOP_CLASS`:

- raw locks not bound to stats, such as `RawMutex::with_config`;
- `Mutex::const_new(RawMutex::new(), ...)`, used in scenarios that require `const` initialization.

**Locks inside `klazy::lazy_static!`** — when `lazy_static` is left unchanged:

- they can still be tracked through the runtime `Mutex::new` once `stats` is enabled;
- `location` falls inside the initialization function/closure, not on the `static ref` declaration line;
- use `static_lock!` instead when an exact label is needed.

## Metric Semantics

| Metric | Meaning |
|------|------|
| `acquisitions` | Number of successful lock acquisitions (the success path of `lock()` and a successful `try_lock`) |
| `contentions` | Number of times a wait occurred before acquisition |

Additional conventions (aligned with the Linux `lock_contended` / `lock_acquired` model):

- **Mutex**: records 1 `contentions` only once it is confirmed that the task needs to sleep, before entering `block_on` (at most once per
  `lock()` round); a CAS that succeeds after pure spinning does not count as contention; on success it records `acquisitions`.
- **SpinLock (SMP)**: a successful acquisition after spin-waiting records 1 `contentions` + `acquisitions`;
  an uncontended immediate success records only `acquisitions`.
- **SpinLock (UP)**: records only `acquisitions`.
- **RwLock**: records 1 `contentions` only once it is confirmed that the task needs to sleep, before entering `block_on` (at most once per round of
  `lock_shared` / `lock_exclusive`); each successful read lock/write lock records 1 `acquisitions`.

## Output Format

`dump_lock_stat()` takes the top `DUMP_TOP_N` (default 5) entries in descending order of `contentions`, filters out all-zero
entries, and outputs a fixed-column-width table.

Data sources are merged:

1. static classes registered through `linkme` (`LOCK_CLASSES`);
2. heap lock classes in the `runtime.rs` runtime registry.

## Feature Gating

| Kconfig / Cargo | Effect |
|-----------------|------|
| Default (off) | `static_lock!` is an identity macro; `Mutex::new` stays a `const fn`; no hook overhead |
| `KFEAT_LOCK_STAT` | Enables `ksync/stats`, `kspin/stats`, `procfs/lock_stat` |

When `stats` is off, `klockstat` can still be linked by `procfs`, but there is no runtime registration or macro expansion.

## Design Trade-offs

- **Aggregate by call site, not by type**: like Linux `lock_stat`, this makes it easy to locate the source position; multiple
  `Mutex::new` on the same line may be split into several classes because their `column` differs.
- **Relaxed atomics**: the statistics are for diagnostic use and do not require a strict happens-before relation with the lock state.
- Runtime registration uses a fixed 1024-slot open-addressed table. Each slot
  is published once through `klazy::Once`; no heap allocation, container growth,
  or label formatting occurs during registration. The key includes file, line,
  column and lock kind. Slots are never removed or reused.
  Hash lookup uses linear probing bounded by the pool capacity, not a scan on
  every counter update. Publication can wait for another CPU's initializer;
  this is not a bounded-latency or NMI-safe primitive.
- This follows Linux's fixed `lock_classes[MAX_LOCKDEP_KEYS]` pool and
  `register_lock_class` exhaustion policy (`kernel/locking/lockdep.c`): capacity
  exhaustion permanently disables diagnostics instead of failing lock creation.
  X-Kernel disables both static and runtime counter recording; it does not
  implement Linux's dependency graph or per-CPU lock-stat arrays. Existing
  references and snapshots remain valid. In-flight counter updates may finish.
- Runtime labels retain compiler-provided static `Location` metadata.
  `LockClassEntry::location` and `LockClassStats::location()` now expose
  `LockLocation` instead of a string reference. Formatting still prints
  `file:line`; static labels are unchanged. Formatting and snapshot vectors
  allocate only on the reporting path, which remains infallible on OOM.
- Snapshot iteration uses nonblocking `Once::get`, without holding a registry
  lock. A concurrent registration may be absent from a snapshot. The dump
  explicitly reports pool exhaustion rather than presenting frozen counters
  as live statistics. `statistics_enabled()` exposes the same state.

## Calling Constraints

- Registration requires no sleep or reclaim permission. Kernel registration
  saves and disables local IRQs until publication finishes, then restores the
  original state, including on unwind. Only another CPU can own an initializing
  slot during ordinary task/IRQ registration. NMI registration is unsupported:
  NMIs can interrupt a slot initializer and must not wait on that same slot.
  Initialization performs no callbacks, logging, allocation or scheduling.
- The table and slot storage exist only with `stats`; the capacity is a local
  diagnostic budget, not a claim that all possible lock sites fit. Exhaustion
  does not retry or re-enable counters until reboot. Host tests use isolated
  small tables for exhaustion and concurrent publication checks.
  A subprocess isolates the permanent disable transition while checking that
  previously registered static and runtime counters stop and the dump reports it.
- Static locks should preferably use `static_lock!`, to avoid a compilation failure under `stats` when `static X = Mutex::new(...)` is used
  because `Mutex::new` is not a `const fn`.
