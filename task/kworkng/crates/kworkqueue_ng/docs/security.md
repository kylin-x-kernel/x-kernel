# Workqueue Security

## Scope And Boundaries

The unsafe service::prepare_retiring_work boundary accepts a trusted provider's
opaque pointer and terminal function. Threat: mismatched pointers, cancellation,
repeated publication or outstanding references can cause leaks/double-free/UAF.
The provider must bind a valid cross-CPU pair, retain publication, publish at most
once and exclude other accesses/cancellation before freeing storage. Work's
private immutable Retirement pair implements Send/Sync under that contract.
Work::run_claimed and the test equivalent copy it before clearing running, end
all Work borrows, complete independent notifications, then invoke it. A new
cancel_sync can observe an empty running slot and permit storage release before
those notifications; retaining a pointer until notification is not sufficient.
No Work access follows. Ordinary Work constructors never install this hook. Completion
does not wait for the hook, and the action must retain its own context until use.

Queue is exported through service.rs with flush and close_and_drain. These
operations do not stop execution providers. Queue's implementation constructors
are crate-private; service binding relies on trusted kernel integration code to
validate readiness and complete CPU/category binding. Queue fields, partitions
and source teardown internals remain private. Global default queues and safe
owning wrappers are outside this crate and this PR's security scope.

The public Work alias fixes the internal payload to Arc<Queue>; callers cannot
edit state, pending links, callback identity or the delayed timer binding. Public
queue/api.rs methods accept pinned borrowed objects and remain unsafe because
Pin does not retain storage. Their callers must join producers/concurrent APIs,
then cancel_sync before release. DelayedWork additionally drains physical timer
delivery, even when logically idle. No Drop-based drain or owning queue reference
to Work is introduced. All publication wrappers delegate to the existing routing
and timer transactions using the real kernel CPU sampler; no new raw projection,
lock order, allocation or occurrence state is added.

Threat: modification of idle delayed work publishes an unexpected borrowed target.
Mitigation: modify_delayed_work carries the same unsafe retention contract as
initial admission. Flush documents that it is not a storage-release barrier;
non-synchronous cancellation/disable do not prove callback quiescence.

Diagnostic assets exist only with stall-check. Each partition owns an optional
Arc<StallRecord>; the registry holds Weak references and the monitor temporarily
upgrades one at a time. Record identity is copied kernel data. It has no Queue,
Work, business capture or callback reference and no handwritten unsafe operation.

Threat: monitoring races queue destruction and dereferences freed memory.
Mitigation: ordinary Arc/Weak ownership retains only diagnostic storage during
inspection. No unregister wait or raw node lifetime protocol is required. The
record/name can be freed by the final owner, including the task-context monitor;
dead weak allocations are removed on registration or inspection. If monitoring
and registration both stop, weak slots may remain until directory destruction.

Threat: monitoring waits on the very queue that is stalled. Mitigation: the
monitor try-locks the directory and record, without taking partition or worker
locks. Busy means unavailable, not healthy. Producers acquire partition -> record;
inspection never acquires partition protection. Output runs without record or
spin protection, but under the registry's task-context Mutex. Its restricted
reporting closure must not register queues or reenter this directory. Record
destruction contains no user callback and cannot reenter registration.

Threat: new submissions hide old backlog. Mitigation: only empty/nonempty changes
and actual claims publish a new interval; allowance release does not. Running-only
and timer-only work are excluded. Warning throttling and interval publication use
the same short lock, removing mixed-version and counter-wrap hazards.

Accepted limitations: contention or unavailable monitoring/output may delay or
miss warnings indefinitely; no bounded delivery, fairness, capacity diagnosis or
strict post-shutdown suppression is promised. A sampled warning can race later
progress or queue destruction. This is an auxiliary warning, never recovery or
proof that a queue is healthy. Diagnostic binding attaches records before
queue exposure; the integrator must call inspect from task context only.
Existing Work pinning, cancellation, flush and source-drain guarantees are unchanged.

