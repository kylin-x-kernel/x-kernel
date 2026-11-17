# ktask — Security and reliability

## Priority inheritance

`TaskPiHost` uses counted task references and pointer identity. Graph-held
callbacks take no scheduler locks; boosts use the owner-RQ/entity guard and
retry migration. Policy updates release these locks before PI-chain refresh.
The adapter adds no unsafe operations, user-word protocol or owner-death recovery.

## Scheduler Entry Ownership

new_kthread_with_entry allocates the root PID before constructing an unpublished
task. PID failure drops the prepared entry in the caller; success transfers the
same owner without reboxing. The prepared-entry test checks address identity,
root PID presence and exactly-once capture destruction for both constructors,
on invocation and cancellation. PID exhaustion is not injected. Existing entry
allocation-failure tests cover rejection before invocation. Fixture assertions
are not a production OOM recovery guarantee; legacy spawn wrappers and nested
stack/name/bookkeeping allocations retain their previous contracts.

The unittest-only sleep probe registration is scoped to one retained task
identity and captured by its prepared timers. Probe methods execute without
sleep-state or timer owner locks. Fixtures must not unwind, sleep in IRQ
context, recursively cancel the timer, or wait for progress from a CPU they
have blocked. Cancellation observations occur only after the core sees Running;
they neither authorize freeing storage nor replace the owned timer's drain.
The registration and callback references are removed after scoped waits retire.

The optional task scheduling observer is installed through an exclusive owned
TaskInner before publication. It is immutable thereafter and retained through
the outgoing stack handoff, so switching needs neither raw callback pointers
nor mutable registration races. The unsafe TaskScheduleObserver implementation
contract excludes allocation, sleeping, unwinding, recursive scheduling,
run-queue lock acquisition and waits for other CPUs. Callbacks preserve IRQ and
preemption exclusion. Same-task selection is not a resource ownership transfer.
One task's callbacks cannot overlap across migration because its SMP on-CPU
handoff precedes remote enqueue; UP execution is serialized by IRQ exclusion.
Sharing observers across tasks requires separate
synchronization. Terminal put must release CPU-local ownership but cannot wait
for cleanup needing scheduler progress; ordinary consumer teardown owns such
draining. Observer destruction follows normal task ownership, not callback
execution, and is not guaranteed complete by observing a task exit code.

Idle and SMP affinity helpers do not allocate an independent callback owner.
The affinity helper retains one KtaskRef directly in a private entry variant,
transfers it to the migration function on execution, or releases it when the
unpublished helper is dropped. Entry consumption remains after switch completion
and IRQ enablement. No new unsafe operation, retry loop, failure swallowing or
NoMemory panic is introduced. This does not make complete helper construction
fallible or allocation-free: existing stack and scheduler-owner allocations are
outside this change. Ordinary kernel boxed entries still need separate caller-context
and rollback audits before their KBox migration.

## Prepared User Entry Ownership

User entries are allocated before clone preparation so NoMemory cannot leave a
new child link or thread registration. KERNEL preparation requires sleepable,
reclaim-safe context. NOWAIT can still fail and does not make capture destruction
safe in arbitrary contexts. Cancellation drops captures without invoking them;
successful execution consumes them after switch cleanup and IRQ enablement.
The private consuming trait dispatches to the existing `KBox::into_inner` API
without introducing unsafe code. Send captures permit transfer to another CPU.
The task's single-consumption protocol is unchanged. Tests inject factory errors
locally, not allocator-wide OOM; stack and scheduler allocation remain infallible.

Prepared PID-less kernel construction transfers the same owner into an Internal
task without allocation or invocation of the callback. The caller must prepare
it before committing startup state, and satisfy capture destruction constraints
on cancellation. This does not change the legacy infallible spawning APIs.

## Scope and trust model

`src/task_api.rs` implements `kiface_task_api::TaskExecutionIf` and
`kiface_task_api::TaskLockDiagnosticsIf`, not a consumer-specific lock factory.
It uses the existing `lock_owner::holder_key` identity, including the live-task
address fallback for an unknown identity token, and returns a nonzero
`OwnerKey`. Identity must remain stable across suspension and migration. The
aligned key invariant is enforced by `OwnerKey`'s constructors: token-derived
keys are projected through `OwnerKey::from_token`, and the `TaskInner` address
fallback is pinned by a compile-time alignment assertion. Lock-word consumers
rely on this to pack flags into the owner word without aliasing another task.
The provider lends
the pinned waiter to synchronous `block_on`; it cannot retain that borrow.
Callers must establish sleepable context for blocking acquisition. Startup
try-lock requires a current task but does not yield. Diagnostic hooks only update
current-task lock metadata. Mutex exclusion and listener wakeup ordering remain
owned by `kmutex`; its non-Send guard prohibits transfer to another task.

Failed `try_join` means the target may still execute and access resources; error
handling must not release them as though exit had completed. The legacy `join`
retains a narrowly annotated fail-stop until those callers can propagate errors
without abandoning ownership. Direct IRQ wait-provider misuse returns an error,
which is not a quiescence certificate. Internal KIRQ callers validate before
mutating IRQ ownership and restore all temporary context guards before waiting.

Task storage uses DeferredArc rather than immediate Arc destruction. The
architecture current pointer owns one raw count; CurrentTask is only a borrowed
view. Switch preparation consumes the real count once, not the borrowed view,
and PREV_TASK covers the old stack through incoming-stack cleanup. Intrusive
queues must preserve the original payload pointer provenance for raw recovery.
Weak expiration ends acquisition, not destructor completion. Runtime destructors
execute in a sleepable normal worker and must not wait for another destruction
serialized behind them on the shared finalizer. NMI retirement is allowed only
after local irq_work initialization. Creation remains boot/task-only with
allocator and local context facilities initialized. BH-disabled task preparation
is permitted under allocation-safe locks; this does not permit sleeping there.
The finalizer now reserves its payload owner with NOWAIT and can return NoMemory.
Task owner preparation remains infallible at its public boundary and treats this
as fatal, preserving the previous Box OOM outcome. Making it recoverable requires
task-constructor error propagation and admission rollback; this migration does
not establish that protocol or make stack/identity/carrier allocations fallible.

No GC task or exit queue retains an additional exit reference.
The diagnostic module owns a task's registration token and inline weak slot.
Retirement closes admission before reclaiming that Weak; a counted reader
protects its storage through upgrade. The retiring writer or final reader owns
removal, never both. A token is atomically consumed before a slot can be reused,
so its later Drop cannot retire a replacement. Registration precedes runnable
publication and must not overlap retirement. Tokens point only to permanent
slots, independent of task migration. NMI readers never wait on interrupted
writers and hold no registry borrow during callbacks; final Weak release still
requires initialized local irq_work. No generic NMI reclamation framework is
introduced. Exited-task diagnostic discovery ends before releasing PREV_TASK;
previously acquired strong owners remain valid.

`ktask` retains each task's `krcu_core::TaskReader` and supplies safe scheduler,
idle and preemption-cleanup boundaries. System reader nodes, watching storage,
cleanup notifications and service policy belong to
[`krcu`](../../../sync/krcu/docs/security.md). The permanently linked
`krcu_core::RcuContextIf` provider is available before GP service readiness;
availability of that provider is not permission to reclaim.

`api::is_sleepable_context` rejects missing or idle current tasks,
interrupt-like/IRQ-masked contexts, preemption protection and explicit reader
nesting. Sleep assertions and prepared waits share this gate; voluntary yielding
also rejects readers. Involuntary preemption remains allowed. Callers still
own dependency-cycle and execution-provider readiness checks.

Scheduler enrollment uses retained DeferredArc-owned outgoing task storage with IRQs
and preemption excluded, before a permitted no-reschedule return, same-task
return or switching. The no-reschedule report follows `can_preempt(1)` and
affinity validation; it must not acknowledge a CPU while an outer preemption
guard still protects implicit readers. Ordinary IRQ-off checkpoints remain
excluded by the task checkpoint gate. The provider
cleans zero-depth membership before observation and enrolls active readers
before reporting. It cannot wake, allocate or schedule under the runqueue lock.
A CPU report never substitutes for the original node's task-reader blocker.
Scheduler and idle callers construct the core's immediate-use observation or
cleanup contexts under their exclusions, then pass them to safe static methods.
The unsafe construction contract remains in force through return; contexts
cannot be saved for another boundary or treated as consumer read guards.
The unsafe `TaskInner::rcu_reader_for_integration` accessor in `src/task.rs`
does not admit unrelated consumers: scope mutation requires current or strictly
nested handlers, and snapshots of retained non-current tasks grant no mutation
or protected-access authority.

