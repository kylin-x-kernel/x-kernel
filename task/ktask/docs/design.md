# ktask — Design

## Purpose and ownership

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

Owned timer preparation uses `ktimer_core::Timer::new_on` with a snapshot of the
current logical CPU after platform per-CPU initialization. The platform accessor
protects the CPU-ID read; allocation does not require extending that critical
section. This avoids an unnecessary first-arm migration for same-CPU use. If
preparation and start occur on different CPUs, the existing start routing moves
the node normally. Timer IRQ readiness remains a start-time requirement.

X-Kernel splits class algorithms from the task runtime:

- `ksched` supplies FIFO / RR / CFS / EEVDF algorithm traits and entities.
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
│   ├── run_queue/             # per-CPU RQ: ownership, placement, switch, GC
│   │   ├── mod.rs             # RunQueue type, per-CPU slots, registry, init
│   │   ├── rq.rs              # leave / pick / switch / kick
│   │   ├── placement.rs       # spawn / wake CPU choice (no remote sched lock)
│   │   ├── ownership.rs       # cpu_id and nr_running / nr_home
│   │   ├── idle_pull.rs       # steal a Ready !on_cpu waiter onto an idle CPU
│   │   ├── migrate.rs         # affinity: migrate_current / migrate_entry
│   │   ├── gc.rs              # delayed exit reclaim
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
`task_run_queue` / `enforce_affinity_placement`. CPU choice lives in
`placement.rs` because it reads each CPU's `nr_running` / `nr_home`; `ksched`
holds one RQ lock and never sees remote load. `rq.rs` owns local leave / pick /
switch. `idle_pull.rs` steals across CPUs. `migrate.rs` only moves a task after
an affinity change. `gc.rs` only consumes `EXITED_TASKS`.

`ktask` is also the `kwork` provider owner: system/custom kwork task hosts,
task-local worker context, completion wait, and monotonic timer waker
registration for delayed work. `kwork` still consumes provider traits and does
not depend on `ktask` directly.

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
│   gc.rs         delayed EXITED_TASKS reclaim                    │
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
| `gc.rs` | Delayed per-CPU `try_unwrap` of exited tasks |
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

`rearm_local_timer` merges schedule, execution-accounting and prepared deadlines.
It holds the prepared CPU lock through the hardware write. A timer notification
uses the same arbitration; the programmer never takes a runqueue lock. Timer IRQ batching
defers intermediate writes and re-reads all sources after callbacks and wakes.
Core overload backoff changes only the prepared service deadline, not scheduler
or accounting deadlines. An empty prepared queue cannot disarm another source.

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

The `preempt` and `smp` features forward to the matching `ktimer_core`
features so lock configuration stays consistent. Feature forwarding does not
initialize timer-service state or mark a CPU ready.

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
- `gc.rs` `poll_gc` runs only in the per-CPU gc task and may block on
  `WAIT_FOR_EXIT`.
- IRQ / IPI paths may call `request_remote_resched` / `kick_running_task`,
  but must not `account` / `refresh` the dest RQ from an IPI callback.

These APIs are not general early-boot helpers except the documented bring-up
entries. Sleepable waits require a current task and a sleepable context.
Reentrancy is bounded by the preempt-check loop above; ordinary scheduling is
not reentrant under a held RQ guard.

## Concurrency model

- Local schedule operations hold a `BaseGuard` via `CurrentRunQueueRef` /
  `KRunQueueRef`. `scheduler` then uses `SpinRaw` because IRQ/preempt
  exclusion is already provided by the guard.
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

- `prepare_task()` turns `TaskInner` into an `Arc` without placing the task
  on a run queue.
- `activate_task()` publishes the prepared task onto a run queue so it
  becomes runnable.
- `spawn_task()` remains the generic shortcut: `prepare_task()` plus
  `activate_task()`.

Callers that need extra publication can finish registry or other externally
visible state before the scheduler can see the task. User-process creation
must keep that order.

