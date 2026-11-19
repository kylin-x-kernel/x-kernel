# kpm — Security and reliability

## PM transaction admission and platform trust

`kpm::suspend_uniprocessor` has no userspace entry, raw device pointer or new
MMIO/assembly. It accepts a prepared RTC accounting owner and a platform
coordinator. Admission checks initialized sleepable task context, enabled IRQs,
and one present CPU before source/platform access. An independent admission
atomic prevents nested transactions; it does not certify task/device/NMI
quiescence. Terminal CPU stop is not used.

The unsafe PlatformSuspendOps trait is the PM hardware/state-handoff boundary:
callbacks that can access unavailable devices must be cancelled/quiesced before
power loss. Restore must make resources reachable by enabled IRQs, clock-change
observers and due timer callbacks usable, or keep those consumers independently
suppressed. Final event rearm precedes finish; interrupt safety cannot wait until
that final release phase.
The coordinator closes scheduler admission and obtains complete task coverage
after prepare. Prepare must quiesce exempt infrastructure/producers, idle/fixup and NMI/watchdog reads
without stranding the PM task or RTC; enter/restore run IRQ-off while logical
time is frozen. They must not sleep, allocate, enable IRQs, unwind or use logical
timeouts. Restore must unconditionally make counter/controller/mappings and
routing valid after any partial entry, with stable counter frequency. Finish
must release preparation without failure after events/time are restored. No
production platform implementation or unsafe Send/Sync implementation is present.
A platform that cannot guarantee recovery must reject preparation before freeze.

| ID | Threat | Impact | Implemented control and remaining obligation |
|---|---|---|---|
| PM-01 | Freezing one CPU while other CPUs consume time | High | Multiple present CPUs are rejected before side effects. Actual CPU topology stability and prohibition on CPU bring-up remain platform obligations. |
| PM-02 | Nested requests disturb owned credentials | Medium | Admission compare-exchange spans prepare through finish; credentials are private executor locals. No global time/token is exposed to platform callbacks. |
| PM-03 | Firmware error skips resume cleanup | High | The executor always restores platform access, clock and event hardware after enter returns, including errors. The unsafe provider must make restoration infallible. |
| PM-04 | RTC or callback waits while time/events cannot progress | High | RTC reads occur outside IRQ guards with advancing MONOTONIC; the source must not depend on paused local events. Platform frozen-time callbacks must not block or poll logical time. The coordinator cannot recover a nonreturning backend. |
| PM-05 | RTC failure silently becomes a successful zero compensation | Medium | Pending accounting is cancelled before event/ordinary execution resumes. Typed Resume error records missing compensation and any simultaneous entry error. No estimated delta is applied on rejected settlement. |
| PM-06 | Ordinary execution resumes with stale deadlines or frozen time | High | Counter rebase precedes RTC settlement; final event arbitration follows settlement/cancellation and notifications. Platform exclusions remain until finish. Contract violations in owned restoration fail-stop instead of releasing execution. |

Initial-source, event-pause, freeze and marker errors unwind only owned,
completed stages. Preparation failure is self-rolled-back by the provider;
finish is called only after preparation succeeded. No offset rollback is
attempted after successful injection, and an error from platform enter does not
prevent settlement of the interval already excluded by the clock freeze.
Cancelled or failed estimates do leave that interval uncompensated; the result
reports this limitation rather than claiming time accuracy.

Each of the three narrow expect sites in SystemOps documents its ownership
proof: a live clock token with a reserved resume identity, or a live same-CPU
event token in the required phase, under IRQ and platform quiescence exclusion.
Failure means a violated unsafe provider/internal ownership invariant. These
sites do not terminate on legitimate RTC reads or refused platform entry.

Fault-injection tests use independent in-memory state and scoped IRQ models,
never the active kernel's freeze/accounting globals. They verify cleanup and
ordering, not NMI, MMIO validity, real S3 or multi-CPU safety. No user private
data is processed. No successful userspace suspend support is claimed.

## Cooperative task freeze boundary

The coordinator now owns actual request/acknowledgment/thaw through a
`SystemFrozenTasks` guard. `SuspendTasks` now supplies explicit kernel
infrastructure exemptions, not an ordinary-task list. The authoritative ktask
registry and activation gate prevent an ordinary task from escaping the snapshot;
new activation is retained until recovery completes. Process-domain visibility
is not suppressed. Exempt tasks remain a trust boundary: the unsafe platform
must keep their accesses safe and cannot suspend a dependency needed by a
checkpoint. Prepare must keep all
task-accessible devices usable until enter, including after individual
acknowledgments, because partial-freeze rollback thaws before finish. This remains unsuitable for a
userspace PM entry without a concrete provider and device/wakeup integration.

Freezing is cooperative. A scheduler Blocked state alone is never acknowledged;
no arbitrary preempted task is parked while holding a PM dependency. Missing
safe points fail with a typed timeout before time/event changes. Cancellation
unwinds partial freezes; it cannot invalidate a guard already used by PM.
All later ordinary failures retain that guard until platform finish. Restore
still must make IRQ/observer/timer-visible resources usable before enabling
those consumers. The freezer has one audited unsafe timer-start call after context admission;
its owned callback only performs allocation-free completion notification and
is synchronously drained before PM can pause events. It introduces no raw
pointer ownership or device access.

