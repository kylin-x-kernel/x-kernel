# ktimer design

## Implemented: Ignored Periodic Record Replay

Emission snapshots ITimer's nonzero interval into the reserved receipt while
holding the timer mutex. ksignal can distinguish periodic retention from
one-shot discard without acquiring a timer lock under SigLock. Retained records
remain pending in the timer's accounting state, so later expirations accumulate
overruns; only valid dequeue freezes that count. Action/mask replay transfers
the existing signal node and schedules a wake through the ready worker.
posix_timer_record_wake_target validates exact reservation identity before
returning a cloned stable target. The runtime uses it only after unlocking.
The process regression drives a CPU periodic timer through ignored expiry,
additional overruns, restored disposition and a single acknowledgement.

## Implemented: Unlocked Timer Retirement

Deletion and exec clearing now transfer retired ownership out of the manager.
`delete_posix_timer` removes one value and returns RetiredPosixTimer;
`clear_posix_timers` takes the entire BTreeMap into RetiredPosixTimers without
allocating replacement storage. Both wrappers are must-use, opaque owners:
callers drop them after releasing their manager lock. Map removal immediately
rejects lookup and old completion, independently of delayed owner destruction.
Single removal may still free BTreeMap nodes inside the transaction; the
guarantee concerns timer payload ownership, not all allocator activity.
Batch removal moves map storage too. Generations are not reset, so delayed
old completion cannot validate a reused timer ID. kprocess refreshes the CPU
timer gate before unlocking, then releases the retired owner. Existing alarm
heap entries can still cause obsolete owner polls; this step neither removes
heap entries eagerly. Retired wrappers cancel invalidated pending ownership
before releasing payloads, outside the timer mutex.
Tests retain the returned owners across lookup, stale completion, ID reuse,
repeat clearing and CPU-gate checks, then verify weak-reference release.

## Implemented: Creation Configuration Before Admission

`PosixTimerConfig::new` validates the clock, then fallibly reserves notification
storage without publishing a timer. Its fields are private and it is consumed, not cloned,
by `ProcessTimerManager::create_posix_timer`. Only admission assigns the timer
ID/sequence, resolves a default timer-ID sigval and inserts the disarmed timer.
ABI target checks remain with the syscall adapter. kprocess captures and
revalidates the selected signal destination before manager admission.

Configuration also carries a weak TimerSignalOrigin captured from the admitting
runtime. Each timer retains it and emission claims its reserved record lease;
TimerSignal and TimerDelivery are no longer Clone, so the emission-to-receipt
handoff cannot be duplicated. Weak conversion/clone adds no allocation and does
not keep the runtime payload alive. Dequeue validates against this origin even
if another runtime has identical numeric timer ID/sequence values. This changes
completion identity. The configuration also retains TimerSignalTarget: a weak
process signal manager and, for private delivery, a non-wrapping SlotId. Each
emission clones this target without allocation. POSIX dispatch uses that target,
not the numeric PID/TID envelope; legacy itimers keep their existing route.
The trusted creation adapter must supply the target selected by notify. Domain
clock tests use expired target fixtures and do not exercise actual dispatch.
SIGEV_NONE still reserves no signal record. Unit clock/accounting tests pass an
explicit expired test origin because they do not perform signal completion.

Admission failure returns `PosixTimerCreateError`, retaining the unconsumed
configuration alongside the original error. The caller moves that result out
of the manager critical section before recovering or releasing the configuration.
The production Process adapter uses an explicit lock scope before error mapping;
there is no implicit conversion that could discard reserved ownership
inside the manager transaction.

`Process::create_posix_timer` pins its runtime first (preserving NoSuchProcess
precedence), prepares the configuration outside the manager mutex, then admits
it and refreshes the CPU-timer gate under the mutex. Notifying timers now reserve
one KArc-backed signal record here with KERNEL permissions. NoMemory returns
before manager admission; SIGEV_NONE skips the allocation. This does not make
BTreeMap/alarm allocations fallible. The public
Process and syscall signatures are unchanged; the lower-level manager now
requires the validated configuration rather than raw clock/notify arguments.

### Implemented: Non-Wrapping Notification Generations

The manager issues u64 generations from an Option<u64> source, independent of
the u32 siginfo field. Creation and successful settime each consume one value;
issuing u64::MAX leaves None permanently. Exhausted creation returns the
unconsumed configuration before allocating an ID. Exhausted settime, including
disarming, returns InvalidInput before changing deadline, pending accounting or
alarm state. Invalid settime arguments do not consume a generation. Deletion,
completion and exec cleanup remain possible and never reset the source.
Siginfo receives the low 32 bits, while the typed receipt retains all 64 bits;
matching ABI bits cannot validate an older notification. Record occupancy is
separate from this schedule generation. Tests cover the last value,
exhaustion, preserved timer state, cleanup and equal low-bit stale receipts.

Manager tests cover unsupported-clock rejection and configuration consumption
with ID/sequence assignment and disarmed-state publication. Existing timer
arming/dequeue tests now enter through this same configuration interface.
An injected ID-allocation rejection exercises the same admission body, checks
that no map entry or sequence is published, and retries the returned configuration
through the public manager interface. CPU-clock and thread-target behavior remain
intact on retry. This does not simulate filling the entire timer-ID space.

### Implemented: Reserved Notification Storage and Deferred Demand

PosixTimerConfig reserves PreparedTimerRecord before admission. Each expiration
claims a unique lease and transfers it in TimerSignal::Posix; dispatch no longer
reconstructs dequeue authority from copied fields. ksignal stores prepared
notifications in the same intrusive FIFO as ordinary notifications without
allocating another queue node. The record remains occupied through dequeue
validation and until the receipt releases it. Queue destruction and rejected
sends also return the lease, without reporting successful delivery.

Rearm resets current-generation accounting, not record occupancy. Expirations
while an older lease remains occupied accumulate in deferred_expirations. The
next claim consumes that count and uses count minus one as overrun. A successful
dequeue closes the old accounting before releasing the lease; expirations after
that cut become new demand rather than changing last_overrun. Stale completion
cannot reset the current schedule's demand.

