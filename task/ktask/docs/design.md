# ktask — Design

## Test Layout

Guest cases and scheduler probes live in `ut-tests/`, mounted by
`unittest::path_mod_test!` at their original scopes. SMP gates remain on
timer-routing and on-CPU tests.

The former host `src/tests.rs` required real scheduler initialization and is
now `ut-tests/task_lifecycle.rs`, using the boot-initialized scheduler.
Spawn/yield, floating-point value preservation, wait queues, join exit codes
and deferred activation remain covered. The IRQ completion case is already
covered by `ut-tests/irq_wait/tests.rs` and is not duplicated. Fixtures join
workers and use persistent wait predicates instead of relying on a yield to
imply waiter enrollment. The floating-point value test does not establish
hardware FP-register coverage on soft-float targets.

## Purpose and ownership

`is_sleepable_context` provides a nonblocking context query for execution-service
startup and joined teardown. Sleep assertions and prepared waits reuse this
query: it requires an initialized non-idle task, no interrupt-like context,
enabled local IRQs and, when tracked, no preemption protection. Explicit RCU
read participation also makes the task non-sleepable without disabling preemption.
It neither grants permission across later context changes nor detects dependency
cycles or held sleepable locks. Workerpool callers query before taking service
locks; task and timer providers must still be initialized before startup.

Remote reschedule and a running-task signal kick use the fixed
`kipi::notify_reschedule` notification. They do not enqueue a closure on the
ordinary IPI callback queue. The static `RescheduleIpiIf` implementation only
calls the existing `on_remote_resched_ipi` path to set the current task's
preempt pending bit. It does not account runtime or reprogram timers. With
preemption compiled out the handler is empty, but an SMP signal kick still
sends the IPI so the CPU leaves user mode. Local kick, task ownership, and
migrate lock order keep the existing protocol.

`ktask` is X-Kernel's kernel task runtime and scheduling orchestration layer.
It owns task create/exit, block/wake, per-CPU run queues, preemption control,
timer-driven events, and optional snapshot/watchdog diagnostics.

It does not implement hardware IRQ dispatch (`khal` / `kruntime`) or class
algorithms (`ksched`). It assembles those capabilities into one task model at
runtime.

Callers that need switch paths, preemption timing, SMP run-queue behavior, or
new scheduling/diagnostic hooks should start here.

## Background

Each task embeds `krcu_core::TaskReader`; `ktask` owns its storage and task
lifetime, not system RCU state or consumer scope policy. Permanent CPU watching
and reader-node arrays, cleanup notifications, and the GP service belong to
[`krcu`](../../../sync/krcu/docs/design.md). The static
`krcu_core::RcuContextIf` contract connects scheduler and idle boundaries to
that owner without making `ktask` depend on the RCU service.

Sleepability, blocking and voluntary yielding reject active reader nesting;
involuntary preemption remains permitted. Before same-task selection returns
or a real switch occurs, the scheduler calls `RcuContextIf::note_context_switch`
with a `SchedulerObservation` constructed unsafely for the retained outgoing
reader and CPU under IRQ/preemption exclusion. The static call consumes that
context immediately; the core defines its safety contract.
The provider drains finished membership, preserves active readers on their
original node across migration, and reports only after enrollment. Before GP
service construction it still maintains enrollment. No callback or wakeup runs
inside this scheduler boundary.

The preemption checkpoint makes one `RcuContextIf::check_reader_cleanup` call,
even without a reschedule request. All RCU cleanup predicates, context checks
and removal policy belong to `sync/krcu/src/context.rs`; cleanup is not
a CPU observation. `current_preemption_disabled` queries the generic task
preemption state. `with_preempt_check_deferred` balances temporary preemption
credit without changing IRQ state or scheduling on restoration;
`request_current_reschedule` retains a request without waking or sending an IPI.
The RCU owner uses these capabilities for its cleanup and private scope exits;
`ktask` does not own their notification policy.

Like Linux `default_idle_call`'s `ct_cpuidle_enter/exit`, idle checks retained
reschedule demand first, drains finished membership while watched, and brackets
only `karch::idle_wait` with quiescence. One `NoPreemptIrqSave` spans CPU lookup,
both transitions and the wait. Balanced IRQ/NMI entries may temporarily activate
watching; their HAL return tail remains RCU-free and cannot schedule before idle
restores watching. Restoration precedes guard release and timer repair.
No explicit read scope or protected borrow may span the idle window.

`rcu_reader_for_integration` is an unsafe borrow for audited integration, not
consumer admission. Its caller retains the task and obeys current-task,
strictly nested handler and core mutation contracts. Private read, wait and
owning-consumer fixtures live under `sync/krcu/ut-tests`; task-local tests retain
coverage of reader lifetime and scheduler boundaries.

For permanent task exit, `finish_task_switch` clears residual participation
on the incoming stack, after publishing incoming current but before releasing
the outgoing owner or on-CPU marker. It diagnoses abandoned scopes after unlinking.
The scheduler releases its exit owner before switching; the CPU current owner
survives in PREV_TASK until incoming-stack cleanup. DeferredArc submits the
actual last release to a normal worker, without an exit queue or GC task.

Owned timer preparation uses `ktimer_core::Timer::new_on` with a snapshot of the
current logical CPU after platform per-CPU initialization. The platform accessor
protects the CPU-ID read; allocation does not require extending that critical
section. This avoids an unnecessary first-arm migration for same-CPU use. If
preparation and start occur on different CPUs, the existing start routing moves
the node normally. Timer IRQ readiness remains a start-time requirement.

X-Kernel splits class algorithms from the task runtime:

- `ksched` supplies the Fair/Idle class arbitrator (`Scheduler`).
- `ktask` owns lifecycle, current-task context, per-CPU run queues, switch,
  and block/wake.

`ktask` implements `kspin::KernelGuardIf` so `kspin` guard acquire/release
hooks the task preempt count: a critical section defers the preemption check
until it exits.

## Preempt-check stack bound

`current_check_preempt_pending()` consumes `need_resched` in a loop. Restoring
IRQs from a run-queue guard can publish a new request, so that guard's drop
must not recurse into another check. Each round temporarily raises the preempt
disable count before releasing the guard: 1 → 2, drop returns to 1, then
`enable_preempt(false)` returns to 0. The next loop iteration handles a new
request. Requests are not dropped, and the call stack does not grow with
request count.

That extra count covers only this check's own guard drop. Ordinary guard exit
is unchanged. After the check returns, the same task resumes, so the bump and
the matching drop belong to that task even if it switched or migrated.
Unittests republish requests through a task-private probe and check completion,
count balance, and bounded stack depth. The probe is not compiled into a
normal kernel.

## Source scope

The analysis covers the entire crate, including feature-gated code and tests.

```text
task/ktask/
├── src/
│   ├── lib.rs                 # exports and feature overview
│   ├── api.rs                 # spawn / yield / sleep / affinity / exit / tick
│   ├── task.rs                # TaskInner / CurrentTask / state / context
│   ├── irq_wait.rs            # kirq::IrqSyncWaitIf provider
│   ├── softirqd.rs            # ksoftirqd/N daemon
│   ├── run_queue/             # per-CPU RQ: ownership, placement, switch
│   │   ├── mod.rs             # RunQueue type, per-CPU slots, registry, init
│   │   ├── rq.rs              # leave / pick / switch / kick
│   │   ├── placement.rs       # spawn / wake CPU choice (no remote sched lock)
│   │   ├── ownership.rs       # cpu_id and nr_running / nr_home
│   │   ├── idle_pull.rs       # steal a Ready !on_cpu waiter onto an idle CPU
│   │   ├── migrate.rs         # affinity: migrate_current / migrate_entry
│   │   ├── stats.rs           # sched_stat counters and /proc text
│   │   └── tests.rs           # run-queue unittests
│   ├── future/
│   │   ├── mod.rs             # block_on and waker-to-unblock bridge
│   │   ├── poll.rs            # poll helpers
│   │   └── time.rs            # sleep timer future
│   ├── wait_queue.rs          # event-driven blocking wait
│   ├── timers.rs              # owned timer adapter and IRQ heartbeat hook
│   ├── tracing_hooks.rs       # sched trace hook registration
│   ├── tests_timer_routing.rs # unittest: ktimer_core owner routing
│   └── snapshot/              # optional task/CPU snapshot and backtrace
├── Cargo.toml
├── Kconfig
└── docs/
    ├── design.md
    └── security.md
```

