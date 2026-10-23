# Prepared Workqueue API Security

## Scope And Trust Boundaries

This analysis covers all kworkng production modules, optional diagnostic features
and the isolated/live kernel test fixtures, including the opt-in release runner
in `benches/kernel.rs`. Internal queue state and execution-provider
implementations are analyzed in the adjacent kworkqueue_ng and kworkerpool_ng
security documents. `kruntime` owns the all-CPU startup barrier and initializes
services before exposing driver producers; `knet` owns network callback state.
The `system` feature excludes replaceable registry/action fixtures from live
boot-service tests. Production publication is never reset in either mode.

Inputs are trusted kernel configuration, callbacks, shared captures, queue names,
CPU IDs and monotonic durations. This crate exposes no public raw-pointer or FFI
input and directly parses no user, device, network, file or firmware data. Callers
may capture such data, but its validation remains their responsibility. The private
retirement callback receives a raw pointer produced only by the matching Owner.

Trusted providers are the core queue/timer lifecycle, normal/BH execution services,
ktask context and wait operations, kspin locking, klazy one-time publication, kirq action registration, boot
CPU mapping and the allocator. Workqueue callbacks are trusted code, not isolated
security principals. IRQ-safe publication is not an NMI-safe interface.

## Protected Assets

The release runner is trusted test code selected only by build configuration.
It intentionally occupies workers and replaces userspace startup, so enabling
it in a deployed image is an availability risk. The option defaults off, has no
user-facing trigger, and is excluded from unit-test builds. A host timeout is
required to detect hung drains; a passing soak does not prove absence of leaks.
Runner result and progress records bypass log filtering through `klogger`
console output. Missing pass markers or a timeout still invalidate acceptance;
normal power-off alone is not proof of success.


| Asset | Protection and required lifetime |
| --- | --- |
| SERVICES: Once<Services> | Private write-once storage; Release/Acquire publishes all defaults together; retain successful services for kernel life |
| Test-only TEST_SERVICES: SpinNoIrq<Option<Arc<Once<Services>>>> | Serial fixture selection; readers retain the selected Once for their closure; never reset production storage |
| Services defaults and provider bindings | Immutable after preparation; every allowed CPU/category validated before exposure |
| Owner<W>::is_closed | Shared IRQ-safe gate held through admission; close before drain, never hold while waiting |
| Owner<W>::storage and private queue | Strong references retain stable W; only Owner::new selects the permanent normal reclamation queue, with no replacement API |
| Storage<W>::work: UnsafeCell<ManuallyDrop<W>> | Shared while any handle/API/callback exists; exclusive destruction only after last Owner and drain |
| Storage<W>::reclaim: Work<fn()> | Private one-shot node; never canceled/reused; survives publication and terminal action |
| Optional diagnostic Directory | Queue core owns record synchronization and weak registration; no Work pointer in records |

## Unsafe Inventory

| Site | Invariant and enforcing boundary |
| --- | --- |
| defaults.rs borrowed queue/modify methods | Caller-supplied Pin plus unsafe retention/producer-join/drain contract; selector resolves initialized providers |
| benches/kernel.rs measure/stress publication | Each producer owns a pinned Work and its atomic captures, flushes each occurrence, and synchronously cancels before release. Callbacks do not block, so all three providers are valid. Kernel panic aborts the run rather than unwinding borrowed storage. |
| normal_queue.rs static publication | Static shared Work cannot move or expire; private normal-only construction certifies callback context |
| work_handle.rs and delayed_work_handle.rs publication closures | Owner borrow retains stable storage; private gate excludes publication after shutdown; only normal queues exposed |
| owned.rs Storage<W>: Sync | W: Drain requires Send + Sync; private UnsafeCell mutated only by unique final reclamation after drain |
| owned.rs Owner::new / prepare_retiring_work | Arc::new_cyclic supplies stable address; immutable function/context pair; final Owner submits once and retains one raw strong reference |
| owned.rs Owner::work | Live Owner borrow excludes final release; no mutable or moving access to W is exposed |
| owned.rs Owner::drop / Pin::new_unchecked | Original Arc provenance retained; publisher and transferred strong references jointly retain the private node; private permanent normal queue permits the retirement hook to sleep |
| owned.rs reclaim_storage / Arc::from_raw | Exactly one Arc::into_raw transfer per final Owner; terminal hook reconstructs it once after node retirement |
| owned.rs reclaim_storage / ManuallyDrop::drop | All public producers are gone; successful ordinary/physical drain ends asynchronous W access; destruction is confined to the W field |
| services/tests.rs borrowed publication | Serial fixtures retain pinned Work/captures through ordinary and timer drains; callbacks match their bound CPU/category; actions are restored before provider storage retires |