The preemption checkpoint delegates once to `RcuContextIf::check_reader_cleanup`.
The krcu provider owns NMI exclusion, ordinary context checks and the complete
cleanup conditional. Active nesting preserves membership; cleanup itself
supplies no CPU observation. Generic `current_preemption_disabled` only queries
task state; `with_preempt_check_deferred` changes no IRQ state and restores its
temporary credit without scheduling. Neither helper grants RCU admission.
Ordinary preemption checks retain pending requests while local IRQs are masked,
even when the count reaches zero, so an inner guard release cannot split an
implicit IRQ-off region. Only the completed normal IRQ tail uses its distinct
IRQ-exit checkpoint. `ktask` enables `kspin/preempt` independently of scheduler
preemption; generic deferred-check scopes must balance that count without
running a check from their own guard release.

Idle drains finished membership before entering quiescence. The same IRQ and
preemption guard spans the wait and restoration; no reader or protected borrow
may span it. Watching resumes before scheduling or timer bookkeeping, and the
IRQ return tail outside kirq's watching scope remains RCU-free. User-mode
transitions and scope-exit notification policy are owned by `krcu`, not `ktask`.

On the incoming stack, `finish_task_switch` removes an exited task's abandoned
participation before releasing its owner or on-CPU marker. Old current is
unpublished, the task cannot resume, and IRQs/migration remain excluded.
The PREV_TASK lock is released before taking the original reader-node lock;
abandoned-scope diagnostics follow unlinking. Registration retirement precedes
this RCU cleanup; neither operation waits for the final strong release.
Cleanup neither detaches immutable runtime fields still borrowed by shared
owners nor unwinds abandoned Rust stack values. No raw task pointer is sent
to a deferred callback. This boundary does not establish general IRQ/NMI
consumer admission or a progress guarantee without a later safe boundary.

This document covers the security boundaries of the entire `task/ktask` crate:
the `lib.rs` integration and `api.rs` entry points; task lifetime and context
in `task.rs`; scheduling in `run_queue/`; blocking in `future/` and
`wait_queue.rs`; timers in `timers.rs` and the scoped provider in `timer_api.rs`;
completion wait and daemon integration in
`task_api.rs` and `softirqd.rs`; and diagnostics in `tracing_hooks.rs`,
`task_registry.rs` and `snapshot/` when enabled. No production module is
excluded from this scope. The unsafe inventory below highlights critical
boundaries rather than claiming to enumerate every unsafe expression or prove
all configurations correct.

The unsafe `ScopedTimer::arm` in `src/timer_api.rs` forwards to `Timer::start`.
The caller must provide initialized local scheduler/timer services and an expiry
callback that does not sleep, unwind, enable IRQs, reenter timer dispatch or
synchronously cancel itself. The owned Timer retains callback storage and
revalidates CPU routing after migration. Its Drop drains delivery before freeing
storage; the caller must leave callback-needed locks free and the target CPU
able to progress, including when returning from the scoped body.

Test-only integration in `ut-tests/task_lifecycle.rs` and `ut-tests/tests_timer_routing.rs` is covered
as validation infrastructure, not as a production service. The prepared-timer
routing driver must run both workers concurrently on the requested CPUs and
join them before returning. `ktimer_core` owns timer scenario storage and
cleanup checks; `ktask` owns task placement and completion. These tests do
not establish hardware timer interrupt or IPI delivery correctness.

Dependency internals (including `karch`, `kspin`, `kcpu_slot`, `ksched` and
`ktimer_core`) are outside this crate's implementation analysis because their
owners define and enforce those contracts. Their contracts at the `ktask`
boundary remain in scope. See the core's [security analysis](../../ktimer-core/docs/security.md)
for intrusive membership, callback lifetime and timer cancellation invariants.
Production IRQ/hardware integration services the prepared core as its only
general timer source. Async sleep/timeout, timerfd, network and watchdog sampling
use owned timers; the legacy waker map and periodic callback vector are removed.

### Idle Wake Boundary

The final work check and architecture wait share a `NoPreemptIrqSave` guard.
The check never clears `need_resched`; an IRQ handled before the check therefore
cannot erase its own wake request. Later IRQs are covered by the architecture's
pending-wake contract. No runqueue lock is retained, and IRQ-tail scheduling
stays excluded until the wait returns. Mask restoration precedes preemption
restoration. Availability still depends on enabled controller wake sources and
the IPI producer's publish-before-notify contract, not on a periodic timer tick.

With `arm-timer-resume-fixup`, an outer no-preempt guard spans the before-wait
sample and the timer repair after IRQ restoration. Pending work cannot schedule
the idle task away before that repair completes; logging and timer notifications
do not extend the IRQ-off wait window.

### Prepared Timer Platform Boundary

`Timer::new` reads the platform CPU ID after per-CPU initialization and passes
it to the core's checked initial-route constructor. The snapshot is not an
affinity guarantee: migration after the read cannot invalidate storage, and
start still selects and locks its actual destination. No IRQ or queue service
is performed during preparation. CPU bounds are checked before storage is
allocated; an invalid platform CPU is treated as an initialization invariant
failure. Allocation and callback lifetime guarantees are unchanged.

#### Protected Asset

| Object / type | Protection requirements |
| --- | --- |
| `timers.rs::PREPARED_TIMERS`: `TimerSet<{ kbuild_config::NR_CPUS }>` | Keep this `pub(crate)` static inaccessible outside `ktask`, at a stable address for the entire kernel lifetime, outliving all registrations, IRQ dispatch and IPI service. Access queue and owner state only through the core's lock protocol. Every registration must keep its node and handler fixed and live until pending and running accesses, including callback cleanup, have drained; the static set's lifetime alone does not retain borrowed timer storage. |

`api.rs::on_timer_fire` calls unsafe `dispatch_due` only on the matching CPU,
with IRQs disabled and no timer/runqueue lock held. The static set outlives all
service. Registration must retain node/handler storage and obey the core's
nonrecursive, non-unwinding callback contract; tests use owning `Timer` handles.
`api.rs::rearm_local_timer` calls unsafe `reprogram_local_timer` under a CPU/IRQ
guard. Its programmer only merges deadlines and writes hardware, never callbacks
or scheduler locks. These are the new unsafe production call sites.

The periodic deadline is CPU-local and changes only with local IRQs and
preemption excluded. A busy-to-busy switch retains it; an idle switch clears
it. A delayed IRQ advances the phase once, without a loop of callbacks. It
shares the existing arbitration and suspend gate; it does not own a timer
node. The existing accounting hook continues to run on every timer IRQ.

| Threat | Asset / trigger / impact | Response |
| --- | --- | --- |
| Stale hardware write | Deadline integrity; concurrent remote advance overwritten by an old snapshot delays delivery | Mitigate: hold the prepared CPU lock through hardware write; notify actual destination after every accepted start. Platform must preserve publish-before-IPI ordering. |
| Lock inversion | Availability; notification or callback re-enters a held timer lock and deadlocks | Mitigate: callback outside locks, sender releases registration locks before notification; the programmer runs under the prepared CPU lock and takes no runqueue lock. |
| Wrong-context delivery | Memory safety and scheduling; an IPI or recursive service invokes a timer callback | Mitigate: IPI endpoint only reprograms; timer IRQ owns dispatch, with IRQs disabled. Real-IRQ and notification-only tests check this separation. |
| Premature boot service | Per-CPU state; notification drain touches an uninitialized scheduler or timer service | Mitigate: retain existing runtime enablement after scheduler and local timer state initialization; static set construction alone is not readiness. |
| Lost independent deadline | Availability; empty/deferred prepared queue suppresses scheduler or periodic deadlines | Mitigate: merge all sources under the local IRQ guard; prepared backoff does not defer other sources. |

`tests_timer_irq.rs` adds bounded real-IRQ/IPI validation infrastructure. Its
owning handles drain callbacks before destruction. These tests do not establish
a hard latency bound, complete allocation coverage, or exhaustive interleavings.

### Owned Async Wait Boundary

