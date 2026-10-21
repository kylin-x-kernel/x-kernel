# Explicit Kernel RCU Validation

## Ownership Stress

This runner follows kworkng's opt-in boot validation mechanism. It uses the real
RCU service and public APIs, not a host simulation or default unit test.
Cargo benchmark auto-discovery is disabled. No latency benchmark is implemented
here; stress counters are correctness observations, not performance results.

Prepare a platform defconfig using `make defconfig`, enable
`KFEAT_RCU_TORTURE=y`, set `RCU_TORTURE_SECONDS=5` for initial smoke or `1800`
for a longer run, then run `make olddefconfig` and `make build`.
Do not enable `UNITTEST`. The runner is excluded from unittest builds and
ordinary production configurations leave the feature disabled.
The workqueue boot benchmark and RCU runner are mutually exclusive in Kconfig.

Run with an isolated disk image and a bounded external command, for example:

```sh
timeout --kill-after=5 180 make justrun ACCEL=n DISK_IMG=target/rcu-test.img \
    XKMAKE_ARGS='--no-net --no-vsock --smp 4'
```

The runner starts after runtime initialization instead of userspace and powers
off after success. The requested duration excludes boot and may be exceeded by
the last batch. A timeout is a failure, not permission to terminate readers and
declare successful reclamation. Increase the host timeout for a long soak.

Before the timed batches, a fixed-storage retirement probe runs 64 withdrawals
and real `synchronize_rcu` waits. Readers select one of two retained slots within
the public read scope and check its atomic retirement marker. A withdrawn slot
is marked retired only after its GP. Reuse clears the marker before publication.
This follows the forbidden-observation principle of Linux rcutorture's Reader
Pipe, not its full multi-stage recycling pipeline. Slots are never freed during
the probe, so an observed premature retirement is a semantic failure, not UAF.

The probe first injects a local premature retirement and requires the same
marker check to detect it. This negative control changes only test storage,
never the production GP implementation. Require
`RCU_RETIREMENT_NEGATIVE_CONTROL detected=1 expected=1` and
`RCU_RETIREMENT_RESULT` with 64 completed waits, nonzero reads for every worker
and zero errors. The negative control checks the oracle's sensitivity to its
forbidden observation; it does not prove every concurrency bug is detectable.
This preflight is additional to the configured timed workload.

Each batch runs at least two readers and two concurrent updaters, with 32
replacements per updater. Read-side work is finite; yielding occurs outside
scopes. It joins every worker before checking per-object return and destruction
counts. Batches reuse no old payload or ledger, bounding retained test storage.
This is repeated finite ownership stress, not continuous callback-queue pressure
or forced CPU migration. Worker counts and batch size are currently fixed.

Each timed iteration also starts one pinned callback producer per present CPU,
each submitting 32 independent records. Records are never published as protected
data; their nonblocking callbacks release themselves in softirq context.
After all producers join, the coordinator drains callbacks and checks every ID
was invoked and destroyed exactly once, with no cancellation or live records.
Require one `RCU_CALLBACK_BATCH` per ownership batch, all records accepted,
`calls` and `drops` equal to that total, and zero `cancelled`, `live` and `errors`.
`peak_live` counts prepared and queued/executing records, not just queue depth;
it must be nonzero and at most the finite per-batch allocation limit.

Callbacks may complete before submission returns or before drain begins. This
workload checks conservation through real concurrent execution, not a forced
in-flight drain window or early-GP detection. Deterministic drain/retirement
tests cover those separate obligations. No latency or throughput inference is
made from these instrumented counts.

Require process exit success, `RCU_TORTURE_BEGIN`, nonzero batch progress,
`RCU_TORTURE_PASS`, and no panic. Each `RCU_TORTURE_BATCH` reports per-reader
hits, reads of updated versions, 64 returned old owners, 65/65 destroyed objects
and zero errors. The final pass marker is printed only after all assertions and
cleanup. Output bypasses the ordinary log filter and remains visible at
`loglevel=0`. Preserve the complete log, source revision, configuration,
duration, actual CPU count and QEMU/host setup. A short smoke run does not prove
long-term stability or weak-memory correctness.

The normal kernel clippy command enables `cfg(unittest)`, which excludes this
runner. Also lint the release-only module after a normal torture-enabled
`make build`. For the x86_64 QEMU/slab configuration:

```sh
cargo clippy -p krcu --no-deps --target x86_64-unknown-none \
    --features torture,kalloc/slab -- -D warnings
```

This supplements, rather than replaces, the configured Makefile build and
kernel validation. Match the target and allocator feature to the configuration.

## Read Costs And Wait Latency

`KFEAT_RCU_BENCH=y` selects a separate release-only runner, mutually exclusive
with RCU torture and workqueue benchmarks in Kconfig. Prepare a platform
defconfig, select the feature, run `make olddefconfig`, `make build` and the same
bounded `make justrun` command above. Do not enable UNITTEST, coverage, lock-stat
or detailed tracing for measurements. For supplemental release-only clippy use
`--features bench,kalloc/slab` instead of `torture,kalloc/slab`.

