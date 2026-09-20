# ktimer-core Security and Reliability

## Scope and Trust Model

This analysis covers all production modules in `src/`, the `cfg(unittest)`
test-support boundary and the host harness in `benches/host/`.
The core owns prepared timer state, not task scheduling or POSIX timer semantics.
Production IRQ/IPI and hardware adapters live in `ktask` and `kipi`, outside this
crate's implementation audit; their contracts at this boundary remain in scope.
The platform invoking unsafe public methods remains responsible for CPU readiness,
execution context, monotonic time, hardware arbitration and remote notification.

Kernel callers provide times, CPU IDs and handler implementations.
The core validates representable times and CPU bounds; it cannot verify arbitrary
callbacks' progress, lack of unwinding, or hardware readiness.
`TimerHandler: Sync` permits shared access, not arbitrary IRQ-context behavior.
Synchronous cancellation is not permission to violate callback lock ordering.

## Assets and External Boundaries

| Asset or boundary | Data direction and protection |
| --- | --- |
| `TimerData<H>` node/handler allocation | `Timer` owns it; tree and dispatch borrow it. Preserve stable address, initialized type and lifetime until all accesses drain. |
| `TimerNode::deadline` and intrusive membership | Replacements and dispatch mutate under the revalidated owner lock; never change an indexed key in place. |
| `TimerIndex::earliest` | Non-owning pointer to a current member; update before removed storage may be released. |
| `TimerNode::owner_cpu` and `RunningTimer::node_addr` | Routing and comparison identity, not authorization or lockless access rights. |
| Platform into `start_on`, `start_on_if_future`, `dispatch_due` | CPU, time and execution assumptions cross an unsafe API boundary. Platform supplies readiness and service; core supplies bounds checks and serialized state. |
| Core into `TimerHandler::on_expiration` | Shared handler borrow plus event/control; callback remains trusted kernel code and must obey IRQ and lifetime constraints. |
| Core into clock/programmer closures | Clock under the future-start lock, dispatch clock outside locks, programmer under the CPU lock. Each must obey its specific public Safety contract. |
| `test_support::with_armed_timer` | Test driver supplies callback and exercise closures; stack storage remains internal and is drained before return. |

There are no public raw-pointer parameters, direct user-memory accesses, MMIO/PIO,
DMA, firmware/boot-metadata parsing, file/network parsing, FFI or assembly in the
core source. Hardware writes are delegated through the supplied programmer.
CPU ownership is a synchronization route, not a credentials check.
Business tags and accumulated counts are not stored by the core. Handler-owned
state requires its own synchronization; a mutable handler field is not an
immutable per-registration payload when replacement races with delivery.
Future syscall adapters must validate their own user input and permissions.

## Memory Safety and Lifetime Invariants

1. `Timer::new` allocates one private `Arc<TimerData<H>>`. While uniquely owned and
   unpublished, it binds the initialized, correctly aligned handler pointer to
   the monomorphized trampoline for the same `H`. No mutable allocation access,
   Arc extraction or raw node escape is exposed after publication.
2. Every participating node and handler stays fixed, initialized and live through
   pending membership, running delivery, callback-local destruction and external
   accesses. `UnsafeRef` and `Pin` do not establish ownership or extend lifetime.
3. `handler()` derives its shared borrow from the bound pointer used by dispatch.
   It does not create an unrelated allocation borrow that could invalidate that
   pointer when the handler's interior-mutable state is written.
4. `Timer::drop` cancels synchronously before fields release the private Arc.
   Its exclusive borrow covers the shell, not the separately allocated data still
   shared by a callback. Final handler destruction must suit the calling context.
5. The owner lock serializes deadline reads/writes, membership and running identity.
   A deadline is replaced only after unlinking. Link state alone cannot prove which
   tree owns a node; removal by address requires exact membership.
6. Dispatch publishes running before unlocking. It ends callback resources and
   all old delivery references before clearing running. Clearing uses only integer
   identity comparison, never a reconstructed dereferenceable pointer.
   Any subsequent delivery obtains a new borrow under the owner lock.
