# krcu - Design

## Purpose And Ownership

`krcu` owns kernel RCU instances and their execution-context integration.
`krcu-core` owns the reader, watching, grace-period and callback algorithms;
it does not own the kernel's arrays or execution services. `ktask` retains
task reader storage, lifecycle and scheduler boundaries and supplies generic
task, wait, timer and preemption support. `kirq` owns balanced interrupt entry
and dispatch. `kruntime` starts the service after the existing CPU-service
rendezvous; POSIX brackets user transfer with this crate's callbacks.

## Source Scope And Integration

- `src/host.rs`: static execution, task, timer and notification contracts,
  supplied by kruntime.
  RCU retains demand publication, CPU progress eligibility and callback budgets;
  the provider only delivers existing IPI/softirq notifications.
  Its current-reader adapter lends host-retained task storage through a
  synchronous body. CurrentReader certifies identity/lifetime, not read admission.
  The task-wait adapter lends prepared parking and a waker; it does not own
  completion predicates or callback markers.
  The timer adapter lends a stack-borrowed one-shot timer and its clock. RCU
  chooses deadlines, cancellation and progress actions; expiry only sets demand.
  Task startup transfers a PreparedGpTask containing the unique driver, service
  reference and startup waker. The host moves it into the ordinary task entry;
  it neither claims control nor publishes RCU readiness.
  The softirq adapter installs the fixed action and raises local pending work.
  RCU retains callback budgets, GP wake decisions and the ready-queue recheck.
- `src/context.rs`: permanent reader nodes and CPU watching, cleanup flags,
  the static `RcuContextIf` provider and user-boundary callbacks.
- `src/service.rs`: service state, sole GP driver/task, progress timer and
  bounded Rcu softirq callback execution.
- `src/read.rs`, `src/synchronize.rs`, `src/owned.rs`: private scope/wait
  machinery behind the exported owning `Rcu` container.
- `src/callback.rs`: public one-shot ownership of prepared callback nodes.
- `src/drain.rs`: private serialized callback drain and reusable completion
  markers behind the exported `drain_callbacks` operation.
- `ut-tests/`: private scope, state, startup, completion and marker fixtures.
  Host-integrated tasks, interrupts and consumers are tested by kruntime.
- `crates/krcu-core/`: reusable algorithms and the static integration contract.

The consumer interface remains in krcu; there is no separate API crate.
`RcuNotifyIf` is a host integration contract, not an alternative callback API.
With IPI support notifications remain deferred and cannot wake tasks inline.
Without it, only local RCU softirq notification is supported and may wake its
daemon. Scheduler-locked cleanup still relies on periodic progress in that
configuration. Neither notification certifies a quiescent state or consumes
reader cleanup demand. This crate does not depend on ktask, kirq, khal, kipi
or ksync. Runtime supplies their concrete capabilities; scheduler/IRQ hooks
reach the RCU owner through the existing krcu-core integration contract.

