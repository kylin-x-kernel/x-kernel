# ktime - Design Document

## Positioning

`ktime` is the system timekeeping semantic layer: based on the monotonic hardware clock provided by `khal::time`, it computes
realtime, and uniformly maintains the realtime correlation, the accumulated sleep time, initialization, and the read interfaces.
`khal` provides only low-level hardware time capabilities, and `drivers::rtc` provides only persistent-clock samples.

## Architecture

```text
hardware counter -> khal::time (monotonic) --+
RTC boot sample / PersistentClock adapter -+--> ktime --> SystemTime
Confirmed sleep interval (future PM) -------+          --> BoottimeInstant
```

Dependency direction:

```text
ktime (timekeeping) -> khal (hardware monotonic clock)
                     -> ktime_types (semantic value types)
                     -> kiface-time-api (PersistentClock capability)
khal -> ktime_types
rtc-driver -> kiface-time-api (implements PersistentClock)
ktime does not depend on rtc-driver; PM supplies its prepared provider value.
```

`ktime` depends on `khal`, and `khal` does not depend on `ktime`, so timekeeping can evolve (NTP
frequency adjustment, multiple clocksource selection, etc.) without changes to the hardware layer.

## Scope

- `initialize_realtime`: establishes the realtime correlation from a persistent-clock sample;
  idempotent, the first sample takes effect.
- `set_realtime_checked`: at runtime, re-correlates the wall clock to the current monotonic instant
  (the settimeofday path). A range check failure returns a `RealtimeOutOfRange` error
  (the wall clock is earlier than `CLOCK_MONOTONIC` or exceeds the upper bound), which the caller maps to `EINVAL`.
- `realtime_and_monotonic`: samples the monotonic clock once under the same correlation read lock and returns the corresponding
  realtime/monotonic value pair; used by consumers that need to keep both timestamps of the moment an event occurred.
- `realtime`: the current wall-clock time; when uninitialized, it falls back to the Unix epoch plus the elapsed monotonic time
  and the injected sleep time.
- `realtime_deadline_to_monotonic`: converts a realtime deadline into the monotonic clock domain.
- `boottime`: returns the monotonic clock plus the accumulated sleep time, using the separate `BoottimeInstant` type.
- `inject_sleeptime`: atomically advances realtime and the accumulated sleep time without modifying the underlying monotonic clock.

## Design Decisions

- realtime = realtime_base + (monotonic_now - monotonic_base), at a 1:1 rate.
- boottime = monotonic_now + sleep_time; the initial sleep_time is zero, and setting the wall clock or initialization
  does not reset this accumulated value.
- `Timekeeper` keeps the correlation and the accumulated sleep time in the same `SpinRwNoIrq`: the read path (syscall/fs timestamps) and the write path
  (boot initialization, settimeofday, sleep compensation) are mutually exclusive, and reads are safe in IRQ context.
- `set_realtime_checked` has no built-in authorization check; the caller (the syscall layer) is responsible for
  the Linux privilege check. The lower and upper bounds of the wall clock relative to `CLOCK_MONOTONIC` are guaranteed by the function's built-in
  range check, and failure is returned as a `RealtimeOutOfRange` error.
- When uninitialized, it falls back to `SystemTime::UNIX_EPOCH + elapsed + injected sleep`,
  which keeps it usable during early boot.
- Before committing, compensation validates the accumulated sleep_time, the current boottime, and the advanced realtime;
  realtime uses the existing settimeofday upper bound, and any error leaves the state unmodified. A zero increment is a no-op.
- `SleepTimeError` distinguishes an out-of-range value (`OutOfRange`) from an invalid monotonic baseline
  (`InvalidMonotonicBaseline`); the latter requires the caller to handle counter/baseline consistency.
- Reading boottime saturates on representation-range overflow, to avoid wraparound caused by a future underlying elapsed time.

## Clock Change Notification