7. Dispatch snapshots the consumed deadline under the owner lock. Explicit rearm
   and external start serialize replacements; completion never changes or removes
   their pending successor. Returning from a callback does not implicitly rearm.
8. The private `TimerControl` constructor binds one node to its current delivery.
   Its lifetime and `PhantomData<*mut ()>` prevent escape or cross-thread use.
   Forgetting it cannot prevent dispatch's independent running cleanup.

`H: 'static` prevents borrowed stack captures in an owned timer. Forgetting `Timer`
retains its allocation and may leak, but does not invalidate the core's borrows.
The index neither retains nor releases Arcs; its Drop only unlinks live nodes.
Owners must explicitly drain asynchronous accesses before reclaiming storage.

## Thread Safety and Progress

CPU slots use `karch::cache::CacheAligned` for the complete lock and protected
state, following the architecture-selected `SMP_CACHE_BYTES`. This changes
layout only, not ownership, locking or lifetime contracts; padding is never
interpreted as initialized data. Compile-time layout assertions and a host
array-address test guard the isolation policy. Larger hardware cache lines may
still cause false sharing, which is a performance limitation, not a safety gate.

`smp` enables cross-CPU atomic exclusion in `kspin`; a multi-slot `TimerSet<N>`
without that feature is not a concurrent implementation. Preemptible kernels must
also propagate `preempt`. The current `ktask` dependency forwards both features.

`owner_cpu` publishes routing with Acquire/Release. Readers lock and revalidate
before accessing deadlines. Migration is allowed only when no delivery is running,
including cleanup; pending membership alone does not prevent migration. It takes
old/new locks in CPU-number order after dropping any single owner lock, removes
old membership and completes insertion before publishing the route. Observers
cannot access the temporary unlinked state through the owner-lock protocol.
One node uses one stable set for its entire participation. Changing to a different
set with numerically identical CPUs would not satisfy this invariant.

`TimerNode` implements `Sync` because safe shared methods use atomic fields and all
deadline/index operations require serialization. Its immutable callback pointer is
only borrowed as `&H` with `H: Sync`. Its `Send` moves an unpublished node, not the
handler; moving an active node remains forbidden by registration contracts.
`CpuTimers: Send` transfers collection access without moving nodes; its cached
pointer obeys the same live-storage contract and running data contains only identity.
There is no manual `CpuTimers: Sync` implementation. `Timer<H, N>` uses auto traits
through its Arc, so transfer/sharing additionally requires `H: Send + Sync`.

Dispatch on a CPU must be unique and nonrecursive, with production IRQs disabled
and migration prevented. Handler and callback-local cleanup run outside timer locks.
The dispatch clock also runs outside locks; the future-only start clock runs once
under the final lock(s) and must not take timer locks or call business code.

Synchronous cancellation releases timer locks before spinning, rechecks routing
on every retry and never reports completion merely because the callback is slow.
It drains timer delivery, not arbitrary business references. Durable quiescence
requires stopping external starts first. Self-wait, callback lock inversion or
preventing the servicing CPU from progressing can deadlock; task/BH/hard-IRQ callers
must all exclude these cases. Cancellation is reusable, not permanent shutdown.

## Unsafe Boundary Inventory

Each row groups operations with the same explicit invariant. The named Safety
contracts and adjacent SAFETY comments in these files remain the detailed proof
obligations; this inventory does not replace them. An entry may identify an
`unsafe fn`, an unsafe block inside a safe function, or an unsafe trait
implementation; inclusion does not by itself make the named API unsafe to call.