Task and interrupt read scopes, scheduler cleanup checks and final user-entry
cleanup borrow current storage through `RcuTaskIf`.
The runtime retains its concrete CurrentTask handle until RCU nesting and task
exit cleanup finish. No allocation or owner-count increment is added. The
adapter preserves IRQ/preemption state and does not decide watching or unlink
eligibility. Task bodies remain preemptible; interrupt bodies remain strictly
nested and skip task-exit cleanup. User entry releases the borrowed handle
before turning watching off; cleanup retains the handle through guard release.
`RcuExecutionIf` supplies interrupt-context facts through runtime integration.
The broad interrupt-context predicate includes BH-disabled tasks; the handler
predicate excludes those tasks and includes only executing IRQ/NMI/softirq
handlers. Queries check NMI before ordinary context snapshots and do not run
scheduler checks. RCU retains scope admission and cleanup eligibility.
`RcuTaskIf::is_sleepable_context` supplies a nonallocating task-context query
without preparing a wait or changing exclusion/pending state. Preflight rejects
NMI before this query, and samples it before adding temporary IRQ exclusion.
RCU separately owns exception and watching checks.
`RcuTaskIf` also supplies the current preemption-count query, reschedule-request
bit, ordinary scheduling checkpoint and scoped check deferral. Deferral removes
only its own preemption credit, without changing IRQ state or running cleanup.
RCU decides when to retain enrollment and when to request a later checkpoint;
the host decides whether that checkpoint may schedule. `RcuExecutionIf` supplies
CPU identity and exception snapshots. Read admission runs in its synchronous
exception-snapshot body while the host retains ordinary IRQ exclusion; RCU
checks NMI, exception presence, watching and interrupt state inside that body.
The host restores IRQ state before the actual read scope, without a scheduling
check or preemption-count update. Cleanup uses the independent protected
exception query. CPU identity is not a pin: queue transfer/execution retains
its existing exclusion, while a demand notification may target a sampled CPU.
The `ipi` feature selects RCU's remote notification protocol, not a scheduler
implementation. The `smp` feature also enables `kspin/smp` for its own shared
state. `kfeat/ipi` selects matching RCU, runtime and task/IPI capabilities
together; `kfeat/smp` includes that assembly. Enabling an RCU feature alone
does not initialize or configure the host scheduler. An alternative host must
provide the selected notification contract; krcu does not depend on kruntime.
Tests of private completion ownership and registration stay with the RCU
implementation. Public synchronous-wait integration and task/timer carrier
prerequisites live in `core/kruntime/ut-tests/`; they use boot's permanent
services without opening private RCU state to external callers.
Public callback lifecycle tests also live in runtime: they observe cancellation,
invocation, original node addresses, execution context and submission allocation
counts through caller-owned probes. Core queue tests check execution budgets;
runtime uses finite per-CPU bursts to exercise real softirq progress.
Internal CPU-slot, synchronous-wait admission and user-boundary fixtures obtain
current CPU/reader state through the same host contracts as production code.
They inspect private RCU state without depending on concrete task storage;
runtime integration tests separately verify the host's context classification.
Public drain admission and downstream-task completion boundaries are tested in
runtime through `PreparedCallback` and `drain_callbacks`. Private marker and
counter fixtures remain here; drain does not join work started by a callback.
Task-switch, migration, IRQ/BH/NMI cleanup and implicit-region integration
fixtures live in runtime. They retain actual task storage and use existing
scheduler integration tokens, not unrestricted access to private RCU state.
The unittest-only `reader_cleanup_requested_for_tests` snapshot lets them
observe demand without clearing it, supplying quiescence or authorizing cleanup.
The marker-installation snapshot similarly observes only whether any drain
marker is installed while a fixture holds the host's drain serialization gate.

`krcu_core::RcuContextIf` is defined in the core's `src/integration.rs` and
implemented here, not in the IRQ crate. Its production operations are
`current_cpu_watching`, `check_reader_cleanup`, `finish_reader_cleanup`
and `note_context_switch`. The provider is statically linked before the GP
service is ready. Watching lookup needs initialized CPU identity, not current
task or service startup; it must not lock, allocate or schedule. It preserves
the caller's IRQ mask, never enables previously disabled IRQs and does not
change NMI exclusion. The reference does not pin its caller. Cleanup and switch operations
consume immediate-use contexts defined by
[krcu-core](../crates/krcu-core/docs/design.md); unsafe context construction
certifies the host's retained task and excluded, watched, non-NMI boundary.
The static methods are safe; their provider delegates unsafe core operations
under that context contract.
The crate exports `Rcu` for scoped reads and task-context synchronous replacement.
It also exports `CallbackNode`, `PreparedCallback`, `preflight_callback_submission`,
`drain_callbacks`,
`with_read`, `with_interrupt_read`, `ReadScope` and the unsafe `synchronize_rcu` operation.
Core reader accounting and manual enter/exit operations remain private.

The crate-level task-context `with_read` establishes current-task participation
and lends a `ReadScope` that borrows its internal guard. Its fields are private;
callers cannot manufacture it from an independent core reader. This token has
no allocation or owning reference count and inherits the core guard's inability
to transfer between tasks. `Rcu::read` bounds a reference by both container and
scope. The container's `with_read` convenience method uses the same protocol.
Neither higher-ranked closure permits protected references to escape in its
result; callers may copy data or clone an independently owned value. Interrupt
handlers use the same host-issued token; no raw core-token read is exposed. Several loads
in one scope protect lifetimes, not a consistent multi-object snapshot.

The task preemption checkpoint makes one `check_reader_cleanup` call. All RCU
cleanup predicates, context checks and removal policy live in `context.rs`.
`needs_reader_cleanup` is an internal notification query, not a static-interface
operation. `reader_node_for_tests` exists only under `cfg(unittest)`.
The provider uses ktask's unsafe reader borrow and generic
`current_preemption_disabled`, `request_current_reschedule` and
`with_preempt_check_deferred` support. The last helper changes no IRQ state
itself and restores its preemption credit without scheduling.

## Permanent State And Watching

`context::CPU_STATE` uses `kcpu_slot::cpu_slot!` for a `CpuRcuState` containing
watching accounting and cleanup demand. The initialized template contains only
atomics, no owning pointers or destructor. Each CPU receives independent storage
in its permanent CPU area, rather than adjacent elements of shared arrays.
The per-CPU infrastructure owns area alignment and stride. No measured speedup
is implied, and reader-node algorithms remain independent of this storage.