`run_queue/` is split by duty. Callers still enter only through
`current_run_queue` / `select_run_queue` / `select_wake_run_queue` /
`enforce_affinity_placement`. Nice updates lock the owner queue in
`set_task_nice`. CPU choice lives in
`placement.rs` because it reads each CPU's `nr_running` / `nr_home`; `ksched`
holds one RQ lock and never sees remote load. `rq.rs` owns local leave / pick /
switch. `idle_pull.rs` steals across CPUs. `migrate.rs` only moves a task after
an affinity change.

`kworkerpool_ng` uses `ktask` for pinned task creation, prepared waits and
owned timers. `kworkqueue_ng` retains work lifecycle and wait predicates;
optional task-local work context supports self-wait diagnostics only.

## Architecture

```text
kruntime::init_scheduler()/init_scheduler_secondary()
        │
        ▼
┌─────────────────────────────────────────────────────────────────┐
│ run_queue/                                                      │
│   mod.rs        RUN_QUEUE / RUN_QUEUES / IDLE_TASK / init       │
│   placement.rs  spawn idle-first, wake SIS, idle-pull source    │
│   ownership.rs  cpu_id / nr_running / nr_home                   │
│   idle_pull.rs  steal a Ready !on_cpu waiter onto an idle CPU   │
│   rq.rs         leave / pick / switch_to / kick                 │
│   migrate.rs    affinity: migrate_current / migrate_entry       │
│   stats.rs      optional sched_stat                             │
└─────────────────────────────────────────────────────────────────┘
        │                         │
        │ on_timer_fire()         │ block/wake/exit/yield
        ▼                         ▼
   update_current() → pending      blocked_resched / unblock_task / resched
        │                         │
        └────── deferred until enable_preempt safe point ──────┘
                                  │
                                  ▼
                       rq.rs::switch_to()
```

| Component | Role |
|---|---|
| `TaskInner` / `CurrentTask` | Task metadata, state machine, context, preempt count |
| `RunQueue` (`run_queue/mod.rs`) | Per-CPU container: algorithm lock, `nr_running` / `nr_home`, per-CPU slots, bring-up |
| `rq.rs` | Local leave / pick / `switch_to` / kick |
| `placement.rs` | Spawn idle-first, wake SIS, idle-pull source choice; no remote RQ lock for policy |
| `ownership.rs` | `cpu_id` and `nr_running` / `nr_home` accounting |
| `idle_pull.rs` | Steal a Ready `!on_cpu` waiter from a busier CPU onto an about-to-idle CPU |
| `migrate.rs` | Affinity enforcement after `set_cpumask`: current `migrate_current`, ready `migrate_entry` |
| `stats.rs` | Optional `sched_stat` counters |
| `Scheduler` (`ksched`) | `add_task` / `pick_next` / `update_current` / `next_preemption_ns` / `enqueue_task` / `leave_current` / `steal_ready_task`. A running task always leaves through `leave_current`. Block/Migrate record vlag; wake/migrate-in use `enqueue_task` PLACE_LAG; idle-pull uses `steal_ready_task` then PLACE_LAG. EEVDF `curr` is a numeric snapshot only. Slice/request are in ns. |
| `future::block_on` | Maps `Future` pending onto task block/wake |
| `WaitQueue` | Event-driven blocking wait API |
| `timers` | Owned timer adapter, static CPU queues and timer IRQ heartbeat hook |
| `snapshot` / `task_registry` | Optional diagnostics: snapshot / watchdog / NMI share a task walk. Mutex-wait checks sample the current tick after each task snapshot and drop a start time later than that sample. |

### Prepared timer core dependency

`ktask` depends on `ktimer_core`, which owns prepared timer storage, deadline
ordering, owner routing, dispatch and cancellation semantics. `timers.rs` now
owns the static production `PREPARED_TIMERS` set. `on_timer_fire` dispatches its
due callbacks outside timer/runqueue locks, with IRQs disabled on the owner CPU.
Async `future::sleep*`, `future::timeout*`, timerfd, network sampling and
per-CPU watchdog sampling now use the owned `ktask::Timer` adapter. Network
and watchdog handlers own their completion-relative periodic policy. The former
waker map and periodic callback vector, including their registration APIs, are
removed; all general timer delivery now uses this one prepared source.
With `sched_stat`, network/watchdog sampling now contributes to
`timer_irq_soft` through prepared dispatch, not `timer_irq_periodic`.
This is a diagnostic classification change, not a change in callback context.

`Timer::handler()` exposes a shared borrow of prepared handler state, allowing
owners such as timerfd to store callback data beside the node instead of in a
separate Arc. It neither transfers ownership nor exposes the core node. The
borrow is tied to the timer; concurrent state access still needs synchronization.

`Timer<H>` binds the core's owned storage to the static production set. Start
pins the current CPU and masks IRQs until registration and hardware refresh
finish. Idle/pending replacement selects that CPU; a running callback retains
its actual owner, which receives a fixed IPI if remote. Local refresh acquires
only the prepared CPU lock. Cancellation delegates to the
core and can leave a harmless stale IRQ. No automatic periodic policy is added.

Each pending async sleep allocates one core-owned node/handler object. Its
`SleepHandler` embeds `SpinNoIrq<SleepState>` (`is_expired`, `Option<Waker>`),
without a second state allocation or a preallocated pool. `poll` and completion
serialize on that lock, so either completion observes the registered waker or
poll observes completion. Completion takes the waker and wakes outside the lock;
poll clones/replaces/drops wakers outside the lock except for swapping the value.
Waker implementations still determine their own clone/drop costs. This is not
a claim that creating a sleep or every executor operation is allocation-free.

Dropping the future drains delivery through the owned timer before freeing wake
state. An elapsed locked registration completes without queuing a callback.
Sleep deadlines beyond the core horizon are capped at `u64::MAX` nanoseconds,
remaining pending throughout the supported clock lifetime (strictly below that
horizon). The public raw timer API rejects over-horizon input. `timeout_at`
keeps operation-first polling when both branches are ready.

`rearm_local_timer` merges schedule and prepared deadlines.
It holds the prepared CPU lock through the hardware write. A timer notification
uses the same arbitration; the programmer never takes a runqueue lock. Timer IRQ batching
defers intermediate writes and re-reads all sources after callbacks and wakes.
Core overload backoff changes only the prepared service deadline, not scheduler
deadlines. An empty prepared queue cannot disarm another source.

`TaskInner` 在创建时记录单调时钟（`start_time_ns`，构造后不可变）。该时间戳由
procfs 读取并换算成 `/proc/[pid]/stat` 的 `starttime` 字段（USER_HZ=100 刻度），
供 runc 等运行时把它与持久化的 `InitProcessStartTime` 比较来判断进程是否仍是
自己创建的那个。任务生命周期全程不修改它。

`ktask/ipi` enables `kipi/timer-reprogram` and supplies `TimerReprogramIpiIf`.
The endpoint only re-reads current deadlines and programs hardware; it does not
dispatch callbacks, account runtime or mark a reschedule request. Notifications
must follow accepted registration after releasing timer locks and target the
returned actual owner, including pending migration or running-owner retention.
Removing a source membership may leave a harmless old interrupt. Every accepted
replacement requires destination refresh even when concurrent starts replace it.

Under `cfg(unittest)`, `lib.rs` links the core's registered tests. With `smp`,
`tests_timer_routing.rs` also calls `ktimer_core::test_support` scenarios. The
core owns scenario state and timer assertions; `ktask` provides two concurrent
tasks pinned to CPUs 0 and 1, joins them, and supplies `yield_now`. The
dependency direction is `ktask -> ktimer_core`; the core does not depend on
the task scheduler. These tests exercise core operations through scheduled
workers, not through the hardware timer interrupt path.
`tests_timer_irq.rs` separately exercises real local IRQ delivery, explicit rearm,
notification-only service and repeated remote advances/pending migrations.

`kspin` and `ktimer_core` preemption are always on. The leftover `preempt`
Cargo feature still forwards the same knobs; this crate does not gate
code on it. The `smp` feature forwards to `ktimer_core/smp` so lock
configuration stays consistent. Feature forwarding does not initialize
timer-service state or mark a CPU ready.