| Source and entities | Unsafe operation, required invariant and enforcing boundary |
| --- | --- |
| `src/timer.rs`: safe `Timer::new` | Its internal unsafe block calls private `TimerNode::with_handler` to bind the typed handler pointer. The uniquely owned, unpublished Arc is already at its final address; the constructor establishes the invariant without caller safety obligations. |
| `src/timer.rs`: safe `Timer::handler`, `try_cancel`, `cancel_sync` | Internal unsafe blocks recover the shared handler or call owner-routed node operations. The private stable Arc, static set and exact type pairing enforce ownership and lifetime. Safe `Drop::drop` calls `cancel_sync` before field destruction; it contains no unsafe block itself. |
| `src/timer.rs`: `unsafe fn Timer::start_on`, `start_on_if_future` | Internal unsafe blocks register the privately owned node through the timer set. Storage invariants are enforced by the owning wrapper; callers must additionally uphold the documented platform placement, dispatch, clock and notification contracts. |
| `src/index.rs`: `TimerNode::with_handler`, `call_typed_handler`, `handler`, `invoke` | Construct erased pointer/function pair and recover shared `&H`. Pointer originates from initialized `&H`, has its alignment/type, and remains valid without moves, exclusive borrows or destruction; invocation uses the live running owner outside locks. Placeholder bindings must never be used for typed handler access. |
| `src/index.rs`: `TimerNode::deadline`, `replace_deadline`, `NodeAdapter::get_key` | Read/write `UnsafeCell<Deadline>`. Owner serialization excludes competing writers; replacement requires unlinking. The private adapter reads stable linked keys only. |
| `src/index.rs`: `TimerNode::set_owner_cpu` | Publish route only after ordered old/new locking, route revalidation, exclusion of running delivery, old membership removal and new membership insertion; all operations use the same set. |
| `src/index.rs`: `TimerIndex::insert`, `remove`, `earliest_deadline_ns`, `pop_due`, generated `NodeAdapter` | Intrusive borrowed pointers and cursor-from-pointer removal. Insert requires stable live unlinked nodes; removal requires membership in this exact tree. Cached pointers always name current members and are updated on removal. The adapter uses the actual embedded link field. |
| `src/index.rs`: `unsafe impl Send/Sync for TimerNode` | Shared atomics and serialized interior mutation; `H: Sync` shared callback access. No ownership transfer of handler data or active-node movement is authorized. |
| `src/cpu.rs`: `unsafe impl Send for CpuTimers` | Moving the collection does not move nodes; tree/cache borrows and serialized mutations retain their insertion lifetime contract. |
| `src/cpu.rs`: `arm`, `try_cancel`, `take_expired` | Create/remove borrowed membership and update deadline/running state under exclusive CPU access. Stable storage and exact tree membership come from owner-routed operations. |
| `src/cpu.rs`: `clear_running` | Finish every node access before clearing the matching address record; retain storage through that clear. Completion does not modify pending membership. |
| `src/dispatch.rs`: `dispatch_round` | Invoke bound handlers, then end borrows and clear running. Matching CPU, serialized service, non-unwinding callbacks and live storage are supplied by its caller. No callback holds the tree lock; this function does not sample the clock. |
| `src/set.rs`: `arm_on`, `arm_on_if_future`, `arm_on_checked` | Owner-routed tree mutation and optional locked clock callback. Fixed live set/nodes, serviceable selected CPUs, no bypass mutation; clock/accept closures are nonallocating, nonsleeping, nonreentrant, non-unwinding and do not acquire timer locks. |
| `src/set.rs`: `try_cancel`, `cancel_sync`, `cancel_sync_with` | Revalidated owner mutation and wait/retry. Retain node/handler through all accesses, release locks before waiting, preserve progress, and drain external users before freeing. The test retry closure must also preserve this contract and not unwind. |
| `src/set.rs`: `dispatch_due` | Forward the borrowed-dispatch contract on the matching CPU. All mutation uses the owner protocol; handler/clock behavior, stable storage and unique IRQ-disabled service are platform obligations. |
| `src/set.rs`: `reprogram_local_timer` | Call the platform programmer under the owner lock. Programmer and capture cleanup cannot allocate, sleep, unwind, invoke business callbacks or take timer/runqueue locks; merge sources and write locally before returning. Deferred writes require a new call. |
| `src/control.rs`: `TimerControl::new`, `rearm` | Construction ties node, lock and lifetime to the running delivery; external ownership also covers any successor. Validated rearm stays on that owner, whose migration is blocked by running. |
| `src/test_support.rs`: `with_armed_timer` | Internal stack node/handler survive exercise and synchronous drain. Driver preserves CPU/context/progress, requests hardware refresh when needed, and forbids unwinding from exercise/callback; no node escapes. |