`change.rs` holds independent REALTIME / BOOTTIME `kpoll::PollEvent`s.
Setting the wall clock and the first initialization publish only REALTIME; a non-zero sleep injection publishes both domains.
Failed updates and zero injections publish nothing. The state and the event generation are committed under the TIMEKEEPER write lock;
the lock is released first, and then waiters are woken; a waker must not run inside the time lock.

A subscriber pre-initializes the notification source in task context, first reads the generation, then reads the time predicate,
and after registering re-checks the generation / the original-clock deadline. `ClockChangeListener` wraps this rule
as a one-shot Future; Drop unregisters it. A notification only hints that a recomputation is needed; it does not mean the timer has expired.
The one-shot wait publicly exposes only `ClockChangeListener` and its constructor; generation sampling stays inside the module,
and no low-level registration API without consumers is exposed. The timerfd runtime uses a separate persistent observer.

The first creation of the notification source may allocate memory; only the subscription API triggers creation, and the write path only reads an existing
source, so there is no first-time allocation in IRQ. Registered wakers must support IRQ-safe wakeup.
The generations of both domains wrap around at usize, assuming that a single wait does not span usize::MAX updates.

## Calling Constraints / Execution Context

Reads, initialization, setting, and sleep injection use an IRQ-safe spinlock; the registration/wait APIs are for task
context only. Wakeup cost grows with the number of waiters, and callers must not hold an outer lock that these wakers re-enter.
This design reuses kpoll's registration and cleanup capabilities and adds no new CI stage.

## Sleep Compensation Boundary

The generic layer only accepts sleep time that has been confirmed and not yet counted in MONOTONIC; the caller is responsible for the formal
suspend/resume transaction, freezing the underlying clock and restoring its baseline, source selection, and exactly-once settlement.
This generic layer does not wire ARM64 fixups, a production PM caller or a host protocol; RTC-based estimation is provided separately by `rtc.rs`.

BOOTTIME / absolute REALTIME waits and timers retain the deadline in the original domain; converting it to the monotonic clock
is only a snapshot for programming the current hardware timer, and it must be recomputed after a clock change.
MONOTONIC and relative REALTIME do not subscribe to clock changes. ITIMER_REAL continues to use MONOTONIC.
`CLOCK_BOOTTIME` and `sysinfo.uptime` read the generic BOOTTIME.
Wiring BOOTTIME into the historical timestamps of input events is left to a separate change; this PR does not change their data layout.

## Verification

Time arithmetic uses a separate Timekeeper; notification tests use a separate Changes / PollEvent
and do not inject sleep time into the running kernel's global state. Consumer tests cover original-domain checks, periodic accumulation, and alternately arming relative/
absolute REALTIME. UTs do not replace formal PM or real host sleep tests.

## Persistent Runtime Observers

`observer.rs` provides persistent runtime subscriptions in addition to one-shot poll listening.
`ClockReading` and its update sequence number are sampled under the TIMEKEEPER lock; callbacks run after unlocking, using the snapshot of that
commit. The registry stores weak references and keeps already-obtained notification snapshots unchanged through copy-on-write.
A notification clones the Arc under the table lock and calls the observer outside the lock; the notification itself does not allocate memory.
Releasing the registration credential deactivates the subscription, regardless of whether the observer Arc is still alive; a callback already obtained may still complete.
When the registry is replaced, stale weak-reference slots are cleaned up, and the storage may retain the earlier concurrent high-water mark.
Callbacks and the final destruction of observers must be compatible with IRQ context, and the notification traversal cost is O(N).

Only when the table Arc is unique and the Vec has spare capacity does registration append in place under the IRQ lock without allocating.
Otherwise, stale slots are cleaned up outside the lock and a new table with geometric growth is prepared, then validated and replaced under the lock.
Holding an old Arc forbids in-place modification, so the pointer check can detect replacement and registration races.
With no concurrent snapshots, the table maintenance cost for consecutive registrations is amortized O(1). A regression with 128 subscriptions bounds
the number of replacements and checks notification and unregistration. When a notification snapshot already exists, registration still needs an O(N) copy
and temporarily keeps two tables; concurrent registrations may retry. This regression is not a latency bound or a scalability benchmark;
continuously overlapping notification snapshots can still cause repeated copying.

