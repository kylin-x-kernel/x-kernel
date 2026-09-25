# Prepared Workqueue Management

## Scope And Ownership

This crate supplies allocation-free submission of prepared ordinary and delayed
work from task, BH and hard IRQ context. Preparation and synchronous waits may
allocate and require initialized, sleepable task context. It does not replace
the existing system workqueue, create global services or supply owning wrappers.

```text
integrator -> service::bind_queue -> Queue -> per-CPU QueuePartition
                                              | waiting / ready / in-flight colors
caller owns pinned Work <---------------------+ non-owning pending links
caller owns DelayedWork -> ktask::Timer -> timer-to-queue transfer
QueuePartitionSource -> kworkerpool_ng dispatcher -> claimed Execution
```

Queue acquisition converts remaining ready demand into SourceRemainder. This is
independent of whether an Execution was acquired: blocked waiting work is not
ready demand, and release publishes a fresh notification when it becomes eligible.

service::prepare_retiring_work supplies the owning facade with a one-shot no-op
Work and a paired opaque context/terminal function. Ordinary constructors set
Work.retirement to None. Execution copies the optional action before ending its
last Work borrow, completes independent accounting/observers, then calls the
action without any further Work access. The action can destroy enclosing storage.
This provider capability is unsafe: the integrator retains publication/storage,
submits once, excludes cancellation/reuse and certifies the callback context.
Cancellation does not invoke the hook. The hook is outside Work completion scope;
it is not an additional business callback or a queue-wide destruction guarantee.
The optional pair adds two pointer words per Work and no per-submission allocation.

The optional stall-check feature implements best-effort backlog observations.
PartitionState owns Option<Arc<StallRecord>>; Directory owns only a Mutex-protected
Vec<Weak<StallRecord>>. Each record contains copied queue identity, CPU, and a short
SpinNoIrq-protected interval start and last-warning timestamp. No Queue/Work
reference, raw pointer, publication version or detached cleanup receipt is needed.

Registration allocates in sleepable preparation before exposure, pruning dead weak
entries. Submission only publishes a true backlog/progress transition under
partition -> record protection; it never touches the directory, clones an Arc,
allocates, wakes GC or runs output. Existing waiting/ready membership defines
backlog. Additional enqueues, rejected changes and allowance release do not reset
age. Delayed timer-only and running-only states are not backlog.

Periodic task-context inspection try-locks the directory, upgrades one weak record
at a time, and try-locks only that record. Busy data is skipped, never declared
healthy. Output receives copied identity/duration after the record guard ends;
the task-context directory mutex remains held, so the restricted reporting closure
must not reenter registration/inspection. No spinlock is held across output.
Weak entries are pruned on both registration and inspection. Registry work is
linear in registered records and has no worst-case reporting deadline.

A zero inspection threshold suppresses warnings while retaining weak-entry
pruning. The returned boolean reports whether a scan occurred, not queue health
or whether warnings are enabled. Such a pass does not consume the warning
opportunity when a later inspection supplies a nonzero threshold.

A monitor reference can temporarily outlive partition destruction, but retains
only plain diagnostic storage. The last Arc frees that storage; the next registry
pass removes the weak slot. No explicit unregister, reader drain, GC extension,
fair-turn state machine or retry timer exists. Shutdown still drains ordinary
work and execution sources, not diagnostic allocations. A previously sampled
warning can race queue shutdown; it describes the observation, not a live handle.

Disabling stall-check removes records and publication hooks. The opaque service
API exports Directory and service::bind_queue_with_diagnostics for root wiring.
The latter copies the name and attaches a record to each partition during cyclic
Queue construction, before any producer can see it. The integrator owns the
directory and arranges optional task-context inspection; no monitor is installed here.
Scoped tests do not constitute full production subsystem acceptance.

