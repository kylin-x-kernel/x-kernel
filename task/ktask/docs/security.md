# ktask — Security and reliability

## Scope and trust model

`api::is_sleepable_context` accepts no external input and adds no unsafe boundary.
It rejects missing or idle current tasks, interrupt-like/IRQ-masked contexts,
and preemption protection when tracked. Sleep assertions and prepared waits
share this gate to avoid inconsistent acceptance. This mitigates invalid-context service startup/stop;
the caller remains responsible for dependency cycles and provider readiness.

This document covers the security boundaries of the entire `task/ktask` crate:
the `lib.rs` integration and `api.rs` entry points; task lifetime and context
in `task.rs`; scheduling in `run_queue.rs`; blocking in `future/` and
`wait_queue.rs`; timers in `timers.rs`; IRQ wait and daemon integration in
`irq_wait.rs` and `softirqd.rs`; and diagnostics in `tracing_hooks.rs`,
`task_registry.rs` and `snapshot/` when enabled. No production module is
excluded from this scope. The unsafe inventory below highlights critical
boundaries rather than claiming to enumerate every unsafe expression or prove
all configurations correct.

Test-only integration in `tests.rs` and `tests_timer_routing.rs` is covered
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

| Threat | Asset / trigger / impact | Response |
| --- | --- | --- |
| Stale hardware write | Deadline integrity; concurrent remote advance overwritten by an old snapshot delays delivery | Mitigate: hold the prepared CPU lock through hardware write; notify actual destination after every accepted start. Platform must preserve publish-before-IPI ordering. |
| Lock inversion | Availability; notification or callback re-enters a held timer lock and deadlocks | Mitigate: callback outside locks, sender releases registration locks before notification; the programmer runs under the prepared CPU lock and takes no runqueue lock. |
| Wrong-context delivery | Memory safety and scheduling; an IPI or recursive service invokes a timer callback | Mitigate: IPI endpoint only reprograms; timer IRQ owns dispatch, with IRQs disabled. Real-IRQ and notification-only tests check this separation. |
| Premature boot service | Per-CPU state; notification drain touches an uninitialized scheduler or timer service | Mitigate: retain existing runtime enablement after scheduler and local timer state initialization; static set construction alone is not readiness. |
| Lost independent deadline | Availability; empty/deferred prepared queue suppresses scheduler deadlines | Mitigate: merge both sources under the local IRQ guard; prepared backoff does not defer other sources. |

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
│  - run_queue/gc.rs: delayed exit reclaim          │
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
| `future::poll_io`; `WaitQueue` notify; `wake_task`, `interrupt_task`, `TaskInner::interrupt` | File, network, device, and signal adapters deliver readiness or wakeup | `poll_io` invokes a caller-supplied operation and forwards its typed result without interpreting payloads. Readiness is published by the owning subsystem, not generic IRQ subscriptions. Payload validation remains with the operation/provider. |
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
5. Runtime mutation of `EXITED_TASKS` and `PREV_TASK` goes through their
   per-CPU `SpinNoIrq` locks; `NoPreempt` only fixes the CPU while resolving
   the slot and does not replace the inner lock's object exclusivity. No
   blocking is allowed under the lock, and GC drops the lock before running a
   potentially slow task drop.

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
| `RunQueueRegistry::try_get` | Restores a `Once<usize>` address to `&'static mut RunQueue` |
| `current_run_queue_ptr` / `current_run_queue_mut` | Current-CPU `RUN_QUEUE` `NonNull` / `&'static mut` |
| `current_exited_tasks_mut` / `current_wait_for_exit` / `current_idle_task` / `current_prev_task_mut` | Same-CPU GC / idle / `PREV_TASK` slots |
| `init` / `init_secondary` | `CurrentTask::init_current` at bring-up |

`RUN_QUEUES` is no longer a `MaybeUninit` array: each slot is `Once<usize>`
and is written once by that CPU's `init` / `init_secondary`. `get` panics if
unregistered. `placement.rs` spawn ranking uses `try_get` and skips CPUs
that have not finished bring-up.

Invariants:

1. A CPU's `RUN_QUEUE` is reachable only after `init` / `init_secondary`.
2. `&'static mut RunQueue` from `try_get` does not mean the caller already
   holds that CPU's IRQ/preempt exclusion. Local paths rely on IRQ/preempt
   off; cross-CPU paths rely on the dest RQ `SpinRaw` (`nr_running` /
   `nr_home` are atomics). Do not hold two `&mut RunQueue` aliases for the
   same CPU.
3. Each `RUN_QUEUES` slot is written once at that CPU's `init` /
   `init_secondary`.

Entries concentrate in `current_run_queue*` / `get_run_queue`.
`placement.rs` only reads remote heuristics through those entries.
`migrate.rs` / `placement.rs` add no new `unsafe`.