The replacement table reserves capacity based on the number of live entries after cleanup, not the old slot count.
Repeated creation/closing reclaims stale slots, so capacity does not keep growing with the history of registrations.
A separate 4096-iteration register/release regression checks that the table length and capacity are bounded, and confirms that closed subscriptions
no longer receive notifications; it is not a heap-memory or latency benchmark. Before replacement, the earlier concurrent high-water mark may still be retained.

The notification and observer regressions are placed in `ut-tests/change.rs` and `ut-tests/observer.rs` respectively,
and are mounted by the corresponding production modules through `unittest::path_mod_test!`, preserving the private-interface test boundary.

## Suspend and Resume Settlement Transaction

`src/suspend.rs` provides `begin_suspend_accounting`, `resume_suspend_accounting`, and
`cancel_suspend_accounting`. The same TIMEKEEPER lock protects the clock, the transaction state, and the commit. On begin, it
saves a consistent ClockReading and allocates a private identifier that is not reused within this boot; only one transaction round is allowed at a time.
On resume, it validates the credential and the monotonic baseline, calls the shared injection arithmetic, which contains no transaction policy, on a copy, and after success commits once
the clock offset and the end of the transaction; the legacy injection entry point checks for an active transaction independently and no longer bypasses the policy by clearing the state first.
On failure the original state is kept, and the transaction can be retried or cancelled; a zero injection ends the transaction but sends no notification. After a non-zero commit, once unlocked, it reuses
the REALTIME/BOOTTIME notifications and observers; cancellation does not change the clock and sends no notification.

This is an adaptation of the settlement part of the responsibilities of Linux timekeeping_suspend/resume, not a complete PM implementation.
During the interval, clock reads and timers keep running, and wall-clock settings are preserved; on resume, the sleep duration is added on top of the current correlation.
The caller first restores a continuous MONOTONIC that does not include the sleep still to be compensated, and then commits the confirmed sleep duration.
The generic transaction does not estimate RTC deltas; `rtc.rs` supplies that adapter.
It does not freeze clocks, restore clocksource/clockevent, or stop CPUs/devices.
It also provides no host protocol. A non-zero call to the legacy inject_sleeptime during a transaction is rejected; a zero call is still a no-op.
The credential only guarantees a single commit within the same round; it cannot detect the caller reporting the same sleep again in a later new transaction or through the legacy injection API.

The credential owns an internal reclaim guard bound to the Timekeeper that issued it; Drop automatically cancels a transaction that still belongs to this credential,
without compensating time, notifying, or restoring hardware; dropping an old credential that was already completed or cancelled does not clear a new transaction.
An ordinary early return on an error path can also release settlement ownership. Drop briefly acquires the time write lock and must not run while holding
that lock or in an NMI; the PM owner remains responsible for rolling back hardware and tasks, and must not mem::forget the credential.
Deliberately bypassing the destructor does not guarantee that ownership is restored. When identifiers are exhausted it fails rather than wrapping around. There is no production PM caller,
and unit tests are also not used to prove
real hardware suspend, multi-core resume ordering, or cross-host clock trustworthiness.

## Persistent-clock RTC fallback

`rtc.rs` owns `RtcSleepAccounting<C>` and `RtcSleepSession`, over the
`kiface-time-api::PersistentClock` contract. An owner retains one source and its
phase calibration. The session exclusively borrows that owner and holds the
existing #1093 single-use `SuspendToken`; no second global transaction or RTC
injection path is created. Driver mapping/discovery happens before this layer.

```text
prepared PersistentClock -> RTC / MONOTONIC / BOOTTIME samples
                                  |
                                  v
                      missing-time estimate + bounds
                                  |
                                  v
             existing resume_suspend_accounting -> notifications
```