This implementation crate owns queue/configuration/control/routing, per-CPU
partitions, prepared Work, cancellation, synchronous stop and ordinary queue drain.
System registration and safe ownership wrappers are outside this crate. Work fixes the
private generic occurrence payload to Arc<Queue> using an alias, not a lifecycle
wrapper. DelayedWork keeps its existing inline Work and prepared timer. Public
cancel/disable/wait methods invoke the original state machine. queue/api.rs provides
unsafe pinned publication methods without exposing CPU samplers or batch/resources.
Queue construction remains a service-integration capability, not root default access.
CpuPolicy, QueueConfig and QueueConfigError describe immutable queue routing.
Configuration validates and owns a sorted boot-CPU subset and positive per-CPU
release limit; it grants no execution capability. Provider readiness, category
binding and diagnostics must be established before a runnable queue is exposed.
The injected presence validator remains private. The service module exports
opaque QueuePartitionSource and Execution types solely for root execution-service
wiring. Their fields and constructors remain internal. service.rs exposes
bind_queue and bind_queue_with_diagnostics as explicit integration functions;
the corresponding Queue constructors are crate-private. Integrators must check
provider readiness before using those functions.
The actual Queue exposes submission, immutable config, flush and close_and_drain,
without another lifecycle wrapper or additional ownership of caller Work storage.
This crate does not register global defaults or switch existing system callers.

Queue owns a compact array of Arc<QueuePartition>. Work is externally owned and
contains its callback, one intrusive pending node and protected pending/running
slots. Each accepted occurrence retains its original Queue/partition/color, not
ownership of Work. WorkState and QueueInFlight are implementation details.

Routing methods operate directly on Queue: selecting returns a CPU ID, while
successful implicit RR admission advances Queue's cursor. There is no separate
selection token. PartitionState directly owns its diagnostic callback list; the
private callback guard, not a list wrapper, carries the unlink obligation.

## Admission And Release

```text
Queue::submit
  -> QueuePartition::accept_from_queue
     -> insert_accepted: pending slot + waiting list + in-flight count
     -> release_waiting: available allowance and no running predecessor
        -> PendingNode::release (concrete function pointer)
           -> Work::release_pending_node
              -> WorkState::release_pending
        -> transfer waiting node to ready; reserve active allowance
  -> later claim_with_demand: ready -> running
```

The concrete release function is bound in Work::with_callback together with claim.
This permits mixed callback types without owning or allocating a submission object.
Release is internal to the partition: acceptance, ready cancellation and running
retirement invoke release_waiting. Callers never perform a separate release call.
An already-ready Work reserves allowance; claim does not charge it again. A running
predecessor blocks its successor until resource cleanup has completed.

## Dependencies And Context

kcpu_id_map validates CPU configuration. intrusive-collections supplies non-owning
pending links and Arc-owned observers. kspin supplies IRQ-compatible protection;
kpoll supplies terminal completion and registration. ktask::PreparedTaskWait supplies
context validation and preallocated parking for synchronous operations. ktask::Timer
owns prepared timer storage; ktime_types and kerrno supply its deadline/error types.
ksync supplies
the sleepable Queue flush setup mutex; submissions and completion never take it.
kworkerpool_ng supplies the source dispatcher and acquisition/execution contracts;
it does not depend on concrete management types.
khal supplies current monotonic time for delayed delivery; khal and kirq also support
cfg(unittest) real CPU/softirq integration tests. Execution-category selection
and global service setup are integration responsibilities.

Lock order is partition -> Work -> in-flight count. Callback execution, resource
destruction, completion notification, allocation and task parking occur outside
management locks. Prepared submission uses no allocation. Synchronous operations
require sleepable context even for idle targets. With self-wait-check enabled,
Work embeds a CallbackNode holding the sole comparison-only task owner_key.
During callback execution, its separate intrusive link belongs to the original
partition's callback list. Work flush/cancel/disable checks this node; Queue flush/shutdown
scans its partitions before closure. Neither accepts an injected validator.
The private callback scope unlinks before clearing identity and before cleanup.
List access takes partition -> node-key locks; entry releases the key lock before
linking. No management lock spans callback execution. Sleeping, migration and
nested callbacks preserve identity. Entry/exit is O(1) without allocation; only
sleepable Queue preflight scans active callbacks. Off builds omit nodes and lists.
OOM handling is deferred by
the approved successful-preparation assumption.

The internal ordinary Work::flush prepares observer registrations, captures the
current pending/running pair under one Work guard and parks without management
locks. Capacity is rechecked before attachment; a growth race retries preparation
without partial capture. It neither cancels nor disables. Later submissions do not
extend its receipts; detached cleanup is included only if captured before removal.
Normal return does not mean the Work is idle or its callback ran. Delayed timer
expedition is not implemented by this ordinary wait.

