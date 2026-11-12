# ksignal design

## Implemented: Realtime Admission Errors

Ordinary non-SI_USER realtime owner exhaustion returns WouldBlock (EAGAIN).
PendingSignals and SigTxn return KResult without changing queue membership or
pending projections on error. Process/thread send_signal return KResult of the
existing wake hint; kprocess propagates the error before task interruption.
The syscall adapters already propagate these results, preserving validation
order and signal-zero handling. TimerSignalSendOutcome::QueueFull represents
ordinary failure with no wake or retained ownership. Prepared leases cannot
reach it because their submission requires no owner allocation.

Disposition checks precede allocation. Fatal epochs are published only after
successful admission, inside the same SigLock transaction. Failure does not
publish pending_changed notifications. Failed ordinary nodes own only plain
siginfo; destruction under the lock releases no timer or runtime references.

SignalNotification::kernel_fallback explicitly permits loss of details for
audited kernel sends: current-kernel signal delivery and fatal trap delivery.
Child-exit notifications retain ordinary admission. Userspace conversion never gains this
policy from SI_KERNEL or another ABI code. This is not NOFAIL allocation:
NOWAIT may fail and delivery then retains only an aggregate pending bit.

Five deterministic regressions cover code-dependent rejection with an existing
FIFO, explicit kernel fallback, shared failure without fatal epoch/wake changes,
ignored admission without allocation, and blocked/unblocked private failures
followed by successful retry. The private seam replaces only the ordinary
allocator in the production send algorithm. No resource-limit accounting,
syscall-level OOM injection or complete memory-pressure safety is claimed.

## Implemented: Realtime SI_USER Allocation Fallback

Linux `kernel/signal.c::__send_signal_locked` does not return EAGAIN for
ordinary SI_USER owner allocation failure, even for a realtime signo. The
shared enqueue algorithm now accepts that failure with a pending bit and the
shared dequeue algorithm synthesizes zeroed SI_USER when no record exists.
The receipt check precedes this policy: prepared timer ownership never degrades.

Realtime admission does not use standard-signal coalescing: every SI_USER send
still attempts NOWAIT allocation. Multiple failed attempts leave one aggregate
bit. Detailed records before or after a failed attempt retain FIFO behavior;
consuming the final record clears the bit without an extra synthesized event.
Two deterministic allocator-seam tests cover repeated failures, blocked-mask
visibility, no-info dequeue and both orders of mixed detailed/bit-only arrivals.
These tests do not inject failure through a syscall or the global allocator.

This is the kill-compatible branch. Other ordinary realtime codes return EAGAIN
unless an audited kernel sender explicitly permits fallback. Fatal epoch
publication follows successful admission under SigLock.

## Implemented: Standard-Signal Allocation Fallback

Ordinary standard signals now allocate detailed KArc storage with NOWAIT only
after coalescing. Failure publishes the existing pending bit without an owner;
ordinary SIGKILL takes that no-allocation path directly. Duplicate standard
sends never retry allocation or upgrade lost information. The API still reports
successful admission, so existing SigTxn dirty tracking and unlocked wake paths
remain in use. Realtime SI_USER uses the fallback described above; other
realtime owner failure returns EAGAIN unless explicit kernel fallback applies.

DetachedNotification carries either a stored owner or an inline signo. A
record-less dequeue synthesizes zero-initialized SI_USER, errno zero,
and no sender/timer payload through SignalInfo::without_sender. Normal handlers
and explicit-mask consumers use the same unlocked completion path. The inline
variant has no receipt and cannot acknowledge a timer. Prepared records never
enter the ordinary allocation/fallback branch, even for SIGKILL; an ABI SI_TIMER
payload without provenance remains ordinary data.

The bitmap is aggregate state, not an extra fallback count: when detailed timer
records follow a degraded ordinary arrival, consuming or cancelling the last
matching record clears the bit without inventing another occurrence. Cancelling
an ignored or detached timer must not clear an unrelated no-info pending bit;
cancellation changes the bitmap only after actually removing a pending node.

Tests inject failure only at the private ordinary-node allocator seam used by
production admission. Coverage includes no-info consumption, coalescing without
retry despite later allocation availability, SIGKILL's allocation-free path,
manager admission/dequeue, normal-handler projection, forged timer data,
prepared records after OOM, and ignored cancellation preserving a fallback bit.
These are not global OOM or syscall/signalfd end-to-end failure-injection tests.

## Implemented: Ignored Periodic Notifications

Each PendingSignals owns an additional intrusive ignored list, using the same
pending link exclusively. A prepared receipt snapshots whether its timer is
periodic. Ignored/unblocked periodic sends return Retained, keep their unique
lease occupied, and publish neither pending bits nor wake hints. One-shots and
ordinary ignored sends are discarded without completion. Blocked sends queue.

Action replacement holds ownership -> shared actions -> SigLock. Installing an
ignored disposition flushes the caller group's private/shared queues even for
blocked signals and same-value updates: valid periodic records move to ignored;
other owners move into an intrusive detached batch destroyed after unlocking.
Restoring a nonignored disposition requeues valid retained records for live
targets. CLONE_SIGHAND peers share serialization, not queue ownership. Normal
handler dequeue hides currently ignored unblocked records before completion;
explicit-mask consumption still acknowledges blocked ignored notifications.
Mask replacement, sigreturn and handler-mask additions replay retained records
that become blocked, allowing explicit-mask consumers to observe them.

Requeue uses the existing ready link to defer task wake to the alarm worker;
the worker validates exact timer storage and current target identity before
waking. SigLock -> ready -> record is the additional lock order, with no
callback under those locks. No additional notification allocation is needed.
Cancellation searches pending then ignored storage by exact identity and
generation. Drain includes both lists. Ignored scans and bulk action changes
are linear cold paths; enqueue and normal FIFO removal stay constant-time.

Deterministic tests cover periodic versus one-shot ignore, blocked flush,
same-value Ignore, mask replay, handler discard versus explicit consumption,
delayed cancellation after reuse, caller-group scope and exited targets. These
do not prove all SMP interleavings or ordinary allocation failure recovery.

## Implemented: Prepared Standard-Signal FIFO

Linux `kernel/signal.c::legacy_queue` coalesces ordinary standard sends whenever
the signo is pending. `posixtimer_queue_sigqueue` bypasses that test and appends
each timer's preallocated record; `collect_signal` keeps the bit set while another
record of that signo remains. X-Kernel now follows those admission and dequeue
rules with one intrusive FIFO per signo. Ordinary and prepared records retain
arrival order within a signo; the bitmap retains signal-number priority and
private-before-shared dequeue policy remains unchanged.

Only a live reserved lease permits the standard-signal bypass. Neither SI_TIMER
ABI data nor a test-only unreserved receipt grants it. One busy timer cannot
prepare a second notification, while distinct timers can independently queue,
complete, return storage and accumulate overruns. Ordinary sends still coalesce
even when only prepared records remain. Cancellation clears the bitmap only
after the last record is removed. Queue drain returns every lease exactly once
without acknowledging delivery. No enqueue/dequeue allocation is added for
prepared timers; ordinary realtime non-SI_USER failure now returns EAGAIN.