Inline test unsafe calls exercise the same node, tree and dispatch boundaries.
Their local owners must outlive every pending/running access and drain before
assertions can end the test. SMP scenarios in `src/set.rs::tests` use local Arcs,
Release/Acquire handshakes and joined workers, not a global storage provider.
Only the dispatch worker completes callback cleanup and clears running; a cancel
observer must not do that work on its behalf.

The host harness imports these core sources and the actual `kspin` lock with
host NoOp guards. Its counting allocator in `benches/host/main.rs` forwards valid
`GlobalAlloc` layouts/pointers unchanged to `System`; its unsafe delegation remains
subject to that allocator contract. Host tests and benchmarks use stable owned
storage or fixed arrays, modeled CPUs, serialized per-CPU dispatch and explicit
draining. They do not establish real IRQ/preemption safety.

## Threat Analysis

Severity: High means UB or memory corruption; Medium means deadlock, panic or
incorrect service; Low means overhead or diagnostic limitations.

| ID | Asset/boundary and threat | Severity | Trigger | Implemented response and residual risk |
| --- | --- | --- | --- | --- |
| T01 | Borrowed node/handler lifetime | High | Owner frees or moves data while pending/running | Private owning Timer storage and synchronous Drop drain; internal raw registrations remain unsafe and must retain external storage. |
| T02 | Tree/deadline integrity | High | Wrong-tree removal or unsynchronized linked-key mutation | Private index plus revalidated owner locking and unlink-before-replace. Unsafe internal callers must prove exact membership; the link bit is not sufficient. |
| T03 | CPU state serialization | High | Multiple CPUs access a non-SMP lock or recursively dispatch one CPU | Feature propagation and explicit platform Safety contracts. Slot count and runtime bounds checks cannot detect misconfigured synchronization. |
| T04 | Delivery progress | Medium | Self-cancel waits, callback lock inversion, nonreturning callback | Locks released before callback and cancel wait; documented progress obligations. No watchdog, forced callback interruption or timeout-success fallback is provided. |
| T05 | Hardware notification boundary | Medium | Remote advance not notified, stale snapshot written, programmer ignores other sources | Actual owner returned by start; locked programmer contract requires fresh merge/write. Notification, barriers and real hardware arbitration remain platform responsibilities and later acceptance work. |
| T06 | Deadline integrity | Medium | Oversized deadline or invalid service clock | `Deadline::new` rejects unrepresentable input before mutation. Delivery snapshots the accepted deadline. Monotonicity and staying below the horizon remain platform obligations. |
| T07 | Software timer service availability | Medium | Explicit near-future rearms repeatedly become due during callback execution | Three rounds and CPU-local service backoff preserve original deadlines and other hardware sources. Infinite explicit immediate rearm or a nonreturning handler remains outside this bound. |
| T08 | Callback/control boundary | High | Wrong erased type, escaped node/control or premature running clear | Constructor pairs type/function, control fields are private and non-Send/Sync, dispatcher owns cleanup. Handler implementation still must not unwind or violate context. |
| T09 | Allocation availability and retention | Medium | Preparation OOM or deliberately forgotten owner | Existing Arc/allocator failure policy; no recoverable OOM API here. Forgetting retains storage safely but can leak; repeated unbounded preparation needs upper-layer resource policy. |

## Failure Modes and Effects (FMEA)

Severity: 1 fatal, 2 serious, 3 moderate, 4 minor.