| Asset / type | Protection requirements |
| --- | --- |
| `timers.rs::Timer<H>` containing `ktimer_core::Timer<H, NR_CPUS>` | Private owned node/handler storage is retained through dispatch cleanup; no raw node, mutable handler or storage owner is exposed. `handler()` returns only a shared borrow tied to the timer; handler locks must not be held across synchronous drain. Start pins execution with IRQs masked and refreshes the actual owner only after releasing core locks. |
| `future/time.rs::SleepHandler(SpinNoIrq<SleepState>)` | Completion and poll serialize `is_expired` and `Option<Waker>` under the same lock. Waker clone, wake and destruction occur outside it. Owned-timer Drop drains delivery before handler storage is released. |

| Threat | Trigger / impact | Response |
| --- | --- | --- |
| Lost sleep wake | IRQ completes between readiness check and waker publication; wait never resumes | Mitigate: both operations use one state lock; completion publishes the terminal state before taking the waker. Poll after completion returns ready. |
| Callback use-after-free | Future is dropped while its callback still accesses wake state | Mitigate: core-owned timer Drop synchronously drains callback and cleanup. No second independent wake-state lifetime is introduced. |
| Wait/callback deadlock | An executor drops the future from its own wake callback, or while holding a callback-required lock | Accepted caller constraint: do not synchronously tear down this wait in those contexts; core cancellation spins and cannot force progress. Kernel wakers schedule tasks rather than polling/dropping futures inline. |
| Waker reentry under lock | Custom waker clone/drop/wake takes a task lock or reenters polling | Mitigate: invoke waker operations outside the sleep-state lock. Executor wakers must still support the existing nonblocking, non-unwinding hard-IRQ wake contract. |
| Early long-wait return | Sleep duration exceeds the core's nanosecond horizon | Mitigate: cap only the sleep adapter at the supported clock horizon; it remains pending while the clock is below that horizon. Raw `Timer::start` rejects such input instead. |

`timers.rs::Timer::{start,start_if_future}` are the new unsafe adapters. Their
guards establish CPU/IRQ stability, the core owns storage, and callers retain
the initialized-service and callback-context obligations. The private async
constructor supplies the existing kernel IRQ-waker contract. Preparation still
allocates once and allocator exhaustion follows kernel policy; no bounded pool
or allocation-free sleep construction is claimed.

A fixed reschedule IPI does not carry a task pointer; multiple requests may
coalesce. Send happens after task-state and run-queue updates. The receiver
only marks the current task and must not touch a dest `RunQueue` that the
sender may still hold mutably. Bring-up still waits for runtime drain rather
than treating CPU presence as "scheduler ready". An SMP build without
preemption still needs a hardware kick: the send path must not be compiled
out with the IPI handler.

`ktask` sits on the kernel concurrency and schedule path. It contains
`unsafe`, per-CPU raw references, context-switch pointers, and cross-CPU
task state. A broken invariant can cause use-after-free on exit/reclaim,
data races on per-CPU mutable aliases, duplicate enqueue or concurrent
switch, or NMI / diagnostic walks of illegal objects.

This crate does not parse user memory. Syscalls still reach it indirectly
through `sleep` / wait / `join` / affinity. The security boundary is kernel
internal concurrency correctness and memory safety.

```text
kruntime timer irq / irq manager / kspin guards
        │
        ▼
┌───────────────────────────────────────────────────┐
│ ktask                                             │
│  - task.rs: TaskInner / CurrentTask / context     │
│  - run_queue/mod.rs: per-CPU slots and registry   │
│  - run_queue/placement.rs: CPU choice, no remote  │
│    scheduler lock                                 │
│  - run_queue/rq.rs: leave / pick / switch / kick  │
│  - run_queue/migrate.rs: affinity                 │
│  - future / wait_queue: block / wake bridge       │
│  - timers: tick callbacks                         │
│  - snapshot / task_registry (optional)            │
└───────────────────────────────────────────────────┘
        │
        ▼
ksched algorithms / karch context switch / allocator
```

- Trust `karch` context-switch primitives and the stack-switch ABI.
- Trust `kspin` guard acquire/release order and the preempt hook wiring.
- Trust `kcpu_slot` CPU-local access.
- Trust `ksched` not to return an illegal entity state.

## External boundaries and validation responsibilities

The public APIs defined by `ktask` do not accept raw-pointer or FFI
parameters, dereference user buffers, or parse device, network, file,
firmware, or bootloader payloads. This does not mean the crate has no
external inputs: trusted kernel adapters pass user-influenced values and
hardware-driven events across the following boundaries. User-memory access
and permission checks belong to those adapters, not to the scheduler.

| Boundary / entry | Inputs and direction | Local check / responsibility |
|---|---|---|
| `Timer::new`, `Timer::handler`, `Timer::start`, `Timer::try_cancel`, `Timer::cancel_sync` | Trusted kernel owners supply handlers and absolute monotonic deadlines; shared handler access exposes only that owner's typed state | Core validates deadline range; unsafe start requires initialized service and IRQ-compatible callbacks. Cancellation is not a permanent shutdown. Owners must not block callback progress during synchronous teardown. |
| `spawn_raw`, `spawn_with_name`, `spawn`, `prepare_task`, `activate_task`, `spawn_task`; `TaskInner::new_user`, `new_kthread`, `new_pidless_kthread`, `set_name` | Kernel task/process code supplies closures, names, stack sizes, identities, and runtime objects | Names may originate from users but arrive as Rust strings; task creation is not a user-buffer parser. Runtime and entry implementations remain trusted kernel code. |
| `set_prio`, `set_task_prio`, `set_current_affinity`, `set_task_affinity`, `TaskInner::set_cpumask` | Scheduler/process adapters supply priorities and typed CPU masks, possibly from syscalls | Adapters enforce permissions. Callers must distinguish initial placement from migrating an already-active task. |
| `sleep`, `sleep_until`, `interruptible_sleep_until`, `wait_for_poll_event_until`; `WaitQueue::wait_timeout`, `wait_timeout_until` | Wait clients supply durations, deadlines, wakers and predicates | User timeout decoding belongs upstream. Callers must obey sleepability and callback-context contracts. All timeout delivery uses owned timers; no raw waker registration API remains. |
| `future::poll_io`; `WaitQueue` notify; `wake_task`, `interrupt_task`, `signal_task`, `TaskInner::interrupt` | File, network, device, and signal adapters deliver readiness or wakeup | `poll_io` forwards the operation's typed result without interpreting payloads. `signal_task` requires pending state to be published and signal locks released before wakeup. Readiness and payload validation remain with the operation/provider. |
| `on_timer_fire`, `register_timer_irq_note`, `register_sched_trace_hooks` | Runtime interrupt glue and kernel instrumentation provide timer events and function callbacks | IRQ routing is external. Callbacks must satisfy their documented execution-context constraints. |
| `TaskInner::set_waiting_lock`, `push_held_lock`, `pop_held_lock`; `snapshot::trigger`, `dump_cpu_tasks`, `nmi_dump_all` | Kernel diagnostic callers supply lock-address identities, reason strings, and CPU selections | These are not user-pointer access capabilities. Diagnostic output can expose task metadata; access control belongs to the invoking diagnostic interface. |

The private `extern "C" fn task_entry()` in `src/task.rs` is a parameterless
architecture context-switch trampoline, not a public FFI input service. Raw
task/context pointers and test `RawWaker` callbacks are internal
implementation boundaries covered by the lifetime invariants below.
MMIO/PIO, DMA, and hardware timer register access belong to HAL/drivers
rather than a public payload API in this crate. The prepared-timer test
integration adds no new external-input API.

### Per-CPU run-queue access invariants

- The local `kcpu_slot` raw helpers resolve a long-lived shared `&RunQueue`
  for the current CPU.
- `RUN_QUEUES` publishes long-lived remote shared references through
  `Once<usize>`.

## Preempt-check stack bound

A wake after IRQ restore can republish `need_resched`. If run-queue guard
drop recursed into another preempt check, successive requests would keep old
frames from returning, walk past the task stack, and corrupt adjacent
objects. Heap-allocated task stacks have no unmapped guard page; growing
the stack does not replace eliminating that unbounded growth.

The corresponding per-CPU invariants are:

1. Each CPU's `RUN_QUEUE` is accessible only after `init` / `init_secondary`.
2. `CurrentRunQueueRef` and the remote `KRunQueueRef` hold only a shared
   `&RunQueue`; state changes go through the scheduler lock or atomic fields,
   and the guard expresses an execution-context constraint, not Rust
   uniqueness.