`krcu_core::RcuContextIf` borrows initially watched state via `current_shared`.
Selection uses the CPU base installed during bring-up before ordinary IRQs/NMIs
are enabled. Atomic-only shared access adds no guard, lock or allocation and
does not require a current
task or the GP service. The returned reference remains valid but does not pin
execution. IRQ entry owns pinning and matching exit. Effective NMI context is
tracked independently by kirq. The preemption probe
returns before scheduler work in NMI, and sleepability rejects it before the
locking ordinary-context query. IRQ/NMI watching and idle/user quiescence are
connected and sampled by the GP service. Read admission requires an initialized,
already-watched handler, never an early architecture entry window.

Remote GP sampling uses `kcpu_slot::area` and `CpuSlot::at`: the published-area
handle pairs with CPU initialization, and the borrow stays within its lifetime.
All fixed participants publish their areas before service activation. A missing
participant area violates that boot invariant and is not treated as quiescence.
Local transitions retain their existing IRQ/preemption and NMI pairing contracts;
neither accessor itself pins execution. Tests compare local/remote identities
and the spacing of different CPU instances without modifying live state.

## Idle And User Boundaries

Like Linux `default_idle_call`'s `ct_cpuidle_enter/exit`, the idle no-work helper
brackets only `karch::idle_wait` with quiescent entry/exit. The final reschedule
check runs first. A retained request skips the wait without changing watching.
The same `NoPreemptIrqSave` spans CPU selection, both transitions and the wait:
IRQ/NMI handlers temporarily activate watching, and the HAL return tail cannot
schedule away before idle restores it. Architecture return bookkeeping may
touch static/per-task exception state, but must not access RCU-protected objects.
After waiting, watching is restored before releasing the guard, timer repair or
ordinary scheduler work. An explicit read scope cannot span this idle window.

The unsafe `enter_user_quiescent`/`exit_user_quiescent` host callbacks bracket
user execution inside `UserContext::run`, not outside that method. POSIX finishes
user-time accounting before entering quiescence and restores watching before
kernel-time accounting, matching Linux `__ct_user_enter/exit`. Transitions run
IRQ-masked without scheduling; return restoration precedes IRQ
dispatch, syscall processing and faults. Between callbacks, only user execution,
audited register/stack transfer and balanced interrupt entries are permitted.
No preemption guard is carried into user mode or across subsequent trap dispatch.
CPU selection occurs after entry accounting; return restoration occurs before
exit accounting. The RCU proof does not assume that accounting's lock release
is itself incapable of triggering a preemption check.
These callbacks are not read-side APIs. GP sampling consumes their transitions
only through the ordered node observation protocol; it never bypasses task
blockers. They do not add unsupported architecture NMI routes.

## Task Participation And Cleanup

Safe-boundary and terminal-task cleanup consume the core's progress hint after
releasing the original node lock. If removal satisfied that node's last old
reader obligation and its CPU observation was already supplied, the host
requests a completion pass through the deferred IPI carrier. It does not scan
other nodes, report a saved period, wake inline under scheduler locks, or run
callbacks. The service revalidates readiness. Without IPI the periodic pass
remains the fallback; NMI scope exit still only defers cleanup.

Each task embeds `krcu_core::TaskReader` for explicit scoped read nesting.
Owning-container reads use this accounting through the host scope policy.
The sleepability predicate and blocking scheduler boundary reject active readers,
and `yield_now` rejects voluntary yielding. Involuntary preemption eligibility
does not inspect this counter. `RcuContextIf::note_context_switch` enrolls an
active reader before a permitted wake probe retains the runner, same-task
selection returns or an actual stack switch occurs; the common path includes
migration helpers. CPU nodes have kernel lifetime and retain no
owning task reference. The task retains its original node across migration;
outer scope exit unlinks it. After service construction the same hook also
reports the CPU boundary, but only after enrollment. An active task remains a
GP blocker on its original node. Before service construction the hook only
maintains enrollment, preserving startup readers. Before either path, the hook
drains a finished reader's deferred membership under IRQ/preemption exclusion.
Active nesting prevents that cleanup; a subsequent CPU report still requires
the normal scheduler observation. Cleanup also runs for same-task selection,
before the early return, and never wakes or invokes callbacks under scheduler
locks. This follows Linux's context-switch deferred cleanup before `rcu_qs`.
For an inactive reader with neither enrollment nor local cleanup demand,
`finish_reader_cleanup` returns after read-only checks instead of issuing an
atomic clear. Enrollment without a notification still requires unlinking, and
a notification without enrollment still requires clearing. This shortcut ends
only cleanup: scheduler observation continues, including when a new GP races
with the cleanup checks. No RCU-demand query is added to the scheduler.

