# ktimer security and reliability

## Scope, assets, and boundaries

All of `src/` and `ut-tests/` is covered. Assets are timer-state consistency (deadlines in
the right clock domain, overrun counts, notification generations) and the
availability of the alarm task. The syscall layer is trusted to convert
user ABI timespecs into validated `TimeSpan` values and to validate
`clockid_t`, `timerid_t`, and notification settings before calling this
crate. The crate trusts that caller-supplied process CPU times belong to
the same process as the manager.

## External boundaries

| Boundary | Direction | Content |
|---|---|---|
| `ksyscall` (trusted) | inbound | already-validated clock IDs, timespec-derived `TimeSpan`, notification configuration |
| `kprocess` runtime | inbound | owner PID, sampled process CPU totals |
| `ksignal` | outbound | `TimerSignal` payloads (signo, timer_id, overrun, sigval bits) |
| Global alarm task | internal | (deadline, pid) heap entries |

No user pointers, MMIO, DMA, FFI, or device input reach this crate; the
`sigval` payload is carried as opaque ABI bits (`TimerSigValue`).

## Unsafe inventory

One unsafe block exists, in `posix_timer.rs`:

| Location | Operation | Invariant |
|---|---|---|
| `TimerSigValue::from_raw` | read of `k_sigval.sival_ptr` | `k_sigval` is an ABI carrier union; the pointer view is read solely to preserve the raw user bits and is never dereferenced |

There is no FFI or inline assembly; architecture time reads come through
`khal`/`ktime`.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Realtime and monotonic deadlines mixed | Medium | untyped scalar deadlines | `TimerInstant` variants make domains explicit; mismatches surface as internal errors instead of wrong expirations |
| T-02 | Arithmetic overflow in deadline or period advance | Medium | huge values/intervals or long overdue gaps | unrepresentable deadlines rejected with `EINVAL` before mutating state; advance uses checked/saturating math with expiration-count clamp; unit test pins state preservation on failure |
| T-03 | Duplicate/stale notification from an old alarm entry | Low | timer re-armed or deleted while a heap entry or pending signal exists | poll re-reads current state; `signal_seq` generation filter drops stale dequeued signals (unit-tested); stale heap entries expire harmlessly |
| T-04 | Signal flood from a tight-interval timer | Medium | interval smaller than scheduling quantum | one pending signal per timer with overrun accumulation (POSIX semantics) bounds queue growth; the alarm task itself only polls once per wakeup |
| T-05 | CPU-time totals from a different process | Medium | caller passes mismatched samples | documented caller contract; the manager has no cross-check (trusted boundary, same as Linux's task accounting) |
| T-06 | Alarm task lost wakeup | Medium | push/peek race between heap and Event | new-earliest push notifies the event; the task re-validates the head after every wait, closing the check-then-wait race |

## FMEA

| ID | Failure mode | Cause | Local effect | System effect | Sev | Response |
|---|---|---|---|---|---|---|
| F-01 | Expired-owner callback missing | handler not registered before first expiry | entries accumulate | timers of one process stall | 3 | `Once` registration at init; stale entries remain but new ones still fire |
| F-02 | Deleted timer's heap entry fires | delete after enqueue | poll finds no timer | no-op | 4 | poll tolerates missing owner/timer |
| F-03 | ID wraparound collision | `i32::MAX` wrap with live IDs filling the space | allocation fails | `timer_create` returns `EINVAL` | 4 | allocation skips live IDs and detects exhaustion |
| F-04 | Alarm task blocked on runtime mutex | owner exiting concurrently | callback waits | alarm queue latency | 4 | bounded by exit path length; head re-check keeps correctness |

## Thread safety

Per-process state is serialized externally by the process runtime mutex.
The alarm heap is `ksync::Mutex`-protected with short critical sections;
`Event` notifications follow `event-listener`'s memory model. The expired
handler is a `fn` pointer published once.

## Failure handling

All fallible APIs return `KResult`; user-visible failures are `EINVAL`
(unknown timer, unsupported clock, unrepresentable deadline, ID
exhaustion). Documented panics are internal-invariant assertions
(clock-domain mismatch in interval accounting, monotonic-ITIMER arm) and
one structural re-lookup `expect` in `set_posix_timer` — none reachable
from validated user input. Clock-change poll-registration failure uses a bounded retry in the alarm
task; syscall-side registration errors propagate to the caller.

## Privacy analysis

Timer state reveals only process CPU usage patterns to the kernel; no
user file or memory content is processed.

## Known limitations

- BOOTTIME includes explicitly injected sleep time; no PM source is wired here.
- CPU-time timers poll on the user-return path, so a process spinning in
  kernel context without returning to user mode delays its own CPU timer
  expirations.
- Heap entries are never explicitly removed on timer deletion.

## Audit checklist

- New clock support must add a distinct `TimerInstant` variant plus
  `checked_add`/`saturating_duration_since` and per-domain alarm queue handling.
- ABI integers must be converted to `TimeSpan`/`SystemTime` before
  entering the manager.
- Each alarm heap must contain only its own clock domain; CPU-time deadlines stay out.
- Any new notification path must carry the current `signal_seq` and be
  validated at dequeue.

### Clock changes and queues

The background task prepares event listeners before reading queues or converting
deadlines; callbacks run outside the queue lock. Registration failures retry after
a bounded sleep to avoid losing the task or busy-looping. CPU deadlines stay out
of the wall-clock alarm queues. This layer neither detects platform resume nor
interprets RTC differences as confirmed sleep.

## Alarm recheck after rollback

Dequeuing consumes the alarm ticket before calling the owner. Even zero-expiration
rechecks restore a consumed alarm; queued tickets avoid duplicate rearm on scans.
The regression uses an explicit rollback sample and does not alter global time.
