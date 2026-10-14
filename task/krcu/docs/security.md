# Reader Accounting Safety

The integration boundary is the host task runtime. It must select the current
task's embedded reader state and reject voluntary sleeping in an active scope.
The accounting API neither accepts userspace pointers nor grants access to
retired memory. Intrusive enrollment is unsafe: the host supplies stable task
storage, scheduler exclusion and lifetime retention. No manual Send/Sync
implementations are present.

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

## Unsafe Boundaries

- `ReaderNode::enroll` converts a borrowed task field into a nonowning intrusive
  reference. The caller must retain stable node/task storage through unlink and
  forbid concurrent enrollment or scope execution on another task. Only the
  outgoing current task can be enrolled by the scheduler.
- `ReaderNode::remove` obtains a cursor from the embedded node pointer. The
  acquired owner pointer identifies the unique owning list; mutations hold that
  node's lock. Removal is exclusive to the outer scope exit or terminal cleanup.
- `ReadGuard::drop` dereferences that owner only while enrollment's retention
  contract holds. It does not move task storage, invoke callbacks or wake tasks.
- `TaskReader::finish_task_exit` requires an inactive old stack, unpublished old
  current and permanent task exit, with ordinary IRQs/preemption excluded. The
  host retains task/node storage until cleanup completes. It shares the normal
  unlink path and must not race any scope entry/exit or scheduler enrollment.

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

No lock covers callback execution, waiting or user-memory access. The private
control surface is not yet connected to actual CPU observations or a public
reclamation API; tests of adjacent identities across wrap do not prove the
missing hardware observation/lifetime protocol.

IRQ/NMI scope use requires deferred-cleanup integration not supplied here.
The current removal path must not run in NMI. This module must not be used as
a standalone concurrent reclamation algorithm.