The provider's `check_reader_cleanup` also drains finished membership without a
reschedule request. It requires an initially preemptible, watched task outside exception,
hardirq, softirq, BH-disabled and NMI contexts. An outer exclusion guard covers
context queries and removal; its release suppresses only recursive checking,
then the existing loop inspects any newly published scheduling request.
User/idle entry drains before leaving watched execution. User entry restores
its temporary preemption credit without invoking the scheduler in the final
architecture callback. These paths admit no new interrupt read scopes.

This supplies safe cleanup opportunities, not arbitrary NMI-exit progress.
The GP task revisits node readiness while a period is active. Periodic inspection
alone cannot remove another task's membership; retained local cleanup demand
also targets reschedule kicks even after that CPU has reported. The notification
does not certify quiescence or permit remote unlinking. Deferred scopes currently
are used by the owning container and controlled interrupt tests. Ordinary core task scopes
unlink on their outer exit. A strictly nested deferred interrupt scope cannot
enroll itself; over an active task scope it leaves the outer nesting intact,
and over an unenrolled task it creates no retained membership.
The cleanup integration test holds the CPU observation complete and verifies
that final preempt-enable releases the blocker, after which a fresh readiness
report makes the marker callback eligible without another CPU observation.
This deterministic fixture does not establish interrupt notification delivery.
The owning container uses a private `with_task_read` host entry.
It masks ordinary IRQs before selecting any per-CPU context slot, rejects NMI
before touching task preemption accounting, and validates watched task context
under temporary exclusion. A permanent slot reference alone does not prevent
migration between selecting the CPU and loading its state. Context queries
balance their internal guards without invoking scheduler/RCU checks, so admission
needs no additional preemption credit. It restores the caller's IRQ state before
running the read callback.
Its outer exit object runs after the core nesting guard, including on unwind;
normal involuntary switching/migration retains task identity, not entry CPU.
An active nested scope or absent enrollment needs no cleanup. Otherwise the
exit reuses the safe preemption probe when IRQs and preemption are enabled;
if either remains excluded, it retains membership and sets the existing
task reschedule request plus a per-CPU cleanup notification. Publication rechecks
the current reader under short exclusion, not on the scope's entry CPU. It takes
no controller lock and suppresses its temporary guard's preemption probe.
Returned preempt/BH guards reach the existing cleanup probe when finally released.
With IPI support an active GP's next timed pass supplies a kick for IRQ-only
deferral. It does not require a new CPU-observation obligation. Without an active
GP or IPI support, only subsequent local safe boundaries supply cleanup progress.
Safe boundaries retain notification while a scope is active; otherwise they
clear it before rechecking/removing enrollment. A later publication survives.
The scheduler performs this on the outgoing CPU before migration; stale demand
can cause a harmless additional check, never another task's remote removal.
The entry rejects exception, IRQ/NMI and BH-disabled entry contexts.
The public `with_interrupt_read` entry accepts already-watched
IRQ, serving-softirq or NMI context, not merely BH-disabled task context. It
shares current's nesting, but never enrolls, unlinks or publishes cleanup demand.
Scopes must finish on the handler stack before return without scheduling or
altering watching. NMI classification precedes any ordinary context-lock query.
Software-context tests cover task/IRQ/NMI nesting, preservation of deferred task
cleanup, and NMI entry from modeled user quiescence. They do not establish real
hardware NMI delivery or validate early architecture entry windows.
The live callback probe also enters nested scopes in each CPU's real RCU
softirq endpoint, without synthetic context or extra exclusion. It checks that
the inner exit preserves active outer nesting, and that the full exit preserves
the interrupted task's enrollment, cleanup demand and local IRQ state. This is
entry/exit integration coverage, not a protected-object reclamation stress test.
Core reporting never enters a scheduler wake path. When a scheduler report is
accepted and the coordinator's completion-pass hint is set, the host publishes
demand through the RCU IPI carrier. The softirq wakes the GP task only after
scheduler exclusion ends. Without IPIs this hook leaves notification to the
existing timed pass, since direct softirq raising could wake a daemon under
the runqueue lock. For permanently exited tasks, `finish_task_switch`
clears residual reader participation after publishing incoming current and
leaving the outgoing stack, but before releasing its owning reference or on-CPU
marker. It diagnoses abandoned scopes and uses krcu-core's ordinary unlink path.
The task subsystem retains storage through unlinking and releases its outgoing
owner afterward. Final destruction uses its deferred finalization mechanism;
that mechanism does not repair RCU lists or permanently retain readers.

## Service Startup

