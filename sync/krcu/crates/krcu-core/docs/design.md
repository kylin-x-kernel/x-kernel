# Reader Accounting

`krcu-core` owns reader accounting, grace-period coordination and callback
bookkeeping without depending on the scheduler or IRQ core. `krcu` owns the
permanent kernel instances, cleanup flags, context policy and GP execution
service, and supplies observations and progress. Core algorithms do not own
that service or its task/timer/softirq resources.
`ktask::TaskInner` embeds one `TaskReader` per task. Scheduler context checks
inspect it independently of the preemption-disable count: explicit read scopes
prohibit voluntary blocking but do not disable involuntary preemption.

`src/integration.rs` defines the static `RcuContextIf` contract used by scheduler
and interrupt boundaries. Its `krcu` provider is linked before GP service startup.
`current_cpu_watching` borrows permanent storage; `check_reader_cleanup`
delegates the complete safe cleanup check to that owner.
`finish_reader_cleanup` consumes a `ReaderCleanup` context and
`note_context_switch` consumes a `SchedulerObservation` context. Their unsafe
constructors certify the caller's exclusion and retained-reader obligations
for one immediate call. These borrowed contexts are neither Send nor Sync;
they are not read guards or deferred requests. The static interface methods
are safe, and the provider uses the context contract for unsafe core operations.
`reader_node_for_tests` is unittest-only.
The contract is distinct from isolated core instances and exposes no consumer
read, synchronize or owned-data API. Private consumer fixtures live in
`sync/krcu/ut-tests`; algorithm tests remain local to this crate.

Algorithm cases under `ut-tests/` use `#[unittest::def_test]` and are mounted
with `unittest::path_mod_test!`. Cases already covered by guest UT are not
also registered with the host harness.

`tests/host/` retains only host-specific coverage: two threads racing period
initialization against reader enrollment, thread-local allocation rejection,
and callback reposting while allocations are rejected. These are private
`cfg(test)` file modules run by `cargo test -p krcu-core --lib`, not independent
Cargo integration targets; this preserves access to the internal protocol
without widening production APIs. The host dev-dependency enables `kspin/smp`
so concurrent host threads use real locks. The guest repost case checks the
protocol without changing the kernel allocator; it does not replace the host
allocation-rejection check. Both use `tests/support/reposting.rs` for the owned
callback fixture, so its unsafe allocation and repost lifecycle has one source.

Scope entry increments nesting, an internally owned guard covers the callback,
and guard destruction decrements nesting. A higher-ranked callback receives only
a borrowed token. Nested scopes remain active until the outer scope exits.
The token is neither Send nor Sync; no public constructor gives ownership to a
caller who could suppress its destructor. The callback can return copied values,
owned values or errors, but cannot return its token borrow.

The counter uses checked atomic RMW through shared references while permitting
scheduler observation, rejecting overflow/underflow without changing state.
Entry is AcqRel, exit is Release and observation is Acquire. This is local accounting,
not a proof of grace-period ordering. Unenrolled scope entry/exit takes no locks
and changes no IRQ mask or preemption state. No reader operation allocates.

## Deferred Reader Exit

`with_read_deferred` retains the scoped-token interface but limits its destructor
to atomic nesting accounting. A zero counter with a non-null owner means that
the scope has ended while its enrollment still blocks reclamation. No separate
pending flag duplicates that state. Reentry preserves the original owner;
`finish_deferred_cleanup` does nothing while nesting is nonzero. A later normal
outer exit, explicit safe-context cleanup or terminal task cleanup unlinks it.
Cleanup removes only task membership, not a CPU observation obligation.
Cleanup returns `ReaderCleanupOutcome`: participation found and a progress hint.
The original node sets the hint under its lock only when removing the last
old blocker with its CPU obligation already satisfied. The hint has no period
identity and grants no reclamation permission; a host may request a fresh
service scan after the node lock is released. Isolated ordinary core scopes
do not notify a system service.

This separates the same responsibilities as Linux's NMI special-unlock return
and safe-context deferred cleanup (`kernel/rcu/tree_plugin.h`). The host must
retain task/node storage and arrange eventual cleanup. The core primitive alone
does not provide interrupt admission or a runtime cleanup notification path.
Ordinary unlink rereads ownership with IRQs and preemption excluded: a scheduler
cleanup may have run after the outer counter decrement but before exclusion.
Strictly nested NMI scopes can update nesting in that interval, but cannot unlink
or touch a node lock. Deferred exit is therefore usable even while its owner
node lock is held by the interrupted context.

