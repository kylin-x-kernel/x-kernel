# kpm — System power management

## Ownership

`kpm` owns system suspend/resume orchestration and the platform handoff contract.
`src/device.rs` owns ordered device participation and successful-prefix rollback.
Runtime startup remains in `kruntime`; task checkpoints remain in `ktask`;
timekeeping and clock-event mechanisms remain in their existing crates.
Future device PM and wakeup coordination can extend this subsystem without
making runtime bring-up their owner. Guest and physical-platform backends must
satisfy the same contract; neither backend is implemented by this extraction.

## Single-CPU PM time transaction

`src/lib.rs` exposes `kpm::suspend_uniprocessor` as an explicit integration API,
not a syscall or firmware sleep implementation. It composes already-owned
`RtcSleepAccounting`/`RtcSleepSession`, `MonotonicSuspendToken` and
`LocalTimerSuspend` rather than duplicating their offsets, epoch or queues.
The crate depends on task, time and HAL contracts; it does not depend on
`kruntime`. The `entry` crate links it independently, including test registration.
There is no device registry, CPU-stop protocol or concrete platform provider.
The optional device entry accepts an explicitly ordered, exclusively borrowed list.

Admission requires a boot-initialized sleepable task with IRQs enabled and
exactly one present CPU. SMP-capable kernels with one present CPU can use it;
multiple present CPUs are rejected before platform preparation or RTC access.
The best-effort terminal SMP-stop is never treated as reversible quiescence.
One independent atomic admission flag excludes reentrant/concurrent transactions
through preparation and final cleanup; its stack guard releases it on return.
The function does not hold a spinlock across device I/O or the platform interval.

The unsafe `PlatformSuspendOps` contract owns four substantive boundaries:
prepare stops producers and NMI/repair readers while
retaining PM-task and RTC progress; enter attempts firmware sleep; restore
unconditionally repairs counter/controller/device accessibility after either
wake or entry error; finish releases preparation after the time transaction is
closed. Preparation errors must undo their own partial work; prepare and finish preserve
the enabled local IRQ state. No current
platform implements the contract, and the existing unsupported power stub is
not sufficient. After preparation, ktask closes scheduler admission, collects
the complete admitted task set and cooperatively freezes non-exempt tasks.
Device suspend, wake sources and CPU rendezvous remain concrete-provider work.

The successful sequence is:

```text
sleepable prepare -> request/confirm task freezing -> ordered device suspend
  -> RTC/accounting begin
  -> mask IRQs -> pause local events -> freeze MONOTONIC -> mark RTC boundary
  -> platform enter -> platform restore (also after entry error)
  -> resume MONOTONIC -> restore event hardware with countdown stopped
  -> restore enabled IRQ state -> RTC settlement and clock-change notifications
  -> final IRQ-protected local deadline arbitration -> reverse device resume
  -> platform finish -> task thaw
```

Both RTC samples run with MONOTONIC advancing and IRQs enabled. Resume sampling
still has local clock events paused and must not wait for those events,
including a timer-based timeout or scheduling wakeup. The platform must keep
ordinary execution excluded while permitting the PM task/source to progress.
Platform preparation must cancel or quiesce callbacks whose devices will be
unavailable. Before restore returns, resources reachable by enabled IRQs,
clock-change observers or due timer callbacks must be usable, or their consumers
must remain independently suppressed. Event rearm precedes finish, so interrupt
safety cannot be postponed to finish. Controller/vector/routing access must be
restored before event hardware access;
NMI and repair paths remain excluded until final finish. Counter frequency must
not change. IRQ-off frozen-time callbacks cannot allocate, block, unwind or
wait on logical time. These are mandatory platform implementation obligations,
not facts inferred from the admission check.

Preparation failures release only completed resources. Initial RTC failure
releases platform preparation; event-pause failure drops accounting; freeze
failure restores the owned event, cancels accounting and then re-arbitrates.
Marker failure first resumes the owned clock and then follows the same cleanup.
No entry is attempted on these branches. Successful preparation is finished
exactly once on every ordinary return, after pending accounting is cancelled.

After entry, even an error must execute platform restoration, monotonic rebase
and event restoration. RTC settlement measures the interval actually excluded
from MONOTONIC, including a failed entry. `SuspendError::Resume` retains both
entry error and settlement result: a successful delta is already committed;
a failed estimate is cancelled and reported as uncompensated, never replaced
with zero. This does not reverse physical sleep or invent a missing duration.
Owned restoration failures are invariant violations and fail-stop rather than
return with a frozen clock; ordinary source/entry errors follow typed results.

The private `SuspendOps`/`SleepSource` adapters isolate runtime mechanics from
the same production executor used by deterministic fault-injection fixtures.
`ut-tests/suspend.rs` uses independent clocks, resource state and admission flags;
it never freezes the active kernel, stops live CPUs or injects global offsets.
The tests cover ordered success, failures at each preparation/entry/settlement
boundary, combined entry/RTC errors, cancellation before unquiesce and nested
admission retry. They do not establish real platform suspend, device recovery
or full task/CPU quiescence. Concrete provider and real sleep verification are
required before wiring a userspace PM request to this API.

