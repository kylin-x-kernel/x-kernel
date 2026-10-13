# ktimer-core Design

## Purpose and Scope

`ktimer-core` provides prepared timers independently of task scheduling.
It owns timer ordering, one-shot delivery, owner-CPU routing, cancellation,
and the owning `Timer<H, N>` lifecycle. It does not implement POSIX timers;
`process/ktimer` retains that separate responsibility.

`ktask` supplies production IRQ dispatch and locked hardware deadline arbitration;
`kipi` supplies fixed timer-reprogram notifications through a `ktask` endpoint.
These platform adapters remain outside the core. `ktask` also links kernel tests
and supplies pinned worker tasks. Asynchronous waits, timerfd, network/watchdog
migration and delayed work are separate integration work, not implemented here.

The core avoids allocating registration records or temporary due-event batches.
Storage is prepared once, then reused for starts, replacements and delivery.
Construction, final destruction and resources used by caller callbacks are outside
the core's allocation-free operation interval.

## Initial Owner Selection

`Timer::new_on` validates a platform-supplied initial CPU before allocation and
binds the unpublished node to that route. `Timer::new` retains the CPU0 default.
This is a lock-route hint, not CPU affinity or registration. Initializing the
likely first-start owner avoids routing a fresh node through another CPU's locks.
Once published, all route changes still use the existing ordered migration locks;
running callbacks retain their owner. No storage or callback ownership changes.

## Source Layout and Dependencies

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | Crate contract, private modules and public re-exports |
| `src/timer.rs` | Owning `Timer`, public `CancelResult`, synchronous destruction |
| `src/control.rs` | Callback-scoped `TimerControl::rearm` |
| `src/deadline.rs` | Private validated `Deadline` and public `Expiration` snapshot |
| `src/index.rs` | Private `TimerNode`, intrusive `TimerIndex`, public `TimerHandler` |
| `src/cpu.rs` | Private `CpuTimers`, running-delivery identity |
| `src/dispatch.rs` | Borrowed, unlocked callback delivery and completion |
| `src/set.rs` | Public `TimerSet`, private routing, migration and cancel loops |
| `ut-tests/test_support.rs` | `cfg(unittest)` scoped storage and cross-crate test entry points |
| `benches/host/` | Independent host workspace for checks and microbenchmarks |

`Cargo.toml` uses workspace dependencies: `intrusive-collections`, `karch`, `kcpu_id_map`,
`kerrno`, `kspin`, `ktime_types` and `unittest`.
There is no dependency on `ktask`, `khal` or process crates.
The `smp` and `preempt` features forward to `kspin`; defaults are empty.
`ktask` propagates its matching features when depending on this core.

## Architecture and Interfaces

```text
platform adapter                      callback implementation
    | new/start/cancel                       ^ on_expiration
    v                                        |
Timer<H, N> --owns--> Arc<TimerData<H>> = { TimerNode, H }
    |                         ^
    | bound to static set     | borrowed, never retained by the index
    v                         |
TimerSet<N> --> [CacheAligned<SpinNoIrq<CpuTimers>>; N]
                            |
                            +-- pending: TimerIndex
                            +-- running: Option<RunningTimer>
                            +-- dispatch_after_ns: Option<u64>
```

The public interface groups are:

- A platform adapter constructs `TimerSet<N>` and calls `dispatch_due` and
  `reprogram_local_timer` for CPU-local service and hardware arbitration.
- An owner prepares `Timer<H, N>`, accesses shared handler state with `handler`,
  and cancels with `try_cancel` or `cancel_sync`. Starting through `start_on` or
  `start_on_if_future` remains unsafe: placement, CPU readiness and subsequent
  refresh/notification belong to the platform adapter.
- The caller implements `TimerHandler` or supplies a compatible `Sync` closure.
  Dispatch calls it with a by-value `Expiration` and a scoped `TimerControl`.
  The handler may use `rearm` to replace its next setting without exposing a node.
- Under `cfg(unittest)`, a kernel harness calls `test_support::with_armed_timer`;
  with `smp`, it can also drive the two worker scenarios re-exported there.

