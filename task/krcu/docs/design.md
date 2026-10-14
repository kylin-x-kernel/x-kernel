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
its identity, outstanding CPU observation and first blocking reader. That reader
and the list suffix after it block this period. The boundary is a nonowning
`UnsafeRef` to retained list storage, not another task reference or a separate
list. No additional atomic state or global lock is needed for this boundary.

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
controls are crate-private and used by the coordinator, not connected to hardware
observations or reclamation. Their locks include the same full fence as
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
Future callback integration must latch due work as eligible before allowing the
next period. The current private engine is driven by deterministic tests only;
hardware observation validity, watched-context integration and full-wrap report
lifetime safety remain outside this bookkeeping implementation.

Period identities use a private 64-bit integer in normal builds. Host and kernel
unit tests narrow that same identity to eight bits, exercising the actual node
and coordinator transitions through repeated complete wraps. Deferred inspection
retains only a node index and reads its current obligations. A separate fixture
holds the reporting CPU's next-period obligation while a previous-period capture
is in flight. This checks the consequence of a watched interval, not whether the
host correctly recognizes and preserves that interval. Neither test permits
arbitrarily old numeric reports or proves hardware observation correctness.

Only controlled task-context tests currently enter scopes. There is no public
current-task RCU read interface, protected-data container, hardware-integrated
grace period or reclamation service. The task-only removal path is not usable from NMI;
interrupt-context scope admission and deferred cleanup are not implemented.