The boot-owned `rcu_gp` task holds the sole callback driver. Runtime starts it
after all discovered CPUs have scheduler, timer and notification services. Its
participant set uses discovered logical CPUs rather than all capacity slots.
A short startup lock serializes duplicate rejection, softirq action installation
and the one-shot driver claim, in that order. An action conflict does not consume
the claim. Successful assignment cannot be undone or repeated; task creation
and the sleepable acknowledgment wait occur after releasing the startup lock.
The task prepares its own `PreparedTaskWait` and an inactive owned timer before
publishing its waker; the startup task waits sleepably for that acknowledgment.
The published waker admits prepared synchronous waits and callback handles.
The raw queue transfer boundary remains private to owning adapters and tests.

## Owning Consumer Policies And Evidence

Synchronous waits borrow a `TaskWait` from the runtime's stack-local
`PreparedTaskWait` adapter. The host validates sleepability and prepares its
waker before invoking the RCU body. RCU then prepares its completion registration
before giving the caller a `PreparedGracePeriodWait`. Owning replacement retains
the new Box until this body is entered, so either preparation failure returns
that same owner without changing publication. No fallible setup follows exchange.
The adapter adds no allocation for type erasure, cannot return an error after
invoking its body, and cannot grant completion merely by returning from park.
Callback drain, the startup caller and the GP task borrow the same prepared
parking contract. Startup prepares its wait before any control claim. The GP
task prepares a different wait on its own stack, then prepares its timer and
publishes readiness. The startup caller still checks RCU's waker publication
after each wake; activation alone is not readiness.

The synchronous-wait module supports owning replacement and the public unsafe
`synchronize_rcu` operation. The latter does not withdraw or own user data;
integrations retain responsibility for withdrawal, lifetime and error handling.
Like Linux's callback/completion wait, it
prepares the task waiter and registers completion before submitting a marker.
The marker embeds its queue node and completion in one Arc allocation. Queue
execution owns a strong reference independently of the waiter, so a resumed
waiter cannot destroy storage still used by the notifying callback. Registration
failure precedes submission; afterward the loop accepts only a completion token,
not a signal or arbitrary wake. An RCU-local SeqCst fence follows the successful
completion observation, including completion before parking or migration. The marker
never owns a retired payload, and potentially sleeping destruction remains with
the caller after successful return. This waits for a GP, not callback drainage.

The public `Rcu<T>` retains one Box behind
an atomic pointer; Acquire reads borrow both the container and the current task's
read token. Its private raw read remains unsafe because an arbitrary standalone
TaskReader token is not proof of host participation. Public `with_read` creates
the genuine host scope internally. Synchronous replacement prepares the
wait first, exchanges with AcqRel, waits without a writer lock, and reconstructs
the old Box only after completion. Before then the old owner remains raw: an
abandoned operation can leak it but cannot run its destructor early. Concurrent
exchanges retain separate old allocations. Final exclusive container destruction
needs no extra GP because outstanding reads and replacements borrow it.
This follows Linux's withdrawal / unlock / synchronize / caller-cleanup split,
not the implementation of `rcu_replace_pointer`, which itself neither waits nor
provides concurrent-writer serialization.
The controlled read contract also admits strictly nested, watched hardirq scopes
that end before handler return without scheduling. A real timer-IRQ test reads
each of three successive publications while a task performs synchronous
replacement, checking live-borrow drop counters and final single destruction.
It drains the rearming timer before dropping the container. This demonstrates
real IRQ read/update integration, not forced overlap, hardware NMI admission,
or weak-memory correctness. The isolated core test covers retirement eligibility
while an interrupt borrow remains active.

Private owning-consumer test access also admits watched, nonscheduling softirq scopes.
Its live-service fixture queues readers on every participating CPU, observes
three stable publications and leaves additional readers outstanding during task
replacement. Each reader checks its version's destruction counter while borrowed.
The task drains all accepted readers before final destruction; callback completion
is published only after releasing its container reference. This verifies real
softirq read/update integration, not forced overlap or a weak-memory proof.

Controlled NMI reads use the audited software entry to activate watching before
dereference and restore the interrupted state afterward. A fixture reads three
successive publications both inside an outer task read scope and from a
controlled quiescent boundary, including nested interrupt scopes. It checks
unchanged enrollment/cleanup state and final task-context destruction. This
does not inject a hardware NMI, overlap replacement with NMI reads, or qualify
early architecture entry windows.

The SMP implicit-region tests use separate pinned reader/updater tasks. The
reader holds only IRQ, preemption or BH exclusion, with no explicit read token.
An isolated GP samples the reader CPU's real watching state and checks that
withdrawal cannot make the callback eligible until that CPU supplies quiescence.
After the borrow and guard end, a controlled user-boundary entry/exit pair
supplies quiescence; no EL0 execution or natural idle is claimed.
The fixture uses no synthetic scheduler reports. Invocation additionally waits
for the borrow-exit handshake, so an eligibility regression is detected without
freeing borrowed storage. These tests exercise guards and watching together,
not boot-service notification, public container admission or weak-memory proof.