Node, index, deadline validation, CPU data, raw registration and implementation modules stay private.
The public cancellation result carries no internal node handle.
Method-level input, output, error and safety contracts live in rustdoc.

## Execution Context and Initialization

`TimerSet::new` is a nonallocating const constructor, not a hardware-ready signal.
Each CPU slot uses `karch::cache::CacheAligned` around both its spinlock and
`CpuTimers`. Its array stride follows the architecture-selected `SMP_CACHE_BYTES`
policy, isolating adjacent slots at that granularity without changing lock
ordering or the public API. This
trades per-CPU padding for less false sharing; it does not eliminate contention
on the same slot or promise isolation on processors with larger cache lines.
It rejects zero CPU slots. `Timer::new` requires an initialized allocator and a
context that permits allocation. Subsequent lock operations require initialized
IRQ/preemption guards, but not a current process thread.

Cross-CPU concurrency requires `smp`; preemptible kernels must enable `preempt`.
The value of `N` only sizes the array and does not enable synchronization.
Host concurrent tests must also enable their SMP lock implementation.

Production dispatch must run on the matching CPU with local IRQs disabled and
migration prevented. Dispatch on one CPU must be nonrecursive and nonconcurrent.
Do not enter it holding timer or runqueue locks. Handlers run outside timer locks
and must not sleep, unwind, enable IRQs or wait for their own delivery.
These obligations also apply to callback-local destructors.

Start, replacement and cancellation use IRQ-safe spinlocks. Synchronous cancellation
does not sleep, but may spin waiting for another CPU. Callers must not hold a lock
needed by the callback or prevent the servicing CPU from progressing.
Final handler destruction must suit the context in which `Timer` is dropped.

## One-Shot Deadlines and Ordered Storage

Every accepted start installs one absolute monotonic deadline. Delivery consumes
that registration. The callback may explicitly call `TimerControl::rearm` to
install another deadline; returning alone neither creates a successor nor cancels
one installed by another caller. There is no restart return value or stop flag.

Intervals, periodic phase, overrun accumulation and business tags belong to the
handler or a higher layer. The core supplies no automatic periodic wrapper or
compatibility parameters. Shared handler state needs its own synchronization:
moving a per-setting tag into a mutable field does not preserve its association
with an old delivery during concurrent replacement. Callers requiring that
association must define their own protocol.

`Deadline` holds only a validated u64 nanosecond value. Conversion from
`MonotonicInstant` rejects values above `u64::MAX` before mutation.
Zero and past deadlines are valid. The maximum is a far-future horizon, not a
cancellation sentinel or a promise of service beyond the clock domain.
The service clock must stay below that horizon.
`Expiration` snapshots the selected registration's deadline under the owner lock,
so later replacement cannot alter the current callback's event.

`TimerNode` contains one `Deadline` in an `UnsafeCell`, an `RBTreeAtomicLink`, an atomic
owner route and an immutable typed handler binding. `TimerIndex` uses
`RBTree<UnsafeRef<TimerNode>>` from pinned `intrusive-collections` 0.10.2.
It neither allocates nor owns nodes. Its non-owning minimum-node cache makes the
earliest query O(1); insert, known-member removal and each due extraction are O(log n).
Removing the minimum updates the cache from the removal cursor's successor.
Equal-key insertion relies on this pinned library's rightward insertion behavior;
the public API does not promise FIFO order for equal deadlines.
Any deadline replacement occurs after unlinking, never by modifying an indexed key.

## States and Concurrency

There is no public state enum. Pending membership and the CPU's running record
jointly represent the following operational states:

| State | Start or replace | Non-waiting cancel | Synchronous cancel |
| --- | --- | --- | --- |
| Idle | Insert; may select another owner CPU | `Inactive` | Return without waiting |
| Pending, without in-flight delivery | Replace on the explicitly selected CPU | Remove and return `Removed` | Remove and return |
| Running, including local cleanup | Install successor on current owner | `Running`, preserving successor | Unlock and wait, then remove any successor |

