# kipi — Security and Reliability Analysis

## Completed-call Boundary

`src/sync_call.rs` validates task context before publishing work. Target
validation checks both queue initialization and fixed-notification activation;
firmware presence alone is insufficient. A heap-owned Send + 'static callback
and Arc completion prevent a lost/stalled IPI from dangling borrowed task
storage. Failed preparation never invokes the body. After publication, the
task cannot return success until the callback signals completion after its
body and captures finish. This has no timeout-to-success escape.

The local branch retains IRQ/preemption exclusion across CPU selection and
invocation, then releases it before waiting. Remote callbacks retain existing
queue-lock restrictions. Captured destructors and bodies must not sleep,
reenable IRQs, acquire an IPI queue lock, or depend on the waiting sender.
No new unsafe operation is introduced. Callback-owner allocation is fallible;
the completion Arc and queue growth retain existing fatal allocation policy.

The caller owns a stable boot-static CPU participant set and excludes terminal
stop and source re-enable. Readiness is not a hotplug exclusion mechanism.
Completion proves software callback return, not hardware quiescence, IPI EOI,
or IRQ action drain. Using it to stop its own transport is unsupported.

## Fallible callback boundary

Single-use preparation stores a concrete FnOnce in an Option inside a KBox-backed
FnMut adapter. Taking that Option transfers the concrete capture safely; the
erased owner is dropped normally afterwards. No new unsafe operation, raw pointer
conversion or custom Send/Sync implementation is introduced. Captures remain
Send and the shared broadcast form remains Send + Sync through its Arc bound.

Remote owner allocation uses NOWAIT outside the target queue lock. Failed
preparation drops captures on the sender without invocation, publication or IPI
notification. Invalid/unready destinations are checked before preparation.
Prepared-owner rejection also destroys captures in sender context. Successful
remote execution and final capture destruction retain IRQ/queue-lock restrictions;
the caller must account for both success and rejection destruction paths.

Broadcast events reuse the existing Arc without a new fallible owner allocation,
so this migration introduces no allocation-error partial fanout. This is not
whole-transport OOM safety: initial Arc allocation and VecDeque growth still use
their old infallible allocator, and NoMemory specifically denotes the new
single-use owner boundary. No reserve, latency, NMI or hotplug guarantee is added.

Tests cover consuming-only captures, unused-owner cleanup, invalid flags,
shared-owner identity/lifetime, local IRQ-masked execution without preparation,
prepared-owner rejection, and injected remote preparation failure before
publication. A real remote dispatch also checks hardirq context, target CPU and
single destruction of a consuming-only capture. Injection replaces only the
private preparation operation; it does
not force global allocator OOM or queue-growth failure.

## Trust Model

### Fixed RCU Notification Boundary

`notify_rcu` validates the destination but not RCU readiness. Its fixed reason
only raises Rcu softirq; it cannot run callbacks or wake the GP task inline.
IRQ-off scheduler callers must release their locks before reenabling interrupts.
The destination action and services must exist before request publication; NMI
and CPU-hotplug use are excluded. Boot-pending RCU reasons use the same retained
word protocol and cannot overwrite scheduler, timer or BH reasons.

### Fixed Timer Notification Boundary

`notify_timer_reprogram` accepts only a typed CPU identifier from trusted kernel
callers, not a timer pointer or user payload. It validates bounds and CPU mapping,
but mapping presence alone does not prove service readiness. The caller publishes
timer state and releases timer locks first. `timer-reprogram` links its own static
provider; generic IPI and BH users need no scheduler or timer provider.

| Threat | Asset / trigger / impact | Response |
| --- | --- | --- |
| Lost refresh | Timer progress; independent ready/pending publication or clearing after handling loses a request | Mitigate: readiness and reason bits share one AcqRel RMW word; take reasons before processing, never clear afterward; retain pre-enable requests. |
| Cross-reason overwrite | Scheduler/timer progress; simultaneous requests erase another reason | Mitigate: fetch-or publishes independent bits, fetch-and retains ENABLED; test simultaneous reasons and requests during service. |
| Receiver deadlock | Availability; sender holds a timer lock required by the endpoint | Mitigate: release timer locks before sending; endpoint cannot sleep, invoke callbacks or acquire runqueue locks. |
| Premature endpoint access | State integrity; boot drain precedes local timer initialization | Mitigate: enablement contract includes local timer readiness; no CPU-hotplug/reset protocol. |