## CPU Watching Transitions

`CpuWatching` models CPU execution, not a task's explicit read nesting. The
host owns permanent per-CPU instances used by IRQ/NMI entry and exit. The idle
host brackets only the architecture wait with quiescent transitions, restoring
watching before bookkeeping or scheduling. User transitions use architecture
callbacks before register transfer and before returned-trap dispatch. The host
GP service samples these permanent CPU states through `CallbackState`.
An odd atomic sequence means kernel execution; an even value
means the narrow idle/user extended quiescent window. Each actual crossing
uses an AcqRel increment with SeqCst fences before and after it. Remote sampling
uses Acquire; a sample is not itself authority to reclaim an object.

Like Linux `ct_nmi_enter`/`ct_nmi_exit`, an interrupt activates an unwatched
CPU before protected work, and only that activation restores quiescence at
exit. Nested interrupts on a watched CPU leave the sequence unchanged. Linux
encodes restoration responsibility with its interrupt nesting count; this
module records it in a non-Send/non-Sync stack entry with explicit exit.
An RMW rather than a saved-value store preserves crossings from an NMI that
completes between the outer state sample and update.

The host must pin each transition to its CPU and place it before/after the
actual protected accesses, outside any scheduling boundary. Quiescent windows
cannot span a task switch. No transition locks, allocates, wakes a task or
reports a GP. Preempted task blockers remain independent of CPU watching.

The node observer samples under its node lock, after the RCU-local
SeqCst fence. Each NodePeriod owns its first watched snapshot, reset at every
begin. An even sample satisfies the CPU obligation immediately; a changed odd
sample proves a crossing since this period's snapshot, as in Linux's watching
snapshot/recheck protocol. An unchanged odd sample, including full-wrap alias,
does not certify progress. No cached sample from before GP start is accepted.
Only the CPU obligation is cleared: old task readers can still block readiness.
The coordinator keeps IRQ/preemption excluded from the fresh node observation
through immediate acceptance; no numeric report is deferred across scheduling.
Inactive CPUs and nodes without a current obligation are ignored. The host
also rechecks retained readiness after old task readers exit, because clearing
the CPU obligation alone may not have produced a report. Ordinary reader
admission remains separate from this unsafe host observation interface.

The interrupt entry token is an unsafe host integration API, not a consumer
read guard. Its explicit exit completes before scheduling is allowed. Effective
NMI context is owned by kirq, independently of the watching sequence; this
module neither detects NMI nor admits interrupt-context task reader scopes.

Deterministic tests cover watched and quiescent entry, nested restoration,
completed interrupt history across later crossings, and parity across wrap.
They do not inject interrupts inside transition instructions or prove the RMW
ordering against a weakened load/store implementation. They are not hardware
NMI tests, compiler-entry audits or weak-memory litmus results.
An isolated retirement test also interleaves a modeled EQS-origin interrupt
borrow with pointer removal and callback submission before GP start. Unchanged
watched samples must keep the callback ineligible, including after scope exit
but before interrupt return. Return to EQS permits completion and one object
destruction. The fixture never invokes a callback while its borrow is live,
even if eligibility regresses; it checks eligibility directly before reclamation.

## Preempted Readers

Each CPU owns a `ReaderNode` with an IRQ-safe locked intrusive list. Scheduling
an active task enrolls its embedded link before handing execution to another
task, including the same-task selection and migration-helper paths. Repeated
enrollment is a no-op only for serial calls: the host must exclude concurrent
enrollment and scope execution by another task or interrupt. A migrated reader
keeps its original node; there is no cross-node transfer or second registration.

The outer exit removes the link under its original node's lock and clears the
owner pointer. Nested exits do not touch the list. The node lock is followed by
a SeqCst fence, including on UP; there is no global list lock. The host excludes
ordinary IRQs/preemption during enrollment and retains the task allocation
until removal. Normal finite scopes unlink before task exit and GC.

