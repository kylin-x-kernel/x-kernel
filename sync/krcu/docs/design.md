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

- `src/context.rs`: permanent reader nodes and CPU watching, cleanup flags,
  the static `RcuContextIf` provider and user-boundary callbacks.
- `src/service.rs`: service state, sole GP driver/task, progress timer and
  bounded Rcu softirq callback execution.
- `src/read.rs`, `src/synchronize.rs`, `src/owned.rs`: private
  `cfg(unittest)` scope policies, synchronous wait and owning consumer.
- `ut-tests/`: controlled host integration fixtures.
- `crates/krcu-core/`: reusable algorithms and the static integration contract.

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
No public consumer read, synchronize or owned-data API is exposed.

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
connected and sampled by the GP service. Interrupt read admission remains closed.

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

Each task embeds `krcu_core::TaskReader` for explicit scoped read nesting. Controlled
kernel tests exercise this accounting; no general RCU reclamation API is exposed.
The sleepability predicate and blocking scheduler boundary reject active readers,
and `yield_now` rejects voluntary yielding. Involuntary preemption eligibility
does not inspect this counter. `RcuContextIf::note_context_switch` enrolls an
active reader
before same-task selection returns or an actual stack switch occurs; the common
path includes migration helpers. CPU nodes have kernel lifetime and retain no
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
have only controlled test callers. Ordinary core task scopes
unlink on their outer exit. A strictly nested deferred interrupt scope cannot
enroll itself; over an active task scope it leaves the outer nesting intact,
and over an unenrolled task it creates no retained membership.
The cleanup integration test holds the CPU observation complete and verifies
that final preempt-enable releases the blocker, after which a fresh readiness
report makes the marker callback eligible without another CPU observation.
This deterministic fixture does not establish interrupt notification delivery.
Controlled owning-consumer tests use a private `with_task_read` host entry.
It selects the current task, validates watched task context under temporary
exclusion, and releases that exclusion before running the read callback.
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
The entry is unittest-only and rejects exception, IRQ/NMI and BH-disabled
entry contexts. It does not authorize general protected-data consumers.
The separate unittest-only `with_interrupt_read` entry accepts already-watched
IRQ, serving-softirq or NMI context, not merely BH-disabled task context. It
shares current's nesting, but never enrolls, unlinks or publishes cleanup demand.
Scopes must finish on the handler stack before return without scheduling or
altering watching. NMI classification precedes any ordinary context-lock query.
Software-context tests cover task/IRQ/NMI nesting, preservation of deferred task
cleanup, and NMI entry from modeled user quiescence. They do not establish real
hardware NMI delivery or authorize general interrupt consumers.
The live callback probe also enters nested scopes in each CPU's real RCU
softirq endpoint, without synthetic context or extra exclusion. It checks that
the inner exit preserves active outer nesting, and that the full exit preserves
the interrupted task's enrollment, cleanup demand and local IRQ state. This is
entry/exit integration coverage, not a protected-object reclamation stress test.
Reporting sends no notification and never
enters a scheduler wake path; service progress must inspect retained node state
separately. For permanently exited tasks, `finish_task_switch`
clears residual reader participation after publishing incoming current and
leaving the outgoing stack, but before releasing its owning reference or on-CPU
marker. It diagnoses abandoned scopes and uses krcu-core's ordinary unlink path.
GC still uses `Arc::try_unwrap` to wait for independent owners and destroy tasks
in task context; it does not repair RCU lists or permanently retain readers.

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
This is controlled carrier integration, not ordinary RCU readiness: general
read-side and callback admission remain closed.

## Private Consumer Policies And Evidence

The synchronous-wait module, including its private unsafe `synchronize_rcu`
helper, is compiled only with `cfg(unittest)`. It is not a public reclamation
interface: external consumers cannot yet obtain a current-task reader. Public
read-side admission and waiting must be exposed together after the complete
context protocol is validated. Like Linux's callback/completion wait, it
prepares the task waiter and registers completion before submitting a marker.
The marker embeds its queue node and completion in one Arc allocation. Queue
execution owns a strong reference independently of the waiter, so a resumed
waiter cannot destroy storage still used by the notifying callback. Registration
failure precedes submission; afterward the loop accepts only a completion token,
not a signal or arbitrary wake. An RCU-local SeqCst fence follows the successful
completion observation, including completion before parking or migration. The marker
never owns a retired payload, and potentially sleeping destruction remains with
the caller after successful return. This waits for a GP, not callback drainage.

The owning integration consumer is compiled only with `cfg(unittest)` until
complete context admission is validated. Its `Rcu<T>` retains one Box behind
an atomic pointer; Acquire reads borrow both the container and the current task's
read token. Reading remains unsafe because an arbitrary standalone TaskReader
token is not proof of host participation. Synchronous replacement prepares the
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

The private owning consumer also admits watched, nonscheduling softirq scopes.
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

Demand is retained independently of wake hints and consumed before each pass.
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
counts across repeated workloads larger than the callback budget. This does not
establish idle/no-tick progress or authorize ordinary protected-data consumers.
An additional fixture submits on every CPU and parks until owning callback
wakers report completion, without yielding helper tasks. Other interrupt sources
are not disabled by that fixture; it is not proof of a completely tick-free run.
