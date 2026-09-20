# Timer Core Host Baseline

## Ownership Checks

`ownership.rs` tests real Timer storage and concurrent Drop/dispatch under
Miri; it is not a performance workload. It models explicit near-future rearm,
three-round exit, deferred service, the 100ms backoff cap, cancellation and
independent CPU service. Non-waiting cancellation preserves any explicit
successor during delivery. Batch handoff tests verify callback-local cleanup,
unlocked callback/clock execution, and destruction of the completed previous
timer from the next callback. `ownership_races.rs` covers concurrent deadline
replacement, immutable delivery snapshots, teardown, remote replacement during
a paused callback, idle owner reselection and pending migration in both directions.
Migration checks cover source/destination minima, replacement return values,
destination-only delivery and rejected future-only requests preserving placement.
It also checks explicit repetition
that stops without rearm and both serialized orders of callback/remote replacement.
These use modeled CPU service, not real IRQ/IPI placement. From outside the kernel tree:

```sh
env RUSTFLAGS='--check-cfg=cfg(unittest)' cargo +nightly-2026-03-08 miri test \
  --manifest-path /path/to/repo/task/ktimer-core/benches/host/Cargo.toml \
  --test ownership --locked --offline
```

Use `MIRIFLAGS=-Zmiri-seed=N` for alternate schedules. SMP must remain enabled.
The intrusive library emits an integer-to-pointer provenance warning; do not
suppress it or interpret a passing run as a complete pointer/IRQ safety proof.

The ignored `cross_cpu_start_cancel_dispatch_stress` test runs two writers and
two modeled CPU dispatchers against one owned timer, with 200,000 one-shot
requests whose distinct deadlines identify each registration, non-waiting/synchronous
cancellation and idle/pending owner migration. It checks
at-most-once delivery, non-overlapping callbacks, final quiescence, reuse on both
CPUs and exactly-once handler destruction. Run it separately in native mode:

```sh
timeout 120 env RUSTFLAGS='--check-cfg=cfg(unittest)' cargo +nightly-2026-03-08 test \
  --manifest-path /path/to/repo/task/ktimer-core/benches/host/Cargo.toml \
  --test ownership --release --locked --offline cross_cpu_start_cancel_dispatch_stress \
  -- --ignored --test-threads=1
```

This is contention stress, not an exhaustive interleaving proof, a long-duration
soak or a hardware IRQ test. Canceled/replaced requests need not be delivered;
the test does not claim an exact delivery total while writers compete.

## Owned API Allocation Acceptance

From outside the kernel tree, run:

```sh
cargo +nightly-2026-03-08 run --release \
  --manifest-path /path/to/repo/task/ktimer-core/benches/host/Cargo.toml \
  --target x86_64-unknown-linux-gnu --bin timer-core-bench -- --check-owned
```

This checks actual owned Timer operations, including invalid input, expired wait
rejection, callback rearm, single delivery, idle/pending owner reselection, both
cancellation APIs and 4096 reuse cycles. Callbacks also start, replace and cancel
a second prepared timer through both cancellation APIs, without inline delivery.
A positive allocation control validates
the probe. Preparation precedes counting; operation allocations/frees must both
be zero. Final destruction is measured separately and must free the timer storage
exactly once. This is deterministic single-threaded host acceptance, not IRQ/IPI,
contention coverage or a before/after performance result.

## Timing Baseline

### Lifecycle Phase Diagnostics

After building, run `timer-core-bench --profile-lifecycle 65536` on a pinned
host CPU in a quiet window. The CSV splits the same real owning Timer and legacy
model lifecycle into prepare, arm, dispatch and destroy. It uses three ABBA blocks
per size and validates exact callback counts and released handler references.
Allocation counting is disabled. Layout sizes are printed separately on stderr.

Each phase adds a timestamp, so these are diagnostic costs, not acceptance
numbers or precise percentages of an uninstrumented run. Prefer N >= 32 to
amortize clock overhead. Legacy dispatch consumes Wakers and frees expired tree
nodes, while owning Timer releases its persistent handler/storage at destruction;
compare the complete lifecycle, not just the two destroy columns. Validate any
candidate with `--compare`, which has no internal phase timestamps.

For temporary internal instrumentation, use an isolated source copy; do not add
host clock probes, counters or diagnostic features to production timer paths.

### Sleep State Allocation Comparison

`timer-core-bench --compare-sleep-state 262144` compares source-derived old/new
sleep-state layouts around the real owning Timer: a separate state Arc plus
Timer versus state embedded in the Timer's handler. Both variants use the same
SpinNoIrq state lock, consume the same counted Waker on delivery, and exercise
the complete prepare/start/deliver/destroy lifecycle. No real Future polling,
scheduler, IRQ or hardware programming is timed; this is not the generic
legacy-BTreeMap comparison or an end-to-end sleep-latency measurement.

The mode runs three ABBA blocks per size, with allocation counting disabled for
timing. Separate probes assert exact counts: 2N+1 allocations/frees per old batch
and N+1 per new batch, including one harness array on each side. Callback totals
and released Waker references are checked. Repeat invocations independently in
a quiet window; report batch-mean medians, not individual-operation percentiles.

This is a separate native microbenchmark, not a kernel UT or QEMU timing test.
It imports the current `ktimer-core/src` files and the actual `kspin` lock and
user-mode guard implementations. `host.rs` supplies only the CPU-ID value wrapper
and BaseGuard trait contract. IRQ/preemption operations are the existing host
NoOp aliases; real IRQ, IPI, topology, scheduling and allocator latency are absent.
Time/error types and intrusive-collections 0.10.2 are real dependencies.