The unified 64-list layout replaces 32 optional standard slots and 33 realtime
lists. It consumes more inline queue metadata but no new heap allocation; enqueue
and dequeue remain O(1). Cancellation searches one FIFO in O(n), now also for
standard signals. Tests cover shared/private mixed FIFO ordering, two independent
standard timers, ordinary/forged-SI_TIMER coalescing, record reuse, cancellation
preserving another timer's bit, drain, and real process-timer dispatch/completion.
Ignored retention is described above; ordinary allocation failure remains separate.

## Implemented: Typed Timer Dequeue Provenance

`SignalNotification` wraps siginfo and an optional non-Clone
`TimerSignalReceipt`. Ordinary `From<SignalInfo>` never grants a receipt,
including for user-supplied SI_TIMER. The kernel timer bridge constructs the
typed notification with ID, sequence and a weak TimerSignalOrigin captured at
creation. The trait keeps ksignal independent of kprocess. Completion upgrades
the weak reference and consults that exact originating runtime, not the current
receiving task or PID lookup. An expired runtime rejects the notification. The
receipt owns no strong runtime reference. A prepared receipt's Drop releases
its unique storage lease and schedules availability without calling the origin.
Its weak reference can retain the Arc control block, which must be released
outside signal locks just like detached queue ownership.

Each signal uses an existing intrusive_collections LinkedList with AtomicLink
and KArcPointerOps. Ordinary
and prepared notifications share the same FIFO. Ordinary admission allocates
with NOWAIT; standard and realtime SI_USER failure preserve the bit, while other
realtime failure returns WouldBlock unless kernel fallback is explicit. Refcount overhead
is accepted. PreparedTimerRecord::new reserves one KArc with KERNEL permissions
before the timer lock, returning NoMemory on failure. Its nonallocating prepare
claims one lease and returns None while occupied, without overwriting payload.
`SigTxn` transfers storage into `DetachedSignal` while publishing pending
projections. Both handler dequeue and masked dequeue (signalfd/sigtimedwait)
call `DetachedSignal::complete` only after releasing SigLock. It consumes timer
authority first, discards stale records, then invokes ordinary observers.
The owning node travels intact across the transaction boundary. Enqueue removes
the receipt's lease owner before storing its payload, avoiding a self-cycle.
Unlocked dequeue/drain reconstructs that owner; completion retains occupancy
through the origin's accounting cut until receipt destruction. Ordinary final
node release also occurs unlocked. The completed `Dequeued` contains only siginfo and the
captured blocked mask, not queue ownership or timer completion authority.

Thread-manager destruction similarly detaches the entire ThreadSlot under
SigLock, then releases its KBox, private pending queue and projection reference
after unlocking. Detachment removes registration and trims empty trailing table
entries before release, so a successor can reuse the index while the old slot
owner remains alive. Stale handles cannot retire that successor. Shared pending
signals are unaffected. This adds no list or allocation and does not change
ordinary-send coalescing.

Slot generations now use a non-wrapping u64 source local to SigLock. Issuing the
last value exhausts that manager permanently, even after all slots retire and
the table shrinks. Registration returns InvalidInput on exhaustion without
changing slots, pending state or projections. The admission transaction returns
the untouched KBox owner on rejection; SigLock::register drops it after unlocking.
Allocation/flag errors still occur during preparation and consume no generation.
Existing handles remain usable, and retirement does not need a fresh generation.
This strengthens slot identity only within its owning manager; it does not
permit cross-manager use of a SlotId.
The signal-manager tests cover high-bit distinctions after index reuse, final
generation admission, rejected registration, preserved projections, retirement
after exhaustion and allocation-policy failure before admission.

Prepared leases return through a separate intrusive ready link. A short global
ready lock serializes membership and one runner, allowing one queued rerun per
record. Lease Drop releases occupancy and schedules the registered alarm wake;
it never calls the origin or takes a timer lock. Registration wakes preexisting
work. process_one_ready_timer_record pops one owner and passes a temporary
PreparedTimerRecord to record_available outside signal, ready and record locks.
The manager must check same_storage as well as timer ID before retrying demand.
Cloning this handle shares storage, never dequeue authority. Ready lock precedes
record lock; prepare and queue transfers take only the record lock. IRQ-safe
locks cover alarm delivery and receipt destruction contenders.

Prepared standard timers bypass ordinary coalescing using their unique reserved
lease, never an ABI SI_TIMER check. Availability enables deferred demand;
requeue wake shares that worker but does not release occupancy or acknowledge.
Dropping a detached notification cancels only its local ownership, never calls a
dequeue completion, and does not consume a subsequently queued notification.
Tests cover retained slot ownership after detachment, stale retirement after
index reuse, standard owner transfer, and discarding detached standard/realtime
timer provenance without acknowledging it or consuming its successor.

Send transactions also return unqueued ownership: `put_notification`, `put_thread`
and `put_shared` return None on insertion and the original notification on
standard coalescing or stale-slot rejection. Nonretained ignored sends carry ownership out
of the transaction as well. Process/thread send, exec-sweep kill and child-exit
publication retain this value until both SigLock and handler-ownership locks
are released. Missing configuration returns before moving the notification
into a transaction; declaration order releases the ownership guard first.
No destructor acknowledges a timer. The public wake hints, fatal bookkeeping
and wake-on-coalescing behavior are unchanged; an unqueued notification is not
interpreted as a new userspace error. Ordinary PendingSignals::put_signal still
returns bool because it cannot carry kernel timer authority.

Tests retry returned typed notifications after coalescing and stale-slot
rejection, checking no premature completion or successor publication. Public
send tests cover ignored, ignored-but-blocked and released destinations. These
retention tests are described above; allocator-destructor callbacks are not covered.

Timer completion no longer depends on a per-signo observer registration;
every timer notification carries its own provenance. The generic observer
interface remains available but is not the timer acknowledgement mechanism.
Coalesced, ignored or destroyed notifications still do not acknowledge timers:
retention/replay transfers the same lease without manufacturing completion.
Completion carries the manager's full non-wrapping u64 generation; siginfo
retains only the low 32 bits. The receipt test checks that high bits reach the
origin even when the ABI sequence has the same low bits as an older generation.

`ut-tests/timer_receipt.rs` exercises ordinary SI_TIMER isolation, all eligible
signal numbers through masked dequeue, normal-path rejection and FIFO,
coalescing and teardown. Completion reenters the signal manager to verify
lock release. These tests use local origin implementations, not a real timer
runtime or a userspace syscall harness. An expired-origin test verifies that a
queued notification does not retain the origin payload or fall back to the receiver.

## Intrusive ownership implementation scope

### Implemented Stable Timer Targets

Timer sends return TimerSignalSendOutcome rather than encoding both rejection
and blocked admission as a missing wake TID. Queued transfers ownership to the
pending queue; Rejected returns the original notification and a TargetGone,
Ignored or Coalesced reason. Coalescing retains the old wake hint, while blocked
admission is Queued with no wake hint. The signal transaction decides admission;
the bridge receives rejected ownership after signal and configuration locks
are released. Ordinary send wrappers still discard returned ownership outside
those locks and keep their old return types and wake behavior.

This ownership handoff also carries prepared leases: rejection does not
complete a timer. Retained denotes ignored periodic ownership without a wake.
Receipt tests cover both shared and private admission, retry after coalescing,
ignored-but-blocked admission, exited targets, released handlers and expired
managers. Retrying a returned notification preserves its dequeue authority.