Lease return queues the record on ksignal's separate intrusive ready link. The
single alarm task processes one ready record and checks alarm deadlines on each
iteration. The runtime bridge checks both the captured weak origin and exact
record storage identity before polling one timer. A reused timer ID cannot
redirect old ready work. The retry returns one delivery, without allocating a
batch, and dispatch occurs after releasing the timer mutex. This also handles
already-expired one-shots with no remaining alarm or CPU-timer gate.

This slice does not implement every optimization in the draft below.
Distinct reserved timers can now queue the same standard
signo without coalescing; repeated expiration of one timer still folds into its
single outstanding receipt. Detached stale receipts still
fail generation validation; queued stale records are actively cancelled.
In particular, returning a rejected lease does not clear pending accounting or
claim successful delivery; ignored periodic replay preserves the same receipt.
Rearm returns a must-use `PosixTimerUpdate`; `finish` cancels the
previous generation after unlocking, before returning old state and immediate
delivery. Delete and clear invalidate before unlocking and cancel during
retirement-wrapper destruction. Cancellation never acknowledges delivery.
Ordinary sends, alarm heaps, timer maps and delivery batch Vecs retain allocation.
Tests cover injected reservation failure, SIGEV_NONE bypass, busy-record rearm,
one-shot deferred demand, and the accounting cut before receipt release.

## Implemented: Single-Consumer Alarm Rescan Wake

The production alarm worker now uses the existing futures AtomicWaker rather
than event-listener for new-earliest-alarm notifications. AlarmWake holds one
inline waker registration and an independent needs_rescan bit. Producers update
the heap, unlock it, publish the bit with Release, then wake the task. Each poll
registers before consuming the bit with an AcqRel swap. A request before first
registration is retained; a request after the empty check wakes the registered
consumer. Requests coalesce because heap contents, not a count of wake calls,
determine work. Cancelling a pending wait does not clear the bit. A consumed
registration is renewed on the next poll; retaining only a one-time registration
would stall later alarms.

spawn_alarm_task is idempotent through Once, enforcing the single-consumer
contract. The worker still checks heap deadlines before selecting the new-alarm,
deadline and clock-change branches. A retained request can cause an extra harmless
scan; it does not itself expire a timer. Existing clock listeners and bounded
registration-error retry remain unchanged.

This replaces a live production wake source, not an unused prepared-record
wrapper. No listener allocation occurs in AlarmWake, and production uses the
audited task-bound KWaker. Heap growth, AlarmTicket Arc construction, task startup,
deadline waits and clock listeners remain outside this guarantee. The record-
ready list now shares this wake source and processes one record per alarm pass.
Tests cover pre-registration publication, coalescing, repeated wake registration,
cancelled/replaced waits and wake readiness while the other select branch stays
pending. They do not inject an interrupt inside AtomicWaker::register or global
allocator failure.

## Draft: Prepared POSIX Timer Notification Records

Status: partially implemented; the implemented sections above define the current
scope. The remaining ignored-record replay behavior below is a design target,
not a current guarantee. Prepared standard-signal admission is implemented. This protocol
is a prerequisite for
the ordinary-signal KBox migration described in `../../ksignal/docs/design.md`.
It is a signal/timer lifecycle change, not a mechanical owner substitution.
Do not infer that all transitions below are already implemented.

The first prerequisite is implemented in ksignal/kprocess: an inline typed
dequeue receipt carries timer ID/sequence authority independently of ABI
siginfo, and all consumers complete it outside SigLock. This removes the
signo-based observer dependency. Reservation, unique lease return and a separate
ready list, pending/ignored cancellation and ignored retention are implemented.
Cancellation uses safe identity search in a FIFO and ignored list rather than the
O(1) raw-cursor protocol proposed below.

### Reference and Compatibility Decisions

The reference is `/root/linux-stable` at `8d3ae59288f1`:
`kernel/time/posix-timers.c::alloc_posix_timer`,
`__posixtimer_deliver_signal`, and `posixtimer_deliver_signal`; and
`kernel/signal.c::posixtimer_init_sigqueue`, `posixtimer_send_sigqueue`,
`posixtimer_queue_sigqueue`, `collect_signal`, and `posixtimer_sig_unignore`.

Linux embeds one prepared sigqueue in each timer. Repeated expiry of that
timer does not add another linked record. Distinct timers can each queue their
record even when they use the same standard signo: this path does not use
ordinary-send `legacy_queue` coalescing. A queue reference keeps the timer
alive during dequeue. Linux drops siglock before acquiring the timer lock for
validation/rearm. Ignored periodic notifications also have explicit retained
state and an unignore path, rather than becoming permanently unacknowledged.

Adopt these ownership and progress properties, not Linux's exact scheduling
algorithm. X-Kernel currently advances deadlines while polling and catches up
overruns on dequeue. Keep that clock model for this change; do not silently
switch to Linux's periodic rearm-at-delivery machinery. Linux's current
delivery code updates si_overrun while rearming, whereas X-Kernel currently
returns the emission-time snapshot in siginfo and the delivery-time count via
timer_getoverrun. Preserve that existing split here and review any ABI change
separately; the two implementations are not identical in this detail.

### Ownership and Interface Responsibilities

The proposed interfaces are provisional names, not existing declarations:

| Module | Interface / resource | Responsibility |
| --- | --- | --- |
| ksignal | `PreparedTimerRecord` | Stable queue links, record state, immutable queued payload, strong storage ownership; no dependency on ktimer/kprocess |
| ktimer | Timer owner + record handle | Desired arming generation, clock/deadline, accumulated overrun, retired state; at most one outstanding record lease |
| kprocess | Dispatch/completion bridge | Move a unique send/dequeue lease between the timer and signal transactions; resolve rejection, cancellation and completion outside SigLock |
| ksyscall | Creation/settime adapters | Validate ABI inputs, map typed errors, publish the timer ID only after admission, retain existing output-copy rollback |

