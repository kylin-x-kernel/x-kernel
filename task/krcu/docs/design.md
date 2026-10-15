# Reader Accounting

`krcu` owns task-reader nesting without depending on the scheduler or IRQ core.
`ktask::TaskInner` embeds one `TaskReader` per task. Scheduler context checks
inspect it independently of the preemption-disable count: explicit read scopes
prohibit voluntary blocking but do not disable involuntary preemption.

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
connects scheduler boundaries and controlled callback delivery. Idle/user/IRQ/NMI
watching and full-wrap report lifetime across those contexts remain separate
integration obligations.

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

Each pass captures the then-current finite unassigned prefix of each queue,
releases its lock, and assigns a full future period. Idle assignment uses
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
eligible lists therefore do not imply that prior callbacks have finished. This
is bookkeeping for future drain support, not a drain API.

Reposting accepts new work into the unassigned list and requires a new full
period. The current finite batch does not chase those submissions. Budget return
leaves persistent readiness for the carrier to recheck; wakeup/sleep handshakes,
timer/IPI progress and context admission belong to host-service integration and
are not supplied by this portable engine. The host currently connects real GP
task and softirq execution for unpublished integration fixtures, not general
consumer admission.

Only controlled task-context tests currently enter scopes. There is no public
current-task RCU read interface, protected-data container or generally usable
reclamation service. The task-only removal path is not usable from NMI;
interrupt-context scope admission and deferred cleanup are not implemented.