The new fixed timer path adds no unsafe operations, allocation, payload storage
or lifetime ownership. It relies on the interrupt backend's publish-before-notify
contract and eventual target IRQ service. Long IRQ masking remains a latency risk.

Scheduler and timer fixed-notification features require matching providers.
`kfeat/ipi` enables these two features together with `ktask/ipi`, which supplies
both endpoints. Built-in BH notifications call kirq directly.
The notification module is always compiled;
linking it does not enable delivery or satisfy service initialization contracts.
No empty fallback drops requests for a feature-selected endpoint.

### Fixed BH Notification Boundary

The protected asset is the static `PENDING: [PendingWord; NR_CPUS]`, including
independent ordinary/high-priority BH reasons. Only typed kernel CPU identifiers
enter the two public notification functions; no Work pointer, user data or closure
is accepted. Bounds and present-CPU mapping are checked before indexing/publication.
All existing atomic ordering and enablement invariants also protect these bits.
The BH path adds no unsafe code or allocation and does not own service storage.

| Threat | Asset / trigger / impact | Response |
| --- | --- | --- |
| Lost BH opportunity | Queue progress; clearing requests after delivery loses a concurrent kick | Mitigate: take reasons before delivery, retain ENABLED and reuse AcqRel request/enable handshake; category bits are independent. |
| Hardirq callback execution | Context integrity; an IPI directly executes business Work | Eliminate from this path: receiver only calls kirq's local softirq raise; kirq decides when a BH pass may run. |
| Premature service use | Availability; demand becomes visible before target action/state exists | Transfer readiness to the service owner: prepare and install before exposing producers; fixed ENABLED is not service readiness. |
| Early daemon absence | Progress; boot drain precedes AP ksoftirqd creation | Mitigate: retained static softirq pending and daemon's initial pending check; runtime keeps startup non-preemptible until local services exist. |
| IPI flood or masked IRQs | Availability/latency; excessive trusted notifications or blocked target | Accept existing transport limitation: reasons coalesce but post-enable calls each send an IPI; no rate/latency bound or NMI/hotplug support. |

Serial transport integration tests temporarily replace Tasklet/High actions and
restore them with kirq's scoped guards. They require quiescent production BH queues
and acknowledge callbacks before restoring handlers. They do not establish global
workng service lifetime/readiness or production-load performance acceptance.

RCU transport tests leave the boot-owned Rcu action installed. Persistent
unittest-only per-CPU counters observe delivery after the receiver marks the
vector pending. They verify deferred delivery with IRQs masked, not one-to-one
request acknowledgment: concurrent service requests may coalesce normally.

The fixed notification `notify_reschedule` does not require the ordinary callback queue to be ready, but the caller must already have published task state to the valid
target's run queue. CPU range/presence is validated at the entry; the static slot is neither allocated nor freed,
and does not hold task pointers. The normal handler must not take reasons before ENABLED; enablement and sending share
a single atomic word, avoiding the lost-request window between separate ready/pending variables. Reasons are not cleared after being taken,
so new reasons arriving during processing are retained. Hardware still relies on the publish-before-notify contract.
The scheduler endpoint only sets the local preemption flag; the independent
timer endpoint refreshes hardware. Neither takes a mutable RunQueue reference,
allocates or sleeps.
`enable_fixed_notifications` may only be called after the current scheduler, IPI queue and handler have been initialized;
it saves the IRQ state and does not use a preemption-disabling guard that would actively reschedule on drop. No new unsafe code is added.
The caller must also be on a CPU-bound startup path, because the internal lock used to enable the line may trigger existing preemption checks;
the primary CPU's late-init thread pins its affinity, and an AP enables the line while IRQs are off and before this round's pending is set.
If enabling the line fails, it panics and does not publish ENABLED, avoiding a mismatch between the software ready state and the hardware line state.

`kipi` is not a userspace-facing boundary module; its main callers are the kernel's page-table, I-cache and
cross-CPU coordination logic. The trust relationships are as follows:

```text
Kernel callers
   │
   │ safe API: run_on_cpu / run_on_each_cpu / run_on_each_cpu_via_ipi
   │           page_table::TlbFlushIf / karch::IcacheFlushIf
   │
   v
┌───────────────────────────────────────────┐
│                   kipi                    │
│  ┌──────────────┐   ┌──────────────────┐  │
│  │  IPI queue   │   │  TLB state path  │  │
│  └──────────────┘   └──────────────────┘  │
└───────────────────────────────────────────┘
   │
   │ kiface / checked publication / IRQ backend
   v
Platform interrupt controller and CPU-local execution context
```

Default trust assumptions:

- The caller uses `kipi` in the correct execution phase, i.e. the target CPU's local queue has been initialized;
- The platform `notify_cpu()` implementation satisfies the publish-before-notify contract;
- `ipi_handler()` can be delivered and executed normally on the target CPU.

## External Boundaries / Attack Surface

`kipi` does not directly receive userspace input, but it does cross several high-risk boundaries:

1. **Interrupt controller boundary**
   It interacts with backends such as GIC/APIC/SBI through `kirq::notify_cpu()`, and relies on their
   IPI delivery and publication ordering being correct.

2. **Per-CPU slot reference boundary**
   The private `ipi_queue()` builds a shared `LazyInit<SpinNoIrq<_>>`
   reference from an explicit CPU area, relying on the precondition that "the target CPU's local queue has been initialized"; queue modification is done only under the lock.

3. **Page-table consistency boundary**
   `tlb.rs` is coupled to the consistency protocol of `page_table` and `memspace`;
   if request publication, ack or retry logic goes wrong, stale TLB entries may remain in effect.

4. **Interrupt context boundary**
   `ipi_handler()` and the TLB shootdown handler run in a non-sleepable context,
   so they must not hold locks that may block, nor execute long paths.

5. **Callback boundary**
   Broadcast/unicast callbacks are supplied by other CPUs and executed on the target CPU. `kipi` only guarantees delivery,
   and does not verify whether the callback logic itself is suitable to run in IPI context.

This module does not directly involve:

- User memory or user pointers;
- DMA buffers;
- Network/filesystem external input;
- FFI.

## Unsafe Code Inventory

### 1. Explicit CPU-area access in `ipi_queue()`

Invariants:

- The target CPU must exist in the logical CPU mapping;
- The target CPU's local IPI queue must already be initialized;
- The return value may only be used as a shared reference; any production or consumption must acquire the inner `SpinNoIrq`.

Safeguards:

- The public API explicitly checks `IPI_QUEUE_READY` before remote enqueue;
- `IPI_QUEUE_READY` is published with Release by the target CPU after initialization, and checked with Acquire by the sender;
- The accessor does not construct a remote `&mut`; multiple producers and the local consumer are serialized by the same lock.

### 2. Guard-backed per-CPU access to `LAST_HANDLED_PENDING_EPOCH`

Invariants:

- Only the current CPU's local slot may be accessed;
- Another CPU's cached epoch must not be mistakenly written into the current CPU's local state.

Safeguards:

- The helpers use `load_current()` / `store_current()` as single by-value scalar accesses on the current CPU, without a guard and without creating references;
- The helpers only expose "current CPU read/write" interfaces and provide no cross-CPU write path.

### 3. Inline-assembly barriers in platform `notify_cpu()`

Locations:

- `drivers/platform/irq/src/gic/gicv2.rs:167`
- `drivers/platform/irq/src/gic/gicv3.rs:86`
- `drivers/platform/irq/src/riscv/mod.rs:83`

Invariants:

- The barrier must cover the ordering between "the request state being written to ordinary memory" and "the IPI being observed by the target CPU";
- Otherwise the target CPU may respond to the IPI first and then see the stale request state.

Safeguards:

- `IntrManagerIf::notify_cpu` now states this contract as an interface requirement;
- The AArch64 / RISC-V backends have explicitly added a barrier on the send path.

## Memory-Safety Invariants

The following invariants are critical to both the robustness and the memory safety of `kipi`:

1. **A per-CPU queue must be initialized before being accessed remotely**
   The private area accessor has no runtime recovery capability, so a violated precondition is a logic error;
   the Release/Acquire on `IPI_QUEUE_READY` is the publication protocol for this initialization.

2. **A TLB request must be published before the target CPU is allowed to see the IPI**
   Otherwise the target CPU may skip the request, and the initiating CPU will wait forever for the ack.

3. **The request slot of the same initiating CPU must not be reused by another task during the active lifetime**
   This is maintained by the no-preempt guard of `ActiveShootdownSlot`.

4. **TLB shootdown must not rebuild residency state inside the flush path**
   Residency is maintained by the scheduler switch path; the flush path may only consume a conservative snapshot.

5. **The pending epoch may only serve as a fast-path gate and cannot replace the real ack protocol**
   The epoch only decides "whether to scan"; the real completion condition is still `acked_seq_by_cpu`.

## Thread Safety

### Generic IPI queue

- Each target CPU's queue follows a multi-producer, single-consumer model;
- Producers serialize enqueue through `SpinNoIrq`;
- The consumer only pops in the target CPU's IPI handler.

### TLB shootdown

- `REQUEST_SLOTS` is sharded by initiating CPU, avoiding a single global pending state shared among initiators;
- `acked_seq_by_cpu` records acknowledgment progress per target CPU;
- `PENDING_EPOCH_BY_CPU` only reduces the scan cost of unrelated IPIs and does not change protocol correctness;
- `needs_retry_full_flush` is used as a fallback re-issue for nested requests on the same CPU, avoiding direct reentrant reuse of the slot.

## Threat Analysis

| ID | Threat | Trigger Condition | Impact | Mitigation |
|------|------|----------|------|------|
| T-01 | The target CPU handles the IPI before it observes the request state | The platform `notify_cpu()` lacks a publish barrier | TLB shootdown is lost and the initiating CPU spins forever | Define and implement the publish-before-notify contract in the IRQ backend |
| T-02 | Two tasks on the same CPU re-enter and reuse the same request slot | Preemption is allowed during the lifetime of `flush_remote()` | Request state is overwritten, which may deadlock or miss a flush | `ActiveShootdownSlot` binds no-preempt to the active slot |
| T-02a | Two cores' `flush_remote` calls wait on each other's ack | IRQs are off while waiting, so the peer's shootdown is not handled | The whole machine hangs during parallel unmaps such as libc-test | The wait loop polls `handle_shootdown()` and does not depend on this core having entered the IPI handler |
| T-03 | Ordinary IPIs share the same vector and cause pointless full-table scans | Every IPI unconditionally scans the TLB slots | Frequent unrelated IPIs add O(CPU_NUM) hot-path overhead | Per-target `pending_epoch` fast path |
| T-04 | A caller broadcasts logic unsuitable for interrupt context to other CPUs as a callback | The callback sleeps, takes a big lock, or runs a long path | IPI handler latency, lock-ordering problems, reduced availability | The `kipi` documentation requires callbacks to be suitable for that execution context; the module provides no safety net |
| T-05 | The queue is enqueued to remotely before it is initialized | An AP has not yet run `init()` but is already an IPI target | The slot is accessed through a raw per-CPU reference before it is ready | The API explicitly checks `IPI_QUEUE_READY` and returns `TargetCpuNotReady` on failure |
| T-06 | The stop IPI cannot reach a CPU spinning with interrupts disabled | The target CPU masks local interrupts for a long time and never enters `ipi_handler` | That CPU is not parked and still mutates shared state before the final step | After a bounded 1s timeout, warn and continue (matching Linux `smp_send_stop`) without blocking the final step |
| T-07 | An NMI watchdog wakes a parked CPU or misjudges the deliberate stop as a lockup | The platform has an NMI/pseudo-NMI hard-lockup watchdog and the NMI source is still enabled after park | The parked CPU is repeatedly woken; after the watchdog panics the platform powers off, turning `halt` into `power_off` | Call `khal::quiesce_nmi()` before park to quiesce the local NMI source (a no-op on platforms without NMI) |
| T-08 | The stop orchestrator task is migrated midway through the protocol | The final step is entered from a preemptible syscall context, and the claim id and broadcast/wait are based on a stale CPU id | The CPU that was left is treated as the orchestrator and never parks, so the protocol necessarily runs out the timeout and leaves a CPU running | `stop_other_cpus` holds a `NoPreempt` guard throughout, pinning the orchestrator to the CPU it claimed |
| T-09 | The wait set includes a present CPU whose IPI queue is not initialized | A stop is triggered before an AP has run `kipi::init()` | That CPU can never ack, so every stop needlessly waits the full 1s timeout and emits a false warning | Snapshot the wait set once at publish time, containing only CPUs whose queues are ready; the broadcast still covers all other CPUs |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Local Effect | System Effect | Detection/Mitigation |
|------|----------|----------|----------|-----------|
| F-01 | The target CPU's IPI queue is not initialized | `run_on_cpu` / broadcast fails | The related cross-core operation cannot be performed | Returns `KipiError::TargetCpuNotReady` |
| F-02 | Queue growth allocation fails | Event cannot be stored | Existing allocator OOM policy is fatal | VecDeque growth remains infallible; QueueFull is not currently emitted |
| F-03 | The target CPU does not ack the TLB request for a long time | The initiating CPU spin-waits | The page-table modification path stalls, which may look like a system hang | Warns at 1 s, re-sends the IPI every 1 s and escalates at 10 s; after 30 s it logs an error, flushes only the local TLB and returns `TimedOut` without the target's ack (a stale remote TLB entry may remain). The wait loop polls `handle_shootdown` to avoid IRQ-off mutual waits. |
| F-04 | The pending epoch is not updated synchronously | The target CPU fast path returns directly without scanning the slots | False negatives / hangs on unit tests or protocol misuse | The production path is encapsulated by `flush_remote()`; unit tests update it synchronously through a helper |
| F-05 | Callback panics | Dispatch does not complete normally | Kernel panic policy applies | No per-callback panic isolation or continuation guarantee |
| F-06 | Single-use owner allocation fails | No event is published or notified | Caller decides whether to retry or fail | Return NoMemory and drop captures in sender context |