These are address-sensitive storage, raw ownership-pointer and synchronized shared
state scenarios. Pin alone cannot retain a borrowed asynchronous object; safe wrappers
add ownership and a teardown protocol rather than hiding the core precondition.
Existing Arc, Pin and core queue/timer drain APIs provide the underlying guarantees.

## Concurrency And Lifetime

Handle admission uses Owner gate -> core partition -> Work locks. Shutdown closes
the gate and releases it before synchronous drain. Last Owner release cannot overlap
another safe handle method because that method retains an Owner borrow. The terminal
hook invokes no business destructor until callbacks and physical timer tails finish.

The hook's Drain::drain still allocates completion-observation resources: for each
nonempty preparation attempt, one or two receipt Arcs and two Vec buffers (three
or four allocations); idle preparation allocates none and growth races can retry.
These are remaining wait-resource allocations, distinct from allocation-free
CompletionWait preparation/parking and from the preallocated reclamation Work.
Allocator exhaustion during final reclamation is not a recoverable drain error;
this migration does not guarantee allocation-free teardown.

The raw Arc transfer keeps Storage alive through the hook. The publisher independently
retains Storage until queue_work returns. Reclamation may destroy W before publication
returns, but never mutates or frees the separate borrowed reclaim node. The core ends
its Work borrow before terminal invocation; no reclaim-node access follows that hook.

Once serializes installation, registers both actions, then publishes readiness with
Release ordering. The short nonblocking installation section disables preemption
to exclude same-CPU initializer self-waits; rollback joins occur after that guard.
Nonblocking Acquire reads return unready during initialization;
an early BH action returns without waiting, while no public producer can reach the
candidate queues. Failed candidates are joined outside initialization and locks.
BH lookup relies on the kirq runner's CPU pin, not a new guard. Diagnostic output
runs outside record spinlocks. Provider teardown is not exposed in production;
serial tests restore actions and finish producers/drains before fixture teardown.
Their selected Once remains Arc-owned through each reader closure and worker joins.

## Threats And Responses

| ID | Trigger / asset at risk | Impact | Response and residual risk |
| --- | --- | --- | --- |
| T01 | Borrowed Work moved/freed while linked or executing | High: dangling access | Unsafe contract requires stable storage, producer/API join and synchronized drain; prefer owned/static APIs |
| T02 | Last handle released from IRQ or its own callback | High: early free or self-deadlock | Prepared normal reclamation defers drain/destruction; no synchronous wait in Drop |
| T03 | Producer races shutdown | High: use after drain or resurrected work | Shared admission gate closes before drain; all clones observe closure |
| T04 | Remote retirement precedes publication return | High: publisher accesses freed node | Publisher retains its own Storage Arc; terminal hook destroys only W before final allocation release |
| T05 | Reclamation double-runs or wrong raw pointer reconstructed | High: double free | Private immutable retirement pair, one final Owner, one transfer, one terminal invocation; no exposed cancel/requeue handle |
| T06 | BH action conflict or dispatch before readiness | Medium: partial readiness or wrong provider | Atomic action registration precedes Once publication; readers cannot see candidates; early actions return without waiting; rollback joins private workers |
| T07 | Arbitrary safe callback is dispatched in BH context | High: illegal blocking | NormalQueue/private handle construction fixes normal execution; BH submission remains unsafe |
| T08 | Timer-only Work survives queue close | Medium: lost progress or retained resources | Caller drains delayed objects before closure; queue shutdown does not discover timers; owned final reclamation still drains |
| T09 | Cyclic/forgotten handle or nonreturning callback | Medium: leaked memory or stalled drain | Accepted limitation; no memory-safety shortcut or automatic recovery; diagnostics do not guarantee progress |
| T10 | Diagnostic registry contention or output delay | Low: missing/stale warning | Best-effort try-lock observation, copied identity and core throttling; no bounded reporting promise |
| T11 | Repeated submission before readiness, including in hard IRQ | Medium: console flooding and IRQ latency | Return NotReady without logging; initialization reports its own failures |