## Execution context and calling constraints

`run_queue/` files constrain execution context. Callers still use the original
entries:

- `mod.rs` `init` / `init_secondary` run once per CPU at scheduler bring-up.
  Only then may this CPU's `RUN_QUEUE` and the matching `RUN_QUEUES` slot be
  read.
- `&mut RunQueue` from `current_run_queue` / `get_run_queue` must be used
  under a `BaseGuard` (usually `NoPreemptIrqSave`). A successful `try_get`
  only means that CPU has registered; it does not mean the caller holds that
  CPU's IRQ/preempt exclusion.
- `placement.rs` may run on spawn, affinity migrate-in, and wake (including
  post-IRQ unblock). Placement only Relaxed-reads each CPU's `nr_running` /
  `nr_home` and does not take a remote `scheduler` lock.
- `rq.rs` `leave_current` / `pick` / `resched` / `switch_to` operate only on
  the current CPU's queue. `switch_to` requires local IRQs already off.
  Idle-pull runs from task context when this CPU is about to idle.
- `migrate.rs` `enforce_affinity_placement` runs in task context after
  `set_cpumask`. A remote running task only gets a resched IPI; the caller
  does not `switch_to` on the remote CPU.
- IRQ / IPI paths may call `request_remote_resched` / `kick_running_task`,
  but must not `account` / `refresh` the dest RQ from an IPI callback.

These APIs are not general early-boot helpers except the documented bring-up
entries. Sleepable waits require a current task and a sleepable context.
Reentrancy is bounded by the preempt-check loop above; ordinary scheduling is
not reentrant under a held RQ guard.

## Concurrency model

- Local schedule operations hold a `BaseGuard` via `CurrentRunQueueRef` /
  `KRunQueueRef`. `scheduler` then uses `SpinRaw` because IRQ/preempt
  exclusion is already provided by the guard. Boot has no such guard yet:
  `init` / `init_secondary` call `disable_local_irq` before the first
  `publish_task` / `install_running` and do not restore that mask.
  `kruntime` enables local IRQs only after interrupt handlers exist.
- `placement.rs` only reads remote atomic heuristics. `ksched` operates only
  on the already-chosen RQ lock.
- Idle-pull: `resched` drops the dest scheduler lock first, locks only src
  for `steal_ready_task`, then locks dest to enqueue. Dest-then-src deadlocks
  against a remote wake.
- Each `RUN_QUEUES` `Once` slot is written once by that CPU's `init` /
  `init_secondary`.
- Blocked wake versus `on_cpu` uses the Dekker handshake in the SMP section
  below.

## Creation and publication

`ktask` splits the user-visible lifetime into two stages:

- `prepare_task()` turns `TaskInner` into a `DeferredArc` without placing the task
  on a run queue.
- `activate_task()` publishes the prepared task onto a run queue so it
  becomes runnable.
- `spawn_task()` remains the generic shortcut: `prepare_task()` plus
  `activate_task()`.

Callers that need extra publication can finish registry or other externally
visible state before the scheduler can see the task. User-process creation
must keep that order.

`init_scheduler()` captures the active permanent kernel page-table root before
constructing the boot CPU's run queue. All tasks created through
`TaskInner::new_common()` start with `TaskContext::new_kernel(kernel_root)`,
including idle, migration, PID-less, and Linux-visible kernel threads. `ktask` uses the same constructor on every
architecture; `kcpu` defines the register semantics. Shared-root architectures
use the supplied permanent root, while split-root architectures keep their
separate user-root default and the already active kernel root. No constructor
reads the creator's user root or borrows its MM without retaining an owner.
The scheduler must be initialized after permanent kernel paging is active and
before secondary schedulers or task construction.
User-task callers install the root of their runtime-owned address space through
`ctx_mut().set_page_table_root(...)` before publication; a user task must not
rely on the constructor to inherit its creator's root.

PID 1 has no special bootstrap conversion path. Late-init threads go through
`posix-process::spawn_init_process` and construct a fresh `User` identity;
the runtime is ready once at `new_user()`. `UserRuntimeSlot` is therefore an
immutable container filled at construction (`ready()` builds, `get()` is
read-only). There is no `EMPTY -> INSTALLING -> READY` state machine and no
later `install_user_runtime()`. The saved page table is activated on first
switch-in; AArch64 refreshes its CPU-local ASID through `switch_page_table_root`.
Exec uses the narrow unsafe `replace_current_user_page_table` for a running user
task after kprocess publishes its replacement binding. The caller holds
IRQ/preemption exclusion and retains both roots; ktask updates the saved context and hardware without
allocation, callbacks or fallible work. AArch64 requires the caller's prepared
ASID and the register writer's ISB; x86 CR3 writes flush translations; RISC-V
and LoongArch use explicit local flushes (SFENCE.VMA and DBAR/INVTLB).
The unittest-only `current_user_page_table_root` returns a scalar saved root
under its own `NoPreemptIrqSave`, without exposing a context borrow.

## Critical flows

### Initialization

Boot CPU:

```text
init_scheduler()
  → retain the permanent kernel root for context construction
  → run_queue::init()
  → disable_local_irq() and leave IRQs masked
  → create the per-CPU idle task (pinned to this CPU)
  → create the per-CPU run queue
  → install current as the main task

kruntime::init_interrupt()
  → kirq::softirq::init()
  → ktask::init_softirqd_current_cpu()
  → create and activate ksoftirqd/0 pinned to the boot CPU
  → enable_local_irq()
```

Secondary CPU:

```text
init_scheduler_secondary()
  → run_queue::init_secondary()
  → disable_local_irq() and leave IRQs masked
  → install current as the idle task
  → create this CPU's run queue

rust_main_secondary()
  → ktask::init_softirqd_current_cpu()
  → create and activate ksoftirqd/N pinned to this CPU
  → enable_local_irq()
```

`ksoftirqd/N` is the `ktask` provider for `kirq::softirq::SoftirqDaemonIf`.
`kirq` only keeps per-CPU softirq pending bits and wakes the current CPU's
daemon when it must defer work. `ktask` creates the daemon, pins it to that
CPU, and blocks it on an IRQ-safe `PollSet`. The daemon calls
`kirq::softirq::run_pending_softirqs()` from ordinary task context for work
that hardirq-exit / BH-enable did not cover or that exceeded the restart
budget. `needs_resched()` only Acquire-reads the current task's preempt
request so `kirq` can decide whether to back off at a batch boundary. It does
not consume the request or call the scheduler; it returns false with no
current task. After each real softirq batch the daemon
calls `yield_now()` before the outer pending/wait loop, matching Linux
`run_ksoftirqd()` `cond_resched()` after `__do_softirq()`. If wait
registration fails (for example under memory pressure), the daemon yields
and retries instead of spinning without a waiter.

After all CPU services are ready, `kruntime::late_init_main` initializes
`kworkng` once. `kworkerpool_ng` creates pinned normal managers and workers
through `ktask`; logical queues share these per-CPU execution services rather
than owning worker tasks. Retained demand wakes the manager, which creates
additional workers in sleepable task context within its configured limit.
Worker occupancy and retirement are explicit pool state, not scheduler tick
accounting or scheduler block/resume hooks.

Normal callbacks may sleep but do not inherit the submitting process context.
BH callbacks run in `kirq` softirq context. `ktask::sleep()`, `sleep_until()`
and `interruptible_sleep_until()` reject hardirq, softirq and BH-disabled
contexts. `yield_now()` is not a sleep/blocking API. Prepared waits provide
durable wake-before-park handling; the queue layer checks context, self-wait
and work lifecycle predicates before blocking and after wake.

Queue closure does not own delayed timers. Callers stop producers and drain
caller-owned delayed work before closing a queue. Pool task lifetime remains
independent of individual queue lifetime.

### Schedule and switch

Voluntary switch paths:

- `yield_now()` → `yield_current()` → `resched()`
- `exit()` → `exit_current()` → `resched()`
- `blocked_resched()` → `resched()`

`resched()` calls `pick_next_task()` on this CPU's scheduler. If there is no
ready task it first idle-pulls a Ready `!on_cpu` waiter from a CPU with
`nr_running >= 2`, then falls back to this CPU's `IDLE_TASK`.