3. `RUN_QUEUES` completes its initialization writes before any read, and
   remote callers only obtain `&RunQueue`; remote state changes must go
   through the scheduler `SpinRaw` or atomic fields, never a whole-object
   `&mut`.
4. The atomic operations on `last_accounted_ns` only provide interior
   mutability; every read/write still requires the scheduler lock, and
   `Ordering::Relaxed` is not itself a cross-field synchronization protocol.
5. Runtime mutation of `PREV_TASK` goes through its
   per-CPU `SpinNoIrq` lock; `NoPreempt` only fixes the CPU while resolving
   the slot and does not replace the inner lock's object exclusivity. No
   blocking is allowed under the lock. Incoming-stack cleanup takes the owner
   and unlocks before cleanup and final ownership release.

The check consumes new requests in a loop and keeps an extra preempt-disable
count while releasing its own guard. After release it pairs the temporary
count with `enable_preempt(false)` so drop cannot reenter scheduling. Keep
the 1 → 2 → 1 → 0 pairing, keep newly published `need_resched`, and still
honor disable-count and exception-context entry limits. No extra cross-CPU
reentry flag or shared lock was added.

## Unsafe inventory

### `run_queue/mod.rs`: per-CPU raw mutable references and the registry

| Entry | Role |
|---|---|
| `RunQueueRegistry::try_get` | Restores a `Once<usize>` address to `&'static RunQueue` |
| `current_run_queue_ptr` / `current_run_queue_ref` | Current-CPU `RUN_QUEUE` `NonNull<RunQueue>` / shared `&'static RunQueue` |
| `with_idle_task` / `PREV_TASK` access | Same-CPU idle / outgoing switch-owner slots |
| `init` / `init_secondary` | `CurrentTask::init_current` at bring-up |

`RUN_QUEUES` is no longer a `MaybeUninit` array: each slot is `Once<usize>`
and is written once by that CPU's `init` / `init_secondary`. `get` panics if
unregistered. `placement.rs` spawn ranking uses `try_get` and skips CPUs
that have not finished bring-up.

Invariants:

1. A CPU's `RUN_QUEUE` is reachable only after `init` / `init_secondary`.
2. `&'static RunQueue` from `try_get` does not mean the caller already
   holds that CPU's IRQ/preempt exclusion. Local paths rely on IRQ/preempt
   off; cross-CPU paths rely on the dest RQ `SpinRaw` (`nr_running` /
   `nr_home` are atomics). Do not hold two `&mut RunQueue` aliases for the
   same CPU. Boot `init` / `init_secondary` disable local IRQs before the
   first scheduler `SpinRaw` and leave that mask in place; entry firmware
   is not the source of the mask. AArch64 sets `DAIF.I` in SPSR only when
   dropping from EL2/EL3.
3. Each `RUN_QUEUES` slot is written once at that CPU's `init` /
   `init_secondary`.

Entries concentrate in `current_run_queue*` / `get_run_queue`.
`placement.rs` only reads remote heuristics through those entries.
`migrate.rs` / `placement.rs` add no new `unsafe`.

### `run_queue/rq.rs`: context switch and `CurrentTask` replacement

`RunQueue::switch_to` switches through raw context pointers and
maintains cross-CPU visibility with `on_cpu` / `PREV_TASK`.
On SMP, in the same block as `set_on_cpu(true)`, it also stores the
incoming task's `holder_key` in that CPU's `CPU_RUNNING_TASK` slot
(`Ordering::Relaxed`) via `note_cpu_running_task`. Remote CPUs load the
slot with `Ordering::Relaxed` in `is_running_on` so an optimistic spinner
can decide whether to keep spinning. The key is not a pointer and is not a
synchronization or handoff word: `on_cpu` / `PREV_TASK` remain that protocol.
`finish_task_switch` is an `unsafe fn` and must run once on the incoming
task's stack after switching, before the next `PREV_TASK` is written. It
takes the outgoing strong reference out of that CPU's slot, then releases
it or transfers it to a deferred wake enqueue outside the slot lock.

Invariants:

1. Local IRQs are already off at switch.
2. `prev` / `next` context pointers are valid and the stacks have not been
   reclaimed.
3. `clear_prev_task_on_cpu()` pairs with the blocked-task wake handoff:
   SeqCst store/load plus a `SeqCst` fence between store and the other
   load (Dekker memory-model requirement; not a TCG-only workaround) so at
   least one side finishes enqueue. Do not IRQ-off-spin waiting for remote
   `on_cpu`.
4. `CPU_RUNNING_TASK` is a relaxed hint beside the `on_cpu` / `PREV_TASK`
   handoff, not part of it. SMP `switch_to` publishes the incoming task's
   `holder_key` before the context switch. A same-task switch returns before
   that store; the slot already names the task from `CurrentTask::init_current`
   or the switch that made it current. Spinners compare the stored key with a
   recorded lock holder. A missing per-CPU area is skipped and reads as not
   running. A stale key only misjudges whether spinning pays off. The next
   switch overwrites the slot; `finish_task_switch` does not clear it and
   does not wait for readers.

### `task.rs`: `TaskContext` interior mutability and architecture current-task pointers

- Shared reads of `UnsafeCell<TaskContext>`
- `CurrentTask::init_current` installs the initial task pointer;
  `prepare_switch` transfers task owners without publishing. The scheduler
  passes the incoming pointer and its matching context to the architecture
  switch, retaining the outgoing owner for switch cleanup.
- `TaskStack` manual allocate/free

Invariants:

1. Task context is mutated only on the controlled schedule path or the current
   user-task root replacement boundary, with IRQs/preemption excluded.
2. After scheduler initialization the current-task pointer names a live task.
   `kcpu` provides architecture-specific publication and observation; it does
   not own task references. No read may separate CPU selection from the load
   across migration. The outgoing slot owner survives publication and remains
   in the handoff until its old stack is inactive. Local NMI readers finish
   before interrupted replacement resumes; this is not a remote raw-pointer
   ownership-acquisition protocol.
3. The stack is freed only after the task is no longer running and has no
   remaining refs; DeferredArc then destroys it on a normal worker.

`api.rs::init_scheduler_with_cpu_num` registers the permanent kernel root once,
while boot still uses it and before any task is constructed.
`task.rs::TaskContextCell::new` passes that root to
`kcpu::TaskContext::new_kernel`; the architecture implementation initializes a
kernel-only context without reading the creator's active user root. Kernel tasks
retain no ownership of their creator's MM. The registered root is backed by the
kernel-lifetime address space; secondary task construction reads the same
initialized value. Shared-root architectures save the supplied permanent root;
split-root architectures retain their separate user-root default. A user-task
creator must explicitly install its runtime-owned root before publishing the task.

`api.rs::replace_current_user_page_table` mutates only the running task's saved
root and activates it on that CPU. Its unsafe caller must retain valid old/new
roots, preserve kernel mappings, publish the matching process binding, prepare
the AArch64 ASID locally, and exclude outstanding context borrows. IRQ/preemption
exclusion prevents concurrent scheduling mutation. The helper follows the
architecture switcher's root-write/flush ordering; it neither drops MM owners
nor calls runtime hooks. User entry remains forbidden until image/context setup
is complete. The unittest root reader returns a value under IRQ/preemption
exclusion and does not add a mutable context escape.

### `task.rs`: immutable user-runtime slot

`UserRuntimeSlot` stores the runtime in
`UnsafeCell<Option<KBox<dyn UserTaskRuntime>>>`. The runtime is written once
in `TaskInner::new_user()` and never mutated again, so the slot no longer
needs a publication state machine (the old `EMPTY -> INSTALLING -> READY`
protocol and `install_user_runtime()` are gone).

The owner is allocated by the caller before task construction. Unsizing into the
runtime trait object preserves its address and does not allocate. Runtime hooks,
publication synchronization and deferred finalization are unchanged. Dropping an
unprepared `TaskInner` still destroys its runtime in the caller's context. Tests
check pointee address preservation, direct unprepared destruction, and deferred
destruction after last-owner release under IRQ exclusion. User entries are
prepared fallibly before clone state; stack allocation remains infallible.

Invariants:

1. `UserRuntimeSlot::ready()` is the only constructor; the runtime is filled
   before the task is shared.
2. Task publication (`publish_user_task` / `activate_task`) is the
   happens-before edge for readers.
3. The runtime is immutable for the task lifetime, so `get()` shared
   references stay valid.

### `timers.rs` / `task_registry.rs` / `snapshot`

