# ktimer security and reliability

## Retirement Ownership

Timer removal separates logical retirement from destruction. The manager
returns a must-use opaque owner for one timer or the detached map; callers must
move it outside their mutex before dropping timer payloads and weak references.
Removal prevents lookup/completion even while the owner remains alive. Clearing
does not reset generation state. Single deletion still permits BTreeMap node
deallocation under the lock; bulk clearing transfers those nodes as well.
No new allocation is needed for either retirement wrapper. Pending records are
invalidated before unlocking and cancelled during unlocked wrapper destruction.
Cancellation also removes ignored records. This does not remove stale heap entries or make the
full timer lifecycle OOM-safe. Retirement tests observe deferred weak release
and stale completion rejection, not allocator fault injection.

## Creation Admission

Only a privately constructed PosixTimerConfig reaches manager admission.
Its constructor validates the clock before the process takes the timer mutex;
the syscall adapter remains responsible for signal and target validation.
The process pins the runtime before preparation and keeps it alive until the
manager transaction finishes, preserving runtime-error precedence. Preparation
publishes no ID, sequence or map entry. Notifying configurations now reserve a
KArc-backed record using KERNEL permissions in that reclaim-safe task context.
NoMemory returns before admission; SIGEV_NONE skips reservation. Existing
infallible map insertion and alarm scheduling remain unchanged.
Internal u64 generations never wrap: exhaustion rejects creation before ID
allocation and settime before mutation, returning InvalidInput. Deletion and
exec cleanup remain available but do not reset the source. Only the low 32 bits
reach siginfo; completion compares the full trusted generation, never ABI data.

Configuration and timer state retain a weak TimerSignalOrigin, not a strong
runtime owner. It is forwarded from creation to each non-Clone emission without
dispatch-time PID resolution. The trusted caller must supply its own runtime;
ksignal upgrades and validates through it outside locks. Expired origins cannot
be rebound to a new runtime. TimerSignalTarget independently binds the weak
destination manager and optional slot at creation; kprocess ignores numeric
envelope routing for POSIX emissions. This does not implement prepared-record
cancellation. Last weak release follows the
unlocked rejection/dequeue paths; a live runtime keeps its own backing allocation
valid while clearing its timers or destroying its fields.

ID-allocation failure transfers the unconsumed configuration in
`PosixTimerCreateError`; it does not release it inside admission. Callers must
carry rejection out of their timer-manager lock scope before releasing it.
The production Process adapter does so before mapping back to KError. The
failure-injection test verifies no publication and successful configuration
retry; it does not exercise allocator OOM or actual full-ID-space exhaustion.

## Reserved Storage and Return Progress

One unique lease protects the reserved payload from emission through receipt
release. Rearm cannot reset occupancy: current-generation expirations accumulate
as deferred demand while an older notification owns storage. Successful dequeue
freezes delivered overrun before the old lease becomes reusable. An old receipt
cannot erase deferred demand belonging to a new schedule.

Return uses a separate intrusive ready link and the existing alarm wake source,
not an allocating alarm ticket or synchronous timer callback from Drop. The
alarm worker invokes the weak origin outside ready/record/signal locks. The
runtime verifies exact record storage identity under the timer mutex, preventing
timer-ID reuse from redirecting deferred progress. Dispatch follows unlocking.
Rearm and retirement actively cancel old pending ownership. Already detached
receipts retain exclusive occupancy until released and fail existing generation
validation. Prepared standard timers bypass ordinary coalescing using unique
reserved leases. Distinct timers retain their own completion and overrun state;
ordinary SI_TIMER data cannot claim this privilege. Periodic status is captured
under the timer mutex; ksignal retains ignored periodic records without taking
that mutex. Requeue wake validates storage identity before capturing a target,
then releases the mutex before signal queries or task wake. Rejection,
retention and cancellation never complete a timer. The process integration
test verifies retained expiration overruns survive until actual consumption.
Tests inject reservation failure and pin busy-record rearm and the accounting
cut; this is not global allocator OOM coverage or whole-timer OOM safety.

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

This crate has no unsafe blocks, FFI, or inline assembly. The syscall boundary
reads the complete user-copied `sigval` as opaque pointer-sized bits;
`TimerSigValue` and `TimerSignal` carry those bits without accessing the union.
Architecture time reads come through `khal`/`ktime`.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Realtime and monotonic deadlines mixed | Medium | untyped scalar deadlines | `TimerInstant` variants make domains explicit; mismatches surface as internal errors instead of wrong expirations |
| T-02 | Arithmetic overflow in deadline or period advance | Medium | huge values/intervals or long overdue gaps | unrepresentable deadlines rejected with `EINVAL` before mutating state; advance uses checked/saturating math with expiration-count clamp; unit test pins state preservation on failure |
| T-03 | Duplicate/stale notification from an old alarm entry | Low | timer re-armed or deleted while a heap entry or pending signal exists | poll re-reads current state; `signal_seq` generation filter drops stale dequeued signals (unit-tested); stale heap entries expire harmlessly |
| T-04 | Signal flood from a tight-interval timer | Medium | interval smaller than scheduling quantum | one pending signal per timer with overrun accumulation (POSIX semantics) bounds queue growth; the alarm task itself only polls once per wakeup |
| T-05 | CPU-time totals from a different process | Medium | caller passes mismatched samples | documented caller contract; the manager has no cross-check (trusted boundary, same as Linux's task accounting) |
| T-06 | Alarm task lost wakeup | Medium | new-earliest publication races wait registration or cancellation | persistent rescan bit is published after heap unlock; every poll registers AtomicWaker before consuming the bit; next iteration rechecks all heap heads |

## FMEA

| ID | Failure mode | Cause | Local effect | System effect | Sev | Response |
|---|---|---|---|---|---|---|
| F-01 | Expired-owner callback missing | handler not registered before first expiry | entries accumulate | timers of one process stall | 3 | `Once` registration at init; stale entries remain but new ones still fire |
| F-02 | Deleted timer's heap entry fires | delete after enqueue | poll finds no timer | no-op | 4 | poll tolerates missing owner/timer |
| F-03 | ID wraparound collision | `i32::MAX` wrap with live IDs filling the space | allocation fails | `timer_create` returns `EINVAL` | 4 | allocation skips live IDs and detects exhaustion |
| F-04 | Alarm task blocked on runtime mutex | owner exiting concurrently | callback waits | alarm queue latency | 4 | bounded by exit path length; head re-check keeps correctness |

## Thread safety

Per-process state is serialized externally by the process runtime mutex.
The alarm heaps are `ksync::Mutex`-protected. New-earliest notifications publish
a persistent rescan bit with Release and use the library AtomicWaker after
unlocking. The sole worker registers on each poll before an AcqRel swap clears
that bit. Wait cancellation leaves pending requests intact; repeated notifications
coalesce. Once makes worker creation idempotent so two consumers cannot replace
each other's registration. The expired handler is a `fn` pointer published once.

The wake-source guarantee covers only its inline flag and registration, with the
audited task-bound KWaker: arbitrary Waker implementations may allocate or run
callbacks. Heap growth, alarm tickets, task creation, timeout and clock-listener
resources retain their existing policies. Tests cover notification before first
poll, after a Pending result, repeated consumed registrations, cancelled/replaced
waits and a select with another permanently pending branch. They do not force
global OOM, inject every atomic interleaving or validate a record-ready protocol.

## Failure handling

Process-facing fallible APIs return `KResult`; manager admission instead returns
the unconsumed configuration with its error for unlocked cleanup. User-visible failures are `EINVAL`
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
