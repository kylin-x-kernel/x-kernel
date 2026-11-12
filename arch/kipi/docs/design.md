# kipi — Design Document

## Fallible generic callback ownership

`Callback` holds either a `KBox<dyn FnMut() + Send>` single-use adapter or an
existing `MulticastCallback` Arc. The adapter owns `Option<F>` for an arbitrary
`FnOnce`; invocation takes the concrete value before calling it. This needs no
unsafe unsized move and does not claim that KBox supports consuming-only erased
FnOnce directly. Consuming `Callback::call` invokes the body at most once;
discarding a callback drops its captures without invocation.

Preparation uses `new` (KERNEL), `new_nowait`, or `new_with_flags`. KERNEL
requires reclaim-safe sleepable task context with IRQs/preemption enabled,
outside non-sleepable RCU and without locks needed by reclaim, I/O or filesystem
callbacks. NOWAIT does not promise reserve access, bounded latency or NMI safety.
Capture construction/destruction obey their own context requirements.

`run_on_cpu` accepts a closure and validates the target first. Local execution
needs no owner allocation. Remote preparation uses NOWAIT before taking the
destination queue lock: callers may have IRQs/preemption disabled, and a generic
IPI sender cannot acquire a sleeping-reclaim requirement. NoMemory returns
before queue publication or notification. `run_on_cpu_prepared` accepts an owner
prepared earlier under the caller's chosen policy without boxing it again.
The previous infallible From<closure> conversion is removed.

Broadcast conversion retains the existing Arc directly instead of allocating
one boxed wrapper per CPU. This avoids new partial-broadcast failure semantics;
each event still owns one shared reference. The callback carrier now has an enum
discriminant in addition to its pointer payload. Initial Arc allocation and
VecDeque growth retain their existing infallible behavior.

The existing handler queue-lock scope is unchanged. Remote callback bodies and
capture destruction must not sleep, acquire IPI queue locks or wait for queued
callbacks. This change does not affect fixed notifications, TLB shootdown or
CPU-stop storage/protocols, and does not make the generic queue NMI-safe.

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Positioning

The `reschedule` feature selects fixed scheduler notifications; `kfeat/ipi`
selects it together with the ktask receiver. Depending on generic kipi alone, such as
in a non-SMP kworkng configuration, does not require a scheduler provider or
enable the scheduler handler branch. Fixed BH notification support is independent.

The local `smp` feature includes the remote instruction-cache, TLB and CPU-stop
providers. Only `kfeat/smp` assembles it with `karch/smp`, `khal/smp` and
`page_table/smp`; kipi does not enable dependency features. In UP builds,
generic notification queues remain available, while karch and khal retain
their local no-op providers. Linking generic kipi must not select SMP implicitly.

Fixed scheduler notifications are provided by `notification.rs`: `notify_reschedule` publishes the reason in a static per-CPU slot,
without going through the generic closure queue. The reasons and the one-way ENABLED bit share a single atomic word, and an AcqRel RMW
connects sending, enabling and draining. Before enabling, requests are only recorded; after enabling, an IPI is sent on every send, even if the reason is already set.
The common handling order is stop, TLB, fixed notifications, generic callbacks. `ktask` implements the handler side through the static
`kiface_task_api::TaskRescheduleRequestIf`, introducing no reverse crate dependency or dynamic subscription.
The primary CPU and the APs call `enable_fixed_notifications` at the stage specified by the runtime; this entry point uses an IRQ guard
to protect line enabling, ENABLED publication and the drain, and does not actively perform a preemption check on exit.
The kiface-task-api contract is implemented by ktask and consumed by this transport.
For outward reschedule delivery, kipi implements
`kiface_cpu_api::CpuRescheduleNotifyIf`; it does not own a second task interface.
The protocol has no runtime shutdown or CPU-hotplug support.

### RCU Notifications

Fixed `notify_rcu` requests have their own pending bit alongside scheduler,
timer and BH reasons. They retain the same publication/enable/drain handshake,
including deferred self delivery. The receiver only raises `SoftirqVec::Rcu`;
the task subsystem owns the action and GP wake. No reverse dependency on ktask,
new callback queue, task wake or reclamation operation belongs to this transport.
Callers install the action and prepare destination services before requesting it.