## Device callback trust and rollback

`DeviceSuspendOps` is an additional unsafe integration boundary. The list contains
exclusive mutable references, not raw pointers or a concurrent registry; the
coordinator cannot certify the dependency order or discover omitted devices.
Device adapters must drain DMA and suppress IRQ, timer, observer and worker
access before resources become inaccessible. Suppression must survive platform
restore, IRQ enable, RTC settlement notifications and event rearm until that
device's resume has restored safe access. The RTC and PM infrastructure must
not depend on suspended resources. Platform preparation still owns task
exempt infrastructure safety, NMI exclusion and asynchronous access. The
system freezer owns ordinary task coverage and scheduler admission. No complete
platform safety is inferred from the device list.

On suspend failure, only the successful prefix is resumed in reverse order;
the failing callback owns its partial rollback. Later time/platform failures
restore devices before platform finish and task thaw. Resume is infallible and
all callbacks must return without unwinding; an implementation unable to
recover must refuse its suspend step. No error is hidden by logging or mapped
to successful sleep: device failures retain index and KError, and existing
combined entry/accounting errors are preserved after device cleanup.

Device callbacks must not reenter PM or wait for frozen tasks, directly or
through worker, completion or device dependencies. Transaction admission remains
held throughout callbacks, and ordinary tasks cannot progress until device
recovery and platform finish complete. A callback that waits for either would
prevent the same transaction from reaching its release phase. Returning from a
refused nested request does not authorize retrying or waiting inside a callback;
adapters must satisfy these progress constraints before opting into DeviceSuspendOps.

There is no allocation, spinlock or additional atomic on this path. Tests
mutate only private fixtures and verify callback order, exact-once cleanup,
retry, execution-phase predicates and no callbacks before admission/freezing.
They do not establish DMA draining, interrupt routing, actual hardware recovery,
SMP safety or a complete driver PM implementation.

## Selected queue freeze boundary

The new workqueue entry accepts trusted kernel-owned `NormalQueue` handles;
there is no userspace pointer, MMIO, DMA, firmware or network parser here.
`kworkng` owns callback/occurrence quiescence and timer cancellation; `kpm`
retains its safe `FrozenQueue` credentials through device/platform recovery.
No new unsafe operation is introduced by the adapter.

Task freezing precedes queue freezing. This relies on explicitly exempt existing
workers and callback dependencies that can finish without frozen tasks. An
unlisted queue on a shared exempt worker is still executable. The unsafe
platform contract must audit/exclude its hardware and timer accesses separately;
selected queues do not certify full worker, device or IRQ quiescence. The task
admission gate may defer newly created workers, so existing worker capacity must
be sufficient for the bounded preparation. No CPU hotplug or SMP support is added.

| Threat / failure | Control and residual limitation |
| --- | --- |
| Queue callback runs against a suspended device | Selected queue guards survive time/event rearm, reverse device recovery and platform finish; callbacks resume before ordinary tasks only after resource recovery. Unlisted queues and IRQ/BH paths remain provider obligations. |
| Claimed callback waits for a frozen task or earlier paused queue | Require audited worker exemptions and dependency ordering; advancing-MONOTONIC timeout/cancellation aborts before device suspension. This does not guarantee arbitrary callback progress or diagnose all dependency cycles. |
| PM/RTC/device resume waits for selected pending work | Exclude PM-critical queues from selection and forbid these waits in the integration contract. Recovery itself is not timed out and must remain infallible. |
| A later queue fails after earlier queues paused | Preserve successful guards through platform finish, then pop in reverse order before task thaw. Failing queue owns its own partial rollback. No device/time entry occurs. |
| Duplicate aliases or external pause ownership | Underlying queue admission returns Busy with the failing index; only guards actually acquired by this attempt are released. |
| Cancellation releases callbacks during hardware recovery | Successful guards ignore later cancellation; both phase handles must be signalled for a whole-preparation cancel. This does not provide late wake/abort semantics. |

Audit field/cleanup ordering whenever extending the executor: devices resume
before platform finish; queues thaw after finish with usable time/events and
before the system task guard drops. Callbacks and platform/device methods must
not unwind. As with existing device participation, panic recovery and forgotten
credentials are not supported cancellation mechanisms.

## Multicore preparation guard

`freeze_system_workqueues` accepts trusted kernel references and performs no
platform transition. It retains global transaction admission and the authoritative
system task-admission gate until queue and task cleanup finish. Queue failure
releases only this attempt's acquired prefix before task thaw; aliases cannot
steal another owner's pause. Cancellation after success cannot release either
kind of guard. Callers must recover their own resources before dropping it.

The guard permits scheduler infrastructure, exempt workers, idle and interrupts
to continue. It is not a DMA, IRQ, CPU or device barrier. In particular, SMP
support here does not relax the UP-only platform-suspend validation. Using this
guard as evidence that shared clocks or hardware may be stopped violates its
contract. Serial integration fixtures use a test-only authoritative task snapshot
for explicit infrastructure exemptions; no production exemption discovery API
or name-based rule is introduced.