## Queue Flush

Queue owns sealed_colors under a sleepable mutex. Four reusable color slots hold
only independent ColorCompletion receipts, not another participation count. Each
receipt owns one existing CompletionObserver per partition. A flush prepares its
own tokens and wake registrations before effects, snapshots old receipt Arcs under
the mutex, registers them unlocked, then revalidates their identities after locking.
Only terminal colors may be reclaimed, and snapshot Arcs defer destruction until
unlock. Registration failure cannot publish a cutoff.

With a free color, one atomic update publishes the next admission color while
preserving closure. Setup visits each partition admission barrier and attaches one
token to the sealed color. The mutex excludes reuse until all partitions have been
visited; empty tokens complete after unlock. Existing QueueInFlight retirements
detach observers before notification, so terminal receipts prove local accounting
and observer membership ended. No second counter or local color receipt field is
needed. Old waiters retain their original immutable receipts across slot reuse.

If all colors are occupied, registered old receipts provide durable completion
wakes and the caller parks without the mutex, before choosing its cutoff. Ordinary
acceptance continues. Once captured, a flush waits for its own and earlier receipts,
not later submissions. Shutdown takes no flush mutex and drains all colors without
needing a free slot. This Queue operation is not exposed on shared defaults.

A CPU-pinned test pauses the shared test admission transaction after its final
sample under the partition lock, then resumes it only after a remote flush publishes
the cutoff. It covers first/last partition visits, repeated color reuse, old-work
drain and exclusion of later submissions without adding production hooks or state.

## Delayed Placement Primitives

PendingSlot now records one PendingPlacement: Timer { deadline_ns }, Waiting or
Ready. The deadline exists only in timer placement. WorkSlot keeps the original
partition for routing/lifetime, but its optional color records whether it owes an
ordinary queue retirement. A timer-only occurrence has no color/count/allowance.
This distinction also survives cancellation after PendingSlot has been detached.

work/state/delayed.rs adds methods on WorkState, not another delayed state owner.
Protected retime changes only the authoritative deadline. Transfer preserves the
payload, partition and Work observer chain, records the reserved current color and
becomes Waiting; a running predecessor still prevents release. Cleanup without a
color releases resources and observers without decrementing QueueInFlight.

QueuePartition::queue_due now performs real timer-to-ordinary transfer under
partition -> Work protection. It revalidates ownership and the current deadline,
borrows the existing Queue payload to sample the current color, reserves its count,
and inserts the same pending node before releasing waiting work. Old/future or
duplicate notifications are inert. Closure, disable and count overflow reject
unchanged. Service demand is published under protection and kicked after unlock.

Logical cancellation recognizes timer-only placement and does not attempt list
removal or return unreserved allowance. It is not physical timer cancellation.
DelayedWork in work/delayed.rs combines one inline Work and one
prepared ktask::Timer. Its timer handler stores only an immutable borrowed Work
address and the constructor-matched delivery function. It owns neither Work nor
callback captures; there is no second Queue, partition, deadline or color field.
The erased binding permits non-'static captures under the external retention
contract while the timer provider owns its own 'static handler allocation.

Pin before first arm. Submission binds the address once, then QueuePartition::arm_delayed
validates admission and programs the timer under partition -> Work protection,
publishing logical timer placement before either guard ends. Timer core locks are
below these locks and are released before delivery. The IRQ adapter samples current
monotonic time, snapshots the owner under Work protection, drops that guard and
revalidates under partition -> Work in queue_due. It never runs business work inline.
Timer IRQ CPU and selected ordinary execution CPU may differ.

DelayedWork::cancel_sync keeps Work::disable_sync's existing guard through physical
timer.cancel_sync, then releases it. No management lock spans either wait. This
covers physical delivery even if Work is logically idle, and prevents a concurrent
arm from escaping the drain. Producers must still be stopped/joined before freeing
storage or closing Queue. Drop alone does not drain ordinary execution.