Use `kalloc_types::KArc` for fallible shared storage, retaining the standard
Arc reference-counting implementation rather than adding custom counting.
The timer and an outstanding
send/queue/dequeue lease each hold storage references. Links must be compatible
with the project intrusive-list implementation. Reservation must be genuinely
fallible, including reference-count backing storage: an infallible Arc::new
around a KBox would not meet the contract. KArc now supplies the reviewed
allocation-policy adapter and raw reference transfer. Its optional KArcPointerOps
integrates ownership with the existing intrusive_collections implementation.
Concrete pending and ready membership and pending cancellation are implemented;
no additional generic queue or per-queue identity allocation is proposed.
No repository-wide KArc conversion is proposed.

Do not put a strong Process/ProcessRuntime reference in a record owned by that
process's pending queues. Use a weak runtime identity or a non-owning stable
owner cookie interpreted by the bridge, so process -> queue -> record does
not retain process -> runtime forever. A numeric PID/timer ID alone is not
sufficient identity across exit, exec or ID reuse.

### Admission and Allocation Policy

After clock/notification/target validation, prepare record storage in a
reclaim-safe syscall task context, before acquiring the timer-manager mutex,
publishing a timer ID or installing an alarm. Use KERNEL only at that audited
outer point. Revalidate target/runtime availability when publishing. Creation
prepares PosixTimerConfig and reserves storage outside the manager mutex, not
inside admission. Admission rejection returns the configuration for release
after unlocking. Reserved storage follows that ownership path rather than
being dropped inside the manager call.

On reservation failure return NoMemory and publish nothing. SIGEV_NONE needs
no signal record. A failed ID allocation, runtime recheck or user ID writeback
releases the unused reservation through the existing creation rollback path.
Remaining BTreeMap/Arc/alarm/batch-Vec allocations must be listed explicitly;
this guarantees notification-record availability, not whole-timer OOM safety.
No allocation of this record is allowed on expiration, send, dequeue, rearm,
cancel or record return. Do not allocate replacement records to hide a busy one.

### Record States and Exclusive Leases

Storage references may be cloned without allocation; authority to send or
complete a notification may not. Replace the cloneable POSIX TimerDelivery
description with a unique lease for this path; legacy itimer descriptions can
retain their separate representation. All transitions below must be checked
against record identity and generation, not trusted from a copied siginfo.

| State | Owner of active use | Permitted transition |
| --- | --- | --- |
| Idle | Timer, unlinked | Expiry claims Sending and snapshots a generation/payload |
| Sending | One dispatch lease, unlinked | Queued, Ignored, or Completing with a typed rejection/cancel result |
| Queued | One pending queue link and storage reference | Detached as Dequeuing; transfer to Ignored on periodic disposition flush; Completing on cancellation or one-shot discard |
| Ignored | Explicit retained ignored-list membership | Transfer to Queued after disposition/mask reevaluation, or Completing on retirement/reset |
| Dequeuing | One detached receipt, no pending-queue link | Validate generation, finalize delivery or discard, then Completing |
| Completing | Bridge, unlinked | Return Idle only after old receipt stops using the record; otherwise retire |
| Retired | No new admission; outstanding references may remain | Unlink/cancel as needed; free storage when the final reference is dropped |

Retirement is also a monotonic invalidation flag applicable to any busy state.
It blocks new publication even if an older dispatch lease still exists. A
pending bool must not be independently toggled to Idle: derive it from this
protocol or update it at the same checked transition. Repeated expiry never
overwrites a queued payload or links the same record twice. Copy user-visible
siginfo out before making storage reusable. Storage occupancy and notification
accounting are distinct: a receipt can occupy storage after its notification
has been acknowledged.

### Accounting Cut and Deferred Demand

The timer-manager lock protects three separate quantities: the unacknowledged
notification's generation and overrun, the last delivered overrun, and the
current schedule's deferred expiration count. All counts saturate. Deferred
count includes the first expiration; when it becomes a new notification, its
overrun starts at `count - 1`, not `count`.

Successful dequeue validation catches up the matching schedule, freezes
`last_overrun`, and closes that notification's accounting in one timer-lock
transaction. This is the accounting cut as well as the delivery linearization
point. Subsequent expirations belong to a new notification, even if the old
receipt still occupies the record in Completing. They accumulate in deferred
demand until the receipt relinquishes authority. Expirations before the cut
are old overruns and must never be redispatched. The current emission-time
siginfo snapshot remains unchanged by this accounting operation.

Rearm starts a fresh schedule generation with empty deferred demand. If the
old record is still occupied, new-generation expirations accumulate only in
the new schedule's demand. Stale completion neither updates `last_overrun` nor
clears that demand. It only releases its matching old lease, then requests
progress for the current schedule. Every cancellation names record identity
and the captured lease generation; delayed cancellation cannot remove a newly
queued generation. Once storage is reusable, claiming Sending atomically
consumes deferred demand under the timer lock. NoDemand, TargetGone, Stale and
retirement are not successful delivery and do not freeze a new last-overrun.

### Locks and Cross-Transaction Handoff

Never hold the timer-manager mutex and SigLock together. A short per-record
IRQ-safe state lock is a leaf: timer-manager -> record or SigLock -> record
is allowed, but record -> either outer lock is forbidden. No callbacks,
allocation, CPU-time sampling, task wakeup or arbitrary destructor runs while
holding the record lock. Pending/ignored links belong only to SigLock;
timer-side code can invalidate a record but cannot unlink it. The distinct
progress link belongs to the ready-list lock described below.

The bridge performs prepare under the timer lock, drops it, submits under
SigLock, drops it, then handles any completion under the timer lock. Dequeue
detaches a strong receipt and republishes pending projections under SigLock,
then validates under the timer lock after unlocking. Every send outcome is
explicit: Queued, RetainedIgnored, TargetGone, Stale or Cancelled. None is
interpreted as successful delivery merely because the transport returned.