### Idle wait handshake

`run_idle` yields first, then pins the CPU and masks normal IRQs. Its final
`need_resched` check does not consume the retained request: pending work skips
sleep. With no retained request, `karch::idle_wait` prevents a later normal IRQ
from being consumed before the sleep instruction. No runqueue lock crosses the
wait. Timer-only notifications are covered by the architecture wake mechanism,
not by treating them as reschedule requests. Each return reevaluates work.

The architecture returns IRQ-off even if it temporarily enabled delivery. The
guard restores IRQs before allowing preemption. With timer-resume fixup enabled,
an outer no-preempt guard keeps the before-wait sample and post-wait repair in
one non-schedulable region, including repair logging and notifications after
IRQ restoration. RCU watching brackets only the architecture wait as described
above; timer repair neither owns that state nor adds a periodic wake tick.

### Hardware timer drive and dynamic schedule timer

`ktask` owns all hardware timer-drive logic. `kruntime` only registers the
clock interrupt vector; the handler calls `ktask::on_timer_fire()`. Schedule
and owned timers share a per-CPU oneshot device and arbitrate by absolute-ns
deadline. They no longer depend on a fixed `TICKS_PER_SECOND` schedule tick:

1. The IRQ entry first calls the `register_timer_irq_note` hook (watchdog
   hardlock heartbeat counts any timer IRQ, not only the 4s sample), then
   charges the current task with `update_current(elapsed_ns)` from
   `now - last_accounted_ns` (idle runtime is not charged; resched is
   requested only when the request is exhausted). The schedule deadline
   stays armed while idle when a class still reports a future event, so a
   throttled RT period boundary can wake the CPU. Only an expired schedule
   slot then runs
   Linux `entity_tick` / `check_preempt_tick`: `need_resched` is set only
   when pick would change (eligible NEXT_BUDDY or an earlier deadline).
   Owned timer IRQs only account, so a later-deadline WF_SYNC wakee is not
   immediately pulled back. An ineligible buddy is armed until eligible by
   `next_preemption_ns`; other waiters are not polled every 10 µs. A failed
   IRQ-tail probe still returns immediately.
2. Due owned timers run outside timer/runqueue locks, with IRQs disabled.
   Hardware rearm is deferred across callback delivery and its wakeups.
3. `next_preemption_ns(current)` recomputes the optional
   `NEXT_SCHED_DEADLINE`, then `rearm_local_timer` to

```text
min(next_sched_deadline?, prepared_service_deadline?)
```

   If both sources are empty, `disarm_timer()` runs. Stale/expired IRQs only
   catch up accounting and rearm; they do not loop catch-up ticks.

   The schedule slot is written from the class view (`next_event_ns()`), not
   from the runner alone: all four writers — `request_resched_on`,
   `refresh_sched_deadline`, the `switch_to` reprogram and the failed-probe
   tail of `preempt_resched` — take the class view even when the current or
   incoming task is idle. A class can hold an event with nothing runnable (a
   throttled deadline task waiting for its next period), and an idle CPU
   receives no other timer IRQ, so dropping the slot there would strand the
   event. The idle class reports no runner event of its own, so nothing is
   armed on behalf of the runner itself.

   With `sched_stat`, IRQs are classified from the schedule slot and prepared
   dispatch result. The existing diagnostic field names remain
   (`timer_irq_sched` / `timer_irq_soft` / `timer_irq_periodic` /
   `timer_irq_stale`); `timer_irq_periodic` stays zero because that source was
   removed. The `soft` name denotes owned timer delivery, not softirq execution.

Schedule deadlines are written by `program_sched_deadline`, matching Linux
`hrtick_start`: a positive relative delay is floored at 10 µs. When
`next_preemption_ns == 0` or the slot is already due, the path sets
`need_resched` and consumes that oneshot schedule slot. It never arms
hardware with interval 0 and never pushes an already-due slot forward every
10 µs (that livelocks the timer IRQ). The floor applies only to the schedule
source and does not affect owned timer precision. Pending preemption is
consumed at the next safe point after the critical section exits.

Owned timers accept `MonotonicInstant` and store bounded absolute nanoseconds
in the core. Hardware arbitration reads the current prepared service deadline
under its CPU lock. Context switch, block/exit/migrate, priority change, and local
`kick` also account and refresh the schedule deadline. A remote wake IPI
only sets `need_resched`. IRQ-tail `WakeProbe` decides whether to switch; a
failed probe arms the remaining-request backup hrtick in `preempt_resched`.
Do not `account` / `refresh` dest's scheduler or timer from the IPI: that
races the waker's remote run-queue critical section. Because a oneshot/NOHZ CPU may have stopped its clock
entirely, `smp` without `ipi` cannot reliably advance a remote runnable
task. `ktask` rejects that configuration at compile time and does not offer
a fake local fallback.

Accepted starts refresh the actual owner after releasing registration locks.
Cancellation can leave a stale hardware IRQ, which rechecks the current queues;
it never permits callback storage to be freed before synchronous drain.

`on_timer_fire` defers hardware rearm across a wake batch
(`DEFER_LOCAL_TIMER_REARM`) to avoid redundant hardware writes during callback
delivery. After the batch it rereads the prepared queue and current schedule
slot, then arbitrates. `switch_to` writes through
`program_sched_deadline_for(incoming, …)`: an immediate pending request lands
on the incoming task because `current()` is still the outgoing task. Under
NOHZ, lone-runner wall time is flushed into EEVDF `curr` via
`flush_running_runtime` before add/wake/priority change so PLACE_LAG does
not use a stale V. `last_accounted_ns` is updated only while holding that
RQ's scheduler `SpinRaw`, so a remote wake flush and local timer
`account_current_runtime` cannot double-count or drop the same wall interval.
The field is an `AtomicU64` solely to provide interior mutability through the
shared `&RunQueue`; its `Relaxed` load/store supplies no synchronization,
which still comes entirely from that same scheduler `SpinRaw`. The run-queue
registry and remote `KRunQueueRef` publish only a shared `&RunQueue`, so
remote mutation goes through the RQ lock or atomic fields and never creates a
cross-CPU whole-object `&'static mut RunQueue`.

The local `CurrentRunQueueRef` likewise holds only `&RunQueue`; its guard
expresses the execution-context constraint, not whole-object uniqueness.
Scheduler algorithm state is mutated through `SpinRaw<Scheduler>`, and load
snapshots / the account epoch use atomic interior mutability, so neither local
nor remote access forms a long-lived `&mut RunQueue`.

`PREV_TASK` uses `SpinNoIrq` inside its per-CPU slot. The switch path
only replaces or takes the owner within the short critical section and never
returns a long-lived mutable reference out of the slot. `PREV_TASK` holds an
optional strong reference, taken by the incoming task after switching stacks.
The reference is released or transferred to a wake enqueue outside the lock.
Callers exclude migration while resolving the current-CPU slot; the inner
lock provides actual object exclusivity.

Owned timer callbacks run in hard IRQ / local-IRQ-off context and must be short
and non-blocking. Repetition is a handler policy expressed with `TimerControl`:
network and watchdog sampling rearm from callback completion plus their period.
The core retains running/cleanup protection and overload backoff; it does not
infer a periodic policy. The independent schedule deadline remains
eligible even while prepared delivery is deferred by overload protection.

`ktask` still uses deferred preemption: the timer path usually only sets
pending; the real `preempt_resched()` runs at the next safe point where
preemption is allowed again.

### Preemption and the kspin interface

`kspin` guard acquire/release call
`KernelGuardIf::disable_preempt` / `enable_preempt`. `ktask` implements that
interface:

- `disable_preempt()`: current task `preempt_disable_count += 1`
- `enable_preempt(true)`: when the count reaches 0, check `need_resched`;
  enter `preempt_resched()` only with local IRQs enabled and outside an
  exception/IRQ trapframe scope. An outer IRQ-only guard retains the request
  even when an inner preemption guard is released. This preserves implicit
  IRQ-off RCU protection without registering an explicit task reader.

This binds the critical-section boundary to the preemption checkpoint so a
task does not switch mid-lock. Exception/IRQ entry exposes whether this CPU
is still inside a trapframe guard via `in_exception_context()`. While that
guard is held, preemption only keeps `need_resched` pending and does not
switch, so an ordinary task never observes a still-exceptional per-CPU
trapframe.