### `run_queue/rq.rs`: context switch and `CurrentTask` replacement

`RunQueue::switch_to` switches through raw context pointers and
maintains cross-CPU visibility with `on_cpu` / `PREV_TASK`.
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
   remaining refs (gc task + `Arc::try_unwrap`).

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
`UnsafeCell<Option<Box<dyn UserTaskRuntime>>>`. The runtime is written once
in `TaskInner::new_user()` and never mutated again, so the slot no longer
needs a publication state machine (the old `EMPTY -> INSTALLING -> READY`
protocol and `install_user_runtime()` are gone).

Invariants:

1. `UserRuntimeSlot::ready()` is the only constructor; the runtime is filled
   before the task is shared.
2. Task publication (`publish_user_task` / `activate_task`) is the
   happens-before edge for readers.
3. The runtime is immutable for the task lifetime, so `get()` shared
   references stay valid.

### `timers.rs` / `task_registry.rs` / `snapshot`

- `timers` binds owned core storage and publishes the timer IRQ heartbeat hook.
- `task_registry` stores weak-ref slots with `Box::into_raw` / `from_raw`.
- `snapshot` holds trap-frame snapshots in `UnsafeCell` and `unsafe impl Sync`.

Invariants:

1. A `task_registry` slot holds 0 or a valid `Box<WeakKtaskRef>` pointer.
2. The CAS winner is responsible for release.
3. Snapshot reads/writes obey session serialization (`begin` / `finish`).
4. Owned timer storage is retained through callback cleanup. The core owns
   membership and routing; `ktask` preserves owner notification and IRQ context.
5. `register_timer_irq_note` only stores a `'static fn()`. The timer IRQ
   path Acquire-loads then transmutes the call; the hook must be short and
   non-blocking.

## Memory and lifecycle invariants