### Timer Reprogram Notifications

The independent `timer-reprogram` feature exports `notify_timer_reprogram` and
calls the `kiface_time_api::LocalTimerReprogramIf` contract defined in kiface-time-api.
`kfeat/ipi` selects it together with `ktask/ipi`, which supplies the endpoint.
Any fixed-notification feature compiles `notification.rs`; generic IPI users
without these features require no fixed endpoint. The scheduler and timer
features each require their own provider, including non-SMP configurations that
explicitly enable them. The BH feature calls kirq directly and needs no new provider.

`PendingWord` stores ENABLED, RESCHEDULE, TIMER_REPROGRAM, BOTTOM_HALF, RCU and
HIGHPRI_BOTTOM_HALF bits in one
`AtomicUsize`. Its private `NotificationFlags` bitflags type represents snapshots
and request masks; integer conversion stays at the atomic access boundary.
Senders publish
state first, then use an AcqRel fetch-or and send an IPI if ENABLED was observed.
Boot enablement uses the same word and drains retained reasons. The receiver
atomically takes all reasons, retaining ENABLED; requests arriving after that
take remain for a later IPI. Handling order is stop, TLB, fixed timer refresh,
fixed reschedule, BH pending publication, generic callbacks. Fixed refresh does
not queue a closure.

The timer sender must release timer locks before notification. The endpoint
re-reads local sources and programs hardware under their synchronization, without
invoking timer callbacks or taking runqueue locks. Notifications may coalesce;
successful return means accepted, not completed. Local IPI queue, scheduler,
timer state, controller and handler must be ready before the existing boot drain.

### Bottom-Half Notifications

Fixed-notification storage, boot handshake and BH notification APIs are always
compiled. Only scheduler and timer endpoints require feature selection because
their implementations are supplied by external providers. The built-in
`notify_bottom_half` and `notify_highpri_bottom_half` call kirq directly;
this stage does not change production runtime registration. The ordinary reason raises kirq's local Tasklet
vector; the high-priority reason raises High. Both reuse the existing pending word,
CPU validation, boot handshake and shared IPI. There is no additional pending
array, closure queue, allocation, callback interface or reverse dependency.

Senders publish source demand and release source locks before notifying. Reasons
are taken before delivery, so a new request during delivery remains for another
IPI. Both categories can coexist; coalescing is not a callback-count guarantee.
Every post-enable request sends an IPI, including self, as with timer notification.
IPI handling only marks softirq pending. Actual BH actions run at hardirq exit or
in ksoftirqd, under kirq's context and restart-budget rules, not in the fixed handler.

ENABLED means fixed IPI delivery is ready, not that a workqueue service is ready.
The service owner must install its action and prepare target state before exposing
producers. Boot drain can precede the AP's ksoftirqd creation: softirq pending is
static and retained, and the daemon checks it before registering/sleeping. The AP
startup NoPreempt guard remains necessary. No CPU hotplug/reset is supported.

This unit provides the transport only. The kworkng runtime owns action
registration and service publication. A non-IPI configuration
uses local kirq raising; linking kipi alone does not enable an IPI line.

`kipi` is the cross-CPU coordination module of X-Kernel. It provides the following upward:

- generic IPI callback delivery and broadcast;
- the implementation binding for cross-CPU I-cache invalidation;
- the cross-CPU notification and acknowledgment protocol for TLB shootdown;
- the system-level CPU stop protocol before reaching the platform power endpoint (the provider of `khal::power::SmpStopIf`,
  `stop_other_cpus`).

It sits on top of `kirq::notify_cpu()` and alongside the scheduler and page-table implementations, providing a unified entry point for
scenarios in the kernel where "another CPU must immediately execute a small piece of local work".

## Background

The kernel has two typical cross-CPU needs:

1. Have a specified CPU or a set of CPUs execute a short callback in interrupt context, for example an I-cache flush.
2. After a page-table modification, notify the other CPUs to discard their local TLB entries and wait for acknowledgment, so that stale mappings do not stay in effect.

Both needs rely on IPIs, but their constraints differ:

- Generic callbacks care more about delivery, queueing and broadcast semantics;
- TLB shootdown cares more about publication ordering, the acknowledgment protocol, concurrency safety and waiting behavior.

`kipi` puts them in the same crate and reuses the same IPI vector, but manages the protocol state in separate layers.

## Scope

Source files involved:

```text
arch/kipi/
├── src/
│   ├── lib.rs
│   ├── event.rs
│   ├── queue.rs
│   ├── sync_call.rs
│   ├── notification.rs
│   ├── icache.rs
│   ├── stop.rs
│   └── tlb.rs
└── Cargo.toml
```

Where:

- `lib.rs`: public API, per-CPU event queues, IPI handler;
- `event.rs`: unicast/broadcast callback model;
- `queue.rs`: per-CPU FIFO queue;
- `sync_call.rs`: task-bound completed execution on an explicitly selected boot-static CPU;
- `notification.rs`: allocation-free fixed reasons and their boot handshake;
- `icache.rs`: the crate interface implementation of cross-CPU I-cache flush;
- `stop.rs`: system-level CPU stop protocol (parking the other CPUs before the power endpoint);
- `tlb.rs`: TLB shootdown publication, IPI sending, acknowledgment and wait protocol.

## Architecture

`kipi` layers "generic event delivery" and the "TLB shootdown protocol" in the same IPI handler:

```text
Initiating CPU
   │
   ├─ run_on_cpu / run_on_each_cpu
   │      │
   │      ├─ enqueue the callback into the target CPU's IPI_EVENT_QUEUE
   │      └─ kirq::notify_cpu(kbuild_config::IPI_IRQ, ...)
   │
   └─ tlb::flush_remote
          │
          ├─ publish the per-initiating-CPU request slot
          ├─ bump the per-target-CPU pending epoch
          └─ kirq::notify_cpu(kbuild_config::IPI_IRQ, ...)
                     │
                     v
Target CPU IPI handler
   │
   ├─ tlb::handle_shootdown()
   │      ├─ fast path: check the pending epoch
   │      └─ slow path: scan request slots, flush and ack
   │
   └─ process ordinary callbacks in IPI_EVENT_QUEUE
```

There are two core state planes here:

- **Per-CPU event queue**: holds generic callbacks, executed in FIFO order;
- **Per-initiating-CPU request slot**: holds the TLB shootdown request;
- **Per-target-CPU pending epoch**: provides the TLB handler with an O(1) fast-path gate.

## Calling Constraints / Execution Context

### Generic API

- `init()` must initialize the local IPI queue on the corresponding CPU before that CPU can receive remote deliveries.
- `run_on_cpu()` / `run_on_each_cpu()` / `run_on_each_cpu_via_ipi()` may only be used after
  `kirq::notify_cpu()` is available.
- Callbacks must be short and must not rely on long-blocking semantics; they ultimately execute in the IPI handler.

### Execution Context

- `ipi_handler()` runs in interrupt context and must not sleep.
- `run_on_each_cpu()` executes the broadcast callback synchronously once on the current CPU, and on the other CPUs
  in IPI context.
- `run_on_each_cpu_via_ipi()` sends even the current CPU through the queue and IPI handler path.

### TLB Shootdown Constraints

- TLB shootdown depends on all APs having started; IPIs are only actually sent after `mark_all_cpus_started()`.
- `flush_remote()` disables preemption for the lifetime of the published request slot, preventing
  another task on the same CPU from reusing the same slot.
- The target set for user page-table shootdown is provided by the mm-owned residency maintained by the scheduling path;
  `flush_remote()` only consumes a snapshot and does not rebuild or reset the residency after the flush finishes.

## State Machine

### IPI Event Queue

Generic callbacks have no complex state machine; the lifecycle of a single event is:

```text
create Callback / MulticastCallback
    -> enqueue into the target CPU queue
    -> send IPI
    -> target CPU pop_one()
    -> run the callback
    -> destroy
```

### TLB shootdown request slot

Each initiating CPU owns one request slot, with the following lifecycle:

```text
inactive
   -> try_activate()
   -> clear_targets / mark_target
   -> publish(request_seq, vaddr/flush_all)
   -> bump the target CPU pending epoch
   -> send IPI
   -> wait for per-target ack (poll `handle_shootdown` while waiting, so it can ack the peer even with IRQs disabled)
   -> deactivate()
```

If the same CPU enters `flush_remote()` again while the slot is active, the slot is not immediately reentered or reused;
instead `needs_retry_full_flush` is set, and the outer caller issues one global retry after the current request finishes.

## Algorithm Flow

### `run_on_cpu`

1. Validate that the target `LogicalCpuId` is within the configured range and present in the logical CPU map.
2. If the target is the current CPU, execute the callback locally and directly.
3. If the target is a remote CPU, check whether its IPI queue has been initialized.
4. Push the callback into the target CPU's `IPI_EVENT_QUEUE`.
5. Call `kirq::notify_cpu(kbuild_config::IPI_IRQ, Specific(...))` to send the IPI.

### `run_on_each_cpu`

1. Validate that the queues of all remote CPUs are ready.
2. The current CPU executes the callback synchronously once.
3. Enqueue the unicast-ized callbacks for the remaining CPUs.
4. Send the broadcast IPI via `AllButSelf`.

### `ipi_handler`

1. With the `smp` feature, call `stop::handle_stop_request()` and then `tlb::handle_shootdown()`; then call `notification::handle_pending()` for fixed notifications.
2. Then loop to consume the ordinary IPI callbacks in this CPU's queue.

This order is deliberate: TLB shootdown is a page-table consistency protocol and has higher priority than ordinary cross-CPU callbacks.

### `tlb::flush_remote`

1. Obtain the current CPU's active request slot and disable preemption.
2. Generate, from the target `KCpuMask`, a target array that excludes the current CPU.
3. Allocate a new `request_seq`.
4. Mark the target CPUs and publish `(request_seq, vaddr | flush_all)`.
5. Bump the `pending epoch` for each target CPU.
6. Send the IPI.
7. Wait for each target CPU to advance `acked_seq_by_cpu[target]` to that `request_seq`.
   The wait loop polls `handle_shootdown()`: the page-table path often reaches
   `flush_remote` with IRQs disabled, and if it only waited for the IPI handler, two CPUs waiting on each other could never ack.
8. Release the active slot; if a nested request from the same CPU was detected in the meantime, issue a full retry flush.

### `tlb::handle_shootdown`

1. Read the current CPU's `pending_epoch` and compare it with this CPU's `LAST_HANDLED_PENDING_EPOCH`.
2. If unchanged, return directly without scanning all slots.
3. If changed, iterate over the request slots of all initiating CPUs.
4. For each slot:
   - read `published_seq` with `Acquire`;
   - if the request's targets include the current CPU and it has not yet acked, perform the local `flush_tlb(...)`;
   - update the ack for that initiating CPU.
5. Record the `pending_epoch` seen at entry as the handled value.

### `stop_other_cpus`

This module implements `khal::power::SmpStopIf` with `#[kiface::provide]` (UP builds do not
link this module; `khal` provides a no-op fallback); before the local IPI event queue is ready
(that is, before `kipi::init()`), the call degrades to a no-op and the endpoint falls straight through to the bare platform endpoint.

System-level CPU stop uses a **flag protocol** rather than a callback queue, which guarantees no memory allocation and delivery even
when the target CPU's IPI event queue is full:

1. The orchestrator uses a single `compare_exchange(NO_STOP_REQUESTED, its own logical
   CPU id)` on `STOP_STATE` to **elect the leader and publish at the same time**: this atomic value is both the stop
   request flag and the orchestrator id, so there is no intermediate state where "the request is visible but the id is unknown"; a concurrent
   caller fails the CAS, sets its ack and parks itself. After publishing, it sends a bare IPI to all other CPUs.