After IRQ dispatch completes and the interrupt controller is acked, `khal`
temporarily suspends the active-exception mark and releases the IRQ handler's
`NoPreempt` guard with `release_without_check`, then calls the explicit
`IrqExitIf` scheduler checkpoint. Guard release only restores its count;
RCU cleanup and pending-work inspection run at the explicit checkpoint.
Only this completed-IRQ entry may consume pending while IRQs are still off;
outer preemption guards and the NMI/exception exclusions remain effective.
Dedicated NMI entry never invokes it. This separates ordinary preempt-enable
from IRQ-return scheduling, as Linux does with `preempt_schedule` and
`preempt_schedule_irq`. The mark is restored before returning to
the interrupted task. Without that tail check, an IPI wake inside an EL1
exception would leave pending until the next schedule timer or safe point.

An exception path may enter a blocking backend (for example a user page
fault), and the task may migrate before the trap handler returns. To keep
the old CPU's active-exception slot from going stale, `switch_to()` suspends
and clears the current CPU's active exception context before replacing
`CurrentTask`. When that task is switched back and returns from the
low-level context switch, the suspended context is restored. Otherwise the
old CPU would keep believing it is in exception context, or the migrated
task would wrongly allow preemption in the second half of the trap handler.

Current-task publication retains the outgoing slot's owning reference until
the incoming initialized pointer has been published and the old stack is
inactive. `PREV_TASK` transfers this owner to `finish_task_switch()` on the
incoming task, both on UP and SMP. Resumed continuations and first-run task
entry each finish exactly one handoff; cleanup resolves the current CPU's run
queue because a resumed task may have migrated. On SMP it also clears the old
task's on-CPU marker and completes any deferred wake enqueue.
Local NMI observations finish before the interrupted switch resumes and can
therefore borrow either task across that publication boundary. Atomic pointer publication does
not synchronize arbitrary mutable task fields or permit remote raw owner upgrades.
`kcpu` owns current-task storage and its architecture access rules, re-exported
by `khal::context`. It accepts opaque pointers; `ktask` alone interprets them
as tasks and manages their owning references. `CurrentTask::prepare_switch`
only transfers ownership: the outgoing reference goes to `PREV_TASK`, while
the incoming owning pointer is passed with its context to the unsafe
`TaskContext::switch_to`. The architecture publishes current immediately before
the low-level stack switch. Initial CPU task installation remains separate.
The accessor protects CPU
selection through the load, not subsequent pointer use. Task-context borrowers
rely on the scheduler's running-task lifetime; NMI borrowers must finish before
interrupted execution resumes. Longer-lived or cross-CPU users need independent
ownership. The writer relies on the scheduler's existing IRQ/migration exclusion
for the entire replacement, rather than taking a second guard around the store.

### Block / wake and WaitQueue / Future

`poll_io_with_cause` preserves the outer task-interruption cause separately from
operation and I/O registration errors. `poll_io` flattens this same implementation
for existing callers. Both check task interruption before polling the operation
and release their owned registrations on completion or cancellation; file-specific
readiness precedence and Linux restart policy belong to their callers.

- `future::PreparedTaskWait` checks sleepable context before publication and
  allocates a `KWaker` Arc. It keeps one strong task ref for
  `blocked_resched`. The handle is not `Send`/`Sync`; `park()` checks current
  task identity. Cross-context delivery uses a Waker that holds only a task
  Weak.
- `PreparedTaskWait::park()` and `block_on` share the existing two woke
  checks: latch first, then again under the run-queue lock, then
  `blocked_resched` publishes Blocked and releases the latch lock. Park does
  not allocate a waker, register a wait, or check signals. The caller keeps
  event registration and rechecks the predicate after every return. A
  spurious wake or stale hint is not completion.
- Prepare rejects no-task, idle, IRQ-masked, hardirq/softirq/BH-disabled, and
  (when preempt counting is configured) a nonzero disable depth. It cannot
  dynamically recognize every unsleepable lock; the caller still owns lock
  constraints. `block_on` does not add this entry check and keeps its
  existing behavior.
- `future::block_on` takes `blocked_resched()` on `Poll::Pending` and marks
  the current task `Blocked`.
- If the waker fires between `Poll::Pending` and the block commit, `block_on`
  clears the wake flag and immediately repolls. It must not yield first: on a
  fully loaded CPU that would turn a completed wake-before-block race into
  queueing delay for an unrelated runnable task.
- A waker restores the task to `Ready` through
  `select_wake_run_queue(...).unblock_task(..., true)`. Selection reads the
  sync hint once (`wake_carries_sync`) and returns it inside
  `WakeRunQueueRef`; `unblock_task` takes the EEVDF sync mark from there, so
  placement and the mark cannot disagree.
- SMP wake follows Linux `select_idle_sibling`: go home if `prev_cpu`
  (`task.cpu_id()`) is idle; otherwise pick a `nr_running == 0` CPU in the
  cpumask; if all are busy stay on prev; only if home is outside the cpumask
  and no idle CPU exists does it use `find_idlest_cpu`. A `with_wake_sync`
  waker that is this CPU's only runner (`nr_running == 1`) nominates this
  CPU (Linux `wake_affine_idle`); unless a veto in `sync_handoff_target`
  applies, the wakee goes there without idle search. Hardirq and an active
  softirq clear that hint before either use: the architecture current pointer
  is still the interrupted task. The IRQ-exit tail is a known hole: hardirq
  depth is already zero and the softirq batch guard has dropped before
  `wake_softirqd`, so a task inside `with_wake_sync` leaks the hint onto the
  pinned ksoftirqd wake (enqueue mark and `wakeup_sync_handoff` only).
  Placement and the EEVDF sync mark share that one result.
- `unblock_task(..., true)` sets `need_resched` on this CPU and, when
  `ipi + preempt` are available, requests the same on a remote CPU.
- After waking `interrupt_waker`, `TaskInner::interrupt()` calls
  `kick_running_task()` if the task is still `on_cpu`: local `need_resched`
  under `preempt`, remote IPI always (`smp` already implies `ipi`). Signal
  delivery to a task running in user mode must take this path; a NOHZ lone
  runner has no periodic tick fallback.
- `WaitQueue` wraps `event_listener` wait/notify and supports timeout and
  predicate wait. Both predicate variants recheck the condition after registering
  the listener so a notification between the first check and registration cannot
  postpone progress until the timeout. `ut-tests/wait_queue.rs` exercises this
  ordering as well as an already-satisfied predicate and a genuine timeout.
- `TaskInner::join()` uses `kpoll::Completion` as the per-task exit wait
  source. Exit publishes `exit_code`, then moves state to `Exited` and
  `complete_all()`. The joiner's real completion condition remains
  `TaskState::Exited`; completion only avoids a lost wake and supports a late
  joiner.
- `ktask` provides `kirq::IrqSyncWaitIf` completion-backed blocking wait so
  `kirq::synchronize_irq()` / `free_irq()` can block the current task. IRQ
  descriptor lifetime and the `in_flight` predicate stay in `kirq`.

### Exit reclaim through DeferredArc

There is no per-CPU GC task, exit queue, or reference-count polling:

1. Exit publishes the per-task completion and removes the scheduler owner.
   That owner is dropped before the non-returning switch, not left on the
   abandoned stack. The CPU current owner still retains the active stack.
2. The switch transfers that current owner into `PREV_TASK`.
3. Incoming-stack cleanup retires diagnostic registration, clears abandoned
   RCU participation and releases the on-CPU marker before releasing the owner.
4. Independent holders may outlive exit. The actual last strong release submits
   destruction to the normal worker through DeferredArc.

Join still observes exit, not destructor completion. Weak expiry likewise does
not imply that the worker has finished destruction. Power-off does not promise
a global finalizer drain.

## Diagnostic registration lifetime

With snapshot enabled, each task carries an 8-byte registration pointer on
64-bit targets (aggregate Task layout may add alignment padding). Registration
before runnable publication returns a permanent slot address, not a CPU inferred
at exit. Task migration never moves that registration. The exited branch of
finish_task_switch retires it directly; Registration::drop is an idempotent
fallback. Exited tasks cease being discoverable even if external owners retain
them. Readers that already obtained a Task owner may finish afterwards.

