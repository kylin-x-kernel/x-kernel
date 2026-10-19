# Reader Accounting Safety

The integration boundary separates `krcu`'s system state and context policy
from `ktask`'s task storage and lifecycle. `krcu` selects the retained current
reader for private scopes; `ktask` rejects voluntary sleeping in active scopes.
The accounting API neither accepts userspace pointers nor grants access to
retired memory. Intrusive enrollment is unsafe: the host supplies stable task
storage, scheduler exclusion and lifetime retention. No manual Send/Sync
implementations are present.

`src/integration.rs::RcuContextIf` is a permanently linked static contract, not
a service-readiness flag. Its provider in `krcu` owns watching storage and
cleanup demand even before GP startup. Watching lookup cannot lock, allocate,
schedule or require current task; CPU identity must already be initialized.
It preserves the caller's IRQ mask, never enables previously disabled IRQs
and does not change NMI exclusion. The safe `check_reader_cleanup` delegates all admission
checks to that provider. `ReaderCleanup` and `SchedulerObservation` in the same
module carry borrowed context into safe static interface methods. Their unsafe
constructors require retained current/outgoing readers, watched non-NMI context
and IRQ/preemption exclusion through the immediate call. They cannot be sent
or shared across threads, and must not be saved for another boundary; the types
do not independently establish those caller obligations. The provider relies
on that contract when invoking unsafe core operations, without waking or
invoking callbacks under scheduler locks. Task lifetime and
terminal cleanup remain the task runtime's responsibility. These interfaces
do not admit public protected-data consumers or alter core GP algorithms.

The guard borrows its reader storage. Normal Rust returns, including callback
error values, release nesting. The borrowed token cannot escape the scope or
move to another task. Payload lifetimes and reclamation are outside the current
API; observing a zero counter is not by itself a reclamation guarantee.

| Failure | Consequence | Handling |
| --- | --- | --- |
| Nesting overflow or unbalanced decrement | False inactive/active accounting | Checked update leaves state unchanged; guard boundary asserts the invariant |
| Voluntary task blocking | Violates ordinary read-side contract | Host sleepability query and scheduler blocking/yield checks reject it |
| Stack abandoned by nonreturning exit | Guard destructor does not run | Post-switch terminal cleanup clears nesting and unlinks before the host releases task storage |
| Counter attached to the wrong task | Host context check misses participation | Host controls the private embedded field; no general current-reader API exposed |

Abandoning a scope is a kernel programming error, not an ordinary reader exit.
The host diagnoses it after removing participation. Cleanup requires proof that
the task cannot resume and no guard or interrupt can access its reader again.
It is not an operation for cancelling a live reader. It neither unwinds abandoned
Rust stack values nor revokes independent resource references. Whole-kernel
halt or panic without completing the switch carries no progress guarantee.

## CPU Watching Boundary

CPU watching state has no reclamation authority by itself. Its unsafe
transition methods require one owning CPU, exclusion of ordinary IRQs and
migration, balanced nested interrupt entries and no scheduling across an entry
or quiescent window. An entry token is not transferable to another CPU; explicit
exit must occur before the interrupted stream resumes. Only the activating
entry may restore quiescence. Forgetting an entry or executing transitions out
of order violates the host contract; no public safe wrapper advertises these
operations as ordinary RCU protection.

Transitions use shared atomic access and no locks, allocation, callbacks or
scheduler operations. SeqCst fences around AcqRel RMWs order protected accesses;
they do not establish correctness of missing architecture hooks. The host GP
service consumes the permanent states through `observe_watching`. Production idle
waits enter quiescence; IRQ/NMI dispatch temporarily restores watching if needed.
User-return callbacks restore watching before accounting or trap dispatch.
The node samples after GP-start
ordering under its lock and binds the first watched sample to that NodePeriod;
the host must supply the correct CPU state, not an arbitrary quiescent object.
Equal odd samples across wrap do not prove progress. A fresh even sample or a
changed sample can discharge only the CPU obligation, never a separately
enrolled task reader. Immediate report acceptance remains in a continuously
watched, IRQ/preemption-excluded interval, without nesting bookkeeping locks.

## Unsafe Boundaries