2. When any CPU enters `ipi_handler` and sees a stop request while not being the orchestrator,
   it first completes all cleanup that would still touch shared state (logging, masking local interrupts, quiescing the local
   NMI/pseudo-NMI sources via `khal::quiesce_nmi`), **and only then** sets its own `STOP_ACKED`
   -- the ack is the parking CPU's last write to shared state, after which it immediately enters a permanent
   `wfi/hlt` park with local exceptions masked. Once the orchestrator observes the ack, it can be sure
   that the CPU will never execute any shared-state path again.
3. The orchestrator polls for the acks of the CPUs in the wait set with a bounded 1s timeout. The wait set is snapshotted
   once at publish time: it contains only present CPUs whose IPI queue is initialized (see Design Decisions).
   CPUs that still have not acked after the timeout do not block the endpoint; a warning is logged for each and the procedure continues, matching the semantics of Linux
   `smp_send_stop()`.

The orchestrator holds a `NoPreempt` guard throughout, pinning it to the current CPU (see Design Decisions); otherwise the CPU id
on which claiming, broadcasting and waiting rely would become invalid after a migration.

Local NMI sources must be quiesced before parking: CPU-level exception masking cannot block a real NMI (such as the x86 NMI
watchdog); if they are not quiesced, a parked CPU would be repeatedly woken by the watchdog NMI, and the "intentional stop"
would be misjudged as a hard lockup, causing a panic followed by platform power-off, which breaks the semantics of `khal::power::halt()`.

## Concurrency Model

### Generic IPI queue

- The queue follows a **per-CPU exclusive consumer, remote producer** model.
- After an Acquire read of `IPI_QUEUE_READY`, a remote CPU obtains a shared reference to
  `LazyInit<SpinNoIrq<IpiEventQueue>>` and enqueues through the inner lock; it does not construct a `&mut` pointing at the remote slot
  just to take the lock.
- The current CPU calls `pop_one()` only in the local IPI handler.
- The queue lock is `SpinNoIrq`, because it is reached from the IPI handler.
- After the target CPU finishes initializing its local queue, it writes `IPI_QUEUE_READY` with Release; the sender's
  Acquire both rejects targets that are not ready and publishes the queue object inside the `LazyInit`.

### TLB request slot

- There is one slot per initiating CPU, which avoids multiple initiators sharing a single global pending bit.
- Slot publication is completed through `published_seq: Release`, and the consumer reads the remaining fields through
  `PublishedShootdownRequest` after an `Acquire`.
- `ActiveShootdownSlot` binds "preemption disabled + slot active" into a single guard type,
  preventing callers from forgetting to maintain these two constraints as a pair.

### Fast path gate

- `PENDING_EPOCH_BY_CPU[target]` is a **per-target-CPU monotonic counter**.
- Each time a shootdown request is published to a target CPU, the request is published first, then the epoch is bumped,
  and then the IPI is sent.
- The target CPU's `handle_shootdown()` compares the epoch first, so that an ordinary IPI does not also have to do an O(CPU_NUM)
  scan of all request slots.

A monotonic counter is used here instead of a boolean `pending` in order to avoid the lost-event window
when multiple initiators publish concurrently.

## Design Decisions

### Why the IPI queue and the TLB protocol share one IPI vector

Advantages:

- Avoids allocating a separate interrupt vector for each kind of cross-core control flow;
- Unifies the usage of `kirq::notify_cpu(kbuild_config::IPI_IRQ, ...)`;
- Makes the IPI handler the single cross-core control entry point.

Costs:

- The handler must distinguish the TLB protocol from ordinary callbacks;
- The TLB path needs a fast path gate, so that an ordinary IPI does not also go through a full slot scan.

### Why TLB shootdown uses "one slot per initiating CPU"

The old single global slot design let concurrent requests from different initiators overwrite each other or deadlock on each other.
After splitting slots per initiator:

- Request identity becomes `(initiator_cpu, request_seq)`;
- Each remote CPU acks each initiator separately;
- Different initiators can publish concurrently without sharing the same completion state.