After a task permanently exits, the incoming task's `finish_task_switch` calls
`TaskReader::finish_task_exit` before releasing the outgoing owner. The old stack
is inactive and current already names the incoming task. Terminal cleanup clears
abandoned nesting and uses the ordinary outer-exit unlink path on the original
node. It diagnoses a programming error without permanently retaining the task.
It does not run abandoned stack destructors, certify a CPU quiescent state or
complete a grace period. GC retains its existing final-ownership protocol;
independent task references may still delay destruction.

## Node Grace-Period Boundary

The node lock protects both the reader list and an optional active period:
its identity and first blocking reader. The CPU-observation obligation is one
atomic boolean on the node; all writes still require the node lock. That reader
and the list suffix after it block this period. The boundary is a nonowning
`UnsafeRef` to retained list storage, not another task reference or a separate
list. No additional global lock is needed for this boundary.

Initialization includes the entire existing list in constant time. While the
CPU still owes an observation, newly enrolled readers append to the blocking
suffix. After a matching CPU observation, new enrollments go at the list head,
ahead of the suffix. They remain enrolled for future periods without extending
this period. This is the ordinary-period role of Linux's `gp_tasks` boundary;
expedited-period placement and hierarchical nodes are not implemented.

Removing the boundary reader advances it to its successor under the same lock.
Interior removal does not move the boundary. Both ordinary outer exit and
terminal task cleanup use this operation. A node is ready only when its CPU
obligation and blocking suffix are both empty; its full reader list need not be
empty. Finishing the period preserves fresh readers for the next initialization.

An active period cannot be overwritten. CPU observations and finalization must
match its identity; readiness alone is not global completion. Identity equality
does not protect against an arbitrarily old value surviving a complete wrap.
Report lifetime must be bounded under continuous IRQ/preemption exclusion in
RCU-watched context, and deferred service must inspect nodes afresh. These node
controls are crate-private and used by the coordinator; observation validity
belongs to host integration. Their locks include the same full fence as
enrollment/removal, including on UP; this is not a complete
publication-to-reclamation memory-order proof.

## Flat Coordinator

`GracePeriodCoordinator` borrows the fixed CPU-node array and owns an immutable
participant set, completed identity, phase and pending-report set. Only admitted
CPUs participate; unused array capacity owes no report. An empty participant set
is rejected. Exactly one coordinator may drive a node's period controls; the
shared array borrow preserves lifetime, not exclusive management rights.

| Phase | Driver responsibility | Reports |
| --- | --- | --- |
| Idle | May claim initialization; current identity is completed + 1, wrapping | Rejected |
| Initializing | Publish all pending bits before visiting any node; close the gate only after every participant is initialized | Matching reports clear pending bits, never change phase |
| Waiting | Claim finalization only after all pending bits clear | Matching reports clear pending bits; duplicates have no effect |
| Finalizing | Finish every participating node before publishing completion and returning to Idle | Rejected |

`begin_period` and `try_complete_period` claim their phase under the coordinator
lock before visiting nodes. Their complete loops are the sole initializer and
finalizer, so overlapping driver calls cannot repeat node control transactions.
No separate progress bitmap or cursor duplicates the loop's ownership. A node
control failure is an internal invariant violation; it cannot be ignored while
publishing successful completion. The driver must check completion after closing
initialization because all reports might already have arrived.

`report_node` snapshots readiness under the node lock, releases that lock and
submits node index plus period identity under the coordinator lock. An outer
`NoPreemptIrqSave` spans both acquisitions. Acceptance checks phase, identity and
the pending bit in one transaction. Node and coordinator locks never nest;
ordinary reader entry/removal does not acquire the coordinator lock. Controls
use the RCU-local full-fence discipline without strengthening generic spinlocks.

The coordinator exposes no waiter, callback or public numeric cookie. Completed
identity is internal ordered bookkeeping, not an arbitrary-age comparison API.
The callback driver latches due work as eligible before allowing the next period.
The portable engine is covered by deterministic tests; the host additionally
connects scheduler boundaries, watching observations and controlled callback
delivery. The host must preserve the continuously watched report interval and
the audited transition boundaries; interrupt reader admission remains separate.