Each of the existing 4096 slots per configured CPU now embeds DeferredWeak and
one atomic admission state, rather than pointing to a separately allocated Box.
An Acquire CAS admits a counted reader only to LIVE storage. Retirement closes
admission; the retiring writer or last reader moves out Weak after the count
reaches zero, then publishes EMPTY for reuse. No reader waits on a publishing or
retiring writer, including one interrupted by NMI. Contended admission uses CAS
retries, not a wait-free latency guarantee. Slot guards cover only Weak upgrade;
formatting, stack walks and user callbacks run after releasing that guard.
NMI final Weak release uses DeferredWeak's existing local irq_work requirement.

Cost on 64-bit targets: 32 bytes/slot, 128 KiB/configured CPU versus the previous
32 KiB static table plus 24 requested heap bytes per registered Weak box.
Four configured CPUs add 384 KiB static storage while eliminating those boxes
and their allocation/free operations. No diagnostic storage is added when
snapshot is disabled. Registration remains a worst-case 4096-slot scan and now
has a larger cache footprint; successful reads add a count CAS and decrement.
Exit performs direct-slot retirement, never a table scan or reader wait. The
default hardlockup NMI period is 10 seconds, so this is not a scheduling hot-path
scan; no latency or throughput improvement is claimed without measurement.

## SMP semantics

Task owners are `DeferredArc<SchedTask<TaskInner>>`; task weak references use
`DeferredWeak`. Run queues instantiate ksched's shared-owner parameter with this
type. Current stores one owning raw payload pointer and CurrentTask borrows it
without refcount traffic. Preparing a switch reconstructs that count exactly once
and retains it in PREV_TASK until the outgoing stack is inactive. The incoming
pointer independently owns its count. No scheduling policy changes.

Final strong release expires Weak immediately and submits payload destruction;
join and Weak expiration do not wait for that destruction. Unactivated tasks use
the same path. The worker destroys TaskInner, its runtime and stack. Creation
uses the installed preparation provider after allocator/per-CPU/trap setup,
including boot before worker binding. Hardirq, serving softirq and NMI creation
are rejected; BH-disabled task preparation remains supported when allocation is
legal under the caller's locks, including scheduler affinity migration helpers.
NMI release requires local irq_work readiness; it transfers the obligation to
ordinary IRQ before worker submission, without inline Task destruction.


- Each CPU has one `RUN_QUEUE`, `IDLE_TASK`, and `PREV_TASK`.
- `task.cpu_id` is **run-queue ownership**, not an ad hoc helper:
  - runnable / running: CPU whose run queue currently owns the task
  - blocked: last owner CPU, used as wake-affinity preference by
    `select_wake_run_queue`
- All ownership updates go through `RunQueue` wrappers. Callers must not call
  `Scheduler::add_task` / `enqueue_task` / `leave_current` or scatter
  `set_cpu_id`:
  - `publish_task`: first enqueue of a new task (`spawn`)
  - `enqueue_task`: enqueue a non-current Ready task (unblock / affinity
    migrate-in / idle-pull)
  - `leave_current`: current running task leaves the execution slot (Yield /
    Preempt / Block / Migrate / Exit)
  - `switch_to_local`: local helper that `switch_to`s without the ready queue
    (for example a migration task). `cpu_id` is stored in the same scheduler
    lock and entity lock as `install_current`
  - `set_owner_cpu`: only boot bring-up of main / idle before a run queue
    exists
- SMP `switch_to` checks `next_task.cpu_id() == rq.cpu_id` so a task cannot
  switch onto the wrong queue.