One pinned task at a time runs on each present CPU. Each CPU has one warmup
round (round 0) and three measured rounds of 32 sequential submit/drain samples.
Storage preparation and allocation precede timing. Logging, result collection
and sorting follow the whole round. The callback checks that it ran on the same
CPU in softirq context; a different CPU or inconsistent timestamps fail the run.

After each callback round, the same pinned task measures 32 sequential public
`synchronize_rcu()` calls, without deliberate reader pressure. This follows
Linux `kernel/rcu/rcuscale.c`'s synchronous call-duration measurement boundary.
`RCU_SYNC_SAMPLE` reports the entire call in `elapsed_ns`, including preparation,
allocation, GP waiting and scheduling. It is not pure coordinator execution
time. Only sample storage allocation and logging are outside that interval.
No protected object is retired by these calls, and no read scope or lock is
held across them. The `sync_wait` summary uses the same percentile convention
and warmup exclusion as the callback metrics. This sequential baseline does
not measure parallel updater scaling or waits blocked by long-lived readers.

Each raw `RCU_LATENCY_SAMPLE` reports nanoseconds for:

- `submit_ns`: time immediately before submit to immediately after its return.
- `callback_entry_ns`: time before submit to the callback's first clock read.
- `completion_observed_ns`: time before submit to return from the subsequent
  drain. This includes drain overhead and scheduling; it is an upper bound on
  callback-return latency, not a timestamp taken at callback return.
  The drain is global and can also include unrelated prior callbacks.

The callback may start before submit returns. It frees its own unpublished
record; this measures the boot service without deliberate reader pressure.
The drain allocates/waits using its normal implementation, and its cost is
included only in caller-observed completion, not in submit time. Clock reads
and callback instrumentation are not subtracted. These are sequential latency
samples, not enqueue-only throughput or saturation measurements.

`RCU_LATENCY` emits nearest-rank P50/P95/P99/max for each metric and round.
With only 32 samples, P99 equals max: this smoke-sized sample set is not a
statistically reliable tail estimate. Preserve the raw samples and exclude
round 0 from measured aggregates. Require `RCU_LATENCY_BEGIN`, exactly
`CPUs * 4 * 32` callback samples and as many synchronous samples,
`CPUs * 4 * 32 * 3` read-batch samples and `CPUs * 4 * 7` summaries,
`RCU_LATENCY_PASS`, exit
success and no panic. PASS verifies execution/accounting, not a performance
target. Record revision, configuration, QEMU version and host contention;
repeat isolated runs before drawing performance conclusions. QEMU TCG results
are not hardware costs or a comparison with Linux on different hardware.

After each synchronous-wait round, the same worker measures read-side batches,
following Linux `kernel/rcu/refscale.c`'s repeated read-section timing principle.
Each of 32 samples has three batches of 1024 operations: a black-box constant
baseline, an empty public `with_read` scope, and a scope reading one immutable
`Rcu<u64>` value. Order rotates across samples. Allocation, checksum assertions,
logging and sorting are outside timing; no updater is introduced. Black-box
inputs/results inhibit elimination, but this is not an instruction-level model.

`RCU_READ_SAMPLE` includes `operations=1024` and total `elapsed_ns`; summaries
`baseline_batch`, `empty_scope_batch` and `object_read_batch` also report total
batch nanoseconds, not per-operation latency. Divide by 1024 only to obtain a
batch average; percentile batch averages are not single-operation tail latency.
Do not subtract the baseline as a claim of exact net cost. IRQs and involuntary
preemption stay enabled, so scheduling/interrupt time can be included. These
serial pinned readers do not measure multicore contention, throughput scaling
or reader latency under active GP pressure. Round 0 remains warmup.

Before concurrent synchronous waits, bounded asynchronous bursts run with 1, 2
and 4 pinned producers and 32/256 records per producer. All records and sample
storage are prepared before a shared start gate; one warmup and three measured
rounds run per group. No allocations or logging are added to the submit timing.
The finite burst borrows Linux rcuscale's bounded outstanding-work principle,
but does not implement its continuous replenishment workload.

`RCU_ASYNC_SAMPLE` separates submit duration, callback-entry latency and
`drained_upper_ns`: submission start to global drain return. The last is an
upper bound on callback return, not its exact timestamp. After all producers
join, `not_entered_before_drain` counts observations whose callback has not yet
entered; this is a sequential snapshot, not exact instantaneous queue depth or
pending-return count. `drain_ns` includes the normal drain implementation,
and `elapsed_ns` spans the start gate through drain return, including joins
and observation scanning. Logging is outside these intervals. The record cap
is workers times batch; this does not measure allocator overhead or peak bytes.
Require all expected samples, groups and rounds, correct callback CPU/context,
monotonic timestamps, `RCU_ASYNC_PASS` and normal exit. A burst with zero pending
entries before drain cannot prove forced-backlog drain behavior. This is not
the complete backlog/memory acceptance workload.