- `timers` binds owned core storage and publishes the timer IRQ heartbeat hook.
- `task_registry` stores inline weak refs behind atomic reader admission.
- `snapshot` holds trap-frame snapshots in `UnsafeCell` and `unsafe impl Sync`.

Invariants:

1. A diagnostic slot publishes initialized Weak with Release; counted Acquire
   admission is required before borrowing it. EMPTY and publishing/retired
   states cannot admit readers.
2. Retirement closes admission; only zero readers permit moving Weak out.
   The value is moved before publishing EMPTY, so slot reuse cannot overwrite
   a borrowed Weak. Actual Weak Drop may occur on the final reader's CPU.
3. Snapshot reads/writes obey session serialization (`begin` / `finish`).
4. Owned timer storage is retained through callback cleanup. The core owns
   membership and routing; `ktask` preserves owner notification and IRQ context.
5. `register_timer_irq_note` only stores a `'static fn()`. The timer IRQ
   path Acquire-loads then transmutes the call; the hook must be short and
   non-blocking.

## Memory and lifecycle invariants

1. **Task state transitions.** `Running` / `Ready` / sleeping / `Exited`
   transitions go through run-queue entries. Sleeping states distinguish
   event-only, interruptible and killable waits in the existing atomic state.
   `transition_state` rejects duplicate-wake reentry. Signal wakeup matches
   the sampled sleeping state, rather than waking a later wait of another kind.
2. **Deferred preemption.** Schedule timer / wake only set `need_resched`.
   The real switch happens at the `enable_preempt` safe point so a critical
   section cannot switch mid-lock. If this CPU is still in an active
   exception context, preemption stays deferred until the exception/IRQ
   trapframe guard is released.
3. **SMP blocked-wake protection.** If a Blocked task is still in remote
   `switch_to`, the waker publishes dest RQ / wake flags and returns. The
   outgoing CPU uniquely claims and finishes enqueue with an atomic swap
   when clearing `on_cpu`. All four handshake operations are SeqCst, with a
   `SeqCst` fence between store and the other load (Dekker; do not weaken
   to acquire/release). Without that fence, real x86 and QEMU TCG can both
   skip enqueue. Do not wait for remote progress in an IRQ-off region.
4. **Wake versus preempt-request split.** A waker only makes the task
   `Ready` and sets local or remote `need_resched`. The real switch still
   happens at a preempt safe point. A remote IPI only sets pending and does
   not account/refresh the dest RQ. An immediate schedule request consumes
   the oneshot slot instead of looping a 10 µs timer. `smp` without `ipi`
   is a compile-time reject.
5. **Exit reclaim isolation.** CPU current ownership survives scheduler-owner
   release and transfers into `PREV_TASK` until incoming-stack cleanup.
   DeferredArc submits final destruction to a normal worker; the switch path
   never destroys the Task payload inline.
6. **Idle special case.** Idle does not enter the ordinary scheduler-entity
   path and does not participate in `update_current`, so algorithm metadata
   is not polluted.
7. **Publish before runnable.** Callers that need extra object-graph
   registration must `prepare_task()`, finish external publish, then
   `activate_task()`, so a task cannot run before it is registered. PID 1
   follows the same rule: `new_user()` constructs a complete runtime, then
   `publish_user_task()` publishes it, then activate. There is no
   post-runnable extension install.

## Threat analysis

| ID | Asset / threat | Severity | Trigger | Existing response and residual risk |
|---|---|---|---|---|
| T-01 | Per-CPU `&'static mut` alias causes UB | High | Concurrent mutable borrows on the same CPU | Guard plus IRQ/preempt exclusion; access is concentrated behind wrappers. Residual: a new path that bypasses those wrappers. |
| T-02 | Switch and wake race: double enqueue, lost wake, or IRQ-off deadlock | High | Remote CPU still switching while this CPU unblocks immediately | SeqCst Dekker handoff (flags store/swap × `on_cpu` store/load plus a non-weakened `SeqCst` fence) and unique claim in `clear_prev_task_on_cpu`. Do not wait for remote `on_cpu`. |
| T-03 | Exited task freed too early (UAF) | High | Joiner or switch path still holds a ref | Per-task exit `Completion` wakes joiners; PREV_TASK protects the outgoing stack; DeferredArc delays final destruction. |
| T-04 | `need_resched` reenters scheduling at the wrong time | Medium | Direct preempt switch inside a critical section or while an exception/IRQ trapframe guard is held | Only set pending; check at the `enable_preempt` safe point; delay the real switch while in active exception context. |
| T-05 | Diagnostic slot reuse races a reader or stale token | High | Early retirement/replacement | Counted immutable borrows, closed admission before removal, one-shot registration token, worker/NMI-safe Weak release. |
| T-06 | Snapshot race reads the wrong trap frame | Medium | Concurrent snapshot sessions | `begin`/`finish` session serialization and per-CPU slot isolation. |
| T-07 | Affinity migrate race drops a task | Medium | Concurrent mutation during migrate | `migrate_current` is under the run-queue critical section. `enforce_affinity_placement` holds the RQ lock for `remove_task` of a queued ready task, is mask-only when the task does not occupy a CPU, and waits in `stop_one_cpu` for a current or remote running task. The stacked `CpuStopDone` stays live until the stopper stores `notify_done` after `notify_one` returns, so the waiter's `Event` is not freed while notify still writes its heap inner. A blocked task is not rehomed on this path; its wake publishes under the destination run-queue lock. |
| T-07f | Affinity migrate publishes a still-`on_cpu` task | High | Yield/preempt requeue as `Ready` before `switch_to`; a remote `setaffinity` dequeues and enqueues on another CPU, which resumes the stack the old CPU is still saving | `dequeue_ready_task_waiting_off_cpu` dequeues only when `!on_cpu` under the source RQ lock; otherwise it releases the lock and spins until the old CPU clears `on_cpu`. A detached seat (`dequeue` returns `None` while already off-CPU) returns immediately, so a prepared task stays mask-only. Idle is never queued and is excluded from the wait. |
| T-07a | `setaffinity` succeeds but the task stays on a forbidden CPU | High | Non-current path only writes the mask, or a deferred wake enqueues on the CPU chosen before the mask change | Success requires the task not occupy a forbidden CPU (running/queued must move; unpublished is mask-only); otherwise `false`/`EBUSY`. `preempt_resched`/`yield` force affinity migrate. `publish_task` rejects a forbidden CPU under the scheduler and entity locks before `attach_owner` and `enqueue`, so `cpu_id` and `nr_home` stay with the previous owner. The caller reselects through `migrate_entry` with the same `EnqueueKind`. Residual: a caller already IRQ-off retries inside that section, so a moving mask can delay timers and IPIs until a publish succeeds. Residual: `enforce_affinity_placement` samples `cpu_id` before the owner lock, so an in-flight publish that already observed the old mask can enqueue on a CPU the new mask forbids and `setaffinity` still returns success. |
| T-07b | A running-task leave path skips deactivate and leaves scheduler residue | High | A new leave path bypasses the unified API | All leaves go through `leave_current`. EEVDF `curr` is non-owning. `pick_next` asserts. |
| T-07c | `blocked_resched` without an extra strong ref frees the task at switch | High | Caller did not clone current | rustdoc `# Panics`; `#[track_caller]` plus `strong_count > 2`; `block_on` clones first; unittest `blocked_resched_survives_with_caller_owned_ref`. |
| T-07d | Prepared wait blocks the wrong task or an atomic context | High | Handle moved across a coroutine, or `park` after taking a lock | Handle is `!Send`/`!Sync`. Prepare checks context before allocation; `park` asserts task identity and context. Callers still own lock constraints that cannot be checked dynamically. |
| T-07e | Early or spurious wake loses a notification or completes early | High | Predicate check races `park`, or the scheduler wakes the task alone | Reuse `KWaker` dual latch checks and the RQ handshake; hold a strong task ref across switch; `park` only returns a wake hint; the caller rechecks the predicate and owns event registration. |
| T-08 | A long or repeatedly rearmed callback stalls scheduling | High | Callback abuse or repeated already-due rearm | API requires short, non-blocking hardirq callbacks. Core dispatch bounds catch-up rounds for deadlines reached during delivery. Residual: it cannot interrupt a callback or bound one round that continually rearms at or before its fixed cutoff. |
| T-09 | Remote wake is not scheduled promptly | Medium | Task is on a remote RQ but that CPU has not reached a preempt safe point | SMP requires IPI. Remote IPI only sets `need_resched`. A failed probe arms backup hrtick in dest `preempt_resched`. Residual: IPI send failure or a late safe point. |
| T-09b | IRQ teardown wait blocks the current task in the wrong context | High | Hardirq/softirq/BH-disabled path indirectly calls the `TaskCompletionWaitIf` provider | `TaskBlockingContextIf::is_sleepable_context` checks sleepable task context before IRQ teardown mutates actions; rejection maps to `IrqDescError::InvalidContext`. `CompletionWait::prepare` independently rejects invalid current, IRQ, BH, preemption and RCU context, even for a signaled completion. Subsequent scoped registration cannot fail. |
| T-09g | Completion notification accesses a retired stack waiter | High | Concurrent notification races synchronous wait return | kcompletion owns pinned scoped nodes and serializes callback delivery with unlinking; the private Sync wake capability borrows a locked latch and strong task ref without retaining either borrow. |
| T-09h | Completion callback inverts scheduler locks | High | Exit publishes completion or a waiter parks concurrently | Exit accounting releases the scheduler lock before notify_exit; parking never acquires completion locks, and notify releases the latch before scheduler enqueue. The run-queue context guard is not a scheduler lock. |
| T-09f | Softirq schedule query consumes the task request early | Medium | A batch-boundary query clears `need_resched` or schedules directly | `SoftirqDaemonIf::needs_resched()` only Acquire-reads. Real scheduling stays at a safe point. Returns false with no task. |
| T-09c | Workerqueue wait bypasses `kworkqueue_ng` lifecycle predicates | High | A `ktask` wait helper blocks without letting `kworkqueue_ng` recheck work state | `ktask` only supplies yield/wait. Work state, cancel/flush predicates, and the deadlock gate stay in `kworkqueue_ng`. |
| T-09d | Custom/dynamic workqueue wrongly creates a dedicated task | Medium | Queue-level API reintroduces host/stop/wake or stores a `WorkQueueHandle` | Normal worker/manager tasks belong to `kworkerpool_ng`. Logical queues share pool tasks; queue closure may wait through `ktask` but does not own or destroy those tasks. |
| T-09e | Interrupt-like context calls `ktask::sleep*()` directly | High | BH workqueue callback, softirq action, hardirq, or BH-disabled path bypasses the upper context gate | `sleep()`, `sleep_until()`, and `interruptible_sleep_until()` check `kirq::context::is_in_interrupt_context()` and fail-fast. `yield_now()` is not a sleep/blocking API. |
| T-10 | Absolute timer deadline truncates on integer conversion | High | `as_nanos()` `u128` is narrowed to `u64` | The raw owned timer API rejects deadlines beyond `u64` nanoseconds without changing the prior setting. Async waits saturate to that horizon. HAL backends clamp at the hardware register boundary. |
| T-11 | An already-due schedule slot is rearmed repeatedly in a non-preemptible region | High | An immediate request is rewritten as a periodic 10 µs hrtick | A due slot only sets pending and is cleared immediately. Owned timer deadlines stay independent. |
| T-12 | Idle-pull steals a still-`on_cpu` task | High | Remote steal after yield/preempt requeue and before `switch_to` finishes | Steal only Ready && `!on_cpu`. `can_idle_pull_task` rejects `on_cpu`. |
| T-12a | Steal changes `cpu_id` and moves prev away | Medium | Wake follows the new CPU after idle-pull migrate-in | Fair wake uses `select_idle_sibling` (home if prev is idle, otherwise seek idle). RT wake uses `find_lowest_rt_cpu`. |
| T-12b | Idle-pull mid-state is missed by `setaffinity` | High | Ready on no RQ after steal and before dest enqueue | That window is mask-only, same as unpublished. Dest `enqueue_task` rejects a forbidden CPU before enqueue and `migrate_entry` publishes the steal as `MigrateIn`. |

