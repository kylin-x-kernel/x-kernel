# krcu - Security And Reliability

## Trust Boundary And Assets

This crate owns permanent reader nodes, per-CPU watching and cleanup notifications
and service state. It trusts `ktask` to retain embedded task readers and supply
excluded scheduler/cleanup boundaries, `kirq` to balance interrupt entry/exit,
POSIX and architecture transfer to delimit user quiescence, and `kruntime` to
complete all-CPU service preparation before `init_service`.
The static `krcu_core::RcuContextIf` provider exists before service startup;
its presence is not GP readiness or consumer admission. The algorithm contracts
remain in [krcu-core](../crates/krcu-core/docs/security.md).

No user pointer, device buffer or MMIO register is interpreted here. External
interrupt and timer delivery affects progress through existing providers;
notifications never establish reader retirement. Incorrect kernel callers can
violate the unsafe lifecycle contract; the service does not validate arbitrary
foreign reader pointers or independently prove architecture entry ordering.

## Watching And Reader Lifetimes

The RCU watching provider returns a permanent CPU object, not proof of CPU
ownership. The IRQ core must exclude migration from lookup through explicit
entry exit. CPU bring-up copies the atomic-only `CpuRcuState` template and installs
the CPU base before local access. The `StaticSlotValue` implementation relies
on watching and cleanup fields having no allocation, owning pointers or Drop;
changing those fields requires reauditing template-copy safety. Remote GP reads
use checked published `CpuArea` handles, acquired after the all-CPU rendezvous.
A missing participant area is an invariant failure, never a quiet CPU report.
No lazy initialization, task lookup, allocation
or node locking occurs in this provider. Effective NMI classification bypasses
the ordinary context lock and blocks scheduler preemption probes. This does not
admit general task-reader scopes in NMI. Safe task-boundary cleanup is implemented;
interrupt-scope admission is limited to controlled unittest fixtures.

`api::is_sleepable_context` accepts no external input and adds no unsafe boundary.
It rejects missing or idle current tasks, interrupt-like/IRQ-masked contexts,
and preemption protection when tracked. Explicit RCU read nesting also rejects
sleeping and voluntary yielding, without preventing involuntary preemption.
Only controlled tests enter these scopes; there is no general safe reclamation API.
Scheduler enrollment borrows stable Arc-owned task storage. On the incoming
stack, `finish_task_switch` removes an exited task's abandoned RCU participation
before releasing the outgoing owner or on-CPU marker. IRQs and migration remain
excluded, old current is unpublished and the exited task cannot resume. The
PREV_TASK lock is released before the original reader-node lock is acquired;
diagnostics run after unlinking. Normal outer exits remove their node first.
GC retains its ordinary final-ownership check before destroying task storage.
Cleanup does not detach immutable runtime fields while shared task references
may still borrow them, or unwind values on an abandoned Rust stack.
No raw task pointer is sent to a deferred callback. IRQ/NMI read scope support
is not implied by this task-context protocol.
Sleep assertions and prepared waits
share this gate to avoid inconsistent acceptance. This mitigates invalid-context service startup/stop;
the caller remains responsible for dependency cycles and provider readiness.

## Cleanup And Execution Context

The scheduler RCU hook first drains zero-depth deferred membership with IRQs
and preemption excluded, before acquiring any node/coordinator lock for the
observation. It operates only on the retained current task, including same-task
selection. Nonzero nesting keeps the original membership intact. This cleanup
does not itself certify CPU quiescence and cannot invoke callbacks or wakeups.
The provider's `check_reader_cleanup` may drain without a scheduling request,
but only after
excluding NMI, exception context, active nesting and existing preemption
exclusion. It then masks IRQs/preemption before querying locking context state,
rejects unwatched/interrupt-like execution, and suppresses recursive guard-drop
checking. `ktask` always enables `kspin/preempt`, independently of the scheduler
preempt feature; the balanced count protocol depends on that dependency.