The PM sequence is prepare source -> begin -> restore/coordinate device and CPU
state as required -> freeze MONOTONIC -> mark_suspended -> sleep -> restore
MONOTONIC and the device/IRQ resources needed for RTC access -> resume_from_rtc
-> finish per-CPU event restoration and re-arbitrate deadlines.
`mark_suspended` is an explicit PM assertion, not hardware detection. Resume
before this boundary is rejected without reading RTC. Earlier aborts cancel.
No production PM/host protocol is connected; real use requires the monotonic
freeze/rebase capability supplied separately by #1165.

RTC I/O runs in the backend's permitted sleepable context with no spinlock held.
Two short `clock_reading()` calls bracket it, retaining the after-read coherent
MONOTONIC/BOOTTIME sample and the read window. Regressing MONOTONIC, changed
resolution, failed reads and windows above max_read_span are rejected. This
budget detects late returns; it cannot interrupt a stuck backend. Constructors
require initialized sources and nonzero resolution, sleep bound and budget.

The estimate starts with RTC elapsed minus already-counted MONOTONIC elapsed.
To avoid repeated whole-second truncation drift, it also subtracts the change
in BOOTTIME-to-RTC phase since a previously committed reference. This adapts
Linux drivers/rtc/class.c rtc_suspend/resume's old_delta/delta_delta correction,
using BOOTTIME instead of REALTIME so wall-clock edits do not change running
elapsed time. A phase shift at least twice (resolution + maximum read span)
resets the reference, analogous to Linux's approximately two-second threshold.
Negative estimates within one quantum plus both observed windows close with
zero compensation; larger deficits fail. Positive compensation above the PM
owner's max_sleep fails. i128 nanoseconds safely represent all input spans and
timestamp differences before checked conversion to nonnegative TimeSpan.

Calibration is committed only after ktime settlement succeeds. Errors retain
the session and calibration for retry; successful zero settlement also closes
the transaction. Explicit cancellation or unfinished Drop discards calibration
and cancels accounting, without restoring hardware or injecting time. Known RTC
edits between sessions require reset_calibration; plausible source edits during
an interval cannot be distinguished from true sleep and must be excluded by PM.

Linux prefers nonstop clocksource, then persistent clock, then RTC fallback.
The PM owner makes that selection here. A prepared session can settle a
confirmed preferred-source delta through resume_with_confirmed_time, without a
resume RTC read; success closes the same token and invalidates RTC calibration.
RTC fallback cannot subsequently inject again. If a preferred source is already
known, PM can bypass RTC entirely and use ordinary ktime accounting. This is a
source handoff protocol, not automatic clock discovery or a trust detector.

The session adds no registry, heap allocation or shared hot-path state. Rust
exclusive borrowing prevents source replacement or parallel use of one owner.
Global transaction arbitration and notifications retain existing timekeeper
locking. Unit tests under ut-tests/rtc.rs use fake clocks, explicit samples and
independent timekeepers; they do not inject into the active kernel's clocks.
Coverage includes quantization over repeated fractional sleeps, bounds, clock
edits, capture failures, phase commit failure and boundary gating. They do not
prove RTC continuity across real host sleep, device recovery or CPU quiescence.

## Coarse Tick-Granular Wall Clock

`realtime_coarse` derives the wall clock at tick granularity: it takes the
timekeeper read lock, floors the monotonic sample to the tick boundary with
`TICK_NS`, and evaluates the existing `ClockCorrelation::realtime_at` at that
instant. There is no cached state, no identity tracking, and no second lock.
This mirrors the role of Linux timekeeping's `xtime_coarse` image
(`ktime_get_coarse_real_ts64`); Linux refreshes that image from the tick path,
while x-kernel computes it per read, which is observationally equivalent
because the derivation depends only on the tick floor and the committed
correlation.

Contract (true by construction): while the correlation is unchanged, repeated
reads within one tick return the same value, the result trails `realtime` by
less than one tick, and successive reads are monotonic; a wall-clock update —
including a backward step — is visible on the next read and rescales the value
even inside the current tick. The value keeps full
sub-second precision of the correlation at the tick instant; filesystems
apply `timestamp_truncate` separately. Consumers needing exact elapsed wall
time (quota timers, RTC, tracing) keep using `realtime`.