TimerSignalTarget retains a weak ProcessSignalManager and an optional SlotId.
The creation adapter captures this before timer admission, without allocation.
Private sends upgrade the manager, then check slot generation and is_target in
the same SigLock transaction as enqueue and fatal bookkeeping. Shared sends
reject a manager with no live slots, but still queue when all live threads block
the signal. Target exit, slot reuse and expired managers never trigger numeric
lookup fallback. A private target follows its slot through TID rebinding.
Dispatch's selected TID is only a wake hint: matches_thread verifies the looked-
up task's manager/slot and liveness before the caller wakes the retained task.
Ordinary signal APIs keep their existing behavior; only timer sends request this
additional liveness check. Both send paths hold the shared action-table guard
through admission, in ownership -> actions -> SigLock -> record lock order.
A peer sharing CLONE_SIGHAND cannot replace a disposition between the ignored/
fatal decision and enqueue, even though its ownership lock is independent.
Both action setters use replace_signal_action. Arrival callbacks and rejected
owner release occur after all these guards are released. Caller-group flush and
retention/replay use that same transaction. Tests cover exit,
reuse, rebinding, unrelated managers, blocked shared sends and expired targets.
The shared-disposition admission test exercises both send routes and both
setters; arrival observers reenter action lookup after enqueue, checking that
callbacks run unlocked. It is not a concurrent interleaving test. Prepared
child-exit publication retains its separate prepare/commit policy contract.

Use the existing intrusive_collections list with KArcPointerOps from
kalloc-types (opt-in feature `intrusive-collections`). Pointer conversion
transfers ownership only; it adds no queue identity allocation, generic record,
generation field, cancellation API or custom list destructor.

Do not add a generic queue or per-queue identity allocation for this glue.
Concrete timer-record linkage and cancellation must be designed under the actual signal
locks: establish membership in the correct list, check notification generation,
transfer dequeued ownership to a receipt, and release final references outside
locks when required. is_linked alone is not a proof for raw cursor removal.
Library duplicate insertion panics; a subsystem must serialize and reject that
condition before insertion if its public contract requires recoverable rejection.
Library teardown is not a timer acknowledgement or an unlocked release protocol.

Production pending and ready queues now use the intrusive ownership adapter.
Typed provenance remains separate from the adapter's pointer conversion duties.

### Concrete Timer Membership and Cancellation Plan

Status: pending/ignored cancellation uses safe identity search. O(1) raw-cursor
removal and the explicit membership enum below remain proposals, not production.
Reservation, pending-node transfer, realtime FIFO and ready callbacks are
implemented as described above; this section proposes the additional protocol
without introducing a general-purpose queue interface.

Use the existing process signal manager as the routing identity, not the
address of a movable list and not an independently allocated queue token. A
prepared record retains a weak identity for its destination manager; a private
destination additionally names its registered `SlotId`. Resolving that weak
identity happens before taking signal locks. Retain the resulting strong owner
until after all signal/record locks have been released, including error paths.
Neither a numeric PID nor a slot index alone authorizes removal. The current
slot-generation source now rejects exhaustion without reuse. A long-lived timer
target now captures weak manager identity plus SlotId for POSIX dispatch. The
extended membership protocol below remains proposed.

An implemented cancellation token owns the exact record, captured target and
invalidated-through generation. Invalidation monotonically rejects old prepare
and admission. Admission checks inside SigLock before fatal/wake bookkeeping.
Token destruction runs outside the timer mutex, resolves the captured manager
and slot generation, and searches only that signo's queue for exact storage
identity. Cancellation is O(n) in that signo's FIFO, including standard signals,
on this cold path, without unsafe cursor reconstruction. This trades cancellation
latency for a smaller membership proof; enqueue/dequeue stay O(1). Pending bits
and dirty projections are updated under SigLock; detached owners drop afterward.
Delayed cancellation cannot remove newer receipts or unrelated owners. In-flight
sends either publish before cancellation and are removed, or reject invalidation.
Already detached receipts retain exclusive occupancy until their actual release;
cancellation never acknowledges delivery or manufactures another lease.

The concrete record's membership is one of Detached, SharedPending,
PrivatePending(slot), SharedIgnored or PrivateIgnored(slot), together with the
active lease generation. These are signal-domain states, not generic list
states. Only the destination's SigLock transaction changes membership or the
pending/ignored link. A leaf record lock can expose invalidation to the timer
side, but timer-side invalidation cannot change list membership. The separate
alarm progress link remains governed by the ready-list lock.

Cancellation takes a record handle and the captured lease generation, not a
bare pointer. Its transaction must perform the following checks in order:

1. Resolve the retained manager identity and, for a private destination,
   validate the registered slot while holding that manager's SigLock.
2. Under the leaf record lock, match the lease generation and exact membership
   for the selected destination/list. Detached, already-dequeued, stale and
   wrong-destination requests leave the list unchanged.
3. Only this successful match permits `cursor_mut_from_ptr` on that list.
   `is_linked` is insufficient: it cannot distinguish pending from ignored,
   a different slot, or a new lease of the same record.
4. Remove through the library cursor and transfer its owned reference to an
   unlocked completion receipt. Clear membership and update pending bits and
   dirty projections before publishing the transaction's result.
5. Release SigLock before completing the timer lease, requesting progress,
   waking a target or dropping the extracted record/manager owners.

The strong cancellation handle keeps the record address alive during the
lookup; it does not by itself prove list membership. Generation matching stops
an old cancellation from removing a newer lease at that same address. Sending
and Dequeuing are owned by their existing unique leases: cancellation marks
invalidation and lets those holders finish, rather than fabricating a second
completion receipt. A failed manager upgrade cannot be redirected through PID
lookup; target teardown must already own the corresponding queue drain.

Bulk teardown uses the library's `LinkedList::take()` while holding SigLock.
Before unlocking, mark each detached record as no longer belonging to the live
queue and republish its pending projections. The detached list keeps the same
link field occupied until each record is popped outside the lock; it must not
be treated as reusable Idle storage merely because membership is Detached.
Complete each popped lease before releasing its owner. This requires neither a
temporary Vec nor a second chain implementation. The linear walk during
teardown is intentional; ordinary single-record cancellation remains direct.

Source checks motivating this plan:

| Existing code | Integration consequence |
| --- | --- |
| `api/process.rs::fork_shared` shares handlers but constructs a distinct manager | Handler identity cannot stand in for pending-queue identity |
| `api/process.rs::send_signal` holds actions through SigLock | Admission serializes with shared action changes |
| `siglock.rs::SigInner::live_index` checks index and generation | Private cancellation must use slot validation, not TID lookup |
| `siglock.rs::DetachedSignal` transfers storage and typed provenance to unlocked completion | Extend this ownership transfer with the prepared-record lease; completed `Dequeued` remains ABI data only |

Required deterministic tests are wrong-manager cancellation, wrong-slot
cancellation, stale-generation cancellation after record reuse, dequeue racing
cancel, pending-to-ignored transfer racing cancel, and bulk detach followed by
delayed cancellation. Assert unchanged membership on rejection, exactly one
completion, and final strong/weak releases outside locks. These are planned
tests, not blanket concurrency results. Caller-group disposition reevaluation,
ignored retention and safe-search cancellation are implemented; the stronger
constant-time membership proposal is not.