Ordinary preemption checks retain pending requests while local IRQs are masked,
even when the preemption count reaches zero. Otherwise an inner guard release
could split an implicit IRQ-off RCU region and falsely report a scheduler
quiescent state. Safe deferred-reader cleanup remains allowed without scheduling.
Only the completed normal IRQ tail uses the distinct IRQ-exit checkpoint under
its entry mask; existing outer counts, NMI and exception exclusions still apply.
User/idle entry also drains while still watched and before any quiescent window.
These hooks do not authorize NMI cleanup or guarantee delivery when no further
safe boundary occurs. General interrupt-context scope admission remains closed.
The private unittest interrupt entry requires initialized, retained current and
already-active watching. Strictly nested scopes finish before handler return,
without scheduling, migration, enrollment changes or stack abandonment. NMI
entry checks the lock-free context marker first and acquires no guard or lock;
exit only decrements nesting. Existing ownership and cleanup demand remain the
interrupted task's responsibility. These fixtures do not validate early entry
windows or real hardware NMI delivery.
The unittest-only host task scope borrows current across involuntary migration;
it does not cache a CPU or dereference a remembered node. The core scope guard
ends nesting before its outer task-exit policy runs. A returned exclusion guard
may keep cleanup deferred, in which case the exit sets need_resched and a local
cleanup notification under temporary exclusion. It does not wake, send an IPI
or take a node/controller lock. It never clears another scheduling request.
Callbacks may not block, alter watching, or leave RCU bookkeeping locks held.
Permanent task exit abandons these stack guards and still requires the existing
post-switch terminal cleanup. An active GP with IPI support services retained
demand at its progress deadlines; neither an unserviced GP task nor a permanently
IRQ-masked CPU has a delivery bound. Without IPI support this still relies on a
later local boundary. No notification publisher is admitted in NMI.
Only a permitted local cleanup boundary consumes notification, before inspecting
the reader. Active nesting preserves it. Remote GP scans only load it, so an IPI
received in an unsuitable context cannot discard the pending cleanup request.
The scheduler RCU hook reports only after preserving the outgoing reader in its
original node. IRQ/preemption exclusion spans enrollment, observation and report
acceptance. Node and coordinator locks never nest, and this hook does not wake
the GP task inline. A reported CPU is not evidence that its preempted task has
exited the read scope. Idle quiescence spans only the architecture wait under
IRQ/preemption exclusion, with no active explicit task scope. Watching resumes
before guard release or timer bookkeeping. IRQ return glue outside kirq's
watching scope must remain RCU-free; the outer idle guard prevents scheduling
in that tail. User quiescence uses IRQ-masked transitions with no scheduling,
explicit reader or protected borrow spanning user execution.
Exit restores watching before accounting or trap dispatch; register/stack
transfer inside the window must remain RCU-free. The GP task samples the matching
permanent CPU state only after GP-start ordering under the node lock. The
coordinator excludes IRQs/preemption through immediate report acceptance.
This does not authorize read admission on unsupported interrupt/NMI paths.

Watching observations discharge only the CPU obligation, never preempted task
readers. The subsequent readiness check reports previously recorded observations
whose old readers have drained. Timer expiry never fabricates an observation.
Progress kicks use the existing fixed reschedule IPI, outside all RCU locks,
only at the active interval's start or progress deadline. A stale pending hint
can cause an unnecessary kick but cannot authorize completion. Old-reader
membership alone does not request another kick, but retained local cleanup
demand does; reader lifetimes remain authoritative in either case.
Callback promotion precedes destination notification. The Rcu softirq executes
only eligible callbacks and restores any budget remainder through the core queue
protocol before requesting another pass. No callback runs under an RCU lock or
in the GP task. Admission remains limited to controlled test objects and
synchronous-wait markers, whose callbacks and cleanup must not block. General
read/context integration is still required before accepting ordinary consumer
retirement.

## Private Consumer Ownership

The private unsafe `synchronize_rcu` bridge and its wait machinery are compiled
only for controlled task-reader integration tests, not exported to consumers.
Its caller must exclude unsupported reader contexts, withdraw all
publication routes and retain the old owner on preparation failure. Completion
registration precedes callback admission; after admission the wait cannot return
an interrupt error or treat a wake as GP completion. Callback and waiter own
independent Arc references to the completion record. Only its nonblocking
completion and queue storage are released in softirq, never the retired object.
The initialized service has a permanent driver claim and fixed participants;
submission from a running CPU cannot reject a prepared request. Violating that
boot invariant is fatal, not a recoverable post-withdrawal failure.

The owning container is not admitted outside kernel integration tests. Its
unsafe read requires the current task's genuine scope; accepting an arbitrary
TaskReader token as safe would permit reclamation while a reader is untracked.
Controlled hardirq readers must already be watched, retain initialized current,
and finish their strictly nested scope before handler return without scheduling.
The CPU cannot cross a later quiescent boundary while that borrow is active.
Controlled softirq readers follow the same watched, nonscheduling, strictly
nested contract. The live callback fixture releases its captured container before
publishing completion, preserving final payload destruction in task context.
Controlled NMI object reads require audited entry with initialized retained
current and watching established before dereference. Nested scopes only update
atomic accounting; they cannot lock, reclaim, schedule or leave borrowed data
after return. Software-entry tests cover task and quiescent origins, not hardware
delivery or unaudited early architecture entry windows.
Separate implicit-region fixtures retain raw allocation ownership and delay
callback invocation until an independent reader-exit handshake, even if the GP
incorrectly grants eligibility. Only the designated reader CPU participates in
that isolated coordinator. A hardware-clock escape bounds its IRQ-off reader;
stalled post-exit GP progress retains storage until the bounded QEMU runner
terminates the test, rather than fabricating quiescence to release it.
Only initialized pointers are published, and only the unique exchange owner
reconstructs each removed Box after the GP. Shared containers require both
Send and Sync payloads: writers may recover values allocated by other tasks.
It exposes neither an unguarded reference nor mutable access. Final Drop requires the
exclusive container owner and performs ordinary caller-context destruction;
it cannot overlap a protected borrow or an in-progress replacement.