## Progress And Callback Execution

`preflight_callback_submission` checks ordinary task execution and permanent
service readiness before a consumer changes publication destructively. It has
no node argument and allocates no wait, transfers no ownership, requests no
progress and retains no CPU pin or admission token. An absent service returns
`BadState` before any current-task access. Once the service exists, NMI is
rejected before ordinary context queries can lock. Host sleepability is checked
before temporary IRQ exclusion; the exception-snapshot body then rejects
exceptions and performs the same watching/waker checks as callback submission.
Invalid context returns `InvalidInput`; an unpublished waker returns `BadState`
after valid context. The shared check does not impose task-only restrictions on
ordinary submission, which still supports watched hardirq and softirq callers.

Consumers prepare all fallible resources before mutation, serialize and recheck
their update, then release writer exclusion and use the existing
`PreparedCallback::submit`. Ordinary preemption/migration may occur; no
arbitrary caller code or explicit context/watching transition belongs between
commit and submit. The preflight result is not a transferable reservation.
Its use relies on startup completing CPU discovery/rendezvous before fixing the
participant set, driver claim preceding waker publication, and neither being
revoked. The actual submit still selects its current CPU under short exclusion
and performs the core driver/participant checks. No queue or GP algorithm changes.
Future teardown/hotplug requires revisiting this contract.

The internal preflight fixture owns isolated inactive reader nodes and service
state. It checks rejection before driver claim and before waker publication,
then watched task success and the shared masked/unwatched checks. It does not
replace live SERVICE or execute early boot. Runtime fixtures check rejection
without consuming prepared ownership or changing publication. A separate
preemptible SMP fixture verifies actual CPU migration after preflight, followed
by submission and exactly-once softirq execution on the destination CPU.

The private `submit_callback` boundary pairs queue transfer with retained
progress notification. It rejects NMI before taking any ordinary guard, and
checks watching and service readiness under local IRQ/preemption exclusion.
CPU selection and transfer share that exclusion; rejection leaves storage with
the caller. Notification follows successful transfer outside queue locks and
the local guard. Synchronous wait markers use this boundary. It allocates no
storage and remains private behind the owning callback API.

`PreparedCallback` holds a stable node pointer and an
unsubmitted cancellation function. It does not allocate or impose a payload
layout. Construction does not depend on service initialization. Submission
looks up the permanent service and returns `BadState` with the original owner
if it is unavailable. Construction adopts the unique submission right; consuming submission
suppresses cancellation before queue transfer and returns the same handle on
rejection. On success only the queued callback owns invocation. There is no
Clone or public raw-pointer access. Its unsafe constructor audits storage
provenance, cross-CPU ownership and cancellation; unsafe submission separately
audits withdrawal and nonblocking post-GP execution.

Cancellation releases preparation, not protected data awaiting a GP. The
synchronous wait embeds its node in its existing Arc allocation and prepares
an independent raw Arc owner before withdrawal. Abandoning preparation drops
that reference without completing the wait; accepted invocation completes it
and drops the same reference. No additional heap wrapper is introduced.

Callback validation separates protocol and carrier checks. An isolated core
fixture reuses the same embedded node over three explicitly driven periods:
neither the completed period nor merely starting its successor permits another
invocation. Live service fixtures submit a preallocated node from a one-shot
timer hardirq and repost the same allocation from its Rcu softirq callback.
They check watched/non-sleepable execution, completion and exactly-once final
destruction. The host core test rejects allocation on its own thread after
preparing the fixture, through submission, period advancement and all reposts.
A separate control verifies rejection and restoration; other test threads keep
normal allocation. This does not inject failure into the kernel notification
or scheduler path, and none of these fixtures measures latency. Timer delivery
is an integration prerequisite, bounded by the external QEMU timeout.

Live task, timer-hardirq and softirq-repost fixtures also compare kalloc's
current-CPU cumulative successful heap-allocation bytes around accepted
submission, including its notification request. The same IRQ/preemption guard
spans both samples; remote allocations and local ordinary interrupt work cannot
contaminate the interval, and frees cannot hide allocations. The observations
are limited to `cfg(unittest)`. This observation neither counts failed attempts
nor measures asynchronous IPI/softirq delivery after the guard is released;
it is not whole-kernel OOM injection or NMI allocation attribution.

