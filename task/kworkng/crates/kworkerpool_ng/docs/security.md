# Execution Service Security

## Scope And Trust Boundary

This analysis covers the entire `kworkerpool_ng` crate, including all modules and
kernel tests; no modules are excluded. Public interfaces are kernel implementation
contracts. There are no public raw-pointer parameters, FFI/assembly boundaries or
direct user, device, network or file-data inputs. CPU IDs, limits, intervals,
source adapters and executions come from trusted kernel callers.

The crate trusts `kspin` locking/pinning, `intrusive-collections` list ownership,
`kcompletion` completion and `ktask` task waits, affinity, exit publication and timers.
It trusts the immutable boot CPU map, `khal` identity/time, and `kirq` context.
BH notification depends on `kipi` and `kirq` retaining notifications without
allocation, sleep or inline business callbacks. System integration must initialize
providers, install/retain actions and serialize BH passes. Management adapters
must preserve acquired execution lifetime independently of source inspection.

## Protected Assets

| Asset | Protection |
| --- | --- |
| `PartitionSource::state`, `links`, `owner` | Unique Weak dispatcher identity; its lock protects state and membership |
| `PartitionDispatch::sources` | Arc-owned intrusive nodes under SpinNoIrq; source release outside locks |
| `SourceDetachment::state`, `completion` | Protected outstanding-inspection count and detachment marker; completion is observe/wait-only to callers |
| `NormalWorkers::pool`, `wakers`, `manager_waker`, `stop_requested` | Worker lock; demand/park decisions also hold dispatch lock |
| `WorkerManager::tasks` | Exclusive manager mutation; release only after published task exit |
| `NormalServices::runtimes` | Complete boot coverage, retained until explicit joined stop |
| `BottomHalfServices::cpus` | Complete detached CPU/priority coverage; retain through producers and active passes |
| Manager timer and task waits | Prepared in task context; timer drained before manager wake withdrawal |

## Unsafe Inventory

| Site | Operation and required invariant |
| --- | --- |
| `src/dispatch.rs`: Sync for PartitionSource<S> | S: Send + Sync; all private mutable state/list access uses its unique owner lock; payload is shared |
| `notify`, `detach` | UnsafeCell mutable access after Weak owner identity check; no reference escapes the lock |
| `detach` | cursor_mut_from_ptr uses Arc-derived pointer; protected Queued state proves exact-list membership and Arc retains storage |
| `begin_inspection` | UnsafeCell access to a node removed from the same locked dispatcher |
| `PartitionInspection::return_source` | UnsafeCell access under the original dispatch lock; inspection borrow retains dispatcher and Arc retains node |
| `src/normal/manager.rs`: run_with_startup | unsafe Timer::start from initialized CPU-bound manager; owned timer outlives delivery, IRQ-safe handler only wakes a prepared task, no service lock held |

The intrusive adapter macro uses the existing library's field-offset/list machinery.
No hand-written raw ownership reconstruction is added. Arc references preserve
allocation/provenance through list transfers. The Weak identity prevents allocation
reuse while ownership comparison remains possible. Independent completion storage
outlives final source payload release and holds no source/dispatcher reference cycle.
Tests exercise these production boundaries through safe fixtures; no additional
hand-written unsafe test boundary is present.

## Threats And Responses