The host scheduler calls `CallbackState::note_context_switch` with its outgoing
task under continuous IRQ/preemption exclusion. When needed, enrollment and
clearing the CPU obligation share one node-lock interval, ordered by the existing
full fence. Initialization cannot split those operations. The
node lock snapshots readiness; acceptance checks that identity under the
coordinator lock without nesting locks or retaining identities in deferred work.
Migration reports the destination CPU but leaves the reader on its original
node. Calls before a period starts do not satisfy a future period. This task-only
scheduler boundary is not idle/user watching or IRQ/NMI read-side integration.

When the outgoing reader needs no enrollment and an Acquire load finds no CPU
obligation, the scheduler skips both locks and the fence. The atomic is the sole
obligation, not a cached period identity. Initialization publishes it with Release
after preparing the period under the lock. A racing false can delay observation,
but leaves the new obligation outstanding: the host must retain timed progress
assistance. A true is rechecked under the node lock before observing its current
period. The scheduler reports only on the CPU-observation transition; a node
still blocked by old readers is later reported by the GP readiness scan. Fresh
readers must still be enrolled after CPU observation, for future periods.

Period identities use a private 64-bit integer in normal builds. Host and kernel
unit tests narrow that same identity to eight bits, exercising the actual node
and coordinator transitions through repeated complete wraps. Deferred inspection
retains only a node index and reads its current obligations. A separate fixture
holds the reporting CPU's next-period obligation while a previous-period capture
is in flight. This checks the consequence of a watched interval, not whether the
host correctly recognizes and preserves that interval. Neither test permits
arbitrarily old numeric reports or proves hardware observation correctness.

## Embedded Callback Storage

`CallbackNode` contains intrusive linkage and an invocation function. Callers
embed it in stable storage; the core neither allocates nor frees that storage.
One list can hold nodes from different containing-object layouts and ownership
policies. Retiring an existing object therefore requires no separate callback
allocation, although embedding the node increases that object's size.

List references are nonowning handles. Unsafe admission requires the caller to
retain storage and callback resources and preserve unique invocation ownership.
Dropping a handle neither cancels the callback nor frees its object. Invocation
consumes the detached handle and saves the function before calling it. The
callback may free its entire containing object; the executor must keep subsequent
accounting elsewhere and never access the node afterward. Callback execution and
resource cleanup must run outside RCU locks.

Allocation policy belongs to the containing object, not the queue. A borrowed
stack-node test drains synchronously before its frame ends; it does not establish
a production asynchronous waiter's notification-versus-return lifetime protocol.

These storage primitives are tested with unpublished fixture objects.
They do not themselves establish GP eligibility or provide a public submission
API. Real retirement additionally needs the queue protocol and validated hardware
observations.

## Callback Period Assignment

Each `CallbackQueue` has its own IRQ-safe lock and intrusive unassigned, waiting
and eligible lists. There are two waiting segments, for the current period and
its successor; this is a bound on distinct targets, not on callback count. Moving
or joining a whole segment uses constant-time list take/splice operations. No
assignment/completion pass scans individual callbacks or allocates queue storage.

One `CallbackDriver` serializes assignment and all coordinator start/completion
operations. It borrows the coordinator to allow concurrent observations/reports;
this borrow does not permit another controller to drive the same coordinator.
The host constructs `CallbackState`, which owns the coordinator and queues.
Its unsafe construction requires exclusive control of the participating nodes;
existing enrolled readers are preserved. `claim_driver` grants exactly one driver
using a one-shot atomic claim. Duplicate claims return `None`, including after
the driver is dropped. A handoff moves the existing driver with its active period.
The coordinator and queues are not exposed to competing host controllers. The
crate-private unsafe driver constructor remains available for isolated protocol
fixtures; ordinary host integration uses the checked claim instead.
`CallbackDriver::advance` reports whether a period remains active, not a numeric
cookie or an instruction to busy-loop. Task placement, notification and progress
timers belong to the host. This boundary alone does not publish readiness or
provide an ordinary RCU retirement API.

The host-facing state accepts raw embedded `CallbackNode` pointers only after
the driver claim and only for participating CPUs. Rejection does not access or
consume the node. Successful submission transfers invocation ownership without
allocation or wakeup; the host must establish readiness and publish retained
demand afterward. A driver claim alone is not a readiness gate. `has_ready` and
bounded `invoke_ready` let the host service eligible work without exposing queues
or period identities. The carrier must recheck readiness after executing its
budget. None of these operations records a CPU quiescent state or advances a GP.
`report_ready` inspects retained node readiness without supplying a CPU
observation. The host can revisit a node after its last blocked reader exits,
including from the GP task's progress check, without caching a period identity.
`needs_cpu_observation` reads only the current node's CPU obligation for progress
assistance. A false result does not mean the node or GP is complete: old readers
may still block it. No numeric identity escapes this query.