## Failure modes and handling

The `SyncWake` contract extends through scheduler callbacks. Intrusive scheduler
queues and numeric park/switch diagnostics avoid allocations; any installed
tracing implementation must also avoid allocation, unwinding, and reentry into
completion or predicate queues. Legacy allocating trace-record/name-cache hooks
are a prerequisite to resolve before claiming the entire wake chain satisfies
this contract.

The unsafe kcompletion host seam also requires waiter stack storage to survive
callbacks that bypass destructors. `CompletionWait` retains a strong current
task reference in its borrowed handle; `TaskInner::kstack` is freed only when
TaskInner is destroyed. A readiness predicate calling non-unwinding task exit
leaks that reference and its stack, rather than freeing a linked node. This is
a resource leak, not permission to cancel waits by exiting. Normal returns and
unwinding unlink before releasing the handle.

Keyed sources may borrow the same notifier, but must serialize unlink against
notification before ending that borrow. Killable parking checks pending state
after `BlockedKillable` publication; checking only before publication could lose a remote
signal wake. The runtime query serializes with pending-state writers, which must
release their state lock before scheduler wakeup. Returning from parking is only
a hint: the source must recheck grants and cancellation, and cannot discard an
already granted resource. Diagnostic hooks are nonblocking and may not reenter
source or scheduler locks. Context preparation does not authorize later parking
while holding a spinlock or after changing execution context.

| ID | Failure mode | Cause | Local effect | System effect | Severity (1–4) | Controls |
|---|---|---|---|---|---|---|
| F-01 | Access `RUN_QUEUES` before init | Bring-up ordering error | panic / UB | Scheduler unavailable | 1 | Each CPU writes its `Once` slot in `init` / `init_secondary`. `get` panics if unregistered. Spawn placement `try_get`s and skips CPUs that are not up. |
| F-02 | Empty `cpumask` | Caller set an illegal affinity | Task has no runnable CPU | API failure or panic | 2 | `set_task_affinity` returns false on empty. |
| F-02a | Remote running affinity cannot move | Stopper not up, or still running on a forbidden CPU after the stop | Caller sees failure | Caller may retry. A failed queue restores the previous mask; a stop that ran leaves the new mask | 3 | `enforce_affinity_placement` returns false → syscall `EBUSY`. |
| F-03 | A NoPreempt critical section is held too long | An overly heavy path | Preempt latency grows | Jitter / worse realtime | 3 | Keep critical sections short; avoid heavy work under the guard. |
| F-04 | Final reclamation lags | External strong owners persist or the shared finalizer stalls | Task payloads remain allocated | Memory growth | 2 | Release independent owners; avoid ownership cycles and destructors waiting for later finalizers. |
| F-05 | Remote resched request is lost or consumed late | IPI send failure, or no safe point before EL1 IRQ return | Woken task is delayed | Throughput / latency jitter | 3 | SMP requires IPI. Send pending only (do not account/refresh dest RQ in the IPI). Arm backup hrtick on a failed probe. Record send failure. Suspend the exception mark after IRQ completion for a tail check. |
| F-06 | Algorithm `update_current` / `next_preemption_ns` misbehaves | Scheduler implementation bug | Distorted preempt policy | Starvation / jitter | 2 | `ksched` unittests plus trace-hook diagnostics. |
| F-07 | Soft-timer deadline truncates to an already-due hardware time | Deadline exceeds the hardware nanosecond range | Timer fires immediately and repeatedly | IRQ storm; task and network paths stall | 1 | Typed deadline through the HAL; backend uses checked conversion and clamps out-of-range values to the farthest representable deadline. |
| F-08 | Near-future rearm becomes due again during callback delivery | Delivery outlasts the new deadline | Repeated catch-up rounds | CPU pressure and delayed timer delivery | 1 | Limit catch-up rounds and back off prepared service; independent scheduler deadlines remain eligible. Network/watchdog use completion-relative periods. A callback that loops or continually rearms at/before the fixed round cutoff remains invalid usage and can prevent IRQ exit. |
| F-09 | Idle-pull and remote wake invert lock order | Dest holds its lock then locks src | Deadlock | Scheduler stops | 1 | Lock only src, then dest enqueue. `resched` has already dropped the dest scheduler lock. |
| F-10 | Affinity missed in the idle-pull window | `setaffinity` after steal and before dest enqueue | Task runs outside the new mask | Isolation violation | 2 | Recheck cpumask under the dest scheduler and entity locks before enqueue. Enforce is mask-only when the task does not occupy a CPU. |
| F-11 | A running user task never sees a pending signal | `interrupt()` only wakes waiters and does not kick the CPU; NOHZ has stopped the clock | Target stays in a user busy loop | Timed signals (for example SIGALRM) are delayed forever; the workload hangs | 2 | `interrupt()` sets local `need_resched` or sends a reschedule IPI for an `on_cpu` task. The user-return path rechecks `is_interrupted` after preempt. |
| F-12 | Mutex-wait watchdog false-reports a long wait | Unsigned wrap subtraction of an old tick taken before the scan against a concurrently updated start tick | A normal short wait is treated as timeout | Watchdog wrongly takes the stop path | 1 | Sample the current tick after reading each task's wait snapshot. Discard the round if the start time is later than that sample. |