The layering reference is Linux v6.12 kernel/power/suspend.c: process preparation
is separate from the IRQ-off platform-entry interval and unconditional resume
cleanup. This implementation is deliberately narrower and reuses X-Kernel's
own time/event credentials and cooperative task requests rather than importing
Linux's device lists, syscore or SMP orchestration.

### Cooperative task ownership

`SuspendTasks` supplies explicit kernel-infrastructure exemptions, a bounded
MONOTONIC wait and a one-shot `FreezeCancellation`. After prepare,
`ktask::freezer::freeze_system_tasks` closes first-activation admission and
collects every admitted task from its lifecycle registry. It excludes PM/idle
automatically and rejects user, duplicate and unregistered exemptions.
The unsafe platform provider must keep exempt infrastructure safe and usable;
it no longer constructs the ordinary task snapshot. Preparation must keep task-accessible resources usable until platform enter,
including after individual checkpoints: failed freezing can thaw partial
acknowledgments before finish. Device power-down cannot occur in prepare.

After successful preparation, the real executor acquires `ktask::freezer`'s
`SystemFrozenTasks` guard before RTC sampling. Its lifetime covers accounting,
clock/event recovery and platform finish on every ordinary return. A freezer
error calls finish without sampling RTC or pausing time. Timeout/cancellation
withdraw requests and thaw partial acknowledgments before returning from the
freezer; preparation must therefore be reversible without inaccessible devices.
A cancellation after the guard is returned cannot release tasks during recovery.
New task objects and process-domain visibility may be prepared while admission
is closed, but scheduler activation is retained until cleanup. System guard Drop
thaws existing requests, then releases deferred activations through normal
admission. This also handles callers that dropped their task references. The
legacy `SuspendTasks::targets` field is replaced by `exempt`; PM callers now
declare only required kernel infrastructure, never ordinary target coverage.

The task subsystem confirms only audited checkpoints or permanent task exit.
First task entry and the POSIX return-to-user boundary provide checkpoints;
existing kernel workers opt in explicitly after releasing relevant locks.
Blocked tasks are not presumed safe, and freeze requests do not generate
signals or EINTR. A task stuck in a syscall or a worker without a checkpoint
causes timeout. SMP still fails admission. Tests use real scheduler freezing on
one CPU and mock time/hardware transitions; they do not validate real S3.

## Validation

Start from a platform defconfig and run `make defconfig` before validation.
`make clippy UNITTEST=y` checks the crate and its migrated fixtures.
`make unittest UNITTEST_CRATE=kpm,ktask SMP=1` exercises coordinator ordering
and task freezer behavior; `SMP=4` also checks rejection of unsupported topology.
These checks do not validate a real platform sleep or device power transition.

## Ordered device participation

`suspend_uniprocessor_with_devices` extends the same executor with a borrowed
`&mut [&mut dyn DeviceSuspendOps]`. The existing `suspend_uniprocessor` delegates
with an empty list, retaining its API and behavior. There is no global device
registration, allocation, dependency sorting or automatic driver adaptation.
The caller supplies consumers before providers; each successful suspend adds
one entry to a completed prefix, and resume consumes that prefix in reverse.
Exclusive mutable borrows prevent list changes while callbacks execute.

Callbacks run after task freezing, with IRQs enabled and MONOTONIC advancing.
A device suspend error is returned as `DeviceSuspendError { index, error }`;
the failing callback must undo its own partial changes. Completed predecessors
are resumed, untouched successors are skipped, then platform preparation is
released and tasks thaw. Every later ordinary error follows the same device
cleanup after the owned time/event state has been restored. Prefix accounting
is reset before each resume callback; completed entries are not resumed twice.

Normal wake performs platform infrastructure recovery, monotonic rebase, RTC
settlement and event rearm before device resume. Devices resume before platform
finish and task thaw. This permits sleepable driver recovery with functioning
timers, but requires asynchronous device consumers to remain suppressed until
their device is ready. Device callbacks cannot wait for frozen tasks. The
selected RTC, event/controller access, and any dependencies of PM or recovery
must remain usable independently; do not put those resources in this list if
their suspend would remove a PM prerequisite.

`DeviceSuspendOps` is unsafe because the generic coordinator cannot prove DMA,
interrupt/callback quiescence or hardware recovery. Resume is an infallible
contract, including after an aborted entry; backends that cannot satisfy it
must reject suspend before reporting success. Callback unwinding is unsupported.
This is an initial sleepable device phase, not Linux's full prepare/late/noirq
pipeline. Device dependency discovery, recoverable resume-error policy, wakeup
sources and concrete Guest/physical-platform integration remain future work.

Deterministic tests use three test-owned device participants to check complete
ordering, failure at every device position, later failures at every time/entry
boundary, untouched devices on early rejection, reusable rollback and public
admission. They assert that devices resume with time/events usable while tasks
remain frozen and platform exclusion remains held. No live device is suspended.