`needs_completion_pass` is a lockless wake hint, set under the coordinator lock
when acceptance clears the final pending report. It resets on finalization and
new initialization. It may be set during initialization; only the driver may
close that gate and finalize nodes. The hint never authorizes reclamation or
callback invocation. Hosts may test it after an accepted scheduler report and
notify outside core locks; a race with driver progress only suppresses an
obsolete wake or requests an extra pass. This follows Linux's last-report GP
wakeup responsibility without embedding a scheduler or waker in the core.

Each pass captures the then-current finite unassigned prefix of each queue
and assigns a full future period under the same queue lock. No pending prefix
is detached across a lock release, so queue-tail inspection sees all work not
owned by the callback executor. Idle assignment uses
completed + 1; assignment during an active period uses its successor. Submissions
after capture remain unassigned until a later capture. The driver never chases
an indefinitely growing producer list within one pass.
The target is computed once per pass, before capture, without starting the GP.
New assignments imply successor work; only passes without new assignments scan
for retained waiting segments before deciding whether to start another GP.

After completion, the driver promotes matching segments on every queue before
starting another period. Eligibility is persistent and independent of numeric
targets, so delayed execution remains eligible through any number of wraps.
Queue handoffs use the same full-fence discipline as node/coordinator operations,
including UP; this bookkeeping does not validate hardware quiescent observations.

## Bounded Callback Execution

An executor claims a queue and takes its eligible list under the queue lock,
then invokes at most its supplied budget outside all RCU locks. The detached
`CallbackBatch` owns the claim until it returns the unexecuted prefix to the
front and releases the claim in the same queue transaction. New promotions may
proceed concurrently but cannot overtake that prefix. A competing or recursive
executor returns without waiting; other queues have independent execution claims.

Outstanding accounting includes all accepted callbacks, including detached and
currently invoking entries. It is reduced only after callback return, using the
batch's own completed count rather than inspecting possibly freed nodes. Empty
eligible lists therefore do not imply that prior callbacks have finished.

## Callback Drain Markers

`CallbackState::enqueue_drain_marker` appends an existing node after a CPU's
outstanding callbacks under that queue's lock. It returns false without touching
the node for an unclaimed driver, absent CPU or queue with no outstanding work.
It neither waits nor notifies; it is a host integration primitive, not a complete
drain API.

The marker inherits the last nonempty segment's eligibility: unassigned first,
otherwise the newest waiting target, otherwise eligible. Waiting slots are
reused, so the newest target is determined by their adjacent wrapping period
identities, not their array indices or numeric maximum. A marker never requires
an independent GP and must not be used to retire a newly withdrawn object.
This follows Linux's `rcu_segcblist_entrain` responsibility.

When only detached/executing work remains, the marker enters the eligible list.
The existing exclusive execution claim prevents it from running before the old
callback returns; any unexecuted batch remainder is restored ahead of it. A
later repost stays after the marker and needs its own GP, so markers do not
chase indefinitely resubmitting producers.

The host must prepare marker ownership and completion accounting before making
the node visible, then request service. Successful insertion can be followed by
execution before the insertion call returns. Across CPUs the host must retain a
setup reference until installation ends; a first marker must not complete the
whole operation while later markers are still being prepared or installed.
Sleepable caller serialization, notification lifetime and stop-producers /
wait / destroy sequencing are host responsibilities not supplied by this core.

## Callback Reposting And Host Integration

Reposting accepts new work into the unassigned list and requires a new full
period. The current finite batch does not chase those submissions. Budget return
leaves persistent readiness for the carrier to recheck; wakeup/sleep handshakes,
timer/IPI progress and context admission belong to host-service integration and
are not supplied by this portable engine. The host's public scope, container and
reclamation contracts are defined in [krcu](../../../docs/design.md).
The task-only removal path is not usable from NMI; host deferred cleanup runs
at safe task boundaries, with retained demand serviced by an active GP's progress
kicks. Controlled IRQ/NMI software-context fixtures do not establish hardware
NMI delivery.