1. **One-way task state machine.** `Running` / `Ready` / `Blocked` /
   `Exited` transitions go through run-queue entries.
   `transition_state` rejects duplicate-wake reentry.
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
5. **Exit reclaim isolation.** Exited tasks first enter `EXITED_TASKS` and
   are reclaimed later by the per-CPU `gc_task`, so the switch path does not
   drop them.
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
| T-03 | Exited task freed too early (UAF) | High | Joiner or switch path still holds a ref | Per-task exit `Completion` wakes joiners; `EXITED_TASKS` + `gc_task` + `Arc::try_unwrap` delay reclaim. |
| T-04 | `need_resched` reenters scheduling at the wrong time | Medium | Direct preempt switch inside a critical section or while an exception/IRQ trapframe guard is held | Only set pending; check at the `enable_preempt` safe point; delay the real switch while in active exception context. |
| T-05 | `task_registry` pointer-slot corruption | High | Illegal write or double free | CAS protocol, 0/ptr two-state constraint, weak-ref upgrade check. |
| T-06 | Snapshot race reads the wrong trap frame | Medium | Concurrent snapshot sessions | `begin`/`finish` session serialization and per-CPU slot isolation. |
| T-07 | Affinity migrate race drops a task | Medium | Concurrent mutation during migrate | `migrate_current` is under the run-queue critical section. `enforce_affinity_placement` holds the RQ lock for `remove_task` of a queued ready task, is mask-only when the task does not occupy a CPU, and waits for remote preempt to finish a running task. |
| T-07a | `setaffinity` succeeds but the task stays on a forbidden CPU | High | Non-current path only writes the mask | Success requires the task not occupy a forbidden CPU (running/queued must move; unpublished is mask-only); otherwise `false`/`EBUSY`. `preempt_resched`/`yield` force affinity migrate. |
| T-07b | A running-task leave path skips deactivate and leaves scheduler residue | High | A new leave path bypasses the unified API | All leaves go through `leave_current`. EEVDF `curr` is non-owning. `pick_next` asserts. |
| T-07c | `blocked_resched` without an extra strong ref frees the task at switch | High | Caller did not clone current | rustdoc `# Panics`; `#[track_caller]` plus `strong_count > 2`; `block_on` clones first; unittest `blocked_resched_survives_with_caller_owned_ref`. |
| T-07d | Prepared wait blocks the wrong task or an atomic context | High | Handle moved across a coroutine, or `park` after taking a lock | Handle is `!Send`/`!Sync`. Prepare checks context before allocation; `park` asserts task identity and context. Callers still own lock constraints that cannot be checked dynamically. |
| T-07e | Early or spurious wake loses a notification or completes early | High | Predicate check races `park`, or the scheduler wakes the task alone | Reuse `KWaker` dual latch checks and the RQ handshake; hold a strong task ref across switch; `park` only returns a wake hint; the caller rechecks the predicate and owns event registration. |
| T-08 | A long or repeatedly rearmed callback stalls scheduling | High | Callback abuse or repeated already-due rearm | API requires short, non-blocking hardirq callbacks. Core dispatch bounds catch-up rounds for deadlines reached during delivery. Residual: it cannot interrupt a callback or bound one round that continually rearms at or before its fixed cutoff. |
| T-09 | Remote wake is not scheduled promptly | Medium | Task is on a remote RQ but that CPU has not reached a preempt safe point | SMP requires IPI. Remote IPI only sets `need_resched`. A failed probe arms backup hrtick in dest `preempt_resched`. Residual: IPI send failure or a late safe point. |
| T-09b | IRQ teardown wait blocks the current task in the wrong context | High | Hardirq/softirq/BH-disabled path indirectly calls the `IrqSyncWaitIf` provider | `kirq` context-gates before entering the provider. `ktask` only supplies the blocking mechanism and does not relax IRQ-sync API constraints. |
| T-09f | Softirq schedule query consumes the task request early | Medium | A batch-boundary query clears `need_resched` or schedules directly | `SoftirqDaemonIf::needs_resched()` only Acquire-reads. Real scheduling stays at a safe point. Returns false with no task. |
| T-09c | Workerqueue wait bypasses `kworkqueue_ng` lifecycle predicates | High | A `ktask` wait helper blocks without letting `kworkqueue_ng` recheck work state | `ktask` only supplies yield/wait. Work state, cancel/flush predicates, and the deadlock gate stay in `kworkqueue_ng`. |
| T-09d | Custom/dynamic workqueue wrongly creates a dedicated task | Medium | Queue-level API reintroduces host/stop/wake or stores a `WorkQueueHandle` | Normal worker/manager tasks belong to `kworkerpool_ng`. Logical queues share pool tasks; queue closure may wait through `ktask` but does not own or destroy those tasks. |
| T-09e | Interrupt-like context calls `ktask::sleep*()` directly | High | BH workqueue callback, softirq action, hardirq, or BH-disabled path bypasses the upper context gate | `sleep()`, `sleep_until()`, and `interruptible_sleep_until()` check `kirq::context::is_in_interrupt_context()` and fail-fast. `yield_now()` is not a sleep/blocking API. |
| T-10 | Absolute timer deadline truncates on integer conversion | High | `as_nanos()` `u128` is narrowed to `u64` | The raw owned timer API rejects deadlines beyond `u64` nanoseconds without changing the prior setting. Async waits saturate to that horizon. HAL backends clamp at the hardware register boundary. |
| T-11 | An already-due schedule slot is rearmed repeatedly in a non-preemptible region | High | An immediate request is rewritten as a periodic 10 µs hrtick | A due slot only sets pending and is cleared immediately. Owned timer deadlines stay independent. |
| T-12 | Idle-pull steals a still-`on_cpu` task | High | Remote steal after yield/preempt requeue and before `switch_to` finishes | Steal only Ready && `!on_cpu`. `can_idle_pull_task` rejects `on_cpu`. |
| T-12a | Steal changes `cpu_id` and moves prev away | Medium | Wake follows the new CPU after idle-pull migrate-in | Wake uses `select_idle_sibling` (home if prev is idle, otherwise seek idle). Matches Linux musical chairs. |
| T-12b | Idle-pull mid-state is missed by `setaffinity` | High | Ready on no RQ after steal and before dest enqueue | That window is mask-only, same as unpublished. Dest rechecks cpumask after enqueue and `migrate_entry`s if dest is not allowed. |

## Failure modes and handling

