# ktimer design

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

## Execution context

- `ProcessTimerManager` is serialized by the owning `ProcessRuntime`'s
  `Arc<Mutex<..>>` (`kprocess::process_runtime`). Its methods run on
  syscall and user-return paths; they do not sleep.
- `poll_cpu_timers` relies on caller-supplied sampled `TimeSpan` user and
  kernel CPU totals of the same process; CPU timers are pure polling, no
  interrupts.
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
count frozen at delivery — the same split as Linux's `it_overrun` and
`it_overrun_last`.

## Algorithms

- Missed-period advance: `skipped = overdue / interval + 1`; the deadline
  moves `skipped * interval` forward from the previous deadline, and the
  expiration count is clamped to `usize::MAX`.
- Overrun reporting: `queued_overrun` accumulates while a signal is
  pending; `last_overrun` is frozen only when that signal is dequeued
  (POSIX `timer_getoverrun` semantics); reads clamp to `i32::MAX`.
- Alarm queue: a `BinaryHeap` of `(MonotonicInstant, Pid)` ordered by
  deadline then pid; a new earliest deadline notifies the task via an
  `event_listener` Event; the task re-validates the head after waking to
  ignore spurious or stale wakeups. Realtime deadlines are converted to
  monotonic at enqueue time (`ktime::realtime_deadline_to_monotonic`), so
  the queue only ever holds monotonic deadlines.
- Timer ID allocation: monotonically increasing with wraparound at
  `i32::MAX` back to 1, skipping live IDs; exhaustion returns `EINVAL`.
  Signal sequences wrap to 1 on overflow.

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
- The alarm queue holds only monotonic instants; realtime conversion
  happens once at enqueue.
- `snapshot` receives the clock-domain "now" and returns interval and
  remaining `TimeSpan` only.
- `SIGEV_NONE` timers track expirations with no notification at all
  (overrun still counted).
- `set_posix_timer` may return an immediate delivery when the timer
  already expired at arm time, avoiding a lost first expiration.

## Resource lifecycle

Timers are created/deleted through the manager (`clear_posix_timers` on
exec). Heap entries carry only `(deadline, pid)` and are garbage-collected
by expiry or left harmless if stale; no explicit removal API exists.
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