For a delivery already admitted before delete/rearm wins the timer lock, the
copied payload may still reach userspace. If invalidation wins first, drop the
stale receipt. This lock-protected validation is the delivery linearization
point; do not promise to revoke a signal already copied to a user handler.
The bridge must consume each receipt once, including failed target lookup and
queue flush. Dropping the final storage reference must only release memory;
it must not call back into timer or signal locks. RAII cancellation must be
arranged at the unlocked bridge scope, not hidden in arbitrary queue Drop.

All removal interfaces transfer strong ownership out of their lock scope;
they do not destroy the extracted owner there. Final record release, including
its weak runtime reference, runs after releasing timer, signal, ready-list and
record locks. Bulk flush detaches an owning chain using existing links, then
walks it outside the lock; it must not allocate a temporary Vec of receipts.
The same rule applies when deleting a timer-map owner or rejecting a ready
entry. Weak-runtime upgrade results must also be dropped outside these locks.
The allocation adapter must document both final strong-reference destruction
and final weak-reference backing-storage reclamation. These requirements
remove hidden destructor lock acquisition; they do not by themselves prove
that every caller is sleepable or that the allocator is IRQ-safe.

### Rearm, Delete, Exec and Exit

Rearm validates all new parameters before mutation, advances a non-wrapping
internal generation and invalidates an old busy lease under the timer lock.
After unlocking, remove a queued/ignored old record under SigLock. A sender
that is still in flight must recheck invalidation before linking; a receipt
already dequeued must fail old-generation validation. Do not publish Idle
until that sender/receipt relinquishes its lease.

The new timer schedule may expire while an old lease is draining. Record that
demand without allocation. Completion must actively catch up and dispatch it
after releasing locks, rather than relying on another periodic tick: a new
one-shot deadline may already have elapsed. The completion bridge must provide
this progress even for CPU timers and for same-generation demand after the
accounting cut. No new dynamic worker is required or allowed to make record
return work. Completion requests progress rather than looping inline.

The proposed integration is a separate intrusive ready list serviced by the
existing alarm task, with a distinct link reserved alongside the record. Its
short IRQ-safe lock protects list membership, the progress state and a rerun
bit. Never nest this lock with timer-manager or SigLock; ready-list -> record
is permitted only for the leaf retirement check. Producers request progress
after publishing demand or returning a lease and releasing outer locks.

| Progress state | Request | Alarm consumer / retirement |
| --- | --- | --- |
| Dormant | Take one storage reference and append once as Enqueued | Retirement forbids new insertion |
| Enqueued | Coalesce; keep the existing reference and link | Pop transfers that reference to Running; retirement detaches it for unlocked release |
| Running | Set rerun; do not link or take another queue reference | Finish requeues the same reference if rerun, otherwise becomes Dormant; retirement prevents requeue |

On pop, clear rerun while changing Enqueued to Running under the ready lock.
The consumer then unlocks, upgrades the weak runtime identity, validates the
current timer and atomically claims eligible demand under the timer lock.
Dispatch occurs after unlocking. On finishing, it takes the ready lock again:
a request concurrent with processing either set rerun before this check or
observes Dormant afterwards and enqueues anew. Thus no request is lost between
the final check and sleeping. A busy signal lease is not itself a reason to
requeue: leave demand with the timer and rely on that lease's return request.
No-demand and dead-target visits terminate without self-rescheduling.

The consumer processes a bounded number of entries per turn, then services
ordinary deadlines and yields as required by the task executor. Register the
wake listener before checking ready-list and deadline state. Notify only after
unlocking. Each Enqueued/Running entry retains record storage, not a strong
runtime reference. Deletion marks retirement under the record lock before
removing Enqueued work under the ready lock; Running work retains its reference
until it observes retirement and releases it outside locks. No teardown waits
for the alarm task while holding a subsystem lock. Failed runtime upgrade
terminates work; it cannot route through a reused numeric PID.

This allows already accumulated CPU-timer demand to progress without another
user-mode return; it does not replace CPU-time sampling or imply that the alarm
task can infer unsampled execution time. Do not reuse `enqueue_alarm` for this
purpose: it allocates an `Arc<AlarmTicket>` and may grow a heap. The actual wake
primitive, executor fairness and allocator context still need source-level
validation before implementation; the table specifies the required protocol,
not guarantees of the current alarm task.

Deletion and exec cleanup mark records retired before removing timer-map
ownership, then detach queued/ignored records outside the timer lock. They do
not wait under SigLock for dequeuers. Outstanding storage references protect
memory without keeping the timer logically valid. Signal target exit/slot
reuse rejects the old routing identity, drains its records, and resolves their
leases without scheduling work to a dead target. Process runtime teardown
drains pending/ignored records and releases owner references without a cycle.

Do not use the truncated u32 siginfo sequence as the sole ABA defense.
Keep record identity plus the checked internal generation independent of the
user ABI. If generation cannot advance, reject rearm before mutation rather
than aliasing an old lease; deletion/retirement must remain possible.

### Queue Semantics and Scope Gate

Ordinary standard sends still coalesce while the signo is pending. Prepared
timer records do not collapse across different timers or disappear behind an
ordinary pending instance. Adopt Linux's aggregate pending-bit semantics, not
an independently counted ordinary no-info instance. A dequeue returns the
first matching record in enqueue order and retains the bit only when matching
records remain. With no matching record, a set standard bit yields the ordinary
zeroed fallback and is cleared. An ordinary OOM followed by a timer enqueue
does not promise an extra fallback after that timer is consumed or cancelled.
A detailed ordinary record followed by timer records remains ordered and is
not overwritten. Same-timer repeated expiry before acknowledgement updates
overrun; after the accounting cut it becomes deferred new-notification demand.

Realtime ordinary and timer notifications require one enqueue order for the
same signo. A separate timer list drained before/after the existing VecDeque
would reorder them; it is not an acceptable shortcut. The recommended endpoint
is one intrusive pending-record order, distinguishing ordinary owned records
from reusable timer records. This necessarily affects the existing realtime
queue representation and normal realtime allocation/admission errors.