- `ReaderNode::enroll` converts a borrowed task field into a nonowning intrusive
  reference. The caller must retain stable node/task storage through unlink and
  forbid concurrent enrollment or scope execution on another task. Only the
  outgoing current task can be enrolled by the scheduler.
- `ReaderNode::remove` obtains a cursor from the embedded node pointer. The
  acquired owner pointer identifies the unique owning list; mutations hold that
  node's lock. Removal is exclusive to the outer scope exit, safe-context deferred
  cleanup or terminal cleanup. IRQ/preemption exclusion spans owner lookup and
  removal, preventing a scheduler cleanup from invalidating a borrowed owner.
- `ReadGuard::drop` dereferences that owner only while enrollment's retention
  contract holds. It does not move task storage, invoke callbacks or wake tasks.
- `TaskReader::finish_task_exit` requires an inactive old stack, unpublished old
  current and permanent task exit, with ordinary IRQs/preemption excluded. The
  host retains task/node storage until cleanup completes. It shares the normal
  unlink path and must not race any scope entry/exit or scheduler enrollment.
- `TaskReader::with_read_deferred` permits only current-reader, strictly nested
  interrupt accounting or exclusive fixtures. Its destructor never follows the
  owner pointer or locks. The host retains storage until cleanup and forbids
  parallel scope execution on another CPU. `finish_deferred_cleanup` requires
  non-NMI execution with ordinary IRQs/preemption excluded and no owner lock
  held; it refuses to unlink an active reader. Missing cleanup stalls progress,
  rather than allowing early reclamation. It does not certify CPU quiescence.

## Node Completion Boundary

The first-blocker reference never escapes the node lock and must always name a
member of that node's retained list. Removal advances it before publishing the
reader's cleared owner, so GC cannot reclaim a reader still used as a boundary.
Later readers inserted ahead of the boundary cannot make the blocking suffix
longer. No readiness check scans or dereferences task storage outside the lock.

The CPU observation is a separate obligation: removing the last old reader
cannot clear it. A caller must validate a real quiescent boundary before using
the internal observation control. The current deterministic fixtures validate
bookkeeping only. No hardware-observation proof or reclamation API is provided.
Matching numeric period identities reject nearby stale operations,
but are not independently safe against full-wrap ABA or unbounded report delay.
Global completion additionally uses the coordinator's initialization/finalization
gates. Bounded report lifetime is not inferred from a locally ready node.

## Coordinator Controls

The host supplies a nonempty fixed participant set covering all admitted CPUs.
Nodes stay stable for enrolled readers, and exactly one coordinator owns their
period-control lifecycle. Initialization publishes every participant obligation
before starting node initialization. Early reports are retained, but cannot
complete an initializing period. Finalization excludes restart until every
participating node has finished; node-operation failures never publish success.

Reports sample the corresponding node rather than accepting an externally
supplied integer pair. A single coordinator-lock transaction checks active
phase, period identity and the pending bit, so duplicate or nearby stale reports
cannot clear another CPU's obligation. Capture and submission keep ordinary IRQs
and preemption excluded while releasing the node lock before taking the
coordinator lock. This exclusion is not itself an ABA proof: host integration
must maintain a continuous RCU-watched interval to prevent an entire counter
wrap during the gap. NMI must not enter these controls.

No lock covers callback execution, waiting or user-memory access. The host
connects scheduler observations and controlled callback delivery, not a general
reclamation API. Tests of adjacent identities across wrap do not prove the
remaining idle/user/interrupt observation and lifetime protocol.

IRQ/NMI scope use requires host deferred-cleanup integration not supplied here.
The scheduler observation entry requires an outgoing current task and continuous
IRQ/preemption exclusion. Enrollment precedes clearing the CPU obligation; a
still-active task remains in the blocking suffix on its original node, including
after migration. A ready identity is submitted before leaving that excluded
interval. The API neither invokes callbacks nor wakes tasks under scheduler
locks. Its contract excludes interrupt-context read scopes; those require the
separate context-tracking integration before admission.