## Failure Modes

| Failure | Local/system effect | Handling |
| --- | --- | --- |
| Invalid context or unavailable service at preparation | No usable handle/queue | Typed error before preparation; rejected callback drops in calling context |
| Invalid CPU/admission/deadline | No newly accepted work | Core typed error; existing arrangement unchanged except independent concurrency |
| Shutdown drain error after closure | Admission stays closed | Retry drain; final Drop still retains storage for reclamation |
| Duplicate init / occupied actions | Candidate not published | Warning and typed error; stop candidate workers unlocked |
| Allocator exhaustion | Kernel allocation failure | Kernel policy; no recoverable OOM promise |
| Reclaimer admission invariant failure | Cannot reclaim normally | Panic while raw strong reference still retains storage; no premature free |
| Callback/destructor panic or nonreturn | Incomplete cleanup / halted progress | No unwind recovery, cancellation timeout or progress guarantee |

## Privacy And Audit

Logs contain queue names, CPU IDs, elapsed times and rejection reasons, not callback
payload contents. Captures may contain sensitive caller data; this module neither
interprets nor zeroizes them. Names must not be used to expose secrets through logs.

Review checks: one raw Arc transfer/reconstruction, no W destruction before drain,
no exposed mutable pinned storage, permanent normal provider availability, matching
CPU/category binding, atomic action publication, and no waiting/destruction while
holding the test fixture selector or admission gate. Live-service tests cover
boot-installed providers, controlled races and lifecycle outcomes; neither test
mode proves all concurrent schedules or worst-case latency under arbitrary load.

## Finalization provider boundary

Carrier's UnsafeCell has one pre-publication writer and one post-removal consumer.
The permanent executor's pending lock synchronizes the handoff. Intrusive list
ownership retains an Arc; the publishing Arc independently spans notification.
The intrusive-collections adapter transfers one Arc owner into the list and
returns it on removal. Embedded LinkedListAtomicLink storage is never used by
another list and each Prepared is submitted at most once. All list mutations
remain under the pending lock; the publishing Arc independently protects the
carrier until notification returns.
Payload destruction and carrier release occur outside the pending lock.

One static executor owns Work. It remains pinned and live for kernel life,
cannot be canceled/disabled, and retains the normal default queue. A running
occurrence can have one queued successor; AlreadyQueued coalesces notification,
not finalization ownership. Unexpected notification errors retain pending nodes
and fail fatally. Panic recovery or resumed processing is not promised.

Before binding, the permanent list retains every obligation without executing
it. Queue publication precedes the binder's locked pending check; producers link
before checking queue publication. This prevents a missed startup notification
without a second queue or migration transaction. Failed service candidates never
publish the finalizer's queue. Permanent boot failure can retain objects forever;
it is not a promise of reclamation after failed boot.
Provider installation is serialized primary startup. The unsafe
kfinalize::service::new_integration entry permits concurrent audited boot/task
preparation after each CPU's allocation/per-CPU/IRQ-lock prerequisites hold.
Caller-held locks must permit allocation and metadata release. This grants no
hardirq, softirq or NMI construction permission.

Consumer callbacks process bounded node batches. No finalization runs inline in
submit, no enqueue allocates a node, and no callback changes execution category.
No BH finalization provider is installed. Normal destructors must not
wait for same-executor finalization: sharing Work introduces serial head-of-line
blocking in addition to existing shared worker-pool dependencies.

The public identity remains separate, so private queue ownership cannot extend
Weak upgrade. Metadata frees on the publishing CPU outside NMI; DeferredArc NMI
release instead uses a preprepared, separate ordinary-IRQ node. This does not
make arbitrary Work/WorkHandle submission NMI-safe. No RCU grace-period handling,
guaranteed latency or global deadlock
detection. Previously recorded host fault/Miri evidence for the per-object Work
backend must not be attributed to this revised backend.
