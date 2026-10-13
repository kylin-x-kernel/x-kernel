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
| Stack abandoned by nonreturning exit | Guard destructor does not run | Host GC retains the exited task allocation permanently, preserving its list node |
| Counter attached to the wrong task | Host context check misses participation | Host controls the private embedded field; no general current-reader API exposed |

Abandoning a scope is a kernel programming error, not a supported reader exit.
Retention sacrifices the entire task and all resources still owned by its
fields, including identity/runtime references and wait state, as well as any
grace-period progress that depends on its protection. Diagnostic weak registry
entries remain upgradeable and cannot be swept as dead. This does not synthesize
an unlock or offer a recovery operation. Other exited tasks
must remain reclaimable. The kernel retirement test intentionally leaves one
such task alive, so it must be isolated from future grace-period progress tests.

## Unsafe Boundaries

- `ReaderNode::enroll` converts a borrowed task field into a nonowning intrusive
  reference. The caller must retain stable node/task storage through unlink and
  forbid concurrent enrollment or scope execution on another task. Only the
  outgoing current task can be enrolled by the scheduler.
- `ReaderNode::remove` obtains a cursor from the embedded node pointer. The
  acquired owner pointer identifies the unique owning list; mutations hold that
  node's lock, and the outer scope exit is the only removal path.
- `ReadGuard::drop` dereferences that owner only while enrollment's retention
  contract holds. It does not move task storage, invoke callbacks or wake tasks.

IRQ/NMI scope use requires deferred-cleanup integration not supplied here.
The current removal path must not run in NMI. This module must not be used as
a standalone concurrent reclamation algorithm.