PID 1 has no special bootstrap conversion path. Late-init threads go through
`posix-process::spawn_init_process` and construct a fresh `User` identity;
the runtime is ready once at `new_user()`. `UserRuntimeSlot` is therefore an
immutable container filled at construction (`ready()` builds, `get()` is
read-only). There is no `EMPTY -> INSTALLING -> READY` state machine and no
later `install_user_runtime()`. The page table is activated on first
switch-in through `switch_page_table_root`; there is no
`activate_current_user_page_table` hot-install path for a running task.

## Critical flows

### Initialization

Boot CPU:

```text
init_scheduler()
  → run_queue::init()
  → create the per-CPU idle task (pinned to this CPU)
  → create the per-CPU run queue (including the gc task)
  → install current as the main task

kruntime::init_interrupt()
  → kirq::softirq::init()
  → ktask::init_softirqd_current_cpu()
  → create and activate ksoftirqd/0 pinned to the boot CPU
  → ktask::init_system_workqueue_worker()
  → create and activate kwork/system_wq/0:manager pinned to the boot CPU
  → create and activate the initial kwork/system_wq/0:M worker
  → enable_local_irq()
```

Secondary CPU:

```text
init_scheduler_secondary()
  → run_queue::init_secondary()
  → install current as the idle task
  → create this CPU's run queue (including the gc task)

rust_main_secondary()
  → ktask::init_softirqd_current_cpu()
  → create and activate ksoftirqd/N pinned to this CPU
  → ktask::init_system_workqueue_worker()
  → create and activate kwork/system_wq/N:manager pinned to this CPU
  → create and activate the initial kwork/system_wq/N:M worker
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
current task or without `preempt`. After each real softirq batch the daemon
calls `yield_now()` before the outer pending/wait loop, matching Linux
`run_ksoftirqd()` `cond_resched()` after `__do_softirq()`. If wait
registration fails (for example under memory pressure), the daemon yields
and retries instead of spinning without a waiter.

`kworkerpool` depends on `ktask` to create `kwork/normal/<cpu>:<worker_id>`
worker tasks and `kwork/normal/<cpu>/manager` manager tasks. `kwork` owns
builtin queue/pool instances and enqueue semantics; `kworkqueue` owns queue
state; `kworkerpool` owns worker scheduling, dynamic scaling, and
CPU-intensive accounting. System / custom / dynamic task-context queues bind
to the same CPU's Normal pool and do not create queue-owned workers. Each
CPU starts one pinned manager and a few initial workers. When the pool still
has runnable work but no running / idle / preparing worker, it only wakes
the manager; the manager creates a bounded new worker in task context. One
callback sleep blocks only that worker task; existing or newly created
workers can keep running other work on the same CPU and kind. Workers wait
on a `PollSet`; the wake path may run from hardirq / softirq / BH-disabled
context. Worker callbacks run in ordinary task context and may sleep, but
they do not inherit the enqueueing process context. BH workqueue callbacks
run in `kirq` softirq context, not as `ktask` worker tasks.
`ktask::sleep()`, `sleep_until()`, and `interruptible_sleep_until()`
fail-fast in hardirq / softirq / BH-disabled context so a BH callback or
other interrupt-like path cannot enter a blocking schedule path.
`yield_now()` only yields the CPU and is not a sleep/blocking API. Creating
a worker task happens only in manager task context, never on an IRQ-safe
enqueue path or under a `kwork` pool lock.

`WorkQueueHandle::alloc()` only creates a logical queue and configures
per-CPU pwq `max_active`. `WorkQueue::start()` applies the same policy to a
static custom queue. Neither enters the ktask registry nor creates
queue-owned workers. `destroy()` closes the enqueue gate and waits until
pending/running instances of that queue in the shared pools reach zero.
Pool worker lifetime is independent of queue destroy.

`TaskExecutionContext` stores an opaque worker-pool execution key on the
current `TaskInner` so the scheduler tick can call
`TaskExecutionAccountingIf` for CPU-intensive sampling. That state follows
the task across migration and does not use a per-CPU slot. Work keys and
queue owners stay in `kworkqueue` / `kwork`; `ktask` does not own work
lifetime or store work handles. Pool drain lives inside the
`kworkerpool::WorkerTask` loop; there is no queue-level drain API for
callbacks. Sleepable workerqueue waits reuse ktask yield/wait. `kwork`
finishes its interrupt-context gate before blocking and rechecks its own
lifecycle predicate after wake.

### Schedule and switch

Voluntary switch paths:

- `yield_now()` → `yield_current()` → `resched()`
- `exit()` → `exit_current()` → `resched()`
- `blocked_resched()` → `resched()`

`resched()` calls `pick_next_task()` on this CPU's scheduler. If there is no
ready task it first idle-pulls a Ready `!on_cpu` waiter from a CPU with
`nr_running >= 2`, then falls back to this CPU's `IDLE_TASK`.

### Hardware timer drive and dynamic schedule timer

`ktask` owns all hardware timer-drive logic. `kruntime` only registers the
clock interrupt vector; the handler calls `ktask::on_timer_fire()`. Schedule
and owned timers share a per-CPU oneshot device and arbitrate by absolute-ns
deadline. They no longer depend on a fixed `TICKS_PER_SECOND` schedule tick:

1. The IRQ entry first calls the `register_timer_irq_note` hook (watchdog
   hardlock heartbeat counts any timer IRQ, not only the 4s sample), then
   charges the current task with `update_current(elapsed_ns)` from
   `now - last_accounted_ns` (idle is skipped; resched is requested only
   when the request is exhausted). Only an expired schedule slot then runs
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
min(next_sched_deadline?, next_execution_tick_deadline?, prepared_service_deadline?)
```

   If all three are empty, `disarm_timer()` runs. Stale/expired IRQs only
   catch up accounting and rearm; they do not loop catch-up ticks. With
   `sched_stat`, IRQs are classified from the schedule slot and prepared
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
`request_resched` also account and refresh the schedule deadline. A remote
wake IPI only sets `need_resched` (same as before the periodic-tick change).
IRQ-tail `peer_preempts_curr` decides whether to switch; a failed probe arms
the remaining-request backup hrtick in `preempt_resched`. Do not
`account` / `refresh` the dest RQ from the IPI: that races the waker's remote
run-queue critical section and can reprogram or disarm the schedule slot,
leaving a busy home idle for a full request. Because a oneshot/NOHZ CPU may
have stopped its clock entirely, `smp` without `ipi` cannot reliably advance a
remote runnable task. `ktask` rejects that configuration at compile time and
does not offer a fake local fallback.