The private queue_after entry accepts a relative TimeSpan and optional execution
CPU. queue_after_on supplies an explicit target and the real CPU sampler. Zero
goes directly through Queue::submit, reserving ordinary accounting and
publishing service demand without waiting for timer delivery. Positive delay uses
checked monotonic addition followed by the timer provider's nanosecond range check;
no saturating/wrapping conversion is used. Both paths remain fresh admission, not
modification, and never execute the business callback inline. The absolute arm_on
entry remains its internal timer integration primitive.

Queue::arm_delayed in queue/delayed.rs owns positive-delay routing. It shares
resolve_target with ordinary submission: unfinished ownership overrides automatic
policy, and conflicting Queue/explicit CPU is rejected. Fresh automatic selection
uses CurrentCpu or RoundRobin. The partition transaction revalidates the owner
snapshot before any timer programming; a changed snapshot retries with the same
absolute deadline. Only accepted fresh automatic RR advances the existing Queue
cursor, including timer-only acceptance. No extra route token or Work CPU field
is stored. Tests inject sampling-time changes without claiming real CPU migration.

The private retime_pending_at primitive updates only an existing pending occurrence.
It retries stale owner snapshots and never accepts a new occurrence or changes its
Queue/CPU. Under partition -> Work guards, range/disable/closure validation and
successful timer programming precede all list/state changes. Timer-only retime
keeps its observers. For ordinary waiting/ready work, the partition removes the
same pending node, and WorkState::withdraw_pending_to_timer detaches only the old
color and observer chain while retaining resources and ownership in the slot.
Ready withdrawal returns allowance and refills eligible waiting work; waiting
withdrawal cannot return a predecessor's allowance. Service wake, old color
retirement and observer notification occur unlocked after withdrawal. No new
withdrawal object, allocation or resource/partition reference field is required.

Private modify_after composes this transition with existing fresh admission.
The relative deadline is sampled once. Queue validates unfinished Queue/CPU
ownership, attempts protected pending modification, then attempts fresh admission
if no pending target remains. Competing admission reports AlreadyQueued internally
and retries modification; no accepted occurrence is canceled to make room.
Claim-first permits one successor using current admission checks. Only fresh
automatic acceptance advances RR. No new state object or stored reference is added.

Zero modification transfers timer placement immediately under partition -> Work:
validate disable/closure and charge the current color before attempting physical
timer cancellation and publishing ordinary waiting. A running timer callback sees
the authoritative placement and cannot insert it twice. Nonwaiting physical cancellation
stays inside those guards so it cannot cancel a later producer's rearm. Pending
physical successor membership may survive a Running cancellation result; it has
no logical timer authority and final cancel_sync drains it before storage release.
Existing ordinary placement remains in its FIFO position with unchanged allowance and scope.
Demand notification remains unlocked; no business callback runs inline.
Delayed cancel_pending reuses ordinary detachment with one internal provider
action under Work protection (and partition protection when pending is owned).
The action attempts timer removal only after route validation, including the idle
fallback under Work protection. Rearm cannot interleave between physical removal
and logical revocation. Cleanup and notifications remain unlocked; no timer action
follows a notification that may rearm. Running timer delivery may survive and must
revalidate placement; this nonwaiting operation is not a lifetime barrier.

Delayed disable reuses the existing independent DisableGuard: ordinary disable
revokes pending work, then a nonwaiting timer attempt runs while admission stays
disabled. disable_sync also drains timer delivery before returning the same guard.
cancel_sync delegates to it and releases only its temporary disable. No new guard,
state owner or stored reference is added. Stop/join producers and synchronously
drain before external storage release. Public integration remains a later unit.
Ordinary Work APIs are not a delayed drain substitute.

The private partition capture_and_expedite transaction is the first delayed-flush
building block. Under partition -> Work it revalidates ownership and preallocated
receipt capacity, checks timer transfer gates and reserves the ordinary color.
It then attempts nonwaiting physical timer cancellation before attaching receipts.
Running delivery rolls back the speculative color charge and requests an unlocked
retry. Any rollback notifications run after releasing management locks. Ordinary
and running-only subjects also require a non-Running timer result before capture.
Only a successful physical stop permits receipt attachment and ordinary publication.
Claim/retime cannot split capture
from transfer. Already ordinary/running subjects are captured without changing
placement or gates. Stale owner/capacity retry and recoverable errors have no effects;
notification remains unlocked and normal release constraints apply.