Demand is retained independently of wake hints and consumed before each pass.
Submission transfers its node before publishing demand and notifying, with no
queue lock held across notification. Fixed IPI transport records an RCU reason
bit and sends the controller notification; its receiver marks Rcu softirq
pending, rather than invoking the GP waker inline. Without IPI transport,
raising softirq may wake ksoftirqd in task context. That wake detaches existing
poll registrations before invoking their wakers. The GP task's own waker is
prepared at startup and unblocks its existing scheduler entity. These are
distinct responsibilities: retained callback queues account for work, while
notification and scheduler wakeups only request execution.

The live prepared-submission burst fixture excludes local IRQ execution while
submitting a fixed workload to its current CPU, then waits for every retained
completion. It checks that batched delivery loses or duplicates no callbacks.
The private service fixture sizes its workload from `CALLBACK_BUDGET` and uses
the production batch executor to check exact per-pass counts and re-notification:
unready work does not rearm, ready remainder does, and an empty queue does not.
The live fixture does not require one interrupt
per submission or inject synchronous reentry into the IPI controller. Selecting
one QEMU CPU does not disable the platform's Cargo `ipi` feature; such a run
must not be reported as validation of the no-IPI wake branch.

Task/timer callers publish demand before requesting Rcu softirq service. With
IPIs, a fixed RCU notification marks the vector pending without waking tasks
inline. The softirq action wakes the GP task when demand is retained and leaves
that demand for it to consume. Independently, it executes at most 32 eligible
callbacks on its own CPU outside bookkeeping locks, then re-raises Rcu if eligible
work remains. Pending GP work alone never causes softirq re-raising. Existing
softirq restart limits and ksoftirqd carry the remainder.
Without IPIs, callers raise the vector directly and must release scheduler/RCU
locks first because daemon wake may acquire scheduler locks. Neither route
supports NMI; restricted scheduler producers are not yet connected. The
task parks when idle with its progress timer cancelled. An active period keeps
a 10 ms progress-check deadline across unrelated wakes; expiry requests another
pass and never supplies quiescence evidence. After an initial or due progress
round, the next interval starts from a fresh clock sample after scanning and
notifications, not from the sample taken before that work. An early demanded
pass preserves the existing deadline even if it expires during the pass.
Deadline addition saturates at the
timer's u64 nanosecond horizon, where delivery is not guaranteed. The kernel owns
the task and state permanently; duplicate startup is rejected rather than
reconstructing its driver. This follows Linux's split between a sole GP thread
and separate observation/callback carriers, not Linux's pre-SMP startup ordering.

When an active interval first starts, and at its progress deadlines, the GP task
requests a reschedule IPI only for CPUs whose nodes still owe an observation
or whose host cleanup notification remains set.
The query is a transient hint: a concurrent report can make a kick unnecessary,
but sending or handling the IPI never clears an obligation. The real scheduler
hook still enrolls active readers before reporting. Old-reader membership alone
does not cause repeated kicks; retained cleanup notification does. The GP task
never clears that notification remotely. Unrelated demand retains the existing deadline
instead of causing another kick round. In the no-IPI single-CPU path the GP task's
own parking supplies a scheduler boundary. Assistance cannot force an
uncooperative CPU to schedule.

Before each control pass the GP task samples each participating CPU's permanent
watching state through the node's GP-bound observation protocol. An even sample
or a crossing since this GP's first watched sample clears only the CPU obligation.
The node lock and fence precede the sample; samples are not cached in the host.
The GP is started by the later driver advance, so its first watching sample can
occur on a subsequent pass. Timer assistance remains enabled for ambiguous
samples, including a full counter wrap to the same watched value.
If sampling did not report readiness, the task rechecks retained node readiness,
allowing an already-observed node to report after its last blocked reader exits.
That second inspection cannot discharge an unobserved CPU. After promotion, the task notifies
each CPU with eligible callbacks; it never invokes callbacks itself. The current
integration test submits unpublished owning objects and supplies real scheduler
boundaries with per-CPU tasks. It checks target CPU, softirq context and completed
counts across repeated fixed workloads, independent of the private budget. This does not
establish idle/no-tick progress or authorize ordinary protected-data consumers.
An additional fixture submits on every CPU and parks until owning callback
wakers report completion, without yielding helper tasks. Other interrupt sources
are not disabled by that fixture; it is not proof of a completely tick-free run.

## Callback Drain

The public drain interface requires a sleepable watched task. A sleepable
mutex supplied by the runtime serializes drain callers from marker installation
through completion. The runtime owns only this gate and prepared task parking;
krcu owns the completion predicate and marker protocol. Permanent service
storage contains one reusable marker per CPU, separate from the mutex payload.
Each queue inserts its marker
at its logical tail, including work already detached for execution. Empty queues
need no marker and the drain does not request a new GP of its own. The service's
normal progress path delivers eligible markers to their owning CPU.