The timer draft's follow-up Linux audit withdraws the proposed registry of all
managers sharing dispositions: action mutation flushes the caller's group,
while the shared actions lock serializes admission by peers. Reuse the
existing manager identity and ownership lock; do not turn shared handler
ownership into global pending-queue membership. The wake-path plan similarly
reuses futures-util AtomicWaker, with registration on every consumer poll.

## Ordinary Signal Payload Ownership

Status: ordinary fallback and realtime admission errors are implemented.
Pending storage uses KArc nodes and intrusive lists, with reserved timer leases.
The classification below defines the implemented policy; remaining validation
and resource-accounting limits are called out separately.

### Sender Classification and Error Propagation

Standard and realtime SI_USER fallback, realtime error propagation and explicit
kernel-only fallback are implemented.
Source audit covers the syscall adapters in `core/ksyscall/src/task/signal.rs`
and `pidfd.rs`, `process/kprocess/src/process_signals.rs`, timer delivery,
process exit, fatal delivery in posix-process, and terminal-generated signals.
Reference Linux is `/root/linux-stable` at `8d3ae59288f1`, specifically
`__send_signal_locked`, `do_tkill`, `do_send_specific`, `do_rt_sigqueueinfo`
and `collect_signal`.

Separate notification provenance from allocation-failure policy. A validated
ordinary siginfo may select a fallback/error policy, but even SI_TIMER with a
matching timer ID remains ordinary data. Only the unique prepared timer lease
can authorize timer acknowledgement. Do not let an ABI code or syscall name
construct that lease.

| Sender | Standard signal owner failure | Realtime owner failure |
| --- | --- | --- |
| `kill`, including group/broadcast, using SI_USER | Publish/coalesce the pending bit | Publish the bit without a detailed record |
| `tkill` / `tgkill`, using SI_TKILL | Publish/coalesce the pending bit | Return WouldBlock (EAGAIN); publish nothing |
| Validated `rt_sigqueueinfo` / `rt_tgsigqueueinfo` | Publish/coalesce the pending bit | SI_USER uses bit fallback; other admitted codes return WouldBlock |
| `pidfd_send_signal` with validated supplied siginfo | Same ordinary rule | Same code-dependent ordinary rule |
| Kernel-only bit-preserving delivery, including SIGKILL and legacy itimers | Preserve the signal; SIGKILL needs no owner | Must be an explicitly audited kernel sender, not inferred from user-controlled SI_KERNEL |
| Prepared POSIX timer lease | No owner allocation during submission | No owner allocation during submission |

The SI_TKILL distinction follows Linux source: `do_tkill` supplies a real
siginfo, not a special kernel sentinel, and the realtime allocation failure
branch rejects non-SI_USER codes. Do not infer identical resource semantics
for all three kill-family syscalls. A queued syscall using a standard signo
still follows standard coalescing, rather than requiring one record per send.

The current pidfd adapter always calls `make_queue_signal_info`; unlike the
Linux null-info branch, it does not separately synthesize kill siginfo for a
nonzero signal. Preserve that unrelated input-validation behavior in this
migration and track any compatibility correction separately. Signal-zero
probes do not allocate or enter the pending queue. Preserve existing target,
namespace, permission and payload checks before admission; this plan does not
claim those checks already match Linux in every case.

Ordinary admission runs under the destination SigLock, after its disposition
decision has been serialized. Check standard coalescing first, then perform
NOWAIT allocation only if detailed storage is needed. For error-returning
realtime admission, allocate before changing pending bits, list order, dirty
projections or selecting a wake action. Map storage NoMemory to the existing
`KError::WouldBlock` only at this signal-queue admission rule; do not change
KBox/KArc allocator errors or introduce a global NoMemory-to-EAGAIN mapping.
Ignored/drop and coalesced results are successful operations, not OOM errors.

The minimal propagation chain is:

1. Pending admission returns `KResult<bool>`; bool retains its current
   queue-change meaning, while an error means no publication.
2. `SigTxn::put_thread` / `put_shared` propagate the error and mark projections
   only for successful queue changes. A successful bit-only fallback counts
   as a change even without an allocated owner.
3. Process/thread manager sends return `KResult<Option<u32>>` / `KResult<bool>`
   respectively. These successful values still select wake behavior, not
   allocation success. Keep target disappearance semantics explicit rather
   than silently changing all existing false/None outcomes to ESRCH.
4. kprocess send helpers propagate queued admission errors before interrupting
   a task. Syscall wrappers already return KResult and must preserve those
   errors. Group sends retain their existing partial-delivery semantics.
5. Kernel senders that cannot report a recoverable queue error use the audited
   bit-preserving route. POSIX timers leave this ordinary route entirely;
   converting their failure into a bit-only fallback is forbidden.

For standard or realtime bit fallback, the bitmap is authoritative. With no
matching detailed record, dequeue clears that bit and synthesizes zeroed
SI_USER data. If detailed records exist, consume their FIFO first and retain
the bit only while more matching records remain; do not invent an extra
fallback occurrence after the final record. This is Linux's aggregate-bit
model, not a count of all arrivals lost under OOM. Unlike the initial
standard-only proposal, enabling realtime fallback requires changing the
realtime dequeue path at the same time as its admission path.

Non-SIGCHLD child-exit notifications may use a caller-selected realtime signo.
Their post-exit sender uses ordinary CLD_* admission. Realtime owner exhaustion
returns WouldBlock without publishing a pending bit; the sender logs the error
and independently notifies the parent's wait-state observers, matching Linux
do_notify_parent. Kernel origin alone does not authorize fallback. SIGCHLD's prepared exit decision is
not a prepared storage lease and does not authorize a timer completion.

Acceptance cases: inject ordinary owner failure for standard and realtime
kill, realtime tgkill, queued SI_QUEUE, admitted SI_USER, and a self-directed
user SI_TIMER payload. Verify errno, unchanged state on rejection, bit-only
fallback consumption, duplicate standard sends without allocation, and no
timer acknowledgement from user payloads. Run each applicable delivery case
through normal handlers, signalfd and sigtimedwait. Check child-exit wait
visibility independently of signal delivery. These tests must exercise the
production admission path, replacing only its allocator at a private seam.

### Implementation Slices

Completed foundations are typed dequeue provenance, unlocked completion,
reserved timer records, intrusive pending/ready links, the alarm progress
consumer and shared ordinary/timer FIFOs for every signo. Standard Box slots
and realtime VecDeque storage have been replaced by owning KArc intrusive lists.

Remaining work is ordered by observable contracts, not by crate count:

1. Validate ignored-record lifecycle under concurrent syscall stress beyond the
   implemented deterministic queue and disposition regressions.
2. Extend allocator-seam coverage to syscall-level memory-pressure validation;
   realtime fallback, error propagation and deterministic tests are implemented.

Each slice needs its own executable tests and synchronized security contracts.
The queue representation change is confined to pending signals, not a general
Vec migration. Per-user accounting, dynamic workers and unrelated timer heap
allocation remain outside this work. No slice may call itself complete based
only on this design document or on ownership-adapter tests.