Accepted starts refresh the actual owner after releasing registration locks.
Cancellation can leave a stale hardware IRQ, which rechecks the current queues;
it never permits callback storage to be freed before synchronous drain.

`on_timer_fire` defers hardware rearm across a wake batch
(`DEFER_LOCAL_TIMER_REARM`) to avoid redundant hardware writes during callback
delivery. After the batch it rereads the prepared queue and current schedule
and execution slots, then arbitrates. `switch_to` writes through
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

The exited-task queue and `PREV_TASK` each use `SpinNoIrq` inside their
per-CPU slot. Scheduling, GC, and the context-switch continuation only
clear/push/pop/clone/replace within the short critical section and never
return a long-lived `&mut VecDeque` or `&mut Weak` out of the slot. The outer
short `NoPreempt` only prevents migration while resolving the current-CPU
slot; the inner lock provides actual object exclusivity.

Owned timer callbacks run in hard IRQ / local-IRQ-off context and must be short
and non-blocking. Repetition is a handler policy expressed with `TimerControl`:
network and watchdog sampling rearm from callback completion plus their period.
The core retains running/cleanup protection and overload backoff; it does not
infer a periodic policy. Independent schedule and execution deadlines remain
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
  enter `preempt_resched()` only if this CPU is not inside an exception/IRQ
  trapframe scope

This binds the critical-section boundary to the preemption checkpoint so a
task does not switch mid-lock. Exception/IRQ entry exposes whether this CPU
is still inside a trapframe guard via `in_exception_context()`. While that
guard is held, preemption only keeps `need_resched` pending and does not
switch, so an ordinary task never observes a still-exceptional per-CPU
trapframe.

After IRQ dispatch completes and the interrupt controller is acked, `khal`
temporarily suspends the active-exception mark before releasing the IRQ
handler's `NoPreempt` guard. That lets `enable_preempt` consume pending at
IRQ tail while IRQs are still off. The mark is restored before returning to
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