One task-bound wait and one shared completion allocation are prepared before
publication; markers only clone that allocation, not allocate per CPU. An
initial count holds installation open. Each queue's count is added before its
marker can run and undone if insertion is unnecessary. Only after visiting every
participating queue does the installer release its initial count. Acquire/release
completion counting carries prior callback effects to the returning waiter;
notifications are hints, not the completion predicate.

The marker takes its completion owner out under a short spinlock and ceases all
marker access before decrementing. Wakeup uses that independent Arc even if the
waiter returns and another drain reuses the marker. The executor never accesses
an invoked node again. This last-access rule permits reuse while the old wakeup
stack unwinds; it does not permit reuse of a linked node. All target callbacks
preceding the marker have returned before the marker runs. A drain does not wait
for separately scheduled downstream work or indefinitely chase later reposts.
Teardown stops producers and reposts before draining.

This follows the responsibilities of Linux `rcu_barrier` and
`rcu_segcblist_entrain`: serialize callers, retain installation accounting,
append behind outstanding work, and wait for preceding callbacks rather than
requesting an independent GP. The kernel's per-queue lock permits remote
insertion without executing an insertion handler on the target CPU. Its
exclusive executor claim keeps detached work ahead of the marker.
The fixed participating CPU set has no hotplug or offloaded callback queues.
Concurrent drain callers each install their own pass; Linux's barrier-sequence
optimization that lets overlapping callers share a completed pass is not
implemented. Independent per-drain completion ownership replaces reuse of one
global completion state, retaining notifier storage across cross-CPU wakeup.
These are representation and service-scope differences, not permission to return
before the targeted callbacks finish. The shutdown order follows Linux's
`Documentation/RCU/rcubarrier.rst`: stop producers and reposts, drain callbacks,
then release owner resources. Any downstream work requires its own drain.

## Ownership Stress And Measurement Boundaries

The runtime-owned default-off release runner in
`core/kruntime/benches/rcu/torture.rs` exercises public scoped reads and concurrent synchronous
replacement. Like Linux rcutorture's initialization and object-circulation
checks, it checks payload contents and keeps an independent per-object return
and destruction ledger. Readers retain the container, not an extra owner of the
borrowed allocation. Each reader must run, updated publications must be observed,
and all objects must be destroyed exactly once after workers stop and the final
container is dropped. Read lengths are bounded; yielding occurs only outside
read scopes. An external QEMU timeout diagnoses lack of progress, never permits
forced reclamation. A separate fixed-storage preflight retains two atomic
retirement markers through 64 withdrawals and real GP waits. Readers must not
observe retirement inside their selected slot's scope. A local injected early
retirement must trigger the same marker check, without freeing test storage or
changing the GP implementation. These checks complement real-allocation
accounting; neither replaces weak-memory modeling or exhaustive interleavings.
It repeats fully verified batches for `RCU_TORTURE_SECONDS`, enabled explicitly
with `KFEAT_RCU_TORTURE`. Default UT does not compile or execute this workload.
See [runner instructions](../../../core/kruntime/benches/rcu/README.md) for configuration and pass criteria.
Each timed iteration also runs bounded per-CPU callback producers through
`PreparedCallback`. Records own their intrusive nodes and release themselves
in softirq; after producer joins, `drain_callbacks` must leave each ID invoked
and destroyed exactly once. Cancellation, live records and context errors must
be zero. Peak live storage includes preparation, not only queued callbacks.
These records are unpublished, so this checks execution/ownership accounting,
not whether a GP protected an independently published object.

Correctness counters are not performance instrumentation. Following Linux
rcuscale's distinction between synchronous and asynchronous operations, measure
read cost, synchronous wait latency, callback submission cost, callback completion
latency and drain time separately. Record throughput, latency distributions and
peak outstanding records with coverage disabled. Async submission throughput
alone does not establish reclamation throughput. CPU count, scheduler, tracing
and virtualization settings must match across compared runs; QEMU timings are
not bare-metal instruction costs.

The independent `KFEAT_RCU_BENCH` runner measures sequential prepared callback
submission, callback entry and caller-observed completion after drain, on pinned
same-CPU paths. Preparation is outside timing; drain overhead remains included
in the completion observation. A subsequent synchronous-wait round measures
the entire public `synchronize_rcu()` call, including its allocation, waiting
and scheduling, without deliberately held readers. Raw samples and round summaries are emitted
outside measurement intervals. This is not maximum-throughput testing or a
reader-pressure GP benchmark; its initial 32-sample rounds validate the runner,
not rare-tail performance guarantees.

The same runner times baseline, empty task read scopes and immutable object
reads in 1024-operation batches. Batch totals include normal IRQ/preemption
interference; they are not single-read tail latencies or multicore scaling
measurements. No updater pressure or baseline subtraction is applied.