### Linux Reference and Limits of the Analogy

Reference: local `/root/linux-stable`, commit `8d3ae59288f1`.

- `kernel/signal.c::__send_signal_locked` checks ordinary-signal coalescing
  before allocation, skips queue-record allocation for SIGKILL and kernel
  threads, and otherwise calls `sigqueue_alloc` with GFP_ATOMIC under siglock.
- Allocation failure for an ordinary standard signal loses detailed siginfo,
  not the pending signal. The out_set path still publishes the pending bit
  and performs notification. Certain realtime queued-signal failures instead
  return EAGAIN; that branch must not be generalized to standard signals.
- `kernel/signal.c::collect_signal` handles a pending bit without a queue
  record: it clears the output and synthesizes signo, errno zero, SI_USER,
  pid zero and uid zero. It does not return "no signal" in this state.
- POSIX timers use a separate prepared-record protocol. In
  `kernel/time/posix-timers.c::alloc_posix_timer`, the timer contains its sigq
  and initializes it before publication. `posixtimer_init_sigqueue` marks the
  record SIGQUEUE_PREALLOC; timer delivery/dequeue maintains its lifetime and
  acknowledgement instead of relying on ordinary-send allocation fallback.

X-Kernel NOWAIT is not Linux GFP_ATOMIC: it avoids sleeping reclaim for this
allocation but promises neither emergency reserves nor allocation success,
NMI safety, allocator reentrancy or bounded latency. The implemented realtime
FIFO and timer preallocation do not imply Linux user accounting or complete
Linux admission, cancellation and ignored-timer semantics.

### Ordinary Standard-Signal State Model

Keep `PendingSignals.set` authoritative. Standard fallback uses the
existing NOWAIT KArc owner allocation fallibly; placement composes with the
extended prepared timer queue below rather than retaining the single-owner slot as the
complete queue model. Do not add a second pending bitmap or allocate before
acquiring SigLock: coalesced ordinary arrivals must still allocate nothing.
The following table describes an ordinary-only queue; the mixed-record rules
following it describe the now-implemented standard-timer admission extension.
Ordinary standard fallback allocation and record-less pending state are implemented.

| Pending bit | Detailed owner | Meaning |
| --- | --- | --- |
| Clear | None | No pending instance |
| Set | Some | First pending instance with its original information |
| Set | None | Pending instance without information: deliberate fast path or owner OOM |
| Clear | Some | Invalid orphaned owner; not a state to publish |

Under the existing `SpinNoIrq` transaction, a duplicate standard signal leaves
the first instance unchanged, including a no-information instance. It must
not allocate to upgrade a degraded instance on a later send. For a new
ordinary instance, attempt NOWAIT owner allocation, then install the owner or
the no-information state and publish the pending bit before releasing the
transaction. SIGKILL should take the allocation-free no-information path.
The existing bool return keeps meaning "newly pending", not "owner allocated";
OOM fallback must return true so dirty projections are republished.

For a selected standard signo, dequeue consumes the first matching record in
enqueue order and retains the bit only if another matching record remains.
An ordinary record yields its payload; a timer record yields its typed receipt.
Only a set bit with no matching record produces the zeroed Linux-compatible
fallback on the stack and clears the bit. The fallback constructor belongs in
`SignalInfo`, alongside the existing
zero-initializing constructors. It must not impersonate SI_KERNEL, retain stale
sender data, or expose uninitialized union/padding bytes. No allocation occurs
on dequeue. Standard and ordinary realtime SI_USER admission may produce this
state; prepared receipts and other realtime codes must not silently degrade.

Synchronous memory faults use `SignalInfo::new_fault`, which populates the
`_sigfault` union arm with the faulting address exposed through `si_addr` and a
positive `SEGV_*`/`BUS_*` code. Generic kernel-originated delivery keeps
`new_kernel`/`SI_KERNEL` semantics and must not be used for faults, because
handlers that recover a fault rely on both `si_addr` and the precise code.

No independently counted no-information instance survives beside a detailed
record. As in Linux `collect_signal`, an ordinary owner OOM followed by a
prepared timer enqueue can be represented solely by the timer record and the
aggregate bit; consuming that last record clears the bit without synthesizing
another SI_USER notification. Conversely, an ordinary send after a timer made
the signo pending coalesces without allocation. A detailed ordinary record
queued before a timer remains in order ahead of it. Cancellation of the last
matching record also clears the bit: do not manufacture a fallback for a stale
timer. These rules avoid an unrepresentable ordinary no-info discriminator.

Slot/shared dirty tracking, release-before-wakeup ordering and pending-source
notifications remain under their current owners in `siglock.rs` and the
manager interfaces. Owner failure must not become a false dequeue result:
that would clear a pending bit while losing delivery and can leave a stale
availability projection. Blocking masks, private-before-shared precedence,
fatal-pending checks and coalescing retain their existing contracts.

### Timer Dependency Found During Review

Standard signal numbers do not imply that all siginfo fields are disposable.
Before typed provenance, `process/kprocess/src/timer_delivery.rs` built SI_TIMER
payloads for POSIX timers and its dequeue observer used timer_id and signal_seq to acknowledge
the matching timer generation. `process/ktimer/src/posix_timer.rs` sets
pending_signal before emitting a delivery; later expirations only accumulate
overruns until dequeue clears it. `PosixTimerNotify::Signal` accepts standard
as well as realtime signal numbers.

In the old observer path, converting a standard SI_TIMER payload to SI_USER
on owner OOM would return Deliver without acknowledging the timer. A periodic timer can remain
pending and stop emitting fresh notifications. A deleted/rearmed timer's stale
notification can also bypass sequence validation. This would be introduced by
the migration, not an acceptable loss of user-visible optional information.

Therefore do not implement universal fallback in the common put_signal path.
The preferred prerequisite follows Linux's ownership principle: prepare a
reusable notification record before timer publication, retain generation and
timer ownership through enqueue/dequeue, and never allocate that record during
expiration. Its concrete interface requires a separate cross-module design:

The [prepared timer record proposal](../../ktimer/docs/design.md#draft-prepared-posix-timer-notification-records)
defines the broader reservation, reuse and retirement protocol. Reservation,
unique receipt leases, unified per-signo queue ordering, one-record ready
progress, explicit cancellation and independent standard-timer admission are
implemented, along with ignored retention/replay. Pointer conversion alone
does not provide these lifecycle guarantees.

- Timer-independent signal-record ownership must live in ksignal or a
  cohesive shared type layer, not require ksignal to call process/timer code
  while holding SigLock;
- timer admission failure must precede timer ID/state publication;
- define idle, queued, dequeuing and retired record states, including deletion,
  rearm, stale generation, target exit, ignored signals and coalescing;
- define what acknowledges a timer when its notification cannot be newly
  queued, including two timers selecting the same standard signal;
- preserve unlocked observer execution and avoid timer-lock/SigLock inversion;
- do not reuse a record until all queue/dequeue references have relinquished it.

The implemented standard-timer bypass is gated by reserved storage, not SI_TIMER,
and has explicit mixed-queue and process-dispatch regressions. Ordinary allocation
failure still needs its own audit rather than inheriting an assumption from Linux; this is
still a partial migration, not completion of the pending-owner work.

### Rejected Shortcuts and Scope

- Returning ENOMEM/EAGAIN for every ordinary signal would alter signal-send
  semantics and can defeat fatal delivery. Losing the pending bit is worse.
- Retrying allocation while holding SigLock, using default KERNEL, or treating
  NOWAIT as NOFAIL is not permitted.
- Storing every full siginfo inline would remove this allocation but adds
  roughly 4 KiB of payload capacity to each 32-slot queue, before Option/layout
  overhead, for every thread and shared queue. That is a different memory
  trade-off requiring measurement, not the default migration strategy.
- A compact timer fallback token still needs lifetime, coalescing and sequence
  acknowledgement rules; it is not a substitute for the timer protocol.
- Realtime storage is intrusive and ordinary admission errors are propagated.
  RLIMIT_SIGPENDING accounting and unrelated Arc allocations remain outside
  this migration. Do not advertise end-to-end OOM safety.

### Validation and Implementation Gate

Standard fallback is enabled on the implemented shared nodes, following the
timer reservation and ignored-record lifecycle work. Realtime propagation is now implemented.
Production and tests must use the same enqueue/dequeue algorithms, with a
private seam replacing only the selected owner allocation.

Required coverage includes first-send OOM, successful payload retention,
duplicate sends without allocation (including degraded-first then successful
allocation availability), SIGKILL without allocation, blocked/unblocked state,
thread/shared projections, wakeup after lock release, private/shared dequeue
priority, and exact zeroed fallback fields. Verify the same fallback through
normal handler delivery, signalfd and rt_sigtimedwait, which share manager
dequeue and observer processing.

Timer tests must cover standard-number periodic notifications after injected
OOM, stale records after rearm/delete, overrun acknowledgement, competing
ordinary/timer sends and multiple timers sharing a signo. Successful realtime
delivery retains FIFO behavior; owner failure follows the admission policy
above. Validation requires four-architecture build/Clippy plus targeted ksignal,
timer, process and signal-consumer tests. The allocator-seam regressions do not
substitute for syscall-level memory-pressure or concurrency stress testing.

The process-handler ownership regressions live in `ut-tests/process_handlers.rs`,
mounted under `api::process::handler_tests` with `unittest::path_mod_test!`.
Only their module location changes; handler lifecycle and assertions do not.

## Purpose

`ksignal` owns the Linux signal data model and the per-process/per-thread
signal state of X-Kernel: signal numbers and sets, `siginfo_t` payloads,
dispositions, pending queues, blocking state, alternate stacks, and the
construction/restoration of user handler frames. `kprocess` embeds its
managers into process/thread runtimes; `kexec` maps the signal trampoline
during exec; `ktimer` produces timer `siginfo` payloads and depends on
`ksignal` types; `ksyscall` and the posix process runtime own ABI decoding,
permission checks, and the teardown actions behind `SignalOsAction`.

## Scope

`ThreadSignalManager::fatal_signal_pending` reads thread and shared pending
SIGKILL under the existing signal-state lock without consuming either queue.
It is safe for the scheduler's killable-wait recheck; senders publish pending
state and release that lock before task wakeup. This query adds no pending flag
and does not classify every default-fatal disposition as a queued SIGKILL.

```text
process/ksignal/src/
├── lib.rs               crate root, CurrentThreadSignalIf provider contract
├── types.rs             Signo, SignalSet, SignalInfo, ChildExitInfo, SignalStack
├── action.rs            SignalAction, SignalActionFlags, dispositions, OS actions
├── pending.rs           PendingSignals (standard + real-time queues)
├── notification.rs      ordinary data versus kernel timer dequeue authority
├── siglock.rs           SigLock: signal-state ownership, transactions, projections
├── trampoline.rs        map_signal_trampoline
├── api/
│   ├── mod.rs           re-exports
│   ├── process.rs       ProcessSignalManager, child-exit notification
│   ├── thread.rs        ThreadSignalManager, frame dispatch, sigreturn restore
│   └── dequeue_observer.rs  per-signal dequeue observers
├── arch/                per-architecture MContext/UContext and trampoline
│   ├── mod.rs, x86_64.rs, aarch64.rs, riscv.rs, loongarch64.rs
└── ../ut-tests/         kernel unit tests
```

## Architecture

```text
            ksyscall (ABI decode, permissions)
                       |
posix/process runtime (check_signals -> process teardown)
                       |
        ProcessSignalManager              ThreadSignalManager
        ├─ siglock: SigLock ────────┐     ├─ slot: SlotId (generation-checked)
        │    ├─ shared: PendingSignals    ├─ stack: SignalStack
        │    └─ slots: per-thread    <----┴─ hint: Arc<AtomicBool>
        │         pending / blocked /
        │         saved_sigmask / hint
        ├─ handlers: Option<Arc<SignalHandlers>>
        └─ fatal_signal: AtomicU64
                  \                            /
                   transaction: select + queue or take + republish
                                |
              handle_signal -> SignalFrame on user stack
                                |
              arch::UContext / MContext + signal trampoline page
```

`ProcessSignalManager` holds process-shared actions and the `SigLock` that
owns every pending queue and blocked mask; each `ThreadSignalManager` holds a
generation-checked slot handle into it rather than a registered `Weak`
reference. One dequeue takes the thread queue first, then the process queue,
from the same transaction. `SignalActions` is
shared by `Arc` so `sigaction` updates are immediately visible to every
thread. Its shared reference stays inside the signal managers: constructors
take an owned `SignalActions` value, and `fork_shared` / `fork_private` create
new process managers with shared configuration or an independent snapshot.
Both create empty pending queues and separate slot tables. Callers query or
replace actions through manager methods, not by retaining the underlying lock
or cloning its reference.

`SignalHandlers` owns the shared action table, its content lock, and the
`pending_changed` poll source shared by CLONE_SIGHAND peers.
`ProcessSignalManager.handlers` owns this process's reference, with a separate
lock for changing that reference. The shared object embeds its action table,
so no separate table allocation is required. Access takes ownership then table
locks; only shared fork clones the shared object's reference.
`reset_actions_for_exec` resets a sole owner's table in place. With other owners
it allocates a replacement outside the locks, rechecks ownership, then copies and
resets the current snapshot before publishing the replacement. The old reference
is dropped outside the ownership lock. Temporary manager references do not count
as table owners. Concurrent fork sees either the pre-exec or post-exec table.
Reset preserves only ignore dispositions; all action flags, masks and restorers
are cleared. Pending and blocked masks are unaffected. Last-thread cleanup
takes the optional configuration reference under the ownership lock and drops
it outside that lock, before parent-visible process completion. Runtime teardown
does the same for unpublished clone rollback; manager Drop is an idempotent
fallback. Retained manager references cannot retain or recreate configuration
ownership. Configuration access and fork/exec return `NoSuchProcess` after
release; signal send and prepared child-exit commit discard late delivery.
Sole-owner exec retains the arrival source; shared-owner exec creates a fresh
source. Neither change moves pending signals or rebinds captured weak sources.
Final configuration release drops its source outside signal locks, waking any
remaining registrations even if manager references are still retained.

## Raw signal construction

`Signo` owns numeric validity. Its `from_raw` constructor checks the input
carrier before decoding the signal, so syscall and clone callers share one
range and width boundary. The constructor has no process-state or user-memory
access; `ksyscall` retains errno mapping and the distinct meanings of zero in
signal probes, timer notifications and child-exit notification requests.
The external `ut-tests/signal_number.rs` regressions cover signed, unsigned
and wide input carriers. Existing typed signal operations keep their contracts.

## Queuing and delivery algorithm

Each shared handler object owns a `pending_changed` poll source. Process send,
thread send, and prepared child-exit commit publish pending state and its hint
under the manager ownership lock, then clone only the selected PollSet. They
release all signal locks before waking and dropping that temporary source.
Child-exit preparation captures policy, not a source; commit selects the current
source. A delayed pre-detach notification therefore still reaches the old source.
Notifications include accepted signals blocked from handler delivery and may
repeat for an already-pending standard signal. Thread managers expose
`register_pending` for signalfd waits. A notification only requests a recheck of
the caller's own pending sets; it neither consumes a signal nor makes another
thread's private signal readable. Waiters register before their final readiness
check, and registration guards cancel subscriptions on drop. This follows Linux's
signal-side notification ownership without binding a signalfd to its creator.

Both managers expose `pending_source() -> KResult<WeakPollSet>` and
`notify_pending() -> KResult<()>`; inactive ownership returns `NoSuchProcess`.
Capture does not add a handler owner or keep the source alive. Explicit notify
clones the current PollSet under ownership, then wakes outside signal locks;
it does not publish pending data or imply matching readiness. Thread registration
captures a weak source, releases ownership, then calls `register_weak`.
Already-inactive capture maps to `PollRegisterError::InvalidState`; a captured
source that later dies registers successfully without installing a waiter or
rebinding. Ordinary fork has an independent source despite any shared file.

`PendingSignals` keeps one `Option<KArc<NotificationNode>>` per standard signal
(1-31; at most one pending instance, including prepared timers) and one
`intrusive_collections::LinkedList` per real-time signal (32-64). Each list uses
AtomicLink and KArcPointerOps, with ordinary and prepared timer instances in
the same FIFO. Nodes carry SignalNotification payloads and their provenance.
The `SignalSet` bitmap drives dequeue order:
`dequeue_signal` picks the lowest-numbered unblocked pending signal, and a
non-empty real-time queue re-marks its bit so the next instance stays
selectable.

`send_signal` (process) mirrors Linux `prepare_signal`/`complete_signal`:
an ignored disposition drops the signal only when no live thread currently
blocks it — a blocked signal stays pending because userspace may install a
handler before unblocking. Target-thread selection picks the first live
non-blocking thread that has not left the group, in the same transaction that
queues the signal. The thread-level `send_signal` follows the same
ignore/blocked policy.

`check_signals` reads the calling thread's projection to avoid locking on the
common no-signal return. The slow
path detaches against the thread's own blocked mask, releases queue storage and
completes any typed timer receipt outside SigLock, then consults generic dequeue
observers and dispatches through `handle_signal`.

Handler dispatch builds a `SignalFrame { ucontext, siginfo, saved }` on the
user stack (alternate stack when `SA_ONSTACK` and configured), rewrites
`UserContext` to enter the handler with `(signo, siginfo*, ucontext*)`
arguments, sets the restorer return path (x86_64 pushes it on the stack,
others use the return register), applies the handler mask plus the signal
itself unless `SA_NODEFER`, and resets the action to default on
`SA_RESETHAND`. Syscall-restart error codes in the trap context are folded
into either a rollback (restart) or `-EINTR` before the handler runs.
Default `SIGCONT` (`SignalOsAction::Continue`) does not fold those codes.
The posix return path rewrites them when no handler ran, and drops the
thread restart-block when a handler publishes `EINTR` for
`ERESTART_RESTARTBLOCK`. `restore` reads the frame back on `rt_sigreturn`, restores the
blocked set from `uc_sigmask`, and re-arms the pending flag.

Child-exit notification is split into `prepare_child_exit_signal` (decision)
and `commit_child_exit_signal` (queueing) so `kprocess` can publish exit and
autoreap state before `SIGCHLD` becomes observable; the default-ignored
disposition of `SIGCHLD` does not suppress queuing, while explicit `SIG_IGN`
and `SA_NOCLDWAIT` request autoreap.

## Execution context

- All managers are lock-based (`SpinNoIrq`). Apart from slot allocation in
  `ThreadSignalManager::new`/`new_with_flags` and the replacement allocation in
  `reset_actions_for_exec` (task context, see below), they involve no sleeping
  within this crate and are safe on syscall and trap return paths.
- `handle_signal`, `check_signals`, and `restore` run on the
  user-return path of the owning thread; `handle_signal` writes to
  user memory through `osvm` checked access (`write_vm`), so it may fail on
  an unmapped stack, which is reported as `CoreDump`.
- Handler invocation itself never happens here: the caller observing
  `SignalOsAction::Terminate/CoreDump/Stop/Continue` performs teardown in
  the posix process runtime, which may sleep.
- Dequeue observers run synchronously inside the dequeue path; observers
  must only rely on properties guaranteed by every path that can dequeue
  their signal (in practice: current user thread with its process runtime
  reachable).
- `register_signal_observer` mutates a global table; registration is
  expected during subsystem init, not on hot dequeue paths.
- `map_signal_trampoline` requires a mutable `MmSpace` and runs during exec
  address-space setup.

## State

Pending-queue state is queue/bitmap bookkeeping without an explicit state
machine. The `SignalDisposition` of each action is `Default | Ignore |
Handler(fn)`, replaced wholesale by `sigaction` and reset to `Default` by
`SA_RESETHAND` during dispatch. `saved_sigmask` is an `Option<SignalSet>`
set by `rt_sigsuspend` and consumed by the next caught-signal frame build
(Linux `saved_sigmask` semantics).

## Concurrency

- Lock order is `ProcessSignalManager.handlers` -> `SignalHandlers.actions`
  and -> `SigLock`. Fork/exec use the same ownership-before-content order; they
  do not lock another process's ownership. `SigLock` is never taken before
  `handlers`.
- `SigLock` owns the whole deliverability predicate: the process-directed
  queue plus each member thread's private queue, blocked set and saved sigmask.
  Deliverability is `pending ∩ ¬blocked`, so observing its factors from
  different critical sections is the bug class this layout removes. Target
  selection and queueing of a process-directed signal are one transaction, so a
  selected thread cannot change its mask in between.
- The fast-path projection is per thread and counts both queues under that
  thread's own blocked mask, the same predicate Linux keeps in `TIF_SIGPENDING`
  (`recalc_sigpending_tsk`). It may over-report (one extra slow-path pass) and
  may never under-report. It is written only by `SigLock::republish`, inside the
  transaction that changed the state, and derived from that state; `republish`
  writes only real transitions. Callers cannot publish an availability answer,
  so a receiver can no longer overwrite a sender's notification with a stale
  observation taken before unlocking. A signal every thread blocks therefore
  keeps the fast path quiet instead of forcing a slow-path pass per user
  return.
- `SigTxn` is the only mutation handle; `SigView` is the read-only capability
  used by query paths. `SigLock::transact` and `SigLock::inspect` construct them,
  they borrow the guarded state for the critical section, and they cannot
  outlive it. Verbs take their inputs from the guarded state — `take` derives
  the delivery mask from the thread's blocked set — instead of from a caller
  snapshot.
- `ThreadSignalManager::masks()` uses one `SigLock::inspect` to copy the
  thread-private pending set, the process-shared pending set and the thread's
  blocked set into a `Copy` `SignalMasks` value. Its private fields keep a
  caller from fabricating a partial snapshot, and the value carries no queue,
  payload or lock handle. Dispositions live under `handlers`, so `SigIgn` and
  `SigCgt` are necessarily a separate sample; consumers such as procfs must not
  describe the combined output as one cross-lock atomic snapshot.
- Transactions record the slots and queues they touched in an allocation-free
  dirty set: `republish` recomputes only those, so mask changes, enqueues and
  dequeues stay O(1) inside the lock, and read-only queries recompute nothing.
  A process-directed change marks all slots, because the answer differs per
  thread; that cost is paid once per process-directed signal, as in
  `complete_signal`, not per syscall return. The dirty set is inline (256 slots)
  and degrades to a full recompute above it rather than allocating in interrupt
  context. Set bits are walked directly, and a transaction that touched nothing
  skips the recompute entirely.
- Dequeue observers, user-memory frame writes, arrival wakes and
  `task.interrupt()` run outside `SigLock`. A signal consumed by an observer
  costs one further transaction instead of a critical section held across a
  callback. Wakes stay outside the `handlers` guard because the arrival source
  is `Arc`-shared with CLONE_SIGHAND peers and cannot move into `SigLock`.
- Slot handles carry a generation, so `exit()`, `Drop` and `rebind_after_exec`
  cannot address a slot that was released and reused. Because `Drop` releases
  the slot, the last manager reference must not be dropped while `SigLock` is
  held (`SigLock` is a non-reentrant `SpinNoIrq`); no path does that, and
  transactions never touch a manager reference. `exit()` only clears the
  slot's target flag: the thread's own pending and blocked state stays
  observable while it unwinds, and `Drop` releases the slot.
- `fatal_signal` remains a packed `AtomicU64` read by the exec/de_thread
  sweep. It is not part of the deliverability predicate, but it is published
  inside the same transaction as the queue so the sweep cannot absorb a fatal
  signal that was already queued.
- `SignalInfo` is `Send + Sync` through an explicit `unsafe impl` because
  the payload is treated as opaque ABI bits (see `security.md`).

## Signal state ownership

`SigLock` (`src/siglock.rs`) is the only critical section that answers signal
deliverability. `ThreadSignalManager` keeps only a slot handle, its alternate
stack and a projection cell; the pending queue, blocked mask and saved sigmask
it used to own per field live in the shared state, so a mask change and an
enqueue are serialized without a second lock. `handlers` and the action table
stay outside it: dispositions are `Arc`-shared with CLONE_SIGHAND peers and are
not part of the deliverability predicate, so they keep their own locks and the
ownership-before-content order.

Slots hold their payload out of line (`KBox<ThreadSlot>`): `PendingSignals` is
about 1.3 KiB, so an inlined table would keep a thread-count peak resident for
the process lifetime. `retire` detaches the slot and pops trailing empty slots
under the lock, then destroys the detached owner after unlocking, so the table
shrinks back after a thread-count peak. Registration allocates the
boxed slot and its projection cell *before* taking the lock and only links the
prepared slot inside the transaction, so no slot-owner/`Arc` allocation or slab
refill runs in the critical section other threads' sends and slow-path dequeues
spin on. The table is pre-sized at construction (before the manager is shared,
so nothing spins on the lock yet); a process that exceeds that capacity pays an
amortized doubling of 8-byte entries, never of the slot payload.

`ThreadSignalManager::new` uses KERNEL and requires sleepable task context,
with IRQs and preemption enabled and no reclaim-dependent locks or non-sleepable
RCU section. `new_nowait` avoids introducing reclaim dependencies into process
clone transactions; `new_with_flags` accepts an explicit policy under the same
context contract as `KBox`. Registration now returns `KResult`: slot allocation
failure leaves the table, generation counter and signal projections unchanged.
Generation exhaustion returns InvalidInput with the same publication guarantee;
slot-owner release happens outside the lock on this error path too.
The projection and manager Arcs and table growth remain infallible; this is not
end-to-end OOM safety. Standard payloads now use KArc ownership with bit-only
OOM fallback, also available for realtime SI_USER and explicit kernel fallback.
Other ordinary realtime failures propagate WouldBlock.

Payload admission distinguishes ordinary and prepared notifications.
`put_thread`/`put_shared` reach `PendingSignals::put_notification`: an accepted
ordinary signal allocates a KArc node with NOWAIT inside SigLock. Standard
failure preserves a bit-only notification, as does realtime SI_USER. Other
ordinary realtime failure returns WouldBlock unless kernel fallback is explicit. Coalesced standard sends and ordinary SIGKILL
allocate nothing. Prepared timer sends transfer their preallocated node without any
allocation; creation reserves it with KERNEL permissions before timer locks.
Linux similarly allocates ordinary records under siglock after coalescing,
and this implementation now supports SIGKILL's no-record path and standard
fallback plus realtime SI_USER fallback and realtime EAGAIN propagation.
NOWAIT is not a reserve
guarantee or Linux GFP_ATOMIC equivalence.

## Design decisions

- Handler functions are typed `unsafe extern "C" fn(i32)` and stored as an
  address; the kernel never calls them directly — it only writes a user
  frame — so no kernel-side indirect call risk is introduced.
- On x86_64 the frame has Linux's 304-byte kernel `ucontext`, `siginfo`, and a
  separate 64-byte-aligned FP area referenced by `mcontext.fpregs`. It carries
  legacy FXSAVE or standard XSAVE metadata plus x87/SSE/YMM. Delivery reserves
  the red zone, checks alternate-stack bounds, writes initialized regions
  individually, and initializes the handler's FP state. Nested handlers use
  the current stack position instead of overwriting the alternate-stack top.
  `ksyscall` copies and validates the frame before restoring registers, mask
  and FP state. Other architectures retain `UserRestorableContext` in their
  frame. No private kernel execution context is exported.
- x86_64 pushes the restorer address on the user stack to match the
  kernel-only `SA_RESTORER` ABI; other architectures use the return-address
  register.
- A trampoline page is mapped at a fixed address per address space so
  binaries without a vdso still get a working `sigreturn`.
- Kernel-originated `sigval` reads expose `None` unless `si_code` implies a
  user or timer payload, keeping union-arm reads (see `security.md`)
  guarded by the code that selected the arm.

## Resource lifecycle

Managers are created by `kprocess` when a process/thread runtime is built
(`ThreadSignalManager::new` self-registers with the process manager). Shared
signal configuration, pending queues and poll sources are kernel-owned; frame
memory lives in user address space and alternate stacks are user-owned.

The last thread's exit releases its process's shared-configuration ownership.
`ProcessSignalManager::drop` repeats that idempotent release as a fallback.
The removed configuration reference is dropped outside the ownership lock;
other processes may still own the same configuration. Final poll-source
destruction detaches registrations and can synchronously invoke their wakers.
Configuration release must therefore occur outside process-domain, membership
and signal locks, as required by `release_handlers`.