| ID | Failure mode | Cause | Local effect | System effect | Severity | Control / validation |
| --- | --- | --- | --- | --- | --- | --- |
| F01 | Invalid explicit setting | Bad CPU or unsupported time | Request rejected without mutation | Existing timer continues | 4 | `Deadline::new`, CPU bounds checks; `deadline.rs` boundary tests and `timer.rs` invalid-target case |
| F02 | Wait deadline already elapsed | Time reaches deadline before locked check | `Ok(None)`, original setting retained | Waiter must resolve its own timeout/wakeup handshake | 4 | `owned_wait_rejects_elapsed_without_replacing_pending_timer` |
| F03 | Old completion overwrites a successor | External start races callback return | Wrong next deadline | Wrong timeout behavior | 2 | Completion never rewrites pending state; explicit callback/remote replacement-order tests and immutable event snapshot checks |
| F04 | Storage reclaimed too early | Destruction races delivery | Invalid borrowed pointers | Memory corruption | 1 | Owning Drop drain; host `ownership.rs` and `ownership_races.rs`, kernel cleanup handshake |
| F05 | Cancel never completes | Callback cannot progress or starts never stop | Spin wait persists | CPU/service starvation | 2 | Progress and producer-stop contracts; no forced recovery; deterministic cancel tests release owner lock |
| F06 | Timer interrupt monopolizes CPU | Explicit rearm workload exceeds service rate | Repeated overdue work | Scheduling delay | 2 | Bounded rounds/service backoff; host overload tests; excludes nonreturning callbacks |
| F07 | Earlier deadline missed | Lost owner notification or stale hardware write | Timer serviced late | Delayed wait or maintenance | 2 | Locked `reprogram_local_timer` tests cover snapshot arbitration; real IRQ/IPI remains outside this PR |
| F08 | Harmless stale IRQ | Cancellation or migration removes membership without refreshing old CPU hardware | Empty/changed timer tree at service | Extra interrupt overhead | 4 | Next platform arbitration must recompute all sources; no stale callback storage is retained |

## Failure Handling and Privacy

Public invalid-time/CPU errors use `KError::InvalidInput` before changing a setting.
`start_on_if_future` distinguishes a valid already-elapsed deadline with `Ok(None)`.
`try_cancel` reports `Inactive`, `Removed` or `Running`; the latter preserves the
pending successor. `cancel_sync` retries with `spin_loop` and returns
whether it removed pending membership, not whether it waited.
Deferral makes `dispatch_due` return `Ok(false)` without invoking handlers.
An invalid CPU is rejected before clock/programmer execution.

`TimerSet::new` panics for zero slots. Internal assertions/expectations check
construction, membership, running identity and deadline invariants, not
recoverable external-input errors. There is no lock poisoning or unwind recovery.
OOM during Arc preparation follows the configured allocator failure path; no
registration-time allocation failure is introduced after preparation.

The core does not log handler contents, pointers or timestamps and stores
no persistent user data. Handler-owned information and any callback logging remain
the caller's responsibility. Host benchmark output contains timing/configuration
metadata rather than production callback payloads.

## Validation and Known Limits

- Inline tests in `deadline.rs`, `index.rs`, `cpu.rs`, `dispatch.rs`, `timer.rs` and
  `set.rs` cover range checks, cache identity, state transitions and cleanup.
  `ktask` drives the two pinned worker scenarios and verifies CPU placement/join.
- `benches/host/owned_checks.rs` counts actual owning-API operations after preparation
  and measures final release separately. The allocation probe includes a positive
  control. `comparison.rs` lifecycle mode intentionally includes allocations.
- Basic host timing uses preallocated storage and a single CPU model; concurrent
  `ownership.rs` / `ownership_races.rs` are separate checks, not timing samples.
  Host Miri may emit intrusive-library integer-to-pointer provenance warnings;
  retain these limitations rather than claim exhaustive pointer safety.
- Core tests do not prove actual hardware notification, IRQ-disabled execution,
  full business wakeup chains, scheduler integration or long-duration system stability.
  Those require the later platform/caller migration PRs.
- No CPU hotplug, permanent shutdown, arbitrary embedded-storage constructor,
  automatic periodic mode, softirq callback mode, slack or hard real-time bound is implemented.

## Audit Checklist

- Every node operation uses its original set and a revalidated owner lock.
- Linked keys remain immutable; earliest cache always names a live member.
- Explicit replacement changes only the successor; callback completion does not overwrite it.
- No old node reference is used after `clear_running`; cleanup finishes first.
- Public safe APIs expose neither mutable handler storage nor borrowed nodes.
- Added platform callers propagate lock features and uphold IRQ/notification contracts.
- Clock/programmer closures and their cleanup obey their different lock contexts.
- Unsafe inventory, Safety contracts and tests evolve together; modeled evidence is
  never reported as production IRQ/IPI or end-to-end acceptance.