### Why the publication protocol is wrapped in `PublishedShootdownRequest`

Reading `is_flush_all` and `published_vaddr` depends on the Release/Acquire chain of `published_seq`. Scattering
`Relaxed` loads across call sites makes it easy for maintainers to change the ordering by mistake.

After folding them into a "published request view" type, the protocol boundary becomes explicit:

- First establish Acquire through `load_published_request()`;
- Then read the remaining fields through the view.

### Why the publish-ordering contract of `notify_cpu()` is pushed down into the interrupt controller implementation

"Publish the request state in ordinary memory first, then send the IPI" is a contract of the IPI primitive, not merely a private requirement of
TLB shootdown.
Therefore `IntrManagerIf::notify_cpu` explicitly requires each architecture backend to guarantee
publish-before-notify semantics, rather than scattering fences at individual call sites.

### Why the stop protocol uses flags instead of IPI callbacks

- Stop must remain reachable even in the extreme case where "the target CPU's IPI queue may be full";
- The stop path is not allowed to allocate memory;
- Stop semantics are global and single-shot (only one orchestrator at a time), so one state word plus a
  per-CPU ack array is enough,
  with no need to reuse the queueing/acknowledgment mechanism of the callback queue.

### Why the stop state is expressed as a single atomic value

The request flag and the orchestrator identity are essentially the same state ("whether anyone has initiated a stop, and
who it is"). When split into two variables, `STOP_REQUESTED` + `STOP_ORCHESTRATOR`, a window remains whichever is
published first: if the request is published before the id is written, an unrelated IPI (such as a TLB
shootdown) arriving at the orchestrator itself within the window makes `handle_stop_request` see "stop already requested
but the id is still `NO_ORCHESTRATOR`"; the orchestrator cannot recognize itself, then acks
and parks -- the one CPU that should reach the platform endpoint stops itself, and the system neither halts nor powers off; and if
the order of the two statements is merely swapped, the store of a concurrent later arrival overwrites the id of the earlier one. Therefore they are merged
into a single `AtomicUsize` (`usize::MAX` means no request, and a CPU id means the request has been published by that
CPU), and a single `compare_exchange` completes both leader election and publication; when an observer reads the request with Acquire, it necessarily
sees the correct orchestrator id as well, and the protocol no longer has any intermediate state that needs extra
ordering guarantees.

### Why local NMI sources must be quiesced before parking

`karch::stop_cpu()` is now a terminal park with signature `-> !`: on AArch64 it uses `msr daifset,
#0xf` to mask all four DAIF classes of exceptions (debug, SError, IRQ including pseudo-NMI, FIQ), while other
architectures mask local maskable interrupts and loop on the wait instruction. But CPU-level masking cannot block a real
NMI (the x86 NMI is not controlled by IF). If the NMI sources are not stopped first:

- On platforms with an NMI watchdog (such as the x86 NMI watchdog), a parked CPU would be periodically
  woken by NMIs and could never truly stop;
- An NMI-driven hard-lockup watchdog would misjudge the "intentional stop" as a lockup, panic and go through
  platform power-off, making the distinction between `halt` and `power_off` ineffective.

Therefore `khal::quiesce_nmi()` is called before park; on platforms without NMI facilities it is a no-op,
introducing no new architectural coupling. On AArch64, where pseudo-NMI is already blocked by the full DAIF mask, quiescing the
PMU source additionally keeps the watchdog counter completely quiet, as defense in depth. The local NMI of the orchestrator (the terminal
CPU) itself is quiesced by `khal::power::halt()` / `power_off()` before calling the bare
platform endpoint, so whether or not an SMP stop hook is registered (such as in UP builds), the terminal CPU
will not be woken by the watchdog NMI.

### Why the orchestrator disables preemption throughout

The claimed orchestrator id, the `AllButSelf` broadcast and the wait loop are all anchored to the `current_cpu_index`
at the time of initiation. When the call comes from an ordinary syscall context (such as `sys_reboot`),
interrupts are enabled and the task is preemptible: if the protocol is migrated midway, the CPU that was left would be treated by all CPUs as the
orchestrator and would never park, while the real orchestrator waits for its ack until the timeout.
The `NoPreempt` guard pins the initiating CPU, at the cost of only a per-CPU counter.

### Why the wait set contains only CPUs whose IPI queue is ready

The wait set is snapshotted once when the stop request is published, and contains only present CPUs whose IPI queue is initialized.
A present CPU that has not yet reached `kipi::init()` (an AP that is slow to come up or failed to boot) cannot receive the
shared IPI and can never ack; leaving it in the wait set would mean every stop waits out the full 1 second
timeout and prints a misleading warning. The broadcast is still sent to all CPUs except self, and a late-ready CPU parks just the same when any
later IPI enters `handle_stop_request`, so shrinking the wait set only affects
waiting, not stop coverage, consistent with the best-effort semantics of Linux `smp_send_stop()`.

## Completed CPU Calls

`run_on_cpu_sync` in `src/sync_call.rs` owns completed execution, not interrupt
source retirement. It validates sleepable task context through
`kiface_task_wait_api::TaskCompletionWaitIf`, validates the target's queue and
fixed-notification readiness, and prepares a callback before publication.
The local branch pins CPU selection and masks IRQs through invocation. The
remote branch uses the existing IPI queue. A private completion is signaled
after the body returns and its captures are destroyed; the task waits outside
IRQ/preemption exclusion. No timeout is reported as completed execution.

This is the completed-call role of Linux `smp_call_function_single(..., wait=1)`
(`kernel/smp.c`, reference revision 8d3ae59288f1), not an implementation of
Linux CPU-hotplug locking. Unlike Linux's CSD spin wait, this task-only adapter
uses the existing prepared completion wait. Callback code must remain short,
nonblocking, and must not acquire IPI queue locks or sender-held locks.
Completion does not mean the surrounding IPI/controller has completed.

CPU membership and source stop/re-enable exclusion belong to the caller.
The supported lifecycle is boot-static CPUs with initialized, enabled IPI
services and no concurrent terminal stop. Readiness rejects early targets; it
is not an online lease. Hotplug requires a separate CPU-lifecycle capability
before this API can be used for dynamic participant sets. Permanent timer/IPI
services must not be dismantled using their own callback transport.

## Drop / Resource Release

- The lifetime of generic callbacks is managed naturally by `Callback` / `MulticastCallback`;
- Broadcast senders clone the Arc and wrap it in a single-use Callback without allocation;
- Events in `IpiEventQueue` are consumed after `pop_one()`;
- `ActiveShootdownSlot` automatically clears the active state through `Drop`, preventing exceptional paths from leaving a slot occupied;
- `kipi` itself owns no cross-CPU long-lived memory pools or device resources; its main resources are
  per-CPU queues and atomic state.


## Shootdown completion status for managed data retirement

The `TlbFlushIf` provider returns `TlbFlushResult` at the internal shootdown /
retirement boundary. `Ok(())` confirms every requested remote target's exact
published request, including any required full retry. Local hardware invalidation
remains synchronous and returns `()`; the architecture metadata hook performs it
before requesting remote completion.

Before `mark_all_cpus_started`, APs may already cache kernel translations but
cannot all acknowledge notifications. A request with a remote target therefore
returns `NotReady`, not successful completion. A scope with no remote target
needs no IPI and succeeds after the architecture's local invalidation.

Same-CPU nesting marks the outer request for a full retry and returns `Deferred`;
a future outer retry is not present completion proof. The bounded give-up path
logs and returns `TimedOut`. Request publication, resend and acknowledgement
ordering are unchanged. The page-table layer owns failed batches and retains or
pins managed holds; managed object owners do not consume per-flush booleans.
Synchronous revocation uses `PageTableMut::try_finish` and handles an error rather
than claiming that stale access was revoked. Raw release still needs such a
confirmed boundary until its owners are migrated.

Tests cover the closed boot gate with local-only and remote scopes, nested slot
ownership and empty remote masks without timing assertions or online-CPU mutation.