That is a separately scoped signal-queue redesign, NOT authorization to sweep
Vec users or silently add a new EAGAIN path in this KBox batch. Before code,
approve that scope and specify realtime send accounting/error contracts. If
the existing realtime container must remain untouched, keep this proposal at
design stage instead of claiming a complete Linux-like timer implementation.

Ignored records follow the transitions below under SigLock and the leaf record
lock. Record metadata includes the lease generation and a periodic snapshot so
this decision never requires the timer-manager lock. All retained transfers
preserve the storage reference and unacknowledged accounting; none acknowledges
delivery. An invalidated generation takes cancellation instead of retention.

| Event | Valid periodic notification | Valid one-shot notification |
| --- | --- | --- |
| Sending finds ignored and unblocked disposition | Retain as Ignored | Complete as IgnoredDiscard; never replay on later unignore |
| Disposition flush removes Queued notification | Transfer Queued -> Ignored | Complete as IgnoredDiscard |
| Ignored disposition but delivery rules require queuing because blocked | Queue, or transfer Ignored -> Queued | Queue |
| Disposition becomes deliverable for retained record | Transfer Ignored -> Queued after target revalidation | No retained record exists |
| Delete, rearm invalidation or target teardown | Cancel, never retain | Cancel |

Do not treat a disposition flush as target teardown. A queued ignored signal
that becomes unblocked follows the common disposition handling path; it must
not accidentally be validated as delivered merely because it was dequeued.
Disposition mutation, its queue/ignored-list changes and pending projections
must be serialized against timer admission. Extend the existing action-owner
locking protocol rather than sampling disposition and later submitting under
an unrelated lock. Wakeups and discard completions occur only after unlocking.
IgnoredDiscard closes that notification without updating last-delivered
overrun; only fresh demand may cause another notification. All signal consumers
use the same detached receipt path. Replace signo-based SI_TIMER observer
inference with explicit prepared
record provenance, so user-supplied siginfo cannot acknowledge a kernel timer
and a timer using a different standard signo is not missed by registration.

### Source Verification and Remaining Prerequisites

The original checks used base `968415963b5a` and locked event-listener 5.4.1.
The follow-up below includes KArc commit `96f160953` and the installed Rust
1.95.0 and linked_list_r4l 0.3.0 sources. These are implementation-constraint
checks, not a compiled implementation of this timer protocol.

**Shared storage:** `mm/kalloc/src/lib.rs::GlobalAllocator::alloc` returns a
recoverable allocation error, but its `GlobalAlloc::alloc` adapter calls
`handle_alloc_error` on failure. Therefore `Arc::try_new` with Global is not a
recoverable reservation in this kernel. A policy-specific Allocator supplied
to `Arc::try_new_in` must reach the fallible allocator entry directly, retain
the original release authority and use the matching deallocator. The allocator
policy belongs in the allocation layer, not a second private heap backend in
ksignal. This prerequisite is now implemented by `kalloc_types::KArc`: its
private SharedAllocator delegates to KBox's fallible backend, preserves the
matching release operation, and lets standard Arc manage strong/weak counts.
Use `KArc::new` only at the audited pre-lock admission point; use KWeak only
for objects actually owned by KArc. Existing process runtimes still use their
existing weak-owner type; this is not authorization to convert them.
Clone, downgrade and upgrade need no new storage, but last-strong payload Drop
and last-weak deallocation still require valid contexts. Existing
`kfinalize::service::IdentityAllocator` demonstrates retaining an independent
release token, but its default provider and the installed kworkng provider
allocate through Global. Neither provider can be reused as evidence of
recoverable timer admission. No kfinalize or dynamic-worker change is proposed.

**Intrusive owner transfer:** KArc now provides as_ptr/into_raw/from_raw with a
same-type, exactly-once strong-reference transfer contract, not standard-Arc
interoperability. Use intrusive_collections with an ownership-preserving
PointerOps adapter; linked_list_r4l is no longer an allowed implementation.
Standard allocator-aware Arc instead exposes
`into_raw_with_allocator` / `from_raw_in`, retaining allocator authority as well
as the pointer. A standard Global Arc adapter cannot be applied to KArc by
casting or by reconstructing a Global Arc.

Do not leak a cloned KArc, fabricate a static borrow, or depend on timer-map
ownership to keep a queued node alive: delete/exec may remove that owner while
a queue or receipt still exists. Likewise, do not introduce a self-retaining
record solely to bypass the missing interface; that adds cycle/drain failure
modes. kalloc_types::KArcPointerOps now provides only pointer conversion for an
intrusive_collections adapter, with the allocator backend kept private. Use the
library's list rather than another generic queue. Hosted tests cover reference
transfer, known-member removal, duplicate-insertion panic recovery and independent
links. Concrete timer integration must establish membership, payload lease
completion, generation uniqueness, teardown, lock ordering and release contexts.

For the current implementation, retaining the original flags is not itself
necessary for release: SharedAllocator uses them only during allocation, and
deallocation always selects the same build-time kernel/host backend. Its test
failure flag likewise affects allocation only. KArc's raw-owner interface now
reconstructs an equivalent release allocator without adding record-header
storage. Retain that invariant explicitly rather than assuming
all allocator-aware Arcs need a per-pointer token, or dropping allocator state
without examining it. Such an interface must consume exactly one strong
reference, preserve pointer provenance and alignment, and reject mixing with
standard Arc or KBox pointers by its unsafe contract. It would also constrain
future per-arena or stateful allocator extensions; those must revisit the raw
representation. Hosted intrusive_collections probes verify duplicate-insertion
rejection, removal, independent links and automatic list teardown. They do not
implement a production timer synchronization protocol; timer integration remains
a separate step.

**Wake source (original audit):** Reusing `EVENT_NEW_TIMER.notify(1)` did not establish an
allocation-free progress request. In event-listener 5.4.1, `Event::notify`
calls `inner()`, whose first use allocates an Arc. More importantly, the no_std
slab backend's contended `Inner::notify` pushes into an unbounded concurrent
queue, which allocates blocks. Warming up the event or using the stack listener
macro does not remove this second path. The implemented alarm-rescan wake above
now removes that Event from the production new-alarm path. Future record progress
still needs its own authoritative ready-list predicate; a rescan request alone
is not a queued record or a completed lease.