### Block / wake and WaitQueue / Future

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
- If the current task is inside a kwork callback, `block_on` notifies
  `kwork` worker-pool accounting at the function boundary. The real
  `Blocked` convergence is aligned with Linux `wq_worker_sleeping()` through
  the scheduler hook and is paired with resume accounting when returning to
  `Running`.
- A waker restores the task to `Ready` through
  `select_wake_run_queue(...).unblock_task(..., true)`.
- SMP wake follows Linux `select_idle_sibling`: go home if `prev_cpu`
  (`task.cpu_id()`) is idle; otherwise pick a `nr_running == 0` CPU in the
  cpumask; if all are busy stay on prev; only if home is outside the cpumask
  and no idle CPU exists does it use `find_idlest_cpu`.
- `unblock_task(..., true)` sets `need_resched` on this CPU and, when
  `ipi + preempt` are available, requests the same on a remote CPU.
- After waking `interrupt_waker`, `TaskInner::interrupt()` calls
  `kick_running_task()` if the task is still `on_cpu`: local `need_resched`
  under `preempt`, remote IPI always (`smp` already implies `ipi`). Signal
  delivery to a task running in user mode must take this path; a NOHZ lone
  runner has no periodic tick fallback.
- `WaitQueue` wraps `event_listener` wait/notify and supports timeout and
  predicate wait.
- `TaskInner::join()` uses `kpoll::Completion` as the per-task exit wait
  source. Exit publishes `exit_code`, then moves state to `Exited` and
  `complete_all()`. The joiner's real completion condition remains
  `TaskState::Exited`; completion only avoids a lost wake and supports a late
  joiner.
- `ktask` provides `kirq::IrqSyncWaitIf` completion-backed blocking wait so
  `kirq::synchronize_irq()` / `free_irq()` can block the current task. IRQ
  descriptor lifetime and the `in_flight` predicate stay in `kirq`.

### Exit reclaim (GC task)

Each CPU run queue is created with a `gc` task that loops on `poll_gc`:

- consume the `EXITED_TASKS` list
- reclaim immediately on successful `Arc::try_unwrap`
- otherwise push back and wait for external refs to drop
- sleep/wake through the `WAIT_FOR_EXIT` waker

This is delayed reclaim so the exit/switch hot path does not do a slow drop.
`WAIT_FOR_EXIT` keeps `PollSet` event semantics because the same CPU's GC
task observes successive batches of exited tasks. It is not a per-task sticky
exit completion.

## SMP semantics

- Each CPU has one `RUN_QUEUE`, `IDLE_TASK`, `EXITED_TASKS`, and
  `WAIT_FOR_EXIT`.
- `task.cpu_id` is **run-queue ownership**, not an ad hoc helper:
  - runnable / running: CPU whose run queue currently owns the task
  - blocked: last owner CPU, used as wake-affinity preference by
    `select_wake_run_queue`
- All ownership updates go through `RunQueue` wrappers. Callers must not call
  `Scheduler::add_task` / `enqueue_task` / `leave_current` or scatter
  `set_cpu_id`:
  - `publish_task`: first enqueue of a new task (`spawn` / per-CPU gc)
  - `enqueue_task`: enqueue a non-current Ready task (unblock / affinity
    migrate-in / idle-pull)
  - `leave_current`: current running task leaves the execution slot (Yield /
    Preempt / Block / Migrate / Exit)
  - `switch_to_local`: local helper that `switch_to`s without the ready queue
    (for example a migration task)
  - `set_owner_cpu`: only boot bring-up of main / idle before a run queue
    exists
- SMP `switch_to` checks `next_task.cpu_id() == rq.cpu_id` so a task cannot
  switch onto the wrong queue.