Failure policy:

- **Fail fast.** Critical invariants use `assert!` (task state, IRQ
  constraints, CPU ids).
- **Deferred recovery.** Preemption uses pending plus a safe-point switch so
  scheduling resumes from a consistent state.
- **Reclaim tolerance.** DeferredArc waits for the final strong owner and transfers
  destruction to a normal worker without a separate exit queue.
- **Diagnostics.** `snapshot` / `watchdog` share a task registry for lock-wait
  and backtrace checks. Mutex-wait elapsed rejects a future sample produced
  by a concurrent update. `watchdog_hardlockup` checks hard lockups in NMI
  context and needs root Kconfig `KFEAT_NMI`.

There is no crash-recovery mechanism.

## Privacy, limitations and verification

`ktask` does not process user private data. Information that may be exposed:

- task name, task id, CPU id (logs and tracing)
- backtraces (when snapshot/watchdog are enabled)
- per-CPU aggregate schedule counters (when `sched_stat` is enabled,
  `/proc/sched_stat` exposes them without task identity)

These are kernel diagnostic outputs, gated by the log channel and build
features. Access policy belongs to the invoking diagnostic interface. Memory
is not explicitly scrubbed on drop here.

Known limits:

1. There is no periodic load-balance daemon. Cross-CPU movement depends on
   affinity, wake `select_idle_sibling`, spawn idle-first plus `nr_home`,
   and idle-pull of one Ready `!on_cpu` waiter from a CPU with
   `nr_running >= 2` when about to idle. `on_cpu` must be rejected; only src
   is locked. `add_task` always `kick(Decision)` (including a busy home) so
   a new peer is not invisible after the dynamic timer is disarmed.
   EEVDF: wake PLACE_LAG plus a full-request deadline; involuntary preempt
   first `peer_preempts_curr` then decides `leave_current(Preempt)`; wake
   nominates NEXT_BUDDY even when `curr` is empty (covers the remote
   leave→pick window). `with_wake_sync` (futex, `Task::interrupt`, and pipe
   data handoff) may sync-preempt an eligible buddy (still requires
   eligibility; skips the deadline compare that Linux `preempt_sync`
   keeps). NEXT_BUDDY and the sync mark are armed under one
   scheduler lock so a remote CPU cannot snatch the buddy between two
   acquires. A running task must leave through unified `leave_current`.
   EEVDF `curr` is not a task-lifetime owner.
2. Fair `select_run_queue` is idle-first (`nr_running == 0`) then `nr_home`.
   Fair `select_wake_run_queue` matches `select_idle_sibling` (home if prev is
   idle, otherwise `nr_running == 0` from `load_count`; stay on prev if none idle; `find_idlest`
   only when home is outside the cpumask and no idle CPU exists). A
   `with_wake_sync` waker with `nr_running == 1` nominates this CPU
   (`wake_affine_idle`). Unless `sync_handoff_target` vetoes it (affinity
   only, today), the wakee stays here without an IPI to an idle sibling.
   Linux may still choose an idle cache-sharing prev, so this handoff is
   stronger and can serialize a two-sided compute pipeline. That pipeline is
   unmeasured. Do not close it by vetoing an idle or remote prev: `lat_ctx`
   depends on pulling the wakee off an idle home.
   `select_wake_run_queue` reads one `wake_carries_sync` result and returns it
   in `WakeRunQueueRef`, which is the only path to the EEVDF sync mark.
   Hardirq and an active softirq clear that result. The IRQ-exit tail does
   not: `wake_softirqd` runs after the hardirq depth and the softirq batch
   guard are both gone, so the interrupted task's flag can mark the pinned
   ksoftirqd wake. Do not treat that window with
   `is_in_interrupt_context` (it also drops BH-disabled task context, where
   `current` is the waker). Do not add a separate sticky-home idle overflow
   before SIS. RT spawn and wake use `find_lowest_rt_cpu` against
   `rt_preempt_rank` instead of idle-first. The snapshot is written when the
   scheduler lock drops after `charge`, a `curr` change, or `set_attr`.
   `next_event_ns` does not rewrite it.
3. SMP must enable `ipi`: a remote wake's `kick` sets `need_resched`
   through IPI. `interrupt()` of a running task (signal delivery) uses the
   same IPI; signal progress must not depend on a later periodic tick.
   `smp && !ipi` is a compile-time reject. IPI callbacks must not lock or
   reprogram dest's scheduler.
4. `on_timer_fire` defers hardware rearm across a wake batch and then
   refreshes the schedule slot from `next_event_ns`. `account_sched_tick`
   (`check_preempt_tick`) runs only when the schedule slot is due;
   otherwise it only accounts.
   Immediate preempt pending from `switch_to` lands on the incoming task.
   Under NOHZ, running-entity wall time is flushed before add/wake.
5. The `unsafe` surface is still large and should keep shrinking into
   smaller wrappers with complete `SAFETY` comments.

`ut-tests/run_queue/tests.rs` and `ut-tests/task_lifecycle.rs` cover wake affinity, blocked
resched, and related invariants. `tests_timer_routing.rs` covers prepared
timer routing. Test source existence is not execution proof.

## Audit checklist

- New `unsafe` blocks have a clear `SAFETY:` invariant.
- New run-queue access paths keep guard acquire/release paired.
- Placement changes stay in `run_queue/placement.rs`, leave/pick/switch in
  `rq.rs`, affinity in `migrate.rs`. Do not put remote load scans in
  `ksched`.
- New block/wake logic keeps `TaskState` transitions one-way and correct.
- `smp` paths consider the `on_cpu` / migrate concurrency window.
- Exit and reclaim avoid early free and leaked refs.
- Tick / preempt changes do not switch mid-critical-section.
- Wake-handoff / memory-order changes follow the IK9KW6 3-way TCG steps in
  `docs/design.md`. Do not substitute one-VM hammering or a guest stress
  unittest.
- Do not move a ready task to another RQ while `on_cpu` is still true
  (idle-pull must reject it; affinity migrate must wait for it to clear).
- Idle-pull locks only src and goes through `steal_ready_task` /
  `enqueue_task` PLACE_LAG.
- Idle-pull's dest publish rejects a forbidden CPU before `attach_owner`
  and enqueue, leaving `cpu_id` / `nr_home` with the previous owner.
  `migrate_entry` then publishes that steal as `MigrateIn`.
- Affinity dequeue of a ready task uses `dequeue_ready_task_waiting_off_cpu`
  (via `with_owner_sched`). Under the source RQ lock it dequeues only when
  `!on_cpu`; otherwise it drops the lock and spins until `finish_task_switch`
  clears `on_cpu`, then retries. A miss while already off-CPU is a detached
  seat and returns without retrying.
- `setaffinity` of a Ready task that does not occupy a CPU (unpublished /
  idle-pull gap) is mask-only and succeeds; it does not spin or return
  `false`.
- Residual: `enforce_affinity_placement` samples `cpu_id` before the owner
  lock. An in-flight `publish_task` that already observed the old mask can
  enqueue on a CPU the new mask forbids, and that `setaffinity` still
  returns success.
- `nr_running` is charged only on publish/enqueue and discharged on
  block/migrate/exit.
- `nr_home` is kept across block and discharged only on exit or a home
  change.
- Fair spawn is idle-first (`nr_running == 0`) then `nr_home`. RT spawn uses
  `find_lowest_rt_cpu`.
- `interrupt()` / unmasked signal delivery to a running task kicks the dest
  CPU (local `need_resched`, remote IPI) instead of relying only on waiter
  wake or a periodic tick.
- Fair wake goes home if prev is idle, otherwise `select_idle_sibling`, and
  stays on prev if no idle CPU exists. `with_wake_sync` and `nr_running == 1`
  keeps the wakee on the waker CPU unless `sync_handoff_target` vetoes it. An
  idle prev is not a veto. Hardirq and an active softirq do not apply that
  handoff; the IRQ-exit tail can still leak it onto ksoftirqd's enqueue mark.
  RT wake uses `find_lowest_rt_cpu`.