The service module exports opaque QueuePartitionSource and Execution types to
the root integration crate. Safe external code cannot construct either directly;
private Queue bindings create sources and protected claims create executions.
Providers retain the existing obligation to run each acquired execution through
cleanup. The export neither owns Work nor changes its unsafe publication contract.
The same module exports opaque Queue construction/drain and WaitError for root
integration. Providers must supply a matching live dispatcher and IRQ-compatible
notification for each allowed CPU, keep execution available through drain and
detach sources before stopping services. Queue owns those source registrations;
the provider owns execution lifetime. No safe production submission API is added.

Covers src/queue.rs, src/queue/**, src/work.rs, src/work/** and src/diagnostics/**.
Kernel unit tests exercise private state and real execution providers, not a
production submission API. No raw user, device, network, file or FFI inputs are
accepted. Trusted kernel inputs include configuration, callbacks, resource payloads,
current-CPU sampling. Callback identity comes from ktask's current task.
Prepared source bindings also trust the provider's CPU/category selection and kick:
it must be IRQ-compatible, retain only independent service state and never run a
business callback inline. Public borrowed publication APIs remain unsafe.
The public configuration constructor accepts kernel CPU IDs, policy and release
allowance after boot CPU-map initialization. Empty, duplicate or unmapped CPUs
and zero allowance reject before configuration exposure. The normalized CPU slice
is owned, so caller mutation cannot change routing. Valid configuration grants no
service readiness or submission authority; the injected test validator is private.
The serial bottom_half test fixture uses kirq's scoped action registration and
real local softirq runner; its local-only Waker asserts the destination CPU.
It suppresses daemon wakes and restores the handler/pending state on exit. This
does not install production services or justify a remote wake/IRQ-allocation claim.

## Protected Assets And Unsafe Inventory

| Asset / site | Protection and obligation |
| --- | --- |
| queue/partition/pending.rs intrusive list | Non-owning UnsafeRef nodes require stable caller-owned Work; partition lock serializes membership |
| work.rs pending_ptr, claim_pending_node, release_pending_node | Constructor binds concrete layout/function pair; pointers derive from the whole Work allocation; no storage release during access |
| work/execution.rs erased Execution | Unique claim, matching callback type and live storage through callback/cleanup; snapshot terminal metadata before clearing running, with no subsequent Work access |
| queue/submission.rs and queue/partition.rs admission/removal | Partition -> Work locking, owner/phase revalidation and external lifetime contract |
| queue/partition/delayed.rs queue_due | Caller retains stable Work through execution; owner and timer placement are revalidated under partition -> Work protection before intrusive insertion; current Queue payload supplies the color |
| work/delayed.rs arm_on, deliver and DelayedTimerHandler | Pin fixes the inline Work address; constructor pairs its concrete layout with the erased function; release/acquire publishes the immutable non-owning pointer; external owner retains storage and captures through all calls and both drains |
| work/delayed.rs queue_after, queue_after_on | Zero delay reuses ordinary unsafe submission with the same external lifetime contract; positive delay uses checked monotonic addition and the partition's range check, never saturating or wrapping into an earlier deadline |
| queue/delayed.rs arm_delayed and queue/routing.rs resolve_target | Queue owns policy/cursor; owner identity and explicit target are checked before selection and the owner snapshot is revalidated under partition -> Work before programming; retries keep the original absolute deadline |
| queue/partition/delayed.rs arm_delayed | Matching Queue/partition/Work/timer is supplied by DelayedWork; admission and range checks precede timer start under partition -> Work; logical publication is infallible after start; timer core releases its locks before delivery |
| work/delayed.rs retime_pending_at and queue/partition/delayed.rs retime_pending | Same external pinned Work/timer binding; revalidate pending ownership, disable and closure under partition -> Work; physical programming succeeds before unlinking the exact ordinary node; no locks span wake or observer completion |
| work/delayed.rs modify_after and queue/delayed.rs modify_delayed | Retained external pinned pair across retry; Queue validates unfinished ownership; only fresh admission advances RR; zero transfer charges before mutation and cancels the old timer under the same guards, with no synchronous wait |
| WorkState::withdraw_pending_to_timer | Moves only color and old observers to local cleanup; resources and partition remain in pending. Partition owns unlink, allowance and count retirement; WorkState never operates QueueInFlight |
| DelayedWork::cancel_sync | Existing independent disable guard spans ordinary cleanup and physical timer drain; no management lock spans waiting; success permits storage release only after producers have been stopped/joined |
| DelayedWork::cancel_pending and Work::cancel_pending_with | Internal timer attempt runs after owner validation under Work protection, before logical detach; no timer access after unlocked observer notification; the provider action must not sleep, allocate, unwind or reenter Work/partition operations |
| DelayedWork::disable and disable_sync | Reuse the ordinary independent guard to exclude rearm through physical cancellation; only the synchronized variant drains delivery; releasing one guard cannot release another caller's disable |
| QueuePartition::capture_and_expedite | Caller retains the pinned Work/timer and performs synchronous-context preflight; protected owner/capacity/gate checks precede physical cancellation and receipt attachment; Running rolls back the charge and requests unlocked retry; successful capture follows physical quiescence, not a post-wait global drain |
| DelayedWork::flush | Validates context/self-wait and prepares both possible receipts before effects; idle physical cancellation rechecks ownership under Work protection; retries yield unlocked; later timer arrangements are never drained after capture; external lifetime obligations remain unchanged |
| WorkState slots and disable depth | Work lock serializes capture, admission and disable; independent borrowed guards release only their own disable |
| Optional Work CallbackNode | Sole comparison-only task key under its own lock; executing task stays live until return; private scope clears before cleanup |
| queue/partition/callbacks.rs run and CallbackGuard::drop | Borrowed UnsafeRef insertion/removal uses stable external Work storage and unique execution; original partition lock protects list; private guard unlinks before cleanup |
| QueueInFlight counts and observer chains | Original partition/color obligation survives detached cleanup; terminal notification outside locks |
| Queue.sealed_colors and control color bits | Sleepable serializer owns cutoff/setup/reuse; atomic update preserves closure; admission and retirement never take this mutex |
| ColorCompletion locals | One-shot independent receipts; all locals must be terminal before reuse; no Queue/Work back-reference or duplicated count |
| CleanupWait and queue drain receipts | Waiter-owned registrations prepared before effects; no Work/Queue back-reference |
| queue/source.rs QueueSource | Partition-protected publication; weak Queue payload avoids cycles; only service kick leaves protection |
| Immutable QueuePartition source binding and detach receipt | Retained through repeated shutdown; prepare registrations before closure, detach after work drain and wait outside management locks |

Execution no longer carries a temporary Queue/source borrow. Its private raw
constructor relies on unsafe publication to preserve Work through consumption;
only test-local typed claims retain a BorrowedExecution lifetime. The explicit
Send/Sync implementations belong to the Retirement pair described above; execution
does not add its own implementations. kspin, kpoll, intrusive-collections, kworkerpool_ng,
ksync and ktask are trusted providers. Queue flush adds no unsafe operation.
The inventory above and threat responses below cover this crate's unsafe boundaries;
provider internals are covered by their own crate security documents.

## Threats And Responses

| Threat and trigger | Impact | Response |
| --- | --- | --- |
| Owner releases/moves linked or executing Work | Dangling access / memory corruption | Mitigate through unsafe publication contract and caller drain; public safe ownership facade not claimed |
| Duplicate insertion or stale CPU ownership | List corruption / wrong cancellation | Mitigate with owner/phase validation under partition -> Work locking |
| Owner changes between delayed CPU selection and commit | Wrong CPU, lost acceptance or extended delay | Retry before timer programming; retain the same deadline across retries and advance RR only after successful fresh automatic acceptance |
| Stale timer notification after cancel or retime | Resurrected or premature execution | Recheck current owner, timer placement and authoritative deadline; transfer consumes timer placement, so repeated notification cannot insert twice |
| Freeing logically idle DelayedWork during physical delivery | Dangling borrowed Work pointer | Require successful delayed cancel_sync, including for idle Work, after stopping producers; pinning alone and Timer's Drop do not prove ordinary drain |
| New arm overlaps synchronous cancellation | Timer escapes drain or an unrelated successor is canceled | Retain the Work disable guard through timer.cancel_sync; arm checks disable under the same Work lock used for publication |
| Queue closes before caller drains delayed work | Expiry cannot enter ordinary accounting | Caller must drain delayed work before Queue shutdown; Closed delivery is defensively ignored, not counted as successful execution or automatic timer teardown |
| Cleanup counted complete too early | Premature teardown | Mitigate by retiring original counts and notifying observers after destruction |
| Retiming an ordinary occurrence is treated as cancellation | Lost future work or premature resource release | Retain accepted resources/owner in the same slot; end only old queueing/count/observer scope after unlink and allowance cleanup |
| Failed retime removes old queueing or resets its deadline | Lost work, changed FIFO or stranded flush | Validate range, disable, closure and physical start before mutation; no recoverable failure remains after successful programming |
| Waker/destructor reentry under management lock | Deadlock | Eliminate those calls under management locks |
| Source reference retires before claimed callback | Dangling Work or Queue | External publication contract retains Work; running slot independently retains Queue; claim does not borrow source |
| Eligible Work is not serviced after cancellation/completion | Lost progress | Publish under the partition lock on every release, then kick unlocked; normal services use durable task waiting, while production activation remains pending |
| Registration retains Queue strongly | Permanent reference cycle | Weak Queue in adapter; kick must not capture Queue/Work/registration |
| Shutdown detaches before accepted Work drains | Lost execution progress | Detach only after all ordinary color receipts complete |
| Concurrent shutdown observes unpublication but an inspection remains | Premature teardown | Every caller waits on the same permanent source receipt, covering inspection reference release |
| Invalid context or direct self-wait | Hang / invalid scheduling | PreparedTaskWait validates context; optional task comparison rejects Work flush/cancel/disable and own Queue flush/shutdown before effects; indirect dependencies remain excluded |
| Reuse before a late old-color commit | Early return / teardown | Final admission sample stays under the partition guard; setup crosses every guard and holds the flush serializer until all visits finish |
| Numeric slot reuse redirects an old waiter | Wait includes unrelated work or ends early | Retain immutable receipt Arcs, compare snapshot identity before effects, never reset/retarget receipts |
| All colors occupied | Admission starvation or deadlock | Wait on registered old receipts outside the serializer; shutdown is independent of that mutex and color space |
| Registration/Waker reentry under flush mutex | Deadlock / partial capture | Prepare registrations unlocked, revalidate snapshot, deliver empty notifications after unlock; no recoverable failure after publication |
| Concurrent submission changes a Work flush scope | Early return or unbounded wait | Capture under one Work lock after observer-capacity recheck; receipts follow only captured occurrences through cleanup, not later successors |
| Allocation exhaustion during preparation | Fatal OOM | Accepted/deferred to kernel-wide effort by user decision; no recovery claim |

## Failure Handling And Validation

| Failure mode | Local/system effect | Response |
| --- | --- | --- |
| Invalid configuration, deadline or admission | Requested work is not accepted | Return a typed error before changing the arrangement |
| Execution provider stops before drain | Accepted work and synchronous waiters cannot progress | Integrator must retain providers until queue drain and source detachment |
| Callback or destructor fails to return | Work remains running; drain may wait indefinitely | No recovery or timeout guarantee; optional diagnostics are warning-only |
| Preparation allocation fails | Kernel allocation failure | Defer OOM policy to the allocator; submission does not allocate |

Audit checks: verify external pinned storage retention, partition-before-Work lock
order, unlocked callbacks/destruction/notification, delayed physical timer drain,
and provider lifetime through source detachment. These contracts do not establish
end-to-end system acceptance before callers have been migrated.

Timer-only placement retains its original partition but has no ordinary color.
WorkSlot::complete must therefore retire a count only when color is present, still
after resource release. PendingPlacement keeps timer deadlines mutually exclusive
with waiting/ready state; release cannot grant allowance to timer placement.
The state primitives do not publish a timer or intrusive node. The partition's
queue_due uses the existing unsafe intrusive insertion contract and real accounting;
all fallible validation/charging precedes placement mutation. Tests retain pinned
Work through logical cancellation/execution and probe unlocked service wake.
The private DelayedWork now binds a real prepared timer and enforces partition ->
Work -> timer programming and unlocked synchronized drain. Real IRQ tests cover
borrowed captures, repeated cancel/reuse, validation failures and cancellation of a
running predecessor with a timer-side successor. They do not prove global
system integration. Pending absolute retime tests
cover ready/waiting withdrawal, observer boundaries, retained resources, timer-only
adjustment, unchanged failure and claim-first behavior. A source/observer Waker
reacquires partition protection to check that withdrawal notifications are unlocked.
Private modify_after tests cover empty-slot acceptance, pending modification,
claim-first successor admission, route-sampling competing admission, disabled/
closed/conflicting-owner rejection and zero expedition with IRQs masked. Zero
charges ordinary participation before logical transfer; nonwaiting timer cancel
is serialized with rearm under partition -> Work. Old physical delivery rechecks
placement, and final cancel_sync still drains it before external storage release.
These controlled interleavings do not prove exhaustive cross-CPU races. Private
relative-delay tests cover zero under masked IRQs, positive monotonic bounds,
overflow and unchanged pending work on rejection. These are semantic checks,
not latency thresholds or allocation measurements.
Nonwaiting delayed-stop tests cover timer/ready revocation under masked IRQs,
claimed predecessors, independent disable ownership, synchronized context rejection
and an observer that rearms before old cancellation returns. The latter checks the
new physical timer remains armed, not merely logical state. Physical in-flight
delivery stress remains separate; all fixtures synchronously drain before release.
Logical delayed-flush capture tests cover timer expedition, pending/running receipt
capacity retry, predecessor exclusion, withdrawal completion without following a
new timer, stale ownership and unchanged closed rejection. Complete private flush
tests additionally cover actual parking, callback completion, withdrawal preserving
physical membership, idle/closed/context handling and independent disable ownership.
A remote-CPU test holds a per-object IRQ tail after Work publication to verify
capture retries before physical delivery exits, then exercises the actual wait.
The test gate is released before assertions and all tasks/timers drain before its
storage is released; it is not a global service replacement. A controlled Running
provider tests retry before capture for timer, ready and running Work, including
charge rollback and modification between attempts. In particular, a later global
timer cancel must not be used to finish a scoped wait while producers can rearm.
Disabled delivery is harmless while synchronous cancellation revokes placement;
Closed delivery indicates caller teardown ordering was not respected. Both leave
cleanup to the caller's stop. Count overflow or any unexpected transfer failure is
a fatal invariant violation, not silent loss of accepted work. No unwind recovery
is provided. Delayed delivery adds no unsafe Send/Sync implementation or owning
callback cycle; the separate Retirement contract is described above.

Registration and context failures precede stop/closure/cutoff effects. Once effects occur,
waits have no recoverable interruption or timeout. Callback/destructor panic recovery
is not supplied. Tests cover cancellation, cleanup, independent disables, concurrent
waiters, ownership, partition/color drain and pointer provenance at earlier checkpoints.
Crate relocation requires rerunning tests under kworkqueue_ng; old Miri evidence is
checkpoint-specific. No private user data is retained beyond caller callback/resources.

The remote flush fixture retains Work in outer Arcs/pins until cancellation and
task joins. Its test-only admission pause spins under the partition guard while
another explicitly pinned CPU publishes the cutoff; it never parks under that
guard and skips when fewer than two CPUs boot. The handshake begins only after
flush's self-wait preflight releases partition locks. This validates a controlled
cross-CPU schedule, not exhaustive concurrency or allocator behavior.

Disabling self-wait-check compiles out callback identity storage and entry/exit
checks, but not context validation or cleanup accounting. Direct self-waits then
remain invalid usage. Destructor-originated and indirect dependency deadlocks are
not diagnosed. Callback panic/task exit is forbidden by the execution contract;
this diagnostic does not provide panic recovery or a task-exit cleanup mechanism.