Use the existing `futures_util::task::AtomicWaker` rather than building another
wake registration primitive. The locked futures-core 0.3.34 implementation
stores one Waker inline; `register` and `wake` perform no container allocation.
They still invoke the supplied Waker's clone/drop/wake operations, so this
choice is valid only with the audited alarm task's task-bound Waker, not an
arbitrary callback. Unlike a permanent cached Waker, `wake` takes the stored
registration: the consumer must register on every poll, not just once.

The progress future registers first, then checks the ready-list predicate
under its lock. A producer publishes to that list, unlocks, and calls `wake`.
A request before registration is found by the predicate check; a request
after an empty check wakes the registered consumer. Repeated wakes may
coalesce because list membership, not wake count, records work. The consumer
must include this future alongside deadlines and clock-change listeners in
its wait selection; waking a task whose selected futures never check the
ready predicate is not sufficient. Dropping/recreating that wait selection
must preserve the global ready predicate and re-register before rechecking.
No dynamic listener, callback owner or worker is created per progress request.

Acceptance tests must exercise publication before first poll, publication
between registration and checking, wake after an empty check, repeated wakes
after registration is consumed, and a ready entry while every other wait
branch stays Pending. A bounded ready batch must also leave a deadline turn
and an executor yield opportunity; a permanently ready progress branch must
not monopolize `select_biased!`. This specifies the future integration, not
an independently useful wrapper to add before it has a producer.

`task/ktask/src/future/mod.rs::KWaker` is allocated once by `block_on` and its
wake method upgrades a weak task reference, sets the parking hint and unblocks
the task. These operations do not allocate a new waker. This supports using a
prepared task-bound waker, not arbitrary user-provided Wakers. The scheduler,
remote reschedule and enabled instrumentation paths still require a complete
context audit before claiming end-to-end allocation-free wakeup. Source tracing
reaches `WakeRunQueueRef::unblock_task`, intrusive scheduler insertion, and
`RunQueue::kick`; remote rescheduling uses the existing fixed notification
path rather than allocating a callback. `register_sched_trace_hooks` explicitly
requires allocation-free wake callbacks. These are supporting contracts, not
allocation-instrumented test results for every enabled trace/log provider.
Do not broaden the guarantee to arbitrary Wakers or NMI. Bounded ready
batches must explicitly yield/service deadlines: the existing alarm loop's
immediate `continue` on expired entries is not a fairness guarantee.

**Disposition synchronization:** `ProcessSignalManager::fork_shared` shares
SignalHandlers between distinct process managers, each with its own owner
lock and SigLock. Send currently drops the shared actions lock before entering
SigLock; action setters update only the action table, and `set_blocked` only
changes the slot mask/projection. These methods do not implement the retained
timer transitions required above. Holding one process's owner lock cannot
serialize another process's shared-action update. Prepared admission must
serialize the action decision with queue publication using the shared action
lock (with a documented actions -> SigLock order), not only the local owner
lock. No code may introduce a reverse SigLock -> actions acquisition.

Follow-up verification of Linux `8d3ae59288f1` narrows the mutation scope:
`copy_sighand` shares the action table for CLONE_SIGHAND, whereas
`do_sigaction` flushes only the caller's `signal->shared_pending` and its
thread group's private queues. Its unignore path likewise calls
`posixtimer_sig_unignore` for the caller's signal group. Shared locking does
not imply an eager walk of every separate group sharing the action table.
The earlier requirement for a registry of all sharing process managers was
too broad and is withdrawn. Match this caller-group scope; do not add a
global owner registry or promise immediate reevaluation of peer groups.

The concrete lock order is the calling manager's ownership lock, shared
actions lock, that manager's SigLock, then the record leaf lock. An action
setter updates the table and flushes/requeues the calling group's records
before releasing the actions lock. Prepared send holds that same actions
lock through admission into its destination's SigLock, so a peer setter is
ordered before or after that decision. No transaction acquires a peer's
ownership lock or a second SigLock. Extracted owners, completions and wakeups
leave all of these locks before execution. All setters, including the
one-shot handler reset, must share the same internal mutation path.

Implemented so far: process/private send admission holds the shared actions
guard through the SigLock transaction, and both action setters use
replace_signal_action. Guard release precedes arrival callbacks and rejected
ownership destruction. The setter now flushes/requeues the caller group's
records. Normal-handler dequeue and blocked-mask replay cover peer action
changes without a registry of all sharing managers.

Installing an ignored disposition flushes existing records even when blocked;
this is distinct from a later send to an already ignored but blocked signal,
which may queue. A same-value action installation must still perform the
required flush. Restoring a disposition can requeue retained periodic records
for the calling group, but cannot replay an ignored one-shot. Exec detaches
its shared action configuration using the existing ownership transaction and
must not flush peer pending queues. Tests must include two managers sharing
actions but owning independent pending state, both orders of send versus
setter, same-value ignore, and exec detachment. These are design requirements,
not claims that the current setter already performs these transitions.

**Realtime admission:** Linux `__send_signal_locked` does not return EAGAIN
for every failed realtime allocation. It rejects when siginfo is not a
special sentinel and its code is not SI_USER; kill-like delivery instead
publishes a pending bit without a detailed record. X-Kernel currently passes
`SignalInfo` through common process/thread send helpers and its leaf queue
returns bool, not an admission error. `sys_rt_sigqueueinfo` and
`sys_rt_tgsigqueueinfo` already return KResult through the bridge, but that
does not mean the leaf queue reports allocation failure today.