- `placement.rs` (`select_run_queue`) for Fair matches Linux fork
  `find_idlest_cpu`: **idle-first** (`nr_running == 0`; a sleeper CPU still
  counts as idle), then `nr_home`. RT uses `find_lowest_rt_cpu` against the
  lock-free `rt_preempt_rank` snapshot. That byte is stored when the
  scheduler lock drops after `charge`, a `curr` change, or `set_attr`. A
  read-only `next_event_ns` (kick, timer rearm, post-switch) does not
  recompute it. After SIS is in place, landing a clone
  on a sleeper CPU is
  safe: the displaced worker can seek idle on the next wake. Among idle CPUs,
  lower `nr_home` still wins (an empty CPU beats a sleeper's CPU). Ties use
  `prefer_local`, then RR.
- `placement.rs` (`select_wake_run_queue`) for Fair matches Linux
  `select_idle_sibling`: go home if prev is idle, otherwise pick a
  `nr_running == 0` CPU. RT uses `wake_rt` / `find_lowest_rt_cpu` instead.
  `nr_running` is `refactor_ksched`'s `load_count`: queued plus a non-idle
  runner. Sticky-home plus "home busy → idle overflow" collapsed schbench to
  about 3-way parallelism (RPS ~450 → ~325). Under SIS the displaced worker
  can seek idle again (musical chairs, still about 4-way on average). Stay on
  prev when no idle CPU exists. `with_wake_sync` with `nr_running == 1` on
  the waker CPU nominates that CPU (Linux `wake_affine_idle`). Pipe
  read/write/splice data handoff, futex, and `Task::interrupt` take that
  path; pipe open, close, and resize are state notifications and use
  ordinary wakes, as in Linux.
  - The candidate is final unless `sync_handoff_target` vetoes it, and
    affinity is the only veto today. That is stronger than Linux, which
    still runs `select_idle_sibling` and may return an idle cache-sharing
    prev. Peers that alternately block gain (`lat_ctx` 9.45 µs → 4.86 µs,
    no IPI to an idle home). Do not veto an idle or remote prev: that is the
    measured path. A pipeline whose two ends both compute can be serialized
    onto one CPU; that case is unmeasured and accepted. `wake_wide` and
    domain rules can still be vetoes.
  - The handoff stacks with `peer_preempts`: a sync-marked eligible buddy
    preempts `curr` without the deadline compare, whereas Linux
    `preempt_sync` still checks the runtime threshold and deadline order.
    That preemption rule predates the sync placement, but pipe data wakes
    now reach it too, so evaluate both migration and preemption effects,
    not only the saved IPI.
  `select_wake_run_queue` decides the hint once (`wake_carries_sync`): a wake
  from hardirq or an active softirq does not carry it, because `current` is
  the interrupted task. The IRQ-exit tail (`wake_softirqd` after both depths
  are zero) can still leak it; see the limit above. The returned
  `WakeRunQueueRef` carries that bool to the EEVDF sync mark.
- There is no periodic balancer. Besides affinity and the wake SIS above,
  an about-to-idle CPU does a Linux-style idle-pull: steal one
  **Ready && `!on_cpu`** waiter from a CPU with `nr_running >= 2`, through
  source `steal_ready_task` (internal `remove_task`, PLACE_LAG) then local
  `enqueue_task`. Lock only src, then dest enqueue, so dest-then-src cannot
  deadlock against a remote wake. Recheck cpumask after enqueue: affinity
  may have excluded dest between steal and enqueue; if dest is not allowed,
  `migrate_entry` immediately so this CPU does not pick a forbidden task.
  Stealing an still-`on_cpu` task (yield/preempt requeue before `switch_to`
  finishes) ran the same stack on two CPUs; `on_cpu` must be rejected.
  Steal changes `cpu_id`. Under SIS that is expected musical chairs: the
  next wake goes home if prev is idle, otherwise seeks idle again.
  Spawn / `activate_task` / affinity migrate-in still use `find_idlest_cpu`.
- `add_task` always `kick` after first enqueue (local `need_resched` plus
  timer re-arm; remote IPI). A dynamic timer is already disarmed for a lone
  task; a busy home cannot rely on a nonexistent periodic tick to discover a
  new peer later.
- EEVDF (`ksched`) matches Linux `pick_eevdf` / `place_entity`:
  - Wake `PLACE_LAG`: `vruntime = V - lag` (lag first inflated by `(W+w)/W`).
    Deadline uses the **full request** (`vd = ve + r/w`, request in ns,
    default `DEFAULT_SLICE_NS` = 2 ms). It does not invent a short deadline
    and does not clamp the wakee extra to V / `min_vruntime` (negative lag
    may be temporarily ineligible). `min_vruntime` includes off-tree `curr`
    as in Linux `update_min_vruntime`.
  - Involuntary `preempt_resched` first calls `peer_preempts_curr()`
    (`curr` compared off-tree by deadline). Only a peer with an earlier
    deadline, or a WF_SYNC eligible buddy, does `leave_current(Preempt)` +
    `pick`. Otherwise pending is cleared and the path returns (avoids a
    pointless requeue and keeps prev's `Arc` on the ready queue before
    `switch_to`). The failed-probe path finishes account + probe +
    `next_preemption_ns` under one scheduler lock, then
    `program_sched_deadline` / rearm after unlock, so three lock acquires
    do not stretch the IRQ-off window.
  - `update_current` matches Linux `update_deadline`: after a completed
    request assign `vd = ve + r/w` and resched only if a waiter exists. A
    lone task does not arm a schedule timer.
  - Wake nominates the wakee as Linux `NEXT_BUDDY` / `set_next_buddy`
    (keeping an existing earlier-deadline buddy). `curr` need not already
    be installed: a remote futex often enqueues after dest `leave` and
    before `pick`. Skipping nomination when `curr` is empty would let pick
    run the just-requeued runner for another full request (schbench
    p99.9 ≈ 2 ms). Pick prefers an eligible buddy; WF_SYNC then sets
    `prefer_sync_buddy`.
  - Futex-like paths use `with_wake_sync` (Linux `WF_SYNC`): an eligible
    next buddy may sync-preempt a half-finished curr even with a later
    deadline. `mark_sync_wake_preempt` sets `prefer_sync_buddy` immediately,
    and a failed `peer_preempts_curr` must not clear the sync mark, or a
    later remote slice-expire pick / IPI probe would miss the buddy.
    Account + PLACE + nomination must finish under one scheduler lock.
  - `TaskInner::interrupt()` uses `with_wake_sync` for `interrupt_waker`
    (the signal sender often blocks next). If the target is on a CPU, kick
    that CPU: local `need_resched` under `preempt`, remote IPI (`smp`
    already implies `ipi`). The IPI pulls `UserContext::run` back into the
    kernel; `preempt` only additionally sets `need_resched` in the handler.
    This is the NOHZ counterpart of Linux `kick_process`. Sample
    `on_cpu` / `cpu_id` once.
- A running task always leaves through `leave_current`:
  - `yield_current` → `Yield` (reset request and requeue; `Running -> Ready`
    must succeed)
  - `preempt_resched` → `Preempt` (keep remaining slice and requeue; same)
  - `blocked_resched` → `Block` (record vlag, do not enqueue; the into_raw
    current slot counts as 1, so the caller must hold another strong ref to
    keep `strong_count > 1`; otherwise panic — see `# Panics` and the
    unittest)
  - `migrate_current` → `Migrate` (source RQ records vlag for dest
    PLACE_LAG, does not enqueue)
  - `exit_current` → `Exit` (clear current accounting, no PLACE_LAG)
- Every leave path, including `yield_current`, calls `update_current` at
  current time **before** `leave_current` / requeue. `switch_to` does not
  re-account an already-enqueued prev, which would mutate an ordered-queue
  key and break scheduler index and weight aggregation.
- Idle does not enter scheduler `curr`: yield/preempt only do
  `Running -> Ready`, without `leave_current` / enqueue. Under EEVDF idle
  keeps `curr == None`; `peer_preempts` treats nonempty ready as preemptible
  when `curr` is empty.
- Helpers that enter the CPU through `switch_to_local` call
  `sync_running_curr` on entry. `preempt_resched` only probes and does not
  sync (so idle is not written into `curr`).
- EEVDF `curr` stores only an identity / vruntime / deadline / weight
  snapshot and does not hold a task `Arc`. `pick_next` requires a prior
  `leave_current` and no longer silently erases stale state.
- `set_task_affinity` writes `cpumask` then `enforce_affinity_placement`.
  Migration happens only while the task **occupies** a CPU: current
  immediately `migrate_current`; ready already on a forbidden RQ is
  `remove_task` then `migrate_entry` (dest enqueue then `kick(Decision)`,
  same as `add_task`); a remote running task under `preempt`+`ipi` gets
  a reschedule IPI, and `preempt_resched` / `yield_current` force-migrate
  when the mask no longer contains this CPU. The caller spins until the
  task leaves the forbidden CPU; failure returns `false` (syscall `EBUSY`).
  A prepared-but-not-activated task, Blocked/Exited, and the idle-pull
  steal-to-enqueue gap (Ready, on no queue) do not occupy a CPU, so the
  path is mask-only. The next enqueue (`activate_task` / wake / dest
  `commit_idle_pull`) chooses a CPU from the new mask. Dest still rechecks
  cpumask after enqueue and `migrate_entry`s immediately if dest is
  forbidden. First pin uses `set_cpumask` then `activate_task` (ksoftirqd /
  watchdog); do not send an unpublished task through the migrate path.
- When re-enqueueing a `Blocked` task, the SMP waker first publishes the
  dest RQ and wake flags. If `task.on_cpu()` is still true, the outgoing
  CPU claims the flags atomically after clearing `on_cpu` and finishes
  enqueue. That handoff prevents double enqueue and avoids spinning
  IRQ-off under `NoPreemptIrqSave` waiting for the remote CPU.
  `arm_wake_enqueue` / `on_cpu` / `set_on_cpu(false)` / `take_wake_enqueue`
  are all SeqCst, with a `SeqCst` fence between each store and the other
  side's load (x86 `MFENCE`, AArch64 `DMB ISH`). That is the two-variable
  Dekker memory-model requirement, not an emulator workaround: a SeqCst
  store followed by a SeqCst load of **another** address is not a
  store→load barrier (C++20 P0668; Rust follows). Release/Acquire can miss
  on both sides on real hardware; without the fence LLVM may lower SeqCst
  to ordinary store/load, and x86 TSO can still miss. Do not weaken the
  fence to acquire/release (only a SeqCst fence emits `MFENCE` on x86).
  QEMU TCG (IK9KW6) was only the first place the bug showed up: the task
  stays `Ready` but is on no run queue.
  The existing unittest
  `wake_of_switching_task_defers_enqueue_without_spinning` covers only the
  single-threaded Deferred path and cannot hit cross-CPU store-buffering.
  A guest block/wake pair also cannot be expected to time out without the
  fence: IK9KW6 passed 20 serial runs in one VM; concurrent host VMs are
  needed to stretch the TCG interleaving window. Deterministic regression
  is therefore a manual reproduction, not a false-green stress unittest.

  **IK9KW6 manual reproduction (aarch64 QEMU TCG, `NR_CPUS=4`)**

  1. Start 3 isolated TCG VMs in parallel, each running harness
     `process-ipc-smoke,getrusage-reentry` (not serial, not one VM
     hammered repeatedly).
  2. Without the `SeqCst` fence, the first round can already stick a VM in
     `shm_deadlock::shm_deadlock_shmget_vs_shmat`: `state=Ready`,
     `on_cpu=0`, `wake_enqueue_flags=0x05` (`PENDING|RESCHED`), the old
     CPU runs idle, and later wakes no longer take `Blocked→Ready`.
  3. Control: the same binary under KVM, or with the `SeqCst` fence kept
     between store and the other load, passes 24/24. Removing the entire
     wake handoff also passes 24/24 but returns to IRQ-off spinning.
- Remote run-queue wake / first enqueue never switches the remote CPU
  directly. With IPI and preemption, a remote pending-resched request
  defers the switch to the remote safe point.
- `TaskInner::on_cpu_mask()` is now only `ktask`'s own residency snapshot.
  User address-space TLB shootdown targets live in
  `memspace::MmCpuResidency`. The current implementation only
  conservatively aggregates the mask on switch-in (over-target is allowed)
  and does not guarantee immediate reclaim of the old CPU footprint after
  switch-away.

## When a switch can happen

| Scenario | Immediate switch? |
|---|---|
| `yield_now()` | Yes (voluntary `resched`) |
| `blocked_resched()` | Yes |
| `exit_current()` | Yes |
| Schedule timer `update_current` | No (usually sets pending first) |
| Local-CPU waker | No (sets pending; preempt at a safe point) |
| Remote waker | No (may IPI remote pending and refresh the schedule deadline) |
| `add_task` / `migrate_entry` (local busy/idle or remote) | No (`kick(Decision)`; remote IPI) |
| `enable_preempt` with `need_resched` | Maybe (`preempt_resched`) |

`ktask` is therefore not a "fixed tick always switches" model. It is a
dynamic schedule timer plus safe-point preemption model.

## Cargo features

| Feature | Role |
|---|---|
| `preempt` | Preemption logic and the `kspin` preempt interface |
| `smp` | Multi-CPU run queues and migration; also enables `ipi` |
| `snapshot` | Task snapshot and backtrace foundation |
| `watchdog` | Watchdog diagnostics (depends on `snapshot`); `watchdog_hardlockup` also needs root Kconfig `KFEAT_NMI` (mechanism + PMU period source, AArch64 QEMU) |
| `ipi` | Required for SMP remote wake / reschedule |

`UserTaskRuntime` is `ktask`'s user-runtime interface. Its scheduler hooks
may run in a preempt-disabled switch context, so implementations must not
block, recurse into the scheduler, or depend on ordinary current-process
context. `TaskIdentity::User` keeps `PidHandle` and `UserRuntimeSlot` on the
same variant. Every user task, including PID 1 init, is filled once by
`TaskInner::new_user(..., user_runtime)` at construction. There is no later
install path. Kernel tasks (`KernelThread` / `Internal`) have no runtime.
Unittests that need a user execution context construct and start a separate
user task through normal `TaskInner::new_user` and wait for it to exit;
they do not temporarily inject a runtime into a non-user task.

With `cfg(unittest)`, `current_user_page_table_root()` reads the current task's
saved root under local IRQ/preemption exclusion. It neither installs a root nor
prepares an AArch64 ASID. Scheduling regression tests hold an outer exclusion
guard when comparing this snapshot with the hardware root, and acquire any MM
metadata before entering that guard. The observer is absent from production.

The scheduling algorithm is selected by Kconfig (default EEVDF).

## Sub-tick / contended wake tests

`future/time.rs` unittests split into two classes:

- **Idle**: no same-CPU spin load; log latency against a 6 ms reference budget
  to observe owned sleep timer precision.
- **Contended (EEVDF)**: same-CPU `spin_loop` spinner; log latency against
  `request + DEFAULT_SLICE_NS + 2ms` to observe schedule wake delay.

These tests assert no early completion, not a wall-clock upper bound. Host load
and QEMU timing affect latency; performance comparisons require controlled runs.

Default request length is `ksched::DEFAULT_SLICE_NS` (2 ms). RR does not
share this EEVDF request: its historical quantum stays
`ksched::DEFAULT_RR_SLICE_NS` = 50 ms so RR context-switch rate is not
multiplied by 25.

## Design decisions

### Why `run_queue.rs` became a directory

One file holding placement, switch, affinity, and counters would tangle
SMP rules with local leave/pick once class placement (for example RT
pin-home) is added. After the split, `placement.rs` only reads per-CPU load
heuristics; `rq.rs` only leave/pick/switches on the already-chosen RQ;
`idle_pull.rs` steals across CPUs; `migrate.rs` only moves a task after a
mask change. External entries and lock order are unchanged.

### Why per-CPU run queues

They cut global lock contention and cache thrash. The schedule path stays
mostly CPU-local and extends more cleanly to SMP.

### Why a tick does not switch immediately

Switching from the interrupt body or mid-critical-section grows concurrency
complexity. Scheduling waits for an `enable_preempt` safe point. The IRQ
handler only offers a tail safe point after device completion and after the
active-exception mark is temporarily suspended.

### Why wake prefers the current CPU when allowed

Wait/future wakes usually run in the thread that released a resource or
signaled an event. If the target's affinity allows this CPU, enqueueing
locally cuts cross-CPU wake latency and lets `need_resched` take effect at
this CPU's preempt safe point. If affinity forbids this CPU, ordinary
placement is used.

### Why the nice path reads-modifies-writes the attributes

`set_prio` / `set_task_prio` change only the task's nice value. They read the
current `SchedAttr`, replace `nice`, and submit the whole attribute
(`RunQueue::set_fair_nice`). They deliberately do **not** build a fresh
`SchedAttr { policy: Normal, .. }`, because that would rewrite the task's
policy and drop its deadline parameters: a `SCHED_DEADLINE` task would be
silently demoted to Fair by a nice change.

Consequences:

- a non-Fair task keeps its class and its bandwidth reservation; the new nice
  value takes effect the next time the task returns to Fair;
- `ksched::SchedAttr` gains per-class fields without breaking this path, since
  no caller spells the struct out field by field any more;
- the nice value is validated by `Nice::new`, and any later structural
  inconsistency is rejected by `SchedAttr::validate` inside
  `Scheduler::set_attr`, before any state changes.

### Why `KernelGuardIf`

`kspin` as a base library must not depend on `ktask`. Injecting the
interface the other way breaks a potential cycle and keeps the crate
boundary clear.

### Why final destruction uses a normal worker

An exiting task may still be held by a joiner or the switch path. Strong
ownership prevents premature reclamation; DeferredArc moves potentially
sleeping destruction out of the last releaser's execution context.

## Drop / resource release

- `KRunQueueRef` / `CurrentRunQueueRef` call `G::release` in `Drop`,
  matching acquire. The preempt-check path temporarily raises the disable
  count so drop cannot recurse into scheduling.
- DeferredArc owns final destruction of TaskInner and its stack, without an
  exit-list owner or polling task. PREV_TASK must retain the task
  that just switched away until execution reaches the incoming stack.

## Boundary with `kruntime`

- `kruntime` registers the clock (and IPI/PMU) interrupt vectors. The timer
  interrupt handler only calls `ktask::on_timer_fire()`.
- `ktask` owns all hardware timer drive: dynamic schedule deadlines,
  owned timer dispatch, hardware rearm, and converting
  due events into schedule accounting, preempt requests, and timer wakes.
- `kruntime` does not know the class algorithm. `ktask` does not route
  hardware IRQs.

## Scheduler statistics

With the `sched_stat` feature, `ktask` keeps per-CPU base counters
(placement, wake, local/remote resched, tick/preempt skips, switch).
`wakeup_fallback` is the Fair `find_idlest_cpu` path. An RT wake that
leaves home increments `wakeup_rt_rank` instead.
`wakeup_sync_handoff` counts a `WF_SYNC` placement onto the waker CPU and
is separate from `wakeup_last_cpu`, because that CPU may not be the wakee's
previous `cpu_id`.
`/proc/sched_stat` is a process-context snapshot. EEVDF additionally emits
algorithm counters such as `wake_handoff` / `wake_sync_preempt`. A watchdog
trigger calls `dump_sched_stats()` for allocation-free base output.

## Redundancy and overload

- **Redundancy**: each CPU has its own idle task and run queue, so there is no
  single global queue.
- **Overload**: class algorithms (`ksched`) and the block model share the
  load. `ktask` places spawn / affinity migrate-in with `find_idlest_cpu`
  and idle-pulls one Ready `!on_cpu` waiter when about to idle. There is no
  periodic load-balance daemon.

## Scheduler trace state

The optional switch callback receives the outgoing task state from the leave decision (`Running=1`, `Ready=2`, `Blocked=3`, `Exited=4`). Consumers can distinguish runnable delay from blocking when combined with wakeup events and embedded event timestamps. The leave path passes this state explicitly through reschedule and switch; a concurrent waker changing Blocked to Ready cannot overwrite the recorded block decision. The private local-helper switch also accepts an explicit leave-decision state. Its sole caller, `migrate_current`, transitions the outgoing task to Ready and passes Ready at that call site; the helper does not hardcode a migration-specific state. Callbacks run with interrupts disabled and must not block or schedule.

### Adjustable-clock waits

`sleep_until_boottime` / `sleep_until_realtime` prepare clock-change listeners
in task context and preserve the original deadlines. Each poll checks the original
clock domain before the change listener and underlying monotonic timer. A clock
change or old timer expiration triggers reconversion; a backward REALTIME step
extends an absolute wait. Relative REALTIME keeps ordinary monotonic sleep.
Cancellation releases poll registrations and synchronously cancels the owned timer.