Private DelayedWork::flush supplies context/self-wait preflight, preparation,
retry/parking and idle-owner handling. It prepares the maximum two receipts once
(one pending, one running), before any physical cancellation. Retries yield with
all management locks released and revalidate current ownership; they do not retain
an entry-time target snapshot. With no owner, Work protection excludes concurrent
publication while checking physical quiescence. FlushError distinguishes wait
preflight failures from transfer rejection, not categories of normal completion.

After capture, only the consumed receipts are awaited. No unconditional
Timer::cancel_sync follows: concurrent retime may have installed an out-of-scope
deadline. Later timer activity belongs to concurrent arrangements. This wait is
not a teardown barrier or proof that a business callback executed. Tests exercise
actual parking, withdrawal, idle/context/closure handling and a remote timer IRQ
held after ordinary publication but before timer-core clears Running. The latter
uses a per-object test-only gate, never a global hook or a production state field.

## Source Binding

Queue::with_dispatch prepares one QueueSource per allowed CPU using Arc::new_cyclic
before exposing Queue. QueuePartition owns an immutable registration; its adapter holds only
Weak<Queue> plus CPU identity. A dispatcher may be shared by multiple queues.
Accepted Work slots, not idle sources, retain Queue. Runtime construction must bind
the matching CPU/category and an IRQ-compatible, service-only Arc<Waker> that neither
retains Queue/Work/registration nor invokes business work inline.

Whenever release_waiting transfers eligible work, it publishes demand under the
partition lock after all Work guards end. Dispatch only updates source state/list;
it neither acquires management locks nor invokes provider code. Only an Arc<Waker>
escapes for unlocked wake and release; RawWaker::clone is not called under protection.
Acceptance, ready cancellation and running retirement all follow this path.

Source acquisition upgrades Weak<Queue>, claims one ready occurrence and releases
the temporary Queue reference. Execution carries a non-owning pointer and unique
claim, not a borrow of that temporary reference. Unsafe publication requires external
Work/captures to outlive execution; the running slot retains Queue until cleanup.
Tests using safe unpartitioned admission retain a test-only borrowed guard.
The dispatcher ends source inspection before invoking the acquired execution.

No worker/BH runtime is created here. close_and_drain prepares source-detachment
receipts and registrations before closure, drains ordinary accounting, then detaches
every bound source and waits for inspections/reference release. Concurrent callers
share each source's permanent receipt; none may return merely because another caller
unpublished it. Immutable bindings live outside PartitionState so detach/notification
never runs under the partition lock. Work publication still holds that lock.
Diagnostic teardown and global service readiness remain integration gates.
The cfg(unittest) normal integration fixture binds concrete Work to autonomous
CPU-bound manager/worker tasks. Tests cover IRQ-masked publication with max_active
refill, growth while a callback waits, and independent closure of queues sharing
one service. They never manually claim work or drive manager passes. Each body
drains its queues before releasing pinned Work/captures, then the fixture stops
and joins the provider. The queue body now also drains source detachment; the fixture
itself is not global production registration. Diagnostics need no teardown join.
The workerpool BH entry now validates actual softirq context/CPU and requests
bounded continuation. Local integration tests bind real Work to a scoped kirq
action; provider tests additionally exercise remote BH notification. Production
initialization remains outside this crate.

## Lifecycle And Limits

Detached WorkSlot::complete owns the shared terminal sequence: destroy detached
resources, retire the original partition/color, release that partition reference,
then notify queue and Work observers. It never borrows Work. Cancellation hands
over its payload; execution has already destroyed its payload before returning
allowance and clearing running, so it hands over an empty resource slot. These
distinct lifecycle transitions remain separate even though notification is shared.

External owners must retain stable Work storage through pending/running work and
concurrent API calls, stop producers and drain before release. Cancellation detaches
pending resources; earlier observers wait for cleanup but later Work capture excludes
that detached cleanup. Queue counts retain it until actual destruction completes.
close_and_drain permanently closes ordinary admission and waits every partition/color;
it does not stop execution providers or join diagnostic references. Callers must
stop producers and synchronously cancel delayed work before closing its queue.
Queue shutdown does not discover or drain timer-only arrangements. Production
worker/BH activation remains an integration responsibility.