| ID | Asset/boundary and trigger | Impact | Response and residual responsibility |
| --- | --- | --- | --- |
| T01 | Source demand arrives during empty inspection or just before worker park | Lost progress | Mitigate with InspectingNotified, protected demand/park handshake and durable task wait; producers publish then kick |
| T02 | Foreign source or invalid list removal | State race or memory corruption | Mitigate with Weak owner check and locked membership transitions before cursor construction; invariant failures panic |
| T03 | Repeated detach or completion before inspection-reference release | Premature teardown or resurrected demand | Mitigate with permanent detached states, first closure only and independent retirement accounting |
| T04 | Adapter claim depends on temporary source storage | Dangling callback data | Transfer lifetime responsibility to the adapter's acquired execution contract; dispatch retains no concrete work storage |
| T05 | Callback, destructor or wake reenters while locked | Deadlock | Mitigate by unlocked acquisition/run, removed-reference release and wake delivery; lock order remains dispatch -> workers/completion |
| T06 | Worker activation precedes phase publication or slot reused before exit | Duplicate ownership or capacity overflow | Mitigate by publishing Prepared before activation and releasing capacity only after observed exit/reference release |
| T07 | Sleeping callback mistaken for idle worker | Premature retirement | Mitigate: Occupied spans callback and cleanup; only Parked retires, with an idle baseline retained |
| T08 | Late notification targets stopped service or uninitialized BH action | Stalled demand or wrong handler | Transfer quiescence/readiness to integration; prepared Waker is not service ownership; no hotplug |
| T09 | BH pass on wrong CPU, in hardirq or recursively | Wrong execution context or concurrent work | Mitigate CPU pinning and effective-context checks; transfer pass serialization and nonsleeping callback rules to provider |
| T10 | Source or stop notice arrives before manager startup/park | Lost management request | Mitigate retained demand/stop_requested and PreparedTaskWait; positive timer recheck observes worker exit |
| T11 | NormalServices dropped without joined stop, or stop invoked by its own worker | Retained live tasks or deadlock | Explicit owner stop protocol; no blocking Drop or automatic self-dependency recovery |
| T12 | Saturated pool or callback never returns | Work starvation | Bound slots and retain demand; accepted residual dependency on callback progress, no forced termination |
| T13 | Invalid initial idle interval or later deadline overflow | Bad timer programming or early retirement | Reject zero/unrepresentable initial deadline; omit unrepresentable later idle deadlines and retain usable workers |

## Failure Handling

Recoverable startup/context/CPU errors use `NormalRuntimeError` and
`BottomHalfPrepareError`. BH execution rejects with `BottomHalfRunError`
before acquiring work. Invalid source ownership and impossible state/count
transitions panic; adapter, callback and Waker panics are not recovered.
Allocation follows kernel policy; this crate does not supply recoverable OOM handling.

| Failure mode | Local/system effect | Control |
| --- | --- | --- |
| Partial normal startup fails | Some manager tasks already exist | Stop and join successful prefix in reverse order before returning original error |
| Partial BH preparation fails | Detached service storage exists | Ordinary ownership releases all prepared CPU/priority entries; no action was published |
| Runtime resources saturated | Backlog waits | Keep accepted demand; occupied completion/retirement recovery enables progress |
| Source detached while inspected | Source not immediately quiescent | Receipt completes after unlocked inspection-reference retirement, not callback completion |
| Manager stop requested | Workers and timer may remain active | Quiescent stop, exit reaping, timer drain, manager wake withdrawal, then join |

## Privacy, Limits And Audit

Payloads are opaque trusted kernel data; this crate does not parse or log user
content. Hostile callbacks and producer teardown misuse are outside the safe
service-progress contract. CPU hotplug, forced cancellation, panic unwind recovery,
reclaim-rescuer guarantees and wall-clock bounds are not supported.
Published task exit is not evidence of final DeferredArc worker stack release.

Audit source ownership, every state/list transition, closure/count conservation,
dispatch-before-worker lock order, unlocked wake/destruction, startup rollback
and task/timer drain together. Tests cover both pure transitions and real task
lifecycle; system readiness, concrete work lifetime and real IRQ no-allocation
acceptance remain integration tests rather than claims of this standalone crate.

Source-detachment publication uses an Arc-owning DeferredWake from kcompletion.
Its receipt stays alive until the deferred task wake runs outside the control lock.
Callers may only observe or wait on the permanent receipt, never complete or reinit
it. CompletionWait validates task context without registration or waiter allocation;
source and receipt allocation at setup remains necessary.