Before replacing realtime VecDeque storage, specify an explicit internal
send policy at the validated sender, rather than treating user-supplied
SI_TIMER as a prepared-record capability. Distinguish ordinary bit-preserving
fallback, fallible queued admission, and a unique prepared timer lease.
Queued admission failure must leave the bit, FIFO and wake projections
unchanged; bit-preserving fallback must be consumable even for realtime
signos if that Linux behavior is adopted. This extends the previous
standard-only fallback scope and needs its own tests and errno mapping.
RLIMIT_SIGPENDING accounting is separate and must not be claimed merely
because an allocation error maps to EAGAIN. Until this contract is implemented
through every sender, leave production realtime storage unchanged.

**Minimum scope:** Record reservation and reuse span kalloc-types, ksignal,
ktimer and the kprocess bridge, plus syscall rollback/error tests. Realtime
FIFO integration and caller-group disposition reevaluation are signal-subsystem
changes, not mechanical Box replacements. Preserve this scope gate until their
admission errors, accounting and membership protocols are approved. Do not
expand into unrelated Vec users, kfinalize, dynamic workers or stopper tasks.

### Review and Test Gates

The following deterministic interleavings are required acceptance cases, not
timing-dependent stress tests alone:

| Interleaving | Required outcome |
| --- | --- |
| Ordinary owner OOM; timer with same signo enqueues; dequeue last record | One timer receipt, bit clear, no invented fallback |
| Detailed ordinary record; two same-signo timers | Three records in enqueue order; ordinary duplicates still coalesce |
| Periodic record queued; disposition becomes ignored; restored before next expiry | Retained record becomes pending again without allocation |
| Ignored one-shot expires; disposition restored | No replay of the discarded one-shot |
| Validate delivery; pause receipt return; same-generation expiry; return receipt | Frozen old overrun unchanged; new demand dispatched once |
| Rearm while old receipt drains; new one-shot expires; old completion runs | Old completion cannot acknowledge or erase new demand |
| Old cancellation delayed until new generation queues | Identity/generation check leaves new record linked |
| Ready consumer runs; request races its final check | Rerun or a new Enqueued entry, never lost demand or duplicate links |
| Delete/exec races Enqueued or Running progress | No new delivery after invalidation wins; storage released outside locks |

The last two cases must include final weak-reference release and failed runtime
upgrade. Instrument allocation and reclamation contexts, not just final counts.

1. Approve the cross-crate queue scope, KArc intrusive ownership adapter,
   caller-group disposition reevaluation, ignored-list lifecycle and the
   dedicated prepared wake mechanism before implementation. Fault injection
   must include first wake, contended wake and shared-handler mutation, not
   only the timer record allocator.
2. Fault-inject creation reservation and every pre-publication rejection. Verify
   no timer/record/ID/queue publication and successful retry; test output-copy
   rollback through the syscall adapter.
3. Verify zero record allocation after admission, one active lease/link per
   timer, same-timer overrun accumulation and distinct-timer same-signo delivery.
4. Exercise each race: sending versus delete/rearm, queued versus cancellation,
   dequeue versus rearm, completion versus new one-shot expiry, and target
   exit/slot reuse. Test process teardown with outstanding receipts and absence
   of reference cycles. No stale completion may acknowledge a newer generation.
5. Cover ignored/unignored and blocked/unblocked timers, normal-handler,
   signalfd and sigtimedwait delivery, mixed realtime FIFO order, saturated
   overrun, timer ID reuse and internal generation exhaustion.
6. Keep legacy itimers separate. Then enable ordinary standard-signal KBox
   fallback and prove it cannot consume a prepared timer notification as SI_USER.
   Run four-architecture build/Clippy and timer/signal/process/consumer tests.

This draft does not claim these tests have run, or that a one-crate Box
replacement can satisfy the protocol. Remaining gates are concrete design
decisions, not reasons to add infallible allocation or NOFAIL retries.

## Purpose

`ktimer` owns the process-shared `setitimer` and POSIX timer state and
converts expirations into `TimerDelivery` values. `kprocess` embeds
`ProcessTimerManager` per process runtime, polls it from the user-return
path, converts deliveries into `ksignal` notifications, and registers the
dequeue-validation callback; `ksyscall` owns ABI parsing (timespec
conversion, `timer_create`/`settime` argument decode). Wall-clock wakeup is
provided by the crate's global alarm task.

## Scope

```text
process/ktimer/src/
├── lib.rs           re-exports (TimerDelivery, TimerSignal, ProcessTimerManager, notify types)
├── delivery.rs      TimerDelivery / TimerSignal descriptions
├── interval_timer.rs ITimer state: interval, deadline, missed-period advance, alarm arming
├── posix_timer.rs   POSIX clock selection, absolute/relative deadlines, overrun bookkeeping
├── manager.rs       per-process timer set and poll entry points
└── runtime.rs       per-domain alarm heaps and alarm task
```

## Architecture

```text
syscall ABI -> TimeSpan/SystemTime conversions (ksyscall)
                    |
           ProcessTimerManager  (Arc<Mutex<..>> in kprocess runtime state)
             |              |
          ITimer[3]      BTreeMap<i32, PosixTimer>
             \              /
        TimerInstant deadlines (Realtime|Monotonic|Boottime|ProcessCpu)
                      |
     runtime_deadline -> per-domain alarm heaps (TimerInstant, Pid)
                      |
        alarm task -> expired-owner callback -> kprocess poll
                      |
                 TimerDelivery -> ksignal SignalInfo
```

`TimerInstant` is a crate-internal closed set distinguishing realtime,
monotonic, boottime, and process-CPU clocks. `ITimer` stores a `TimeSpan`
interval and an `Option<TimerInstant>` deadline — never bare unitless
integers. POSIX timers reuse `ITimer` as their specification and add
notification and overrun state.

For explicit POSIX timer notifications, `ksyscall` copies the complete
`sigevent` and converts its `sigval` to pointer-sized opaque bits. `ktimer`
keeps those bits in `TimerSigValue` and `TimerSignal`; `ksignal` builds the
outbound `siginfo` union using its full-width pointer member.

## Execution context

- `ProcessTimerManager` is serialized by the owning `ProcessRuntime`'s
  `Arc<Mutex<..>>` (`kprocess::process_runtime`). Its methods run on
  syscall and user-return paths; they do not sleep.