## Service Startup And Failure Handling

Private callback admission checks NMI before ordinary guards, then watching and
readiness before touching the supplied node. Rejected submissions retain caller
ownership and do not request progress. Successful submission transfers ownership
before notification; a callback may already have executed when the submitter
returns. Callers must therefore never inspect the transferred node afterward.
The synchronous marker releases its extra raw Arc reference on rejection before
reporting a violated prepared-service invariant. This boundary remains test-only;
it does not yet admit arbitrary callbacks or certify their destructors.

The private prepared handle consumes a unique node owner rather than exposing
repeatable raw submission. Its cancellation function is a separate unsafe
contract: it must be safe without a GP and must not reclaim objects still used
by protected readers. A rejected submission returns the handle unchanged, so
the caller can retry; no implicit cancellation runs on rejection. The handle
is Send only because construction certifies transferable storage and cleanup;
it is not Sync and does not permit shared submission. Accepted submission
suppresses Drop before publishing the node, including when invocation frees
the enclosing allocation before submission returns. Neither path may access
the node after its ownership is consumed.

Admission and lifetime checks are independent:

| Submission condition | Ownership result |
| --- | --- |
| Watched task, hardirq or softirq on an initialized participant | Success consumes the handle; invocation can finish before submission returns. |
| NMI or unwatched execution | `InvalidInput`, with the unchanged handle returned and no cancellation or invocation. |
| Service not ready or CPU outside the fixed participant set | `BadState`, without queue transfer. |

Restoring an admissible context permits retry of the returned handle, but does
not relax withdrawal or callback/destructor requirements. Rejection is neither
a GP nor evidence that protected readers have stopped using the object.
CPU participation checks do not implement CPU hotplug or teardown.

`init_service` requires all discovered CPU services to be initialized before
creating the GP task. Static state retains its nodes and the one-shot driver
claim; duplicate startup cannot replace an active controller. Serialized startup
installs the action before claiming control.
Registration conflict leaves control unclaimed; the existing action is untouched.
The startup lock is never held across task creation or the acknowledgment wait.
Allocation failure after assignment follows the fatal kernel policy, not rollback.
The task publishes
only its waker after preparing its task-bound wait and timer. No ordinary
callback admission or general RCU-ready flag is published. The Rcu softirq action is
installed before task activation. Fixed IPI notification only marks it pending;
the action later wakes the GP task without clearing demand. The no-IPI fallback
raises softirq directly and may wake its daemon, so it cannot be called under
scheduler/RCU locks. Neither route supports NMI. No timeout can certify a reader's
exit. Timer cancellation runs outside bookkeeping locks. Saturating deadline
arithmetic validates representation, not progress at the host clock horizon.

## Unsafe Traceability

| Path | Required invariant |
| --- | --- |
| `src/context.rs`: current reader access and static provider | Retained task storage; current or strictly nested execution for mutation; initialized CPU identity; original reader-node lifetime. |
| `src/context.rs`: cleanup and switch reporting | Safe static methods consume immediate-use core contexts whose unsafe construction certifies the boundary; provider core calls retain exclusion, enrollment-before-observation and no callback/wakeup under scheduler locks. |
| `src/context.rs`: user entry/exit | Same-CPU IRQ-masked pair, no protected borrow or scheduling across the unwatched window. |
| `src/service.rs`: state construction, observation and invocation | One permanent controller; all admitted CPU services initialized; fresh ordered observations; eligible callbacks outside bookkeeping locks. |
| `ut-tests/callback.rs`: preparation, submission and cancellation | Unique stable node with enclosing provenance; transferable resources; cancellation safe without a GP; accepted transfer suppresses cancellation before publication. |
| `ut-tests/read.rs`, `ut-tests/synchronize.rs`, `ut-tests/owned.rs` | Test-only admission; retained current and valid context; unique withdrawn ownership and independent callback/waiter lifetimes. |

## Failure Modes And Audit Limits

| Failure | Consequence | Control or residual limit |
| --- | --- | --- |
| Early service startup or competing node controller | Invalid observations or corrupted lifecycle | Runtime rendezvous, single startup claim and permanent storage. |
| Cleanup notification mistaken for quiescence | Premature callback eligibility | Notifications are hints; only validated observations and drained readers complete a GP. |
| Lost or delayed progress delivery | Retained callbacks and storage | Persistent demand and timed assistance; no completion inferred from timeout, no hard delivery bound. |
| Callback blocks or abandons its stack | Softirq stall or unreleased execution claim | Only controlled nonblocking callbacks admitted; no general consumer interface. |
| Invalid user/IRQ/NMI boundary | Untracked protected access | Explicit host contracts; modeled tests do not qualify unaudited hardware entry paths. |

The crate does not process user-private payload contents or promise memory
scrubbing. Test source documents coverage, not a new execution result.
Audit provider availability separately from service readiness, keep cleanup
local, retain node/task storage through unlink, and preserve the evidence limits
above when changing context admission.