- `placement.rs` (`select_run_queue`) matches Linux fork `find_idlest_cpu`:
  **idle-first** (`nr_running == 0`; a sleeper CPU still counts as idle),
  then `nr_home`. After SIS is in place, landing a clone on a sleeper CPU is
  safe: the displaced worker can seek idle on the next wake. Among idle CPUs,
  lower `nr_home` still wins (an empty CPU beats a sleeper's CPU). Ties use
  `prefer_local`, then RR.
- `placement.rs` (`select_wake_run_queue`) matches Linux `select_idle_sibling`:
  go home if prev is idle, otherwise pick a `nr_running == 0` CPU. Sticky-home
  plus "home busy → idle overflow" collapsed schbench to about 3-way
  parallelism (RPS ~450 → ~325). Under SIS the displaced worker can seek idle
  again (musical chairs, still about 4-way on average). Stay on prev when no
  idle CPU exists.
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
- `add_task` always `request_resched` after first enqueue (remote IPI / local
  pending plus schedule-deadline refresh). A dynamic timer is already disarmed
  for a lone task; a busy home cannot rely on a nonexistent periodic tick to
  discover a new peer later.
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
  `remove_task` then `migrate_entry` (dest enqueue then `request_resched_on`,
  same as `add_task`); a remote running task under `preempt`+`ipi` gets
  `request_resched`, and `preempt_resched` / `yield_current` force-migrate
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
| `add_task` / `migrate_entry` (local busy/idle or remote) | No (`request_resched`; remote IPI) |
| `enable_preempt` with `need_resched` | Maybe (`preempt_resched`) |

`ktask` is therefore not a "fixed tick always switches" model. It is a
dynamic schedule timer plus safe-point preemption model.

## Cargo features

| Feature | Role |
|---|---|
| `preempt` | Preemption logic and the `kspin` preempt interface |
| `smp` | Multi-CPU run queues and migration; also enables `ipi` |
| `sched_fifo` | FIFO cooperative scheduling |
| `sched_rr` | RR preemptive scheduling (implies `preempt`) |
| `sched_cfs` | CFS preemptive scheduling (implies `preempt`) |
| `sched_eevdf` | EEVDF preemptive scheduling (implies `preempt`) |
| `snapshot` | Task snapshot and backtrace foundation |
| `watchdog` | Watchdog diagnostics (depends on `snapshot`); `watchdog_hardlockup` also needs root Kconfig `KFEAT_NMI` (mechanism + PMU period source, AArch64 QEMU) |
| `ipi` | Required for SMP remote wake / reschedule |
| `tls` | Optional extension |

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

One file holding placement, switch, affinity, GC, and counters would tangle
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

### Why `KernelGuardIf`

`kspin` as a base library must not depend on `ktask`. Injecting the
interface the other way breaks a potential cycle and keeps the crate
boundary clear.

### Why a GC task reclaims exited tasks

An exiting task may still be held by a joiner or the switch path. Delayed
reclaim avoids use-after-free and a slow drop on the hot path.

## Drop / resource release

- `KRunQueueRef` / `CurrentRunQueueRef` call `G::release` in `Drop`,
  matching acquire. The preempt-check path temporarily raises the disable
  count so drop cannot recurse into scheduling.
- Exited tasks first enter `EXITED_TASKS`. `gc.rs` `poll_gc` uses
  `Arc::try_unwrap` to delay-drop `TaskInner` and its stack. `switch_to`
  must not drop the task that just switched away.

## Boundary with `kruntime`

- `kruntime` registers the clock (and IPI/PMU) interrupt vectors. The timer
  interrupt handler only calls `ktask::on_timer_fire()`.
- `ktask` owns all hardware timer drive: dynamic schedule and execution deadlines,
  owned timer dispatch, hardware rearm, and converting
  due events into schedule accounting, preempt requests, and timer wakes.
- `kruntime` does not know the class algorithm. `ktask` does not route
  hardware IRQs.

## Scheduler statistics

With the `sched_stat` feature, `ktask` keeps per-CPU base counters
(placement, wake, local/remote resched, tick/preempt skips, switch).
`/proc/sched_stat` is a process-context snapshot. EEVDF additionally emits
algorithm counters such as `wake_handoff` / `wake_sync_preempt`. A watchdog
trigger calls `dump_sched_stats()` for allocation-free base output.

## Redundancy and overload

- **Redundancy**: each CPU has its own idle/gc and run queue, so there is no
  single global queue.
- **Overload**: class algorithms (`ksched`) and the block model share the
  load. `ktask` places spawn / affinity migrate-in with `find_idlest_cpu`
  and idle-pulls one Ready `!on_cpu` waiter when about to idle. There is no
  periodic load-balance daemon.