## Reproduce

From the repository root, build first, then arrange a quiet measurement window:

```sh
bash task/ktimer-core/benches/host/run.sh build
bash task/ktimer-core/benches/host/run.sh measure /tmp/timer-baseline-01 16 5 200000
```

The standalone Cargo workspace has its own lockfile. The script builds from
outside the kernel tree so generated Kconfig Cargo flags do not leak into this
host build. `BENCH_TARGET` overrides `/tmp/timer-core-bench-target`.
No product source or kernel configuration changes are needed.

Use the same host CPU, toolchain, inputs, rounds and event count for comparisons.
Coordinate with other agents before measuring. The script checks processes at
both ends, but cannot exclude a short overlapping workload, SMT interference,
turbo changes, interrupts or host scheduling. It pins one logical CPU, not an
isolated physical core. Output includes metadata, source/binary hashes, raw CSV,
and a validated summary. Do not edit measured sources while a run is active.

## Workloads

Each size (1/32/256/4096) and case has a 4096-event warmup before measured rounds.
Node/handler storage is allocated beforehand and remains fixed through cleanup.
Nodes occupy a contiguous boxed array, with one shared counted handler.

| Case | Timed work | Unit |
| --- | --- | --- |
| rearm | Replace a pending node's complete setting through TimerSet::arm_on | ns/rearm |
| cancel_arm | Synchronous pending cancellation, then complete start | ns/two-operation cycle |
| dispatch | Drain N same-deadline one-shots, registration outside timing | ns/event |

Updates visit `(i * 73) & (N - 1)` and use increasing deadlines. Deadline
validation is timed for updates. Dispatch uses a fixed simulated time; each
callback executes black_box(event) and one relaxed atomic increment. It does not
subtract callback overhead. Each delivery batch is timed separately with
std::time::Instant; at N=1 clock-reading overhead is relatively significant.
Reported values are medians of round-level means, **not per-operation p50/p99**.

The counting System allocator covers registration, operations and final timer
cleanup after prepared storage allocation. A successful sample requires zero
allocation/free calls, exact callback counts, expected removed memberships and
fully unlinked nodes. Preparation and final external Box releases are outside
that interval. There is no performance threshold or timing assertion in UT.

## Limits

### Legacy Algorithm Comparison

```sh
bash task/ktimer-core/benches/host/run.sh build
bash task/ktimer-core/benches/host/run.sh compare /tmp/timer-compare-01 16 65536
```

`comparison.rs` models the old `TimerRuntime` insert/remove/split-off paths from
`task/ktask/src/future/time.rs` on main at `45386bd06f3f962671b8785d77c9aeba93523c88`,
the independent core PR's baseline. It uses actual BTreeMap/Waker and the
same host SpinNoIrq as the current production owned Timer code. This is a
source-derived host model, **not a run of mainline or the old kernel binary**.
Per-CPU lookup, hardware arbitration, IPI, scheduling and task wake costs are
excluded. Old dispatch consumes Wakers; new callbacks use prepared Wakers by
reference. Both wake the same atomic-count handler and validate delivery counts.

- `start_cancel`: register N future one-shots, then cancel N pending timers;
  prepared object construction is excluded. Unit is one start/cancel pair.
- `dispatch`: register N equal deadlines outside timing, then deliver the entire
  batch, including legacy split-off/deallocation or new running cleanup.
- `lifecycle`: create N registrations/owned objects, start, deliver and destroy;
  includes the new preparation cost. A boxed handles/timers array on each side
  is harness overhead included here; the shared Waker is prepared outside timing.

Sizes are 1/32/256/4096, with 4096-event warmup and three ABBA blocks per case.
Each batch uses a monotonic simulated clock. Report medians of six batch means,
not per-event latency percentiles. Allocation counters are disabled for timing;
separate probes report allocation/free calls (not bytes) in the same operation
intervals. New reusable paths assert zero; lifecycle intentionally includes
allocation/destruction. Timing CSV zeros are placeholders, not allocation results.
The core has no automatic periodic mode. Old periodic benchmark rows are not
comparable and are no longer emitted or accepted by the current summary script.

### Platform Coverage

This first baseline covers uncontended single-CPU core paths, not cross-CPU
contention, lock-wait tails, real IRQ allocation behavior or end-to-end waits.
Existing timer/Future semantics differ, so it makes no mainline speedup claim.
Later contention tests must distinguish same-node, same-tree and independent-tree
loads; platform tests must separately measure IPI and hardware delivery latency.
## Independent CPU-slot comparison

The host harness imports `arch/karch/src/cache.rs` directly, sharing the exact
`CacheAligned` implementation with production without linking kernel services.
The input checksum includes that source file and the architecture-specific
`cache_layout.rs` policies it selects from.

After `run.sh build`, run the following on an otherwise quiet Linux host:

```sh
/tmp/timer-core-bench-target/release/timer-core-bench --cpu-slots 16 17 2000000
```

Choose two available CPUs on different physical cores (not SMT siblings), and
use the same pair for both binaries. Each worker pins itself with `taskset`,
prepares its own timer, warms up, then repeatedly starts and cancels it on its
own modeled CPU slot. CSV reports each worker's elapsed time and the slower
worker's nanoseconds per start/cancel pair. Construction, pinning and warmup
are excluded. Run alternating before/after samples with identical arguments;
archive binary hashes and host topology. This tests independent-slot traffic,
not same-slot contention, hardware IRQs or end-to-end kernel latency. The
reported set size/alignment also exposes the padding cost.