- `poll_cpu_timers` relies on caller-supplied totals for the same process.
  `ITIMER_VIRTUAL` uses user time, `ITIMER_PROF` uses user plus system, and
  POSIX process-CPU timers use scheduler runtime. CPU timers are polled at
  user return; a periodic timer IRQ gives a busy userspace task return points.
- The alarm task (`spawn_alarm_task`) is a sleepable kernel task using
  `timeout_at` on the current conversion of the earliest deadline; the expired-owner
  callback re-enters `kprocess` polling and may therefore block on the
  process runtime mutex.
- `register_expired_task_handler`/`spawn_alarm_task` are one-time
  bring-up calls.

## State machine

A disarmed timer has no deadline. Arming with a nonzero value enters the
armed state. On expiration a one-shot timer returns to disarmed; a
periodic timer advances from the *old* deadline by an integer number of
intervals (drift-free), saturating at the clock maximum. A non-
representable nonzero deadline is rejected with an error *before* any
state changes — it is never silently treated as a disarm request.
`settime` on a POSIX timer resets pending-signal and overrun state.

POSIX timer notifications add a generation counter (`signal_seq`), bumped
on every re-arm: a dequeued signal whose sequence no longer matches is
dropped by `on_timer_signal_dequeued`.

Expirations are collected by polling (the alarm task or the CPU-timer poll)
while the notification is dequeued on the signal path, so `on_timer_signal_dequeued`
catches the timer up to the delivery instant before it freezes the overrun
count: intervals that elapse between the last collection and the delivery
belong to the notification being delivered, and only intervals after it
produce the next notification.

A collected expiration count saturates when narrowed for the overrun counters,
and the counters saturate on accumulation; `i32::MAX` clamping happens only at
`timer_getoverrun()`. The delivered signal carries the count captured when the
notification was generated (`siginfo.si_overrun`, and `ssi_overrun` for
signalfd readers), so it can differ from `timer_getoverrun()`, which reports the
count frozen at delivery. This differs from the referenced Linux implementation,
which updates the delivered `si_overrun` during timer delivery. The prepared
record proposal preserves the current XKernel behavior; changing that ABI
behavior requires a separate decision and tests.

## Algorithms

- Missed-period advance: `skipped = overdue / interval + 1`; the deadline
  moves `skipped * interval` forward from the previous deadline, and the
  expiration count is clamped to `usize::MAX`.
- Overrun reporting: `queued_overrun` accumulates while a signal is
  pending; `last_overrun` is frozen only when that signal is dequeued
  (POSIX `timer_getoverrun` semantics); reads clamp to `i32::MAX`.
- Alarm queues: one BinaryHeap per wall-clock domain, ordered by original
  deadline then pid. A new earliest deadline requests an AtomicWaker-backed
  rescan after unlocking. The worker converts each domain's head to monotonic
  when selecting the next wait, retaining original deadlines so clock changes
  can invalidate conversions without reordering heap contents.
- Timer ID allocation: monotonically increasing with wraparound at
  `i32::MAX` back to 1, skipping live IDs; exhaustion returns `EINVAL`.
  Internal u64 notification generations never wrap; exhaustion returns EINVAL
  before creation or settime mutation, without preventing deletion or cleanup.

## Concurrency

Per-process state is externally serialized by the runtime mutex. The
global alarm heap is a `ksync::Mutex`; heap push/peek/pop are short
critical sections. Stale heap entries (timer rearmed or deleted) are
harmless: the poll path re-reads timer state and the `signal_seq` filter
drops notifications from an older generation. Multiple enqueues for the
same timer are expected — each `arm_deadline` supersedes earlier entries
observationally, and the task re-checks the head deadline.

## Design decisions

- Deadlines across different clocks are an enum, not shared nanosecond
  scalars, so cross-domain comparisons surface as explicit mismatches
  (panics are confined to internal invariants, never user input).
- Each alarm queue holds instants of its own clock domain; conversion to
  monotonic happens when the task compares heap heads.
- `snapshot` receives the clock-domain "now" and returns interval and
  remaining `TimeSpan` only.
- `SIGEV_NONE` timers track expirations with no notification at all
  (overrun still counted).
- `set_posix_timer` may return an immediate delivery when the timer
  already expired at arm time, avoiding a lost first expiration.

## Resource lifecycle

Timers are created/deleted through the manager (`clear_posix_timers` on
exec). Removal transfers timer ownership for destruction outside the manager
lock. Heap entries retain a deadline, PID and alarm ticket, not the timer owner;
they are removed by expiry and can cause obsolete owner polls. No explicit
heap-entry removal API exists.
`ProcessTimerManager` is dropped with its owning process runtime.

## Kernel unit-test layout

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/interval_timer.rs`: mounted by `src/interval_timer.rs` as `tests`.
- `ut-tests/posix_timer.rs`: mounted by `src/posix_timer.rs` as `tests`.
- `ut-tests/manager.rs`: mounted by `src/manager.rs` as `tests`.
- `ut-tests/runtime.rs`: mounted by `src/runtime.rs` as `tests`.

### Preserve alarm clock domains

The alarm queues maintain separate min-heaps for MONOTONIC, REALTIME and BOOTTIME,
ordered within their original domains. The background task compares current
monotonic conversions of the three heap heads and listens for new timers and both
clock-change events. Changes trigger a new head comparison rather than permanently
ordering deadlines with stale offsets. Relative REALTIME arming uses MONOTONIC;
rearming snapshots the old domain before replacing the setting and updating
`armed_clock`. ITIMER_REAL and CPU timers retain their existing semantics.

## Consumed alarm tickets

Each queued wall-clock alarm owns a ticket. Dispatch consumes it before owner
rechecking. A rollback can make that recheck find no expiration; the owner still
rearms if the previous ticket was consumed. An unchanged live ticket suppresses
duplicate entries. Replacing a POSIX timer snapshots the old clock domain before
replacing its spec, avoiding old-deadline interpretation in the new domain.