The unlocked CPU-pending check can only skip work, never authorize completion.
It cannot bypass enrollment of a previously unregistered reader. Period setup,
observation and reader-boundary changes remain serialized by the node lock;
observation retains its full fence. A racing setup missed by the check leaves
the CPU obligation set and requires the host's continuing progress assistance.
After observing a CPU with blocked readers, the GP readiness scan must continue
until their exit; repeated scheduler calls no longer submit duplicate reports.

The current removal path must not run in NMI. This module must not be used as
a standalone concurrent reclamation algorithm.

## Callback Storage Ownership

The containing object owns an embedded `CallbackNode`; no field offset or
allocation layout is imposed by the core. Unsafe admission must pair its function
with the actual containing layout and retain storage, callback code and resources
until invocation. A callback recovering its enclosing object needs pointer
provenance derived from that object's raw pointer, not a shared field reference.
Cross-CPU transfer must be valid for the enclosing payload and its resources;
the node's own auto traits do not establish that property for an erased payload.

Intrusive UnsafeRef handles do not own Rust cleanup. The private queue protocol
must not clone them into competing invocations, free or move their storage early,
or lose them. Abandoning a handle does not cancel accepted work and may leak the
retained resources. Exactly-once scheduling and teardown remain obligations of
queue integration, not guarantees supplied by this storage primitive.

Invocation is unsafe and requires unique detached ownership, completed retirement
obligations and a permitted execution context. A debug assertion detects linked
nodes but does not prove the absence of aliases or GP completion. The callback
and resource cleanup must not block, unwind or wait on this RCU engine. The saved
function may free the whole containing object during invocation, so the executor
must keep later accounting independently and never inspect the old node again.
Borrowed storage needs an independently proven lifetime through callback return;
a notification alone does not prove that an executor has finished using it.

## Callback Queue Control

Submission requires withdrawal before publication to the queue and retention
until callback return. It is allocation-free but not NMI-safe. Queues own only
nonowning links; dropping a queue is not cancellation or successful reclamation.
Only one driver may assign their targets and start/finalize their coordinator.
`CallbackState` owns and hides the coordinator/queues from the host. Its unsafe
constructor requires exclusive ownership of participating node period controls.
The safe `claim_driver` method grants control only once, even under competing
claims; the flag is never reset on driver drop. Moving the driver preserves an
active GP; losing it stalls progress rather than authorizing a fresh controller
to assign work to an already-started period. An exclusive driver borrow serializes
passes. Internal protocol fixtures using the private unsafe constructor must
separately enforce this boundary. State and accepted storage must remain alive
through execution; dropping either owner does not cancel accepted callbacks.

Host submission checks the immutable participant set and the one-shot driver
claim before touching the raw node. Rejected nodes remain caller-owned. These
checks do not establish runtime readiness, payload transfer safety or withdrawal:
the unsafe submission contract assigns those obligations to the host. Only the
core constructs intrusive handles; the host transfers the original raw field
pointer and must retain its provenance and containing allocation until invocation.
The host execution entry checks CPU membership and delegates the bounded claim
to the queue. It does not manufacture observations or bypass period eligibility.

The driver captures before reserving the future period and promotes all due
segments before another period starts. At most two live targets exist; matching
identity is valid only within that bounded control protocol. Once promoted, no
callback relies on an ancient numeric target. Queue locks never nest with node or
coordinator locks and no callback executes under any of them.

The execution claim remains owned while its detached batch is preempted or
invoking a callback. Restoring the old remainder precedes releasing that claim,
so neither a competing executor nor a later marker can pass the batch. Counts
remain conservative until callbacks return; no execution bookkeeping uses a
node after invocation. A callback must return normally: neither panic/unwind nor
nonreturning task exit is an accepted cancellation mechanism for a batch.

`invoke_ready` is unsafe because simulated observations or an invalid host context
cannot authorize real reclamation. Its caller must certify observation validity,
independent retirement obligations and nonblocking invocation/cleanup outside
dependent locks. These host-integration operations are not an ordinary consumer
API. The host connects controlled delivery, scheduler observations and remote
watching-based CPU observations. The latter must sample the matching permanent
CPU state, not a cached integer or arbitrary quiescent fixture.
Paired IRQ/NMI entry hooks alone do not certify quiescence. `report_ready` only inspects
retained readiness in watched context and cannot discharge an unobserved CPU.