A running timer may also have a pending successor, but its next delivery cannot
start until the previous running record is cleared. `RunningTimer` contains only
`node_addr` for identity comparison; it holds no restart flag or deadline copy.
Explicit rearm and external start serialize under the owner lock. The last
successful setting wins; invalid input leaves the accepted setting unchanged.
Callback completion only retires running identity and never rewrites the successor.

Each `SpinNoIrq<CpuTimers>` protects its tree, running record, service deferral and
the deadlines routed to that CPU. `owner_cpu` uses Acquire/Release for routing only.
Operations read the route, take the corresponding lock and recheck it before access.
A running node cannot migrate, even with a pending successor. Idle or pending
migration drops the single lock, takes the old/new locks in ascending CPU order,
and revalidates route and running state. After accepting the request it removes
any old membership, installs the new membership and publishes the new owner
before either lock is released. Node storage never moves. Replacing pending
membership still reports replacement, not a newly added registration.
Every operation on a node uses the same stable `TimerSet` instance.

### CPU Placement Requirements

Each explicit start selects the requested CPU when no delivery is in flight.
The caller's executing CPU alone does not request migration. A running delivery
includes extraction, callback execution and callback-local cleanup; a start in
that interval retains the actual owner and does not schedule a later migration.
Callback-scoped rearm always keeps that owner. Rejected requests change neither
placement nor deadline. This replaces the earlier active-owner retention policy;
it does not add permanent CPU binding or CPU hotplug support.

Replacement and extraction serialize: replacement first removes the old setting;
extraction first preserves the old delivery and puts the successor on its owner.
Concurrent replacements commit complete CPU/deadline settings, with the last
accepted operation winning. Same-owner replacement remains a single-lock path;
cross-owner replacement requires two locks and must be measured separately.

## Critical Flows

### Start and Future-Only Start

`Timer::start_on` validates a new deadline, then calls the private `TimerSet::arm_on`.
`arm_on_checked` resolves and locks the owner before `CpuTimers::arm` replaces
the complete setting. It returns the actual owner to the platform, which must
refresh local hardware or notify that owner. Ordinary start accepts past deadlines
for asynchronous service and does not call the handler inline.
Every accepted start must arrange that refresh even when another start races it.
A notification services current queue state, not a saved node or deadline. Removing
membership from the old CPU may leave a harmless stale interrupt, as cancellation
does; no source-CPU notification is required by this core.

`start_on_if_future` uses the same route with a locked monotonic clock check.
The clock is sampled once under the final owner lock, or both migration locks.
An elapsed deadline, including equality, leaves the original state unchanged.
Invalid input or CPU selection does not call the clock. Acceptance does not promise
the deadline is still future on return and does not replace a waiter's wake/block handshake.

### Dispatch and Completion

`TimerSet::dispatch_due` calls `dispatch_round` for at most three catch-up rounds.
The due check passes its held CPU guard into the round's first dequeue, avoiding
an adjacent unlock/relock. The round consumes that guard before invoking a handler.
Each round fixes its eligibility cutoff. Under the CPU lock, `take_expired`
removes one due node, snapshots its deadline and publishes running identity.
After unlocking, dispatch invokes the bound handler.
It then locks again, ends the old node borrow and clears running. Under that
same lock it can take the next eligible node; it never prefetches a batch of
borrowed nodes. The new callback again runs after unlocking.
No implicit reinsertion or periodic arithmetic occurs during completion.
The clock is sampled at dispatch entry and after each round, outside timer locks;
per-node observation/completion samples are unnecessary without core accounting.

If three rounds still leave overdue work, service is deferred by the elapsed
dispatch duration, clamped to 1ns through 100ms and saturated at the time horizon.
`dispatch_after_ns` affects software timer service, not node deadlines.
Both dispatch entry and hardware deadline queries respect it; cancellation need
not wait for it. This is not a callback-count budget or a way to interrupt an
infinite callback or repeated immediate self-rearm within one round.