## Failure Management

- The generic IPI delivery path returns errors as a `Result` instead of failing silently;
- The main TLB shootdown failure mode is a target CPU that does not ack. The initiator spins with a bounded wait: it warns at 1 s, re-sends the IPI every 1 s, escalates at 10 s, and after 30 s logs an error, flushes only the local TLB and returns `TimedOut` without the target's ack;
- A nested TLB request on the same CPU does not panic directly; instead it sets `needs_retry_full_flush` and is re-issued after the current request finishes;
- The requirements on the platform's IPI ordering semantics are written into the interface contract, so implicit dependencies are not scattered across call sites.

## Privacy Analysis

`kipi` does not handle user private data, and does not directly touch user buffers, file contents, or network payloads.
What it handles is mainly:

- CPU IDs;
- Callback objects;
- TLB shootdown protocol state;
- Addresses and request sequence numbers in diagnostic logs.

Note that diagnostic logs may contain virtual addresses or CPU topology information, so they should still be treated as kernel-internal debugging information.

## Known Limitations

- Generic IPI callbacks and TLB shootdown share a single IPI vector, so the design requires a stable handler order;
- TLB shootdown spins while waiting for acks, which is unsuitable for targets with extremely long latency;
- `kipi` only guarantees cross-CPU delivery; it does not guarantee that the callback itself is suitable to run in IPI interrupt context;
- `pending_epoch` is only a performance-optimization gate and not complete protocol state, so a test that manipulates internal slots directly
  must update this gate in sync.

## Audit Checklist

- Check whether every newly added `notify_cpu()` call site depends on publish-before-notify semantics.
- Check whether any path enters code that may sleep within the lifetime of an `ActiveShootdownSlot`.
- Check whether `flush_remote` polls `handle_shootdown` while waiting for acks (two cores waiting on each other with IRQs off).
- Check whether any new test that hand-builds request slot state updates `pending_epoch` in sync.
- Check whether a new broadcast callback can really run in the IPI handler context without sleeping or occupying the CPU for a long time.
- Check whether a new platform IRQ backend honors the ordering contract of `IntrManagerIf::notify_cpu`.


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