- `set_prio` / `set_task_prio` keep the task's policy. Lock order is the
  owner run-queue scheduler lock, then `sched_entity_lock`. `cpu_id` is
  rechecked after both locks. A move retries; an unregistered owner panics
  instead of spinning with the IRQ guard. `publish_task` and
  `switch_to_local` store `cpu_id` only inside that hold. A deferred wake
  records `wake_cpu` and leaves `cpu_id` on the CPU that still owns the
  entity until enqueue.

### Adjustable-clock waits

Registration errors propagate rather than being swallowed or treated as expiration.
Generation is sampled before reading the deadline predicate and rechecked after
registration. The IRQ-waker and cancellation-context restrictions of `TimerFuture`
still apply.

## Local clock-event PM boundary

The local event lifecycle trusts a privileged PM owner to park execution and
quiesce timer producers/repair, preserve CPU ownership, restore interrupt
controllers and mappings, restore MONOTONIC and settle sleep before final rearm.
It does not authenticate a real sleep or mask all external programming paths.
Calling legacy low-level HAL/backend operations or idle repair directly during
the transition violates this coordination contract. No userspace entry or new
device pointer is exposed. Hardware accesses remain in existing backend helpers.

| ID | Threat | Impact | Implemented control and residual obligation |
|---|---|---|---|
| TE-01 | Late timer IRQ or ordinary reprogramming uses a frozen clock | Medium | Scheduler arbitration is gated and paused timer IRQs return before sampling or callbacks. PM must separately quiesce backend repair and low-level bypasses. |
| TE-02 | Old or wrong-CPU token restores unrelated hardware | Medium | CPU identity, checked generation and phase validation precede all device operations; identities never wrap. PM must prevent scheduling/migration across calls. |
| TE-03 | Resume replays an obsolete deadline | Medium | Existing arbitration reads live scheduler/prepared slots and holds its timer lock through programming. No cached hardware compare is restored; callbacks run only through later IRQ delivery. |
| TE-04 | Early abort or lost token leaves events stopped | Medium | Explicit restore/resume also handles abort. must_use and private token fields retain ownership; Drop does not guess hardware/time readiness. Forgotten tokens require PM recovery and are unsupported. |
| TE-05 | Nested pause breaks timer wake-batch suppression | Medium | Separate per-CPU state, with entry rejected in a deferred wake batch or IRQ/NMI context. |

State uses the existing CPU-slot abstraction with IRQ/preemption guards and
adds one unsafe StaticSlotValue implementation for EventState: its Copy u64 and
fieldless enum contain no references, ownership, destructor or self-relative
addresses, so copying the template into independent CPU areas is valid. No raw
register access or manual Send/Sync implementation is added. Tests use scoped
local event restoration and owned callback storage, cancelled before IRQs are
unmasked. State-machine tests do not touch the live event. The tests do not
establish real device-power-loss recovery, NMI quiescence or multi-CPU PM order.
The feature processes no user private data.

## Cooperative freezer

The freezer APIs change scheduler progress, not hardware state. The low-level
`freeze_tasks` still requires caller-owned target coverage. System PM uses
`freeze_system_tasks`: its lifecycle registry is authoritative for admitted
ordinary tasks and admission defers new execution until cleanup. Kernel
infrastructure exemptions remain an explicit caller/platform obligation; they
cannot include user tasks or be inferred from scheduler Blocked state.

Only explicit checkpoints and permanent exit confirm quiescence. Blocking or
preemption while holding a mutex is not a safe point. Callers must release PM
and recovery dependencies before checkpointing. Deadlock or absent cooperation
is reported by timeout while IRQs and logical time still run, before the PM
caller can freeze clocks. Cancellation removes all requests and wakes partial
acknowledgments. Once a confirmed guard is returned, cancellation cannot thaw
it under hardware recovery. Its owner must drop it only when resources are safe.

Per-task request slots use `SpinNoIrq`: publication, checkpoint, cleanup and
incoming-stack completion can run on different CPUs. No slot holder enters the
scheduler, waits, allocates or destroys its final Arc under that lock. Scheduler
blocking takes the wake latch before the request predicate; publishers release
the slot before taking scheduler locks. The existing deferred-wake/on_cpu
handshake is retained, including its store-load fences.

A checkpoint parks in Frozen, a state excluded from ordinary wakeup. Confirmation
is published only after switching stacks. The switch tail captures a specific
request before remote enqueue becomes possible; a late completion cannot confirm
a replacement round. An old checkpoint retains only the released old round.
Release-before-park is checked after Frozen publication and uses an explicit
state-selected wake, so cancel/drop cannot strand a late arrival. Completed exit
and registry retirement occur on the incoming stack rather than at early Exited
publication. A request arriving during exit gets completion notification as well.

The owned deadline timer uses the existing initialized, allocation-free
completion IRQ callback and is drained before return. Remote notification failure
or missing safe points prevents success and reaches the bounded timeout. Task
freezing does not stop scheduler infrastructure, CPUs, interrupts, DMA or device
access by exempt tasks. The kpm platform-suspend entry retains its UP restriction;
physical SMP suspend still requires a reversible CPU rendezvous and backend.

### Publication admission and registration lifetime

The diagnostic registry is intentionally not a PM source: it is optional and may
drop registrations at capacity. System preparation instead owns an unbounded
intrusive list with a preallocated node per non-idle task. Registration carries
a weak owner while admitted; permanent exit and destructor fallback unlink it
under the same registry lock. Its sole unsafe list removal checks the Active
phase under that lock, establishing that the node belongs to this exact list;
`cursor_mut_from_ptr` is undefined for a node outside it. Admission links only
Prepared registrations, which are unlinked, so the list's duplicate-insertion
panic is unreachable.
Only this module mutates its links, and removal releases the list reference
outside the registry guard. There are no new unsafe Send/Sync implementations.

An activation before closure is represented in the snapshot even if enqueue is
still pending; an activation after closure receives a strong deferred owner.
The registry-to-registration lock order is fixed, and registry locks are dropped
before scheduler enqueue or notifications. Exit may already own a run-queue
lock; no registry path takes that lock in the opposite order. IRQ-safe locking
also covers activation and retirement called under scheduler IRQ exclusion.

Snapshot storage is reserved outside the IRQ-safe lock after admission closes.
Only retirement can change active membership before the fill, so pushes cannot
reallocate under the lock. Dropping a completed system guard thaws before
reopening admission. Gate rollback drains deferred owners on all ordinary
error paths. Forgetting a guard is not a supported PM cancellation mechanism.

Private scheduler fixtures select infrastructure exemptions and invoke the
shared freeze implementation under one continuously owned publication gate.
They never reopen admission between exemption and target snapshots. A regression
fixture activates a non-cooperative task in that interval and checks that it
stays deferred until the frozen guard is released. Public entry validation is
still covered separately; this fixture does not exempt production PM tasks.

This is scheduler admission, not a ban on allocating detached task objects or
updating process-domain visibility. Those objects cannot execute until admitted.
Explicitly exempt kernel workers can still run, so platform preparation must
drain/suppress their device accesses before resources disappear. Missing safe
points refuse suspension by timeout rather than silently claiming global
quiescence. No new production worker exemptions or real platform PM are enabled.

### Explicit freezable wait wakeup

`PreparedTaskWait::park_freezable` rejects user/non-sleepable contexts before
setting its scoped flag. A request wakes only an explicit participant; it does
not convert arbitrary blocking into a freeze acknowledgment. Confirmation still
requires checkpoint entry after dependency release and an incoming-stack
scheduler confirmation.
A sleeping callback using ordinary waits remains unconfirmed and can time out.

The request is published before reading the opt-in flag, and no request lock is
held while waking the scheduler. The existing blocked-state stop predicate
rechecks the slot under scheduler exclusion; this closes both publication-before-
park and wake-before-block races. Its run-queue-to-request lock order has no
reverse publisher edge. The flag is task-context-only, independent atomic state;
no IRQ reader or raw pointer is added. It introduces no new unsafe code or
Send/Sync implementation. The prepared handle retains its task and remains
neither Send nor Sync. Ordinary waker hints survive confirmed freezing, while
request cancellation never independently releases a successfully returned guard.