| ID | Failure mode | Cause | Local effect | System effect | Severity (1–4) | Controls |
|---|---|---|---|---|---|---|
| F-01 | Access `RUN_QUEUES` before init | Bring-up ordering error | panic / UB | Scheduler unavailable | 1 | Each CPU writes its `Once` slot in `init` / `init_secondary`. `get` panics if unregistered. Spawn placement `try_get`s and skips CPUs that are not up. |
| F-02 | Empty `cpumask` | Caller set an illegal affinity | Task has no runnable CPU | API failure or panic | 2 | `set_task_affinity` returns false on empty. |
| F-02a | Remote running affinity cannot move | No preempt/IPI, or a late safe point | Caller sees failure | Caller may retry; the mask may already be updated | 3 | `enforce_affinity_placement` returns false → syscall `EBUSY`. |
| F-03 | A NoPreempt critical section is held too long | An overly heavy path | Preempt latency grows | Jitter / worse realtime | 3 | Keep critical sections short; avoid heavy work under the guard. |
| F-04 | GC reclaim lags | External holders keep `Arc`s | `EXITED_TASKS` piles up | Memory growth | 2 | Retry `Arc::try_unwrap`; join semantics release refs. |
| F-05 | Remote resched request is lost or consumed late | IPI send failure, or no safe point before EL1 IRQ return | Woken task is delayed | Throughput / latency jitter | 3 | SMP requires IPI. Send pending only (do not account/refresh dest RQ in the IPI). Arm backup hrtick on a failed probe. Record send failure. Suspend the exception mark after IRQ completion for a tail check. |
| F-06 | Algorithm `update_current` / `next_preemption_ns` misbehaves | Scheduler implementation bug | Distorted preempt policy | Starvation / jitter | 2 | `ksched` unittests plus trace-hook diagnostics. |
| F-07 | Soft-timer deadline truncates to an already-due hardware time | Deadline exceeds the hardware nanosecond range | Timer fires immediately and repeatedly | IRQ storm; task and network paths stall | 1 | Typed deadline through the HAL; backend uses checked conversion and clamps out-of-range values to the farthest representable deadline. |
| F-08 | Near-future rearm becomes due again during callback delivery | Delivery outlasts the new deadline | Repeated catch-up rounds | CPU pressure and delayed timer delivery | 1 | Limit catch-up rounds and back off prepared service; independent scheduler deadlines remain eligible. Network/watchdog use completion-relative periods. A callback that loops or continually rearms at/before the fixed round cutoff remains invalid usage and can prevent IRQ exit. |
| F-09 | Idle-pull and remote wake invert lock order | Dest holds its lock then locks src | Deadlock | Scheduler stops | 1 | Lock only src, then dest enqueue. `resched` has already dropped the dest scheduler lock. |
| F-10 | Affinity missed in the idle-pull window | `setaffinity` after steal and before dest enqueue | Task runs outside the new mask | Isolation violation | 2 | Recheck cpumask after dest enqueue. Enforce is mask-only when the task does not occupy a CPU. |
| F-11 | A running user task never sees a pending signal | `interrupt()` only wakes waiters and does not kick the CPU; NOHZ has stopped the clock | Target stays in a user busy loop | Timed signals (for example SIGALRM) are delayed forever; the workload hangs | 2 | `interrupt()` sets local `need_resched` or sends a reschedule IPI for an `on_cpu` task. The user-return path rechecks `is_interrupted` after preempt. |
| F-12 | Mutex-wait watchdog false-reports a long wait | Unsigned wrap subtraction of an old tick taken before the scan against a concurrently updated start tick | A normal short wait is treated as timeout | Watchdog wrongly takes the stop path | 1 | Sample the current tick after reading each task's wait snapshot. Discard the round if the start time is later than that sample. |

Failure policy:

- **Fail fast.** Critical invariants use `assert!` (task state, IRQ
  constraints, CPU ids).
- **Deferred recovery.** Preemption uses pending plus a safe-point switch so
  scheduling resumes from a consistent state.
- **Reclaim tolerance.** GC requeues on `Arc::try_unwrap` failure instead of
  freeing too early.
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
   leave→pick window). `with_wake_sync` (futex and `Task::interrupt`) may
   sync-preempt an eligible buddy (still requires eligibility; does not
   bypass EEVDF). NEXT_BUDDY and the sync mark are armed under one
   scheduler lock so a remote CPU cannot snatch the buddy between two
   acquires. A running task must leave through unified `leave_current`.
   EEVDF `curr` is not a task-lifetime owner.
2. `select_run_queue` is idle-first (`nr_running == 0`) then `nr_home`.
   `select_wake_run_queue` matches `select_idle_sibling` (home if prev is
   idle, otherwise `nr_running == 0` from `load_count`; stay on prev if none idle; `find_idlest`
   only when home is outside the cpumask and no idle CPU exists). Do not add
   a separate sticky-home idle overflow before SIS.
3. SMP must enable `ipi`: a remote wake's `kick` sets `need_resched`
   through IPI. `interrupt()` of a running task (signal delivery) uses the
   same IPI; otherwise a NOHZ lone runner never leaves user mode.
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

`src/run_queue/tests.rs` and `src/tests.rs` cover wake affinity, blocked
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
  (idle-pull must reject it).
- Idle-pull locks only src and goes through `steal_ready_task` /
  `enqueue_task` PLACE_LAG.
- Idle-pull rechecks cpumask after dest enqueue and `migrate_entry`s if dest
  is not allowed.
- `setaffinity` of a Ready task that does not occupy a CPU (unpublished /
  idle-pull gap) is mask-only and succeeds; it does not spin or return
  `false`.
- `nr_running` is charged only on publish/enqueue and discharged on
  block/migrate/exit.
- `nr_home` is kept across block and discharged only on exit or a home
  change.
- Spawn is idle-first (`nr_running == 0`) then `nr_home`.
- `interrupt()` / unmasked signal delivery to a running task kicks the dest
  CPU (local `need_resched`, remote IPI) instead of relying only on waiter
  wake or a periodic tick.
- Wake goes home if prev is idle, otherwise `select_idle_sibling`, and stays
  on prev if no idle CPU exists.