This follows the fixed-cutoff/retry/backoff approach of
[Linux v6.12 hrtimer](https://github.com/torvalds/linux/blob/v6.12/kernel/time/hrtimer.c)
(`__hrtimer_run_queues` and `hrtimer_interrupt`). Here the platform must keep
independent scheduler/accounting deadlines eligible while software timers defer.

### Hardware Arbitration

`reprogram_local_timer` reads the service deadline and calls the platform's
programmer while holding the CPU timer lock. The programmer merges other local
sources and writes hardware before returning. `None` means this tree is empty,
not that all hardware sources can be disabled. A deferred write must re-enter
this method to obtain a current deadline, rather than use a stale snapshot.
The programmer must not acquire timer/runqueue locks, allocate, sleep, unwind
or invoke business callbacks, including during capture cleanup.
The integration lock order is timer lock before hardware programming.
This core sends no IPI and performs no hardware access itself.

### Cancellation and Destruction

`try_cancel` takes the revalidated owner lock once; running delivery is left alone.
`cancel_sync` repeatedly checks under that lock, unlocks when running, and uses
`spin_loop` before retrying the route.
It removes the pending successor only once no delivery is running.
The returned bool reports removal of pending membership, not whether it waited.
Neither cancellation refreshes hardware; a harmless stale interrupt may remain.
Durable quiescence requires callers to exclude new external starts.

`Timer` owns a private `Arc<TimerData<H>>` with node and handler in one allocation.
Unique unpublished initialization binds the handler pointer to its exact type.
The shell can move, but no API exposes mutable data, the Arc or the node.
`handler()` derives its shared reference from the same binding used by dispatch;
handler interior mutability still needs its own synchronization.
Dropping the shell synchronously drains accesses before fields free the allocation.
`&mut Timer` never becomes an exclusive borrow of the allocation during a callback.
`H: 'static` ensures forgetting the shell leaks storage instead of invalidating a
captured stack borrow. No arbitrary embedded-storage initialization API is provided.

## Design Choices

- Intrusive ordered storage eliminates per-registration allocation while retaining
  logarithmic operations and a constant-time minimum query.
- A single node deadline and an immutable delivery snapshot separate the next
  registration from the in-flight event without automatic restart coordination.
- The owning shell provides lifetime retention without per-dispatch Arc cloning.
  A merely borrowed, forgettable cancellation guard would not keep storage alive.
- Platform operations remain unsafe because CPU readiness, interrupt context and
  hardware notification cannot be established by this platform-independent crate.

## Validation and Limits

After configuring the actual platform, kernel tests can be run with:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make unittest UNITTEST_CRATE=ktimer_core,ktask::tests_timer_routing
```

Inline tests cover deadline boundaries, index identity, replacement, cancellation,
one-shot dispatch and owning lifetimes. `set.rs` supplies two-worker scenarios;
`ktask` creates, pins and joins those workers. The cleanup scenario checks both
callback and callback-local Drop phases before cancellation completes.
These tasks actively drive modeled service, not a production timer IRQ.
Non-SMP excludes the two-worker driver. One-CPU execution does not prove non-SMP
feature coverage; build success does not prove guest boot or test execution.

Placement acceptance checks cover both migration directions, old/new minimum
updates, unchanged replacement return values, rejection without route changes,
and delivery on only the selected CPU. Concurrent checks cover replacement,
cancellation and dispatch, preserving running/cleanup ownership and at most one
callback per timer. Prepared migration must allocate and free no storage.

See [host checks and benchmarks](../benches/host/README.md) for commands and limits.
Basic timing preallocates storage and measures uncontended batches at
1/32/256/4096 nodes. Owned-operation probes count allocations separately from final
release. Lifecycle comparison includes construction and destruction. Multithreaded
ownership checks and Miri are distinct from single-thread timing.
Batch-mean medians are not per-operation latency percentiles. Host results do not
establish real IRQ/IPI latency, hardware safety or end-to-end business performance.
Real IRQ/IPI integration checks live in `ktask::tests_timer_irq`; end-to-end
business acceptance belongs to the later caller migration PRs.
The one-shot API intentionally removes interval/tag parameters and accumulated
counts. Later integration branches must migrate their periodic callers explicitly;
old periodic benchmark results do not validate this API. Host checks cover
explicit repetition that stops, both serialized replacement orders, immutable
in-flight deadlines, overload backoff, ownership and cleanup-aware cancellation.