A separate backlog phase runs on systems with at least two present CPUs.
It preallocates 256/1024 records, enters a real task read scope on the last CPU,
then submits the burst only after observing read entry. The reader spins with
IRQs/preemption enabled and a five-second failure deadline; it never sleeps or
yields inside the scope. After submission, the coordinator requires no observed
release and no reader timeout before allowing that scope to finish. It then
drains callbacks and joins the reader. Any timeout or incomplete accounting fails.
One warmup and three measured rounds run for each size.

`RCU_BACKLOG_RELEASE` is the sorted series of per-record timestamps taken after
freeing the callback's own allocation. `remaining` counts records not yet
confirmed released at that point; the timestamp comes before callback return,
so it is not an executor-return timestamp or exact instantaneous queue depth.
An independently owned observation survives the allocation; no freed node is
inspected. Instrumentation includes an Arc clone/drop and clock read per callback.
`RCU_BACKLOG` reports the fixed peak record count and requested record payload
bytes (`count * size_of::<Record>()`), not allocator metadata, observation
storage, fragmentation or total kernel memory. `release_to_drain_ns` includes
reader exit scheduling and the global drain, not merely callback execution.
Require every record timestamp, zero final remaining, `RCU_BACKLOG_PASS` and
normal exit. UP emits an explicit skip and gains no backlog coverage. This
controlled workload supplements rather than replaces continuous pressure and
exact callback-return measurements. It does not change the production service.

The runner finally measures concurrent synchronous waits with 1, 2 and 4 pinned
workers (skipping counts above the present CPU count). Each group has one warmup
and three measured rounds, with 128 calls per worker. A shared start gate opens
only after every worker has allocated its sample storage and all load readers
are ready. Loads run separately: no readers (`hold_ns=0`), and one reader
per present CPU holding each public read scope for at least 10 us or 1 ms.
Readers remain interruptible/preemptible, spin only for the finite clock-based
interval and yield outside scopes. Unlike Linux `rcuscale`'s empty IRQ-masked
reader load, this deliberately exercises finite preemptible read sections.
Each positive duration also runs a CPU-load control with the same body but no
RCU scope (`read_scopes=0`, versus `1` for real readers). This distinguishes
CPU/scheduler pressure from scope-related work; it does not isolate individual
internal RCU stages. There is no object replacement; these are wait costs, not
ownership checks.

`RCU_SYNC_SCALING_SAMPLE` records each full public call duration on its pinned
CPU. `RCU_SYNC_SCALING` reports pooled nearest-rank percentiles and total completed
calls. Its `elapsed_ns` uses the coordinator's clock from opening the gate until
all joins finish, including scheduling overhead but excluding logging. Divide
completed calls by that interval for observed batch throughput; this is not the
number of distinct GPs, since concurrent waits may share a GP. Require 128
successful calls per worker per round, all worker joins and
`RCU_SYNC_SCALING_PASS`. A timeout fails the run. Exclude round 0 from analysis
and group results by `hold_ns`, `read_scopes` and worker count.

Each `RCU_SYNC_READER` reports its CPU, completed scopes sampled after writer
joins but before stopping readers, final total, and actual minimum/maximum
body duration (`min_body_ns`/`max_body_ns`, including the unprotected control).
Require nonzero in-window progress for every load reader,
final total at least that count, and minimum at least `hold_ns`. Reader cleanup
is outside the throughput interval. Clock polling and progress counters are
part of this synthetic pressure; involuntary preemption can extend its duration.
The workload does not guarantee every wait overlaps a reader or prove early
reclamation impossible; deterministic retirement tests cover that obligation.

## Preempted Reader Scheduling

`RCU_PREEMPT_*` compares identical finite clock-polled bodies with and without
a public read scope, for 1/2/4 pinned readers. Each reader has a competitor
pinned to the same CPU. Inside each of sixteen 10ms bodies, a unique nonzero
marker is published and cleared before returning. The body never blocks or
yields. A competitor observing that marker therefore witnesses a real switch
away from the active body, not a same-task scheduler decision. Count distinct
witnessed bodies, not total switches; every reader must have a nonzero count.

Round0 is warmup; rounds1-3 alternate mode order. Output includes body witness
counts, fixed arithmetic-work iterations and elapsed time for each reader and
the batch. Joins/dispatch contribute to batch elapsed time; allocation and logs
do not. Clock polling, marker operations and competitors are part of both
controls. This is synthetic workload throughput under actual preemption, not
isolated context-switch instruction cost or GP latency. There is no updater.
All readers and competitors are joined. Run only via opt-in RCU_BENCH with an
external timeout; a missing witness fails instead of silently claiming coverage.
