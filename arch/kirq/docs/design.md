# kirq — Design Document

## Runtime Affinity

`runtime/affinity.rs` owns lookup-only routing transactions; `model/affinity.rs`
defines snapshots/errors and `backend/affinity.rs` defines immediate and
deferred irqchip operations. `set_irq_affinity` uses management admission to
retain one Line, then its short lock to serialize against source control and
flow completion. Registry exclusion is not held during chip programming.
`irq_affinity` only copies the locked observation and never reads hardware or
creates a mapping.

`IrqAffinitySnapshot` separates requested policy, effective routing, the
`is_pending` accepted-but-not-effective state, and hardware failure. `Ok`
from `set_irq_affinity` means the request was accepted; callers must inspect
`is_pending` before treating the request as effective. Immediate GIC/PLIC
transactions return `is_pending = false`. Deferred transactions store no
second pending mask: the pending target is `requested`.

`IntrManagerIf::immediate_affinity` selects permanent immediate operations for
the exact source. `IntrManagerIf::deferred_affinity` selects a controller that
needs real ACK/EOI progress. PerCpu, NMI, MSI and fixed descriptor affinity are
rejected. A deferred request is validated and recorded without writing the RTE
and without masking the old route; the old route remains usable until a real
hardware completion. Replacing the request replaces `requested`/pending, but
cannot clear a latched routing failure.

The generic flow layer only connects the two real completion points:

- edge ACK: KIRQ invokes the backend transaction before EOI, then EOI, then
  resumes action handling. The backend masks, writes/verifies destination and
  restores mask before returning; KIRQ commits effective before EOI.
- level Fasteoi: KIRQ invokes prepare before EOI (mask), performs EOI, then
  invokes finish. The backend checks Remote IRR after EOI; still-set keeps the
  request pending without changing destination, while clear permits the
  verified write and mask restoration.

The generic layer does not interpret APIC registers. `IrqDispatch::complete`
is the hardware-claim path; replay uses a distinct `complete_software_only`
path, so an empty completion closure cannot be mistaken for EOI. Replay and a
completion without a matching Line never advance deferred state. No worker,
timer, pending table, or in-flight counter is introduced. A pending request may
remain indefinitely when no further interrupt arrives, and queries expose that
state.

Pre-write rejection preserves the preceding snapshot. Hardware failure retains
the last accepted request, clears effective routing and closes ordinary flow,
registration and enable admission. Recovery uses existing disable depth:
disable the source if it is not already logically disabled, retry routing with
`KeepMasked`, then explicitly enable (or use ordinary auto-enable registration).
A successful retry does not clear an unrelated trigger-configuration fault.
Ordinary configuration cannot clear a routing fault or overwrite a route
established through this API. A fault also forbids descriptor/flow rebinding,
retaining the original recovery capability even after the last action is gone.

Ordinary disable retains pending. Last-action withdrawal cancels pending and
reconciles `requested` to `effective` (or `None` when no route was confirmed)
without clearing a routing failure. Retaining a Line for later re-registration
does not resurrect the cancelled request; a new affinity request is required.
Line retirement stops deferred progress and old completions cannot affect a
later Line. Source-identity rebinding otherwise invalidates observations.

A failed route retried while disabled is accepted as pending while the fault
stays latched. Activation and registration preflight recognize only that narrow
`pending + currently disabled` recovery state; they do not relax the general
enable guard. Regular/shared registration still publishes no action until the
masked prepare/finish pair verifies the route; a failed recovery returns without
publishing. Configuration/enable then runs the same pair without a real EOI. A
verified finish clears the fault before unmask. If the pair cannot complete and
no confirmed old route exists, activation fails closed. If a confirmed old route
exists, it may resume service while the request remains pending. This decision
uses current suppression and confirmed route availability, not
`effective == None` as an activation-history proxy.

### Initial Policy And Action History

The first successfully published action establishes an otherwise-absent
`requested` policy. Ordinary `IrqAffinity::Any` uses
`kcpu_id_map::present_mask()`; fixed `IrqAffinity::Cpu(n)` uses `{n}`. Source-
managed PerCpu/NMI/MSI/no-flow sources do not receive an ordinary default.
The value is computed before publication and committed only after action
publication succeeds; a rejected/configured-failing registration leaves no
policy or history. Explicitly established requested values are retained.

The four boot entries call `init_boot_cpu_id_map` before primary kernel/IRQ
initialization. If the map is unexpectedly unavailable, ordinary Any
registration fails with `IrqDescError::NotReady` before action/history/policy
publication. No procfs read or user setter initializes it.

`IrqAffinity::Cpu(n)` is range-checked against the compile-time logical CPU
bound before constructing a mask; an impossible descriptor returns
`InvalidCpu` without action/history/policy mutation. "Representable" is not the
same claim as "present".

NMI publication uses the same prepare/commit path as regular/shared actions.
A source-managed NMI without explicit owner policy remains unknown; an explicit
fixed owner policy is committed only after the NMI action publishes.

`has_action_history` is set only by successful regular/shared/NMI publication.
It is not cleared by action removal; it is a lifecycle fact, not a VFS
visibility flag.

### Line Retirement Subscription

`IrqLineHandle::subscribe_retirement` allocates an independent intrusive slot
outside the Line lock, then verifies the exact Line is still the current
directory entry under `IRQ_STATE -> Line`. The slot links without allocation.
Retirement marks the Line retired, moves the entire list under the Line lock,
releases KIRQ locks, and then invokes observers. An unlinked subscription stops
future notification but never waits for an already captured callback. A
subscription whose Line already retired does not touch intrusive links.

Only a real Line retirement notifies; ordinary `free_irq` action removal does
not. Current production retirement is x86_64 `free_msix`; callback runs in its
sleepable task context without KIRQ locks and may enter the procfs namespace.

## Sparse Line Access And Retained Identity

`runtime/handle.rs` owns the opaque `IrqLineHandle` and the public
`lookup_irq_line` / `next_irq_line` operations. `next_irq_line` is inclusive and
uses the XArray's sparse successor under array writer exclusion; it skips
retired candidates only after releasing that exclusion, advances with
`checked_add(1)`, and treats `usize::MAX` as terminal. Enumeration is
task/control-only, rejects NMI, hardirq, serving-softirq, BH-disabled task and
active RCU readers before taking the array lock, and creates no mapping or
action.

The handle retains the exact Line Arc observed in `DIRECTORY`; the virq is a
label, not identity or permission. `is_same_line` compares the underlying Line,
not the wrapper allocation; `is_current` checks the directory Arc and retired
bit. `affinity_snapshot` copies the stored policy without a source-capability
check, while the existing `irq_affinity` remains capability-gated. Neither
query reads hardware or creates a mapping lease.

`has_action_history` records successful publication and survives action
removal. It does not imply that an action is currently installed.

`is_user_affinity_source_supported` is a static source/descriptor
classification, checked under `IRQ_STATE -> Line`: ordinary immediate/deferred
Any sources are user-routable; fixed, PerCpu/NMI/MSI, no-flow and controllerless
sources are not. It does not inspect action presence, per-action restrictions or
temporary routing state. Procfs uses it only when constructing stable inode
permissions; writes independently re-run the checked setter.

Set uses `with_retained_line`: acquire the expected Line's management mutex,
take `IRQ_STATE`, verify `descs[virq]` is still the same Arc, take the Line,
reject retirement, call `begin_management`, release Line then `IRQ_STATE`, and
run `apply_locked` under a fresh Line guard. Management admission is ended on
every exit before the mutex is released. `is_managed` keeps `is_unused()`
false across the gap between short locks; the mutex alone is not the exclusion
proof. A stale handle returns `UnknownIrq` before hardware and never redirects
to a replacement Line. This access is trusted kernel control, not user
authorization or source-owner policy.

## User Routing Protection

`IrqRegisterOptions::restrict_user_affinity` declares a source owner's
user-routing restriction for a regular action. The flag lives in the existing
`IrqAction` (`USER_AFFINITY_RESTRICTED`) and is published atomically with that
action by `publish_regular_action`; there is no Line cache, global table or
separate policy count. `ActionSet` scans installed action flags under Line
exclusion on the control path. Removing the action removes the restriction.
Protected shared registration is not supported in this PR; existing
regular/shared conflict rules reject the combination.

This is deliberately **user-only**, not Linux `IRQF_NOBALANCING` / `IRQD_NO_BALANCING`.
The trusted kernel `set_irq_affinity` remains policy-independent (but still
subject to source, context, fixed, PerCpu, NMI, MSI and failure rules), and
automatic initial routing is unchanged. A source owner that needs a specific
initial route must establish it through the trusted path before requiring
protection. PMU SPI-to-CPU association and real-delivery validation remain
Ticket07 owner integration.

Observation and write eligibility are distinct:

- `IrqLineHandle::irq_affinity` remains the identity-bound observation and is
  available for a protected Line; policy does not hide requested/effective/
  pending/failure state.
- `IrqLineHandle::check_user_affinity_change` samples change eligibility
  (`source -> teardown -> action presence -> policy`) without hardware access.
  It is not an authorization token and does not promise a target mask is
  programmable.
- `IrqLineHandle::set_user_affinity` repeats the checks under
  `with_retained_line` and the Line lock, then runs the existing
  `apply_user_locked` transaction. Policy denial is before the chip callback.
  No-action and teardown reject the user change entrance; they do not suppress
  observation.

The PMU adapter's ordinary SPI path uses
`IrqRegisterOptions::disabled()` or `auto_enable()` with
`restrict_user_affinity()`; the PPI and real-NMI paths retain their existing
lifecycle. `enable_irq` remains non-sleeping and does not call a task affinity
setter.

## Shared Descriptor Values

IrqTrigger, IrqController and IrqDomainId are re-exported from irq-types with
unchanged public kirq paths. Only value definitions move; domain, mapping,
handler and release behavior stay in kirq. No driver-contract dependency exists.

## Consumer Lifecycle Order

Initialization follows `map -> consumer ready -> register/configure -> source
production`. Mapping establishes identity and flow, not action ownership or
permission to start the producer. Before registration can expose a callback,
the consumer must have initialized the state and resources that callback may
access. Default ordinary registration enables delivery; disabled registration
keeps delivery closed until explicit enable. Device production starts only when
the consumer and delivery path are ready.

For shared detach, the consumer stops its own producer and excludes restart,
then unlinks its action and drains admitted execution before freeing callback
resources. Non-last removal does not stop other producers or mask surviving
actions. Mapping lifetime is independent of action lifetime: ordinary action
removal retains the descriptor, and MSI allocation release has separate owner
and hardware-quiescence requirements. PerCpu and NMI retain their dedicated
source-owner shutdown contracts; this sequence does not replace them.

## Interrupt Entry Responsibilities

The execution boundary is separate from mapping and action ownership:

| Stage | Owner | Required ordering |
| --- | --- | --- |
| Trap capture | `kcpu` architecture entry | Save interrupted state and exclude ordinary IRQ delivery before calling the trap callback. AArch64 additionally saves/restores PMR or the hardware-NMI mask. |
| Trap adaptation | `khal::irq` | Add the preemption credit; do not reinterpret an arbitrary task call as a hardware entry. |
| Ordinary entry | `runtime/entry.rs` | Establish RCU watching, lifecycle hooks and hardirq context before asking the irqchip to claim. |
| Claim/classification | `IntrManagerIf` provider | Acknowledge the actual controller source, classify IRQ versus NMI, and transfer each successful claim to exactly one generic consumer. Spurious delivery creates no claim. |
| Action and completion | `runtime/dispatch.rs` or `runtime/nmi.rs`, orchestrated by `runtime/entry.rs` | Retain the admitted software objects through the original controller completion; completion ordering follows the selected flow, not a universal after-handler rule. |
| Ordinary exit | `runtime/entry.rs`, then `khal::irq` | Finish hardirq/deferred work, lifecycle hooks and watching before HAL releases its preemption credit and runs the completed-IRQ scheduling checkpoint. |

"IRQ-on" describes the interrupted context, not the mask during dispatch.
Ordinary IRQs remain excluded even when GIC opens an NMI nesting window.
An NMI classified after ordinary entry retains that outer IRQ lifecycle and
exit tail. Dedicated NMI entry instead establishes effective NMI context and
watching without ordinary lifecycle hooks, deferred work or the completed-IRQ
scheduling checkpoint. Its claim scope includes controller completion.

This follows Linux's separation between architecture entry
(`arch/arm64/kernel/entry-common.c`, `__el1_irq` / `__el1_pnmi`), generic IRQ
context (`kernel/softirq.c`, `irq_enter_rcu` / `irq_exit_rcu`) and irqchip
claim/classification. The Rust module boundary is different; it does not add
another controller lock or hardware shutdown guarantee.

The current VMM does not directly call these KIRQ entry functions. AArch64
VM-exit handling restores host delivery and lets architecture entry service the
interrupt. Any future direct VM-exit adapter must establish the complete trap
contract, not merely hold an IRQ mask. Trap callbacks, HAL and KIRQ entry
functions, and `IntrManagerIf` dispatch/completion methods are unsafe boundaries.
`PendingIrq::new` transfers the unique claim through an unsafe constructor;
completion and Drop rely on its lifetime-long CPU/context obligations. The
`kiface` facade preserves the qualifier and rejects mismatched provider safety.
These signatures express the proof obligation, not an additional masking or
scheduling mechanism; see `security.md` for the complete contract.

The two generic claim consumers remain safe: they take ownership of an already
validated `PendingIrq`, rather than asking the irqchip to repeat its construction
proof. The normal consumer rejects effective NMI context before taking normal
flow locks; the NMI consumer establishes its own effective NMI scope. This
adds no token, allocation or extra IRQ mask. Only the raw trap entry, controller
entry/completion bridge and raw claim constructor carry unsafe caller contracts.

## NMI Action Lifetime

`runtime/nmi/action.rs::NmiAction` owns one stable `KBox<Handler>` in
`IrqLine::nmi_action`. Its per-CPU `AtomicPtr<Handler>` slots publish access
to that same immutable owner; they are neither per-CPU action owners nor reader
counters. Registration supports only `IrqFlow::PerCpu`; unbound and other flows
are rejected before publication. Normal-IRQ claims for an NMI-owned Line do not
invoke this owner or publish a second ordinary action. No live replacement is offered.

### Publication and scoped access

Registration prepares the owner outside short locks and installs it under
Line management/state exclusion. Installation alone does not publish CPU-local
access. `prepare_local_action` first completes local controller preparation,
then release-publishes the current CPU's slot under continuous Line and CPU/IRQ
exclusion. Initial preparation failure leaves no local publication and retains
the controller cleanup obligation. Enable must follow successful preparation.

The private `NmiAction::with_current` owns a `NoPreemptIrqSave` guard, selects
the current CPU, acquire-loads its slot and lends the handler only to its
callback. The borrow cannot escape that callback. It neither locks nor clones
an owner, updates a reference count, allocates or waits. True NMI dispatch also
has an outer `runtime::rcu::with_nmi_claim` CPU/IRQ guard spanning dispatch and
original claim completion. The Domain/XArray read scope spans both operations
to retain the Line, independently of action ownership.

`dispatch_claim` rejects normal-IRQ handler delivery for an NMI-owned Line.
It completes the original claim under the already-held Line lock, without
execution admission, a CPU-local owner borrow, handler invocation or new diagnostics.
Drain takes that same Line lock, so it cannot pass an ongoing rejected completion.
This is software serialization, not hardware quiescence. Neither this rejection
nor true NMI queues replay. Ordinary shared-action drain and
reclamation have separate requirements and are unchanged.

### Local revocation and final release

Unsafe `teardown_percpu_nmi` requires the source owner to stop local generation,
exclude restart and ensure no active claim crosses the transition. Under the
Line lock and CPU/IRQ guard, it completes backend teardown before clearing only
the current CPU's publication. Failure retains the local publication and
preparation obligation; it does not authorize owner release. Local IRQ exclusion
prevents ordinary revocation during a borrow, and a nested NMI finishes before
the interrupted local teardown resumes. Remote CPUs cannot revoke that slot.

Removal closes preparation/enable admission and checks all local enabled and
preparation records. `NmiAction::take`, under management and Line exclusion,
also refuses any nonempty publication slot. Only after every local revocation
may it recover the sole owner. The returned owner is destroyed outside Line,
CPU/IRQ and management guards. Removal needs neither an action grace period
nor a running RCU callback service; directory retirement still uses RCU.

### Source-owner ordering

Local bookkeeping is not a remote execution barrier. A PMU start may pass its
source gate, tear down a vacant local source and pause before preparing it
again. An enabled/prepared-only snapshot would omit that admitted start.
The PMU owner therefore closes admission and performs synchronous stop calls on
every boot-static CPU permitted to start, including unprepared CPUs. Those
completed target calls follow each CPU's old pinned IRQ-off interval before
restoration can admit a new generation.

Each stop must also stop counters, disable delivery and complete controller
teardown; acknowledgement alone does not prove hardware pending/active state
is clear. An earlier local NMI finishes its original PMU claim before the normal
stop callback resumes. Completion of that callback is not completion of its
surrounding, unrelated IPI claim. This protocol assumes boot-ready CPUs and
excludes concurrent CPU hotplug or stop. Other source owners must provide
equivalent start-versus-restore exclusion.

Partial shutdown failure retains the action, Closing and unfinished local
obligations for retry. Local disable is non-nesting, so retry needs no separate
disable-credit bookkeeping. Restoration must not
reopen the source gate before old admitted starts and local claims are excluded.
This is the installed action-lifetime protocol, not a general shared-action
reclamation scheme or a change to the RCU grace-period algorithm.

## NMI Close Admission

NMI removal first closes enable admission on the retained Line. If any tracked
CPU-local slot remains enabled, removal returns false without withdrawing the
handler owner. Closing survives management-mutex release: both ordinary and
force-enable reject it, while local disable remains available. The owner can
finish local shutdown and retry removal on the same Line. Successful withdrawal
recovers the owner only after every local publication is revoked. Management
cleanup releases teardown/admission before the returned owner is destroyed
outside its guards; an active shutdown claim keeps Closing through destruction.

`prepare_percpu_nmi` and `teardown_percpu_nmi` retain the selected Line and pin
the executing CPU through the controller transaction. A local slot retains its
exact `PerCpuNmiChip` before the first write, including on partial failure.
Failed preparation/teardown prohibits enable. Free rejects any remaining local
preparation record, not just enabled slots. Closing permits disable/teardown
but rejects further preparation. Repeated successful preparation is a no-op.

The GIC provider admits standard PPIs only. It waits for Redistributor RWP,
checks disabled and inactive state, and verifies attribute programming. Teardown
also clears and checks pending state after the source owner stopped generation.
PMU startup prepares each CPU before enabling its line; later watchdog setup
rechecks admission without rewriting an enabled source. PMU counter control
stays in the source owner, outside the Line lock. Terminal watchdog quiescence
still retains its callback and preparation, rather than claiming full teardown.

These controller checks are not a remote-handler drain. Source owners must
stop producers and confirm each CPU's disable/teardown before removal.
The single `NmiAction` owner serves only true NMI dispatch through CPU-local
publication. Completed local teardown revokes access only after the old handler
and its original claim have finished. Empty publication slots permit owner
recovery without an action RCU wait or a second ordinary in-flight wait.
No global NMI table or exclusion of unrelated NMI sources is needed. Directory
RCU remains independent and cannot clear pending hardware.

`runtime/nmi/shutdown.rs` owns `NmiShutdown`, a non-cloneable close claim.
`begin_nmi_shutdown` retains the original Line, closes admission and snapshots
only CPU slots with enabled or prepared state. A second claim and ordinary
unregister are rejected while this claim exists. A source owner may share it
through `Arc` with remote work, then recover unique ownership after callback
captures have been destroyed. `finish` checks original-Line identity and local
obligations before withdrawing the action. Failure returns the same handle.
Closing remains in force through the old handler's destruction and claim release.
Dropping an unfinished handle abandons the claim but leaves Closing and the
action intact for retry; forgetting it conservatively prevents retirement.
Neither operation implicitly broadcasts, stops a producer or proves physical
drain. Source coordination and CPU availability remain caller responsibilities.

The unittest-only `NmiShutdownProbe` substitutes a synthetic controller and is
re-exported through `tests_irq` for runtime integration. The runtime fixture
uses real synchronous IPI calls on two boot-ready CPUs: one completes teardown,
the other retains its preparation after a failed acknowledgement. It checks
that ordinary removal and final retirement cannot release the action early,
then retries only the unfinished CPU. Capture destruction precedes recovery of
the unique shutdown handle. This tests software composition, not GIC delivery.

## Fallible Index Ownership

Domain and logical indexes use prepared XArray storage. NOWAIT allocation
precedes registry and Line exclusion; commit publishes neither entry when
capacity is missing. Allocation failure returns `IrqDescError::NoMemory`;
an existing binding needs no new capacity. Dynamic numbering may still advance
after an unsuccessful attempt. Backend hardware validation remains separate
from container indexing; the former IO-APIC linear-table size limit no longer
bounds a sparse XArray.

MSI publication prepares both eventual retirement records and retains them in
the Line. Preparation failure returns the unpublished reservation to the
allocation caller for backend rollback. Release allocates no records; rejected
release retains both records with the live reservation for retry. Successful
withdrawal closes Line admission, removes both indexes, submits their RCU
retirement records outside exclusion, and only then consumes the reservation
in the backend. Old readers retain storage, not permission to admit service.
The source owner still must stop device messages and satisfy transport ordering.

## CPU-local Ordinary Flow

`prepare_percpu_forwarding` retains an already registered Line and obtains the
backend's `PerCpuForwardingChip`. It does not start a producer or transfer
resource ownership. The consumer calls unsafe `set_local_state` under IRQ and
migration exclusion to delegate guest completion or return it to the host.
The consumer must retain registration, exclude competing management and stop
the producer before returning ownership. No claim may cross this transition.
GICv2 supplies this capability for validated ordinary PPIs; other providers
return None. This is separate from guest-interface discovery:
`IntrManagerIf::guest_interface` supplies firmware-validated GICH/GICV physical
pages, not permission to delegate any particular IRQ.

`IrqFlow::PerCpu` keeps enablement, configuration generation
and failed-configuration admission in one `LocalControl` per logical CPU inside
the retained Line. These bounded slots are protected by the Line lock; they are
not an online-CPU inventory. The action owner is shared; its PerCpu borrow
lifetime is protected by source-owner shutdown, not a shared execution count.
The outer IRQ/preemption guard keeps CPU selection, hardware apply and software
commit on the same CPU. An enabled local slot blocks flow replacement and Line
reclamation even without actions.

Generic regular/startup and shared registration reject resolved PerCpu flow
with `UnsupportedFlow` before controller configuration or action publication.
Dedicated registration also refuses enabled local slots; it is not a disable
operation. Local nonsynchronous disable remains responsible for masking hardware
before clearing the calling CPU's enabled state. Linux's
`irq_percpu_disable` likewise pairs hardware masking with the CPU's enabled bit
(`kernel/irq/chip.c`). Other CPU slots remain unchanged.
Local disable is non-nesting: repeated calls mask the same source, and one
enable reopens it. Both operations apply the controller gate on every call,
matching Linux `irq_percpu_enable` / `irq_percpu_disable`. Ordinary shared Lines
retain nested disable depth; this change does not relax source-stop obligations.

`try_register_percpu(spec, handler)` provides a narrower single-action contract.
It requires sleepable task context and an already prepared descriptor. The
resolved flow must be `PerCpu`, otherwise registration returns `UnsupportedFlow`.
An occupied Line or any enabled local slot returns `Ok(false)` without configuring
hardware or publishing the handler. Post-resolution rejection may retain merged
descriptor metadata; failure does not promise metadata rollback.
Successful registration configures only the calling CPU and leaves delivery
disabled; every participating CPU must explicitly
enable its own source. `try_register_percpu_startup` has the same ownership and
disabled-slot requirements with the existing nonwaiting startup context contract.
Allocation/configuration failures retain the ordinary registration error contract.

GICv3 irq_work SGI15, fixed runtime timer/IPI handlers and the VMM host-vtimer
route use these APIs. Generic free, synchronous disable and synchronize reject
PerCpu flow; local enable and nonsynchronous disable remain available.
PerCpu execution has no count. Clear enabled slots are admission
prerequisites, not proof that old claims completed or hardware producers stopped.
Registration provides no source-owned stop/free protocol or runtime unload guarantee.

Like Linux `handle_percpu_irq`, local handlers do not acquire a shared running
owner or create pending/replay work: callbacks may overlap across CPUs. The short
Line lock checks local eligibility and captures actions for accepted execution,
which then runs and completes unlocked under source-owner lifetime protection. Rejected local delivery skips
actions and consumes its original claim exactly once under the retained Line
lock. Action removal refuses a Line with any enabled local slot, but this is
only a necessary check. Before unsafe removal, the owner must stop every
participating source, wait for every admitted dispatch to return fully,
and exclude restart and new dispatch until removal returns. Generic synchronous
drain is not available for PerCpu. CPU membership remains source-owner responsibility.
Removal neither masks the freeing CPU nor adds a local disable operation: the
owner has already completed shutdown on all participants, as in Linux
`__free_percpu_irq` (`kernel/irq/manage.c`).

`ut-tests/local_control.rs` checks repeated local disable followed by one enable,
independent CPU state, configuration and recovery;
the runtime `local_flow_handlers_overlap_on_distinct_cpus` fixture checks parallel
callbacks and disabled delivery using synthetic claims, not physical IRQ timing.

## Ordinary Level Flow

`IrqFlow::Level` uses `LevelChip::mask_ack` under the retained Line lock before
actions, consumes the original claim completion once, and rejects running or
ineligible arrivals with pending recorded. Actions run without that lock. The
running owner clears running and unmasks only if the Line still admits actions;
disable, teardown and configuration failure prevent reopening. Eligible level
pending is satisfied by hardware reassertion, never an IPI or software replay.
Dispatch admission includes the tail unmask transaction. This follows Linux
`kernel/irq/chip.c::handle_level_irq`, distinct from Edge and Fasteoi flows.

The LoongArch provider acknowledges EIOINTC ISR before child dispatch; the PCH
level hook masks the leaf and parent gates. Its leaf ack is empty for level
sources, and unmask clears PCH cause before reopening parent and leaf. No GIC-like
late deactivation is invented for this hierarchy. Local per-CPU sources remain
separate from this level flow; ordinary local sources use `PerCpu`. Tests in
`ut-tests/level_dispatch.rs` cover overlapping and disabled claims, one completion
per claim, no recursive action execution and no tail unmask after handler disable.

## One-shot Local irq_work

`irq_work` is a permanent per-CPU bridge from NMI to ordinary hardirq, not a
worker thread or a sleepable execution provider. Runtime calls `init_current_cpu`
after scheduler/controller/allocator setup and before exposing NMI consumers.
The platform prepares a dedicated ordinary IRQ and returns a permanent, local,
NMI-safe notification function. Unsupported providers return None; queue returns
NotReady without consuming the caller's node. No teardown or CPU hotplug exists.

`IrqWork` is a stable-address, one-shot raw callback node. Its unsafe queue
contract transfers responsibility and requires storage to survive until unlink
and callback handoff. A claimed node cannot be reused, even after completion.
There is no cancel/flush API and no strong-reference or allocator policy here.

NMI producers use the lock-free context query before touching ordinary guards.
Normal producers pin execution and mask IRQs. A release-CAS pushes a node onto
the local atomic stack; every successful producer sends a notification. Hardware
may coalesce signals because the queue retains each responsibility. No FIFO or
wait-free guarantee is provided. NMI never runs the consumer inline.

The ordinary IRQ consumer acquire-detaches batches and preserves unfinished
nodes in a CPU-local backlog. It executes at most 64 callbacks per invocation,
then re-notifies for leftovers. Producers never modify the detached backlog.
The consumer copies next and callback before calling; it never touches that node
again, allowing callbacks to free or forward their allocation. A callback may
be interrupted by a producer NMI; new incoming nodes remain discoverable at the
budget boundary. Callbacks must preserve IRQ-off context and cannot sleep.

AArch64 GICv3 uses reserved ordinary SGI15 with direct system-register writes,
not the existing locked/logging sender. Other current backends explicitly lack
the capability. See irq-driver docs for affinity and readiness constraints.
Higher layers may bridge to kworkng from the callback, but this does not make
standard Arc/Weak metadata frees or DeferredArc automatically NMI-safe.

## Interrupt Watching And NMI Context

The static `krcu_core::RcuContextIf::current_cpu_watching` contract borrows
permanent CPU state owned by `krcu`. Its provider is linked before GP service
readiness and requires initialized CPU identity, not a current task or GP task.
`kirq` owns neither the provider nor the system RCU arrays and cleanup policy.
Ordinary IRQ entry establishes watching before lifecycle hooks and hardirq
bookkeeping, retaining it through controller completion, deferred execution and
lifecycle exit. Its explicit exit precedes the HAL scheduling tail.
Dedicated NMI entry and controller-classified NMI dispatch use the same nested
scope operation. Effective NMI depth is established before watching entry and
removed after watching exit; claim completion stays inside that scope.
Depth counts scopes, not physical interrupts. This covers a dedicated entry
which later invokes the common classified-claim helper without retiring the
outer context early.

NMI depth uses a shared atomic CPU slot, not the ordinary context lock.
`is_in_nmi`, `interrupt_context_level`, `is_softirq_context` and the combined
interrupt predicate can classify NMI without that lock. Detailed ordinary
context snapshots remain unsuitable for NMI. Snapshot queries mask IRQs before
selecting the CPU slot and balance their internal preemption credit without
running scheduler/RCU checks. They are observations, not scheduling checkpoints;
normal IRQ tails and other explicit checkpoints retain pending-work handling.
Runtime lifecycle callbacks keep
their existing locking mechanism and are not used to implement RCU entry.
The idle host may enter quiescence around its architecture wait. An interrupt
activating that CPU restores quiescence before returning to idle; the host's
outer preemption guard excludes scheduling until idle restores watching.
User-return callbacks restore watching before calling the IRQ dispatcher, so
its HAL tail may schedule in watched state. The krcu GP service consumes
ordered watching observations through krcu-core; kirq owns balanced interrupt entry
and exit, not grace-period coordination.

## Atomic Softirq Group Registration

`bottom_half/softirq.rs::open_softirqs` registers a bounded set of fixed actions
under one action-table lock. It validates every vector and existing slot before
writing, rejecting duplicate vectors, oversized input or conflicts unchanged.
An empty group succeeds without effects. Single registration delegates to this
transaction and retains its existing conflict warning. This supports publishing
both workqueue BH priorities without partial installation. It does not initialize
the caller's providers, modify pending bits or offer runtime unregistration.
Dispatch copies an action under the table lock and releases it before invocation.

## Effective Softirq Context

`context::is_softirq_context` observes `interrupt_context_level` and is true only
when the effective level is Softirq. A hardirq interrupting a softirq handler
therefore returns false, while `is_serving_softirq` continues to report the active
softirq nesting. Execution services use the former to reject invalid BH dispatch
before acquiring work. This query neither pins the CPU nor serializes passes;
the caller establishes those conditions. Existing nesting APIs are unchanged.

## Positioning

`kirq` is the generic interrupt core of X-Kernel. It sits between the architecture trap adaptation layer and the platform interrupt
controller backends, and provides drivers, IPI, timer and NMI users with unified IRQ description,
registration, mapping, enabling, dispatch and completion semantics.

`kirq` sits above irqchip drivers, which implement controller operations
(configure, mask/unmask, claim, complete, priority and IPI delivery) through
`kirq::IntrManagerIf`. IRQ context, softirq and threaded IRQ semantics belong
to kirq. General sleepable workqueues belong to `kworkng`; kirq supplies
IRQ/BH context and dispatch capabilities.

`khal::irq` retains only the trap-entry adapter and HAL-specific helpers; generic IRQ state and
dispatch ownership belong to `kirq`.

## Background

Before the refactoring, the generic IRQ descriptor, domain mapping, dispatch, NMI, MSI and later
bottom-half extension points were scattered across the HAL and platform/driver glue. This easily led to two kinds of problems: first, different
platforms interpreted hwirq/virq/domain inconsistently; second, to stay compatible with control-plane state, the interrupt hot path
reintroduced global locks or implicit identity fallbacks.

`kirq` owns descriptor publication and Domain mappings. Normal dispatch resolves
stable `PendingIrq`/`IrqRef` claims directly to retained Lines; numeric-only
queries borrow the same Domain XArray. Trap adapters, irqchip backends
and devres providers retain only their integration responsibilities.

## Test Layout

Mapping-only fixtures use two `cfg(unittest)` software domains with strict lookup
and independent XArray indexes, including equal-hwirq namespace-isolation tests.
They do not borrow a real controller's domain for synthetic out-of-range hardware
IDs. Both registry entries are absent from production builds; real-provider
admission tests remain separate.

The guest tests and the two shared fixture modules live under `ut-tests/` at the crate root and are mounted in their original parent modules through
`unittest::path_mod_test!`. Module names, visibility, and registration paths are unchanged;
the external test call paths of `context::test_support` and `softirq::test_support` are also unchanged.

| File (relative to `ut-tests/`) | Test cases | Additional gate |
| --- | --- | --- |
| `backend_msi.rs` | 1 | None |
| `backend_msi_unittest_tests.rs` | 1 | Non-x86_64 |
| `bottom_half_context.rs` | 10 | None, 9 of them serial |
| `bottom_half_context_test_support.rs` | 0 | fixture |
| `bottom_half_deferred.rs` | 6 | None, all serial |
| `bottom_half_softirq.rs` | 17 | None, all serial |
| `bottom_half_softirq_test_support.rs` | 0 | fixture |
| `runtime_action.rs` | 6 | None |
| `runtime_manager.rs` | 49 | None, 43 of them serial |
| `rcu_irq_entry.rs` | 2 | None, 1 of them serial |
| `rcu_context.rs` | 1 | None |
| `unhandled_unittest.rs` | 4 | None |
| `irq_work.rs` | 6 | None, all serial |

There are 13 guest modules in total (including 2 fixtures with zero test cases) and 103 test cases; x86_64 has 102 test cases.
82 test cases remain serial, and there are no user test cases. All guest modules require `cfg(unittest)`.
`context::test_support::with_nmi_context` retains the safety contract of the mainline; the newly added NMI classification
test case verifies, while holding the ordinary context lock, that the query bypasses that lock. The separate mount of `tests_rcu_entry` retains
the RCU watching balance checks for dedicated NMI and nested IRQ/NMI.

The four test cases for unhandled IRQ counting also retain host coverage: `tests/host/unhandled.rs` uses
`#[test]`, and `ut-tests/unhandled_unittest.rs` uses `#[unittest::def_test]`.
The two test bodies are identical and respectively verify the reporting boundary, suppression counting, and saturation semantics; later changes must be kept in sync.
The host mount retains the module name `unhandled_unittest`, with the condition `all(test, not(unittest))`;
the guest macro invocation has an additional outer `cfg(unittest)`, so a standalone host import of the production source does not need to resolve
`unittest`, and the two mounts are mutually exclusive. The host tests were not converted to guest tests, and no public test API was introduced.

## Ordinary Flow And Replay

`dispatch_claim` resolves and retains the original Line, checks it under its
state lock without a directory lock, and selects one private flow handler with
an exhaustive match. Accepted ordinary execution takes admission under that
lock before snapshot/unlock; rejected ordinary claims complete under the lock
without a credit. Each handler consumes the already-held Line
guard and the original completion closure; it owns admission rejection, action
execution and the flow-specific completion order. Level mask/ack precedes claim
completion and actions; Edge completes before actions and consumes coalesced
pending edges; Fasteoi completes after actions under the original Line lock.
There is no production Legacy flow or Legacy dispatch path. An unbound Line
cannot execute actions, but an arriving claim still receives completion.
The outer IRQ/preemption guard
spans the entire operation, including the directory-to-Line handoff. No helper
releases and reacquires the Line merely to select or admit a flow.
The PerCpu handler checks local admission under that same guard. Rejection
completes under the retained Line lock without execution admission; acceptance
borrows the action and runs it and completion unlocked, without an execution
count or ordinary running gate. The source owner must exclude reclamation until
every admitted dispatch returns, including its handler, original completion and tail work.

`IrqStateDesc::flow` is `Option<IrqFlow>`: a newly prepared candidate starts at
`None`, which is not a dispatch policy or fallback capability. Backend selection
must establish `Some(PerCpu)` or explicit Fasteoi, Edge or Level operations.
Unknown or foreign sources return `UnsupportedFlow`; fixed local sources retain
their explicitly selected PerCpu flow. Standard PLIC and ordinary GIC SPIs use Fasteoi: GIC uses hardware
pending retrigger and PLIC software replay. x86 selects Edge or level Fasteoi;
ordinary LoongArch PCH sources use Level mask/ack and conditional unmask.
No source acquires an implicit production fallback flow.
PLIC validates declared domain/controller, supported source bounds, flags,
trigger/polarity and fixed-target policy. Unknown trigger is accepted without
inventing a level annotation. Selection is independent of BH executor readiness.
The stored flow and descriptor are protected by Line state; live actions,
aggregate execution or an explicit queued-replay flag prevent incompatible binding
changes. After the last action is withdrawn, admitted completion and queued
service still preserve their original flow and source description through these
separate gates; queue membership does not imply execution admission.
Rejection preserves the descriptor generation and existing directory/mapping
identity. Once these gates and enabled local-CPU ownership are absent, compatible metadata enrichment and
flow reselection are allowed; this is not dynamic hardware-affinity migration.

Private fixtures use `cfg(unittest)` `IrqStateDesc::test_new` to select an
explicit fake Fasteoi chip, or install a specific test flow. This is not a
production fallback and does not validate a real controller binding. Binding
tests distinguish retained action/execution ownership from provider rejection
of unsupported identities; idle state does not make an unsupported source valid.

`generic_handle_irq` is only for the active `IntrManagerIf::dispatch_irq`
invocation on the CPU that created the claim, with local IRQs masked and generic
hardirq context already entered. It is not an arbitrary safe software-dispatch
interface; synthetic fixture dispatch does not broaden this production contract.
The legacy boolean enable bridge and force-enable behavior remain control APIs
for an admitted binding, not a Legacy flow or a way to bypass source validation.

Fasteoi admission sets running only for an eligible callback chain. A rejected
concurrent claim completes itself without clearing that ownership or creating
an unconditional replay. Disabled/action-less delivery sets pending and invokes
mask before original-cookie completion under the same Line lock. Handlers run
outside registry/Line locks. The owner reacquires the original Line, clears
running, completes the original claim while still locked, then releases Line
state before scheduling replay and dropping admission. Lock order is registry
then Line for admission, and Line then chip for hardware transactions; no
registry lock is held across completion. A normal tail does not unmask.

Registration delivery/enable plans carry `flow_unmask` independently of the optional
management `enable` write. Preparation is read-only: it computes the next disable
depth and selects unmask using the proposed enabled/depth state plus current
action and teardown ownership. Application runs configure, optional enable,
then optional flow unmask under Line exclusion. Only after application returns
does `commit_platform_plan` publish configured generation, enabled state and
disable depth, before releasing the same Line guard. Discarding a prepared
plan leaves those fields unchanged and preparation retryable. The private plan
must not outlive that transaction or be committed to another Line. A logically
enabled Line suppressed
by a no-action arrival therefore recovers when its first action is registered,
even if `enable` is None. Pending replay is considered after the transaction. PLIC
owns configured priority and hardware masking, so masked priority updates and
the enable-time default do not independently release suppression. These source
operations do not replace generic disable depth or teardown state.

`IntrManagerIf::configure` returns `Result<(), IrqDescError>`. A configuration
error stops application before enable/flow-unmask and before committing the
prepared control state. Regular/shared/disabled registration configures before
installing a new action; NMI registration configures before publishing its
sole Line-owned action. Both normal and legacy enable propagate the
error without committing configured generation, enabled state or disable depth.
No error return claims hardware rollback: descriptor metadata and mappings may
already have been resolved, previous actions remain owned, and a provider may
have masked or partly configured the source. A retry must configure it again.
For a source previously enabled, logical enable accounting may remain unchanged
while failed hardware configuration leaves delivery masked. Callers must handle
the error rather than assume the old hardware state is restored.
Disable/withdraw delivery operations retain their separate infallible contract;
they do not configure a source. LoongArch retains its existing completion
guarantees, not a new readback guarantee. The x86 IO-APIC provider masks and
checks trigger/polarity readback before restoring previous delivery. PLIC wiring is
fixed and its configuration hook has no register operation. The checked GIC
provider supplies actual trigger readback, with controller-specific completion.
Regular/shared registration now applies and commits stale configuration before
installing the new action, then computes the delivery plan with that action
present. Both stages retain the same Line guard. Rejected action admission
precedes configuration; clean metadata skips the configuration stage only when
no configuration failure is latched. Disabled
regular registration also configures before publishing ownership, without
enabling delivery; a later enable reuses that configuration. NMI configuration
precedes Line-owned publication while the source owner keeps delivery stopped.
Configuration failure and delivery restoration are separate commit boundaries;
unchanged logical accounting does not certify the prior hardware state.

`runtime/resend.rs` owns an `intrusive-collections::LinkedList` whose
`ReplayAdapter` retains `Arc<IrqLine>` owners through each Line's embedded
`LinkedListAtomicLink`. Queue operations remain serialized by the queue lock;
the atomic link does not make the queue lock-free. Enqueue allocates no storage,
and dequeue transfers the retained owner to replay. Eligibility requires
an ordinary flow, pending, enabled depth-zero actions, no teardown, no running
owner and no queued replay. Edge flow requests hardware retrigger and never
enters the software queue. Level flow clears pending without replay. Only after
these conditions hold, explicit `LevelHigh`
or `LevelLow`, or a provider-validated `FasteoiResend::Level`, clears pending
without enqueueing, relying on the validated level
contract for hardware reassertion. Nested disable therefore retains pending
until final enable makes service eligible. Neither PLIC identity nor Fasteoi
selection alone supplies this annotation. PLIC selects software resend; an unknown
trigger alone does not authorize software service for another controller.
The generic LevelLow rule does not broaden the PLIC provider's accepted
trigger configurations. Insertion marks membership under Line exclusion but takes
no execution admission, then links under the queue lock;
notification follows unlocking.
Consumers pop before taking Line state, clear membership and recheck eligibility.
The queue retains no client callback snapshot. If withdrawal or a hardware
dispatch made service unnecessary, the consumer releases its retained owner
without handler or chip access. Otherwise it calls `begin_running_dispatch` under the
Line lock after eligibility checks and before capturing current actions, then
runs them under ordinary running ownership with
local IRQs/preemption excluded. Software replay has no hardware claim or cookie
and completes through a no-op callback. Work is bounded to 64 entries per run,
with another notification when the queue remains nonempty.

Hardware-only resend is a separate chip capability. Under Line exclusion, an
eligible non-level source invokes its retrigger operation instead of entering
the software queue. Success consumes pending; failure retains pending for a
later eligible scheduling attempt, without falling back to a BH callback.
There is no automatic retry timer or delivery guarantee on failure. Hardware
failure is not currently reported through the management API; a provider with
reachable rejection must close that diagnostic/recovery prerequisite before
production integration. Unlike Linux's error-returning resend helper, this
internal path retains pending on rejection. Hardware
acceptance creates neither a software in-flight credit nor a completed claim:
the subsequent real interrupt follows its own flow admission policy and completes
its own cookie. Drain does not imply that the controller has no pending request.
This separation follows Linux `kernel/irq/resend.c` and the GIC drivers'
`handle_enforce_irqctx` requirement. Ordinary GIC SPIs select this policy and
issue a vendor ISPENDR write without a readback acceptance test, since another
CPU may already have consumed the pending bit. Private/PER_CPU and NMI-owned
sources retain their separate contract. The current forwarded virtual timer is
a PPI and is not admitted to this flow; future SPI forwarding needs explicit
ownership integration before publication.

Last-action withdrawal suppresses replay by changing Line eligibility; it does
not wait for queued entries. There is no eager cancellation/unlink operation.
Pending service belongs to the live Line, not an action-registration epoch.
If an action is later registered on that same live Line, delayed replay may
service the newly eligible action, never the removed action. Linux's
`clear_irq_resend` can remove queued work but cannot cancel an already-popped
entry; KIRQ currently retains lazy queue removal instead of that eager cleanup.
These are different queue policies, not identical cancellation guarantees.
Non-last shared withdrawal leaves surviving actions eligible for dispatch and
replay, subject to their normal enable, configuration and flow state. It adds no
whole-Line teardown credit. After the retained-Line execution drain,
`with_action_removal` explicitly completes successful removal under the original Line lock,
without a registry handoff. Flow restoration is considered only when completion
actually releases a teardown credit and `can_run_actions` permits it. Ungated
survivors own their normal flow mask/unmask interval; removal completion must not
unmask over a running survivor. Pending service is reconsidered after unlocking.
Unknown-trigger pending can queue replay; explicit level pending follows the
level rule above. Last-action removal, disable depth or remaining teardown does
not reopen the source.

`with_existing_line` instead runs a general lookup-only management transaction.
It retains the selected Line, takes its management mutex, rechecks directory
identity and pins membership before invoking the body. Its result is only data:
returning a value does not release teardown, unmask a source or schedule replay.
`ManagementAdmission::drop` releases only membership admission on ordinary
success or failure. The removal wrapper explicitly completes teardown and
admission together before reconsidering pending service, while the same mutex
is still held. Removed handler owners leave this scope before destruction.
NMI shutdown preparation uses the general transaction; NMI withdrawal uses the
removal wrapper. Neither operation creates or replaces an IRQ mapping.
`is_running` owns accepted Level/Edge/Fasteoi execution and running replay;
PerCpu execution has no counter and relies on source-owner lifetime protection.
Normal-IRQ claims for NMI-owned Lines complete under the Line lock
without execution admission.
Ordinary drain checks running ownership, not PerCpu execution or pending queue entries.
Sync/free may return before a queued executor runs: it has no saved action cursor
and cannot snapshot an already removed action. It must reject retired, disabled
or action-less Lines before handler or chip access. Queue membership explicitly
blocks incompatible flow/source mutation even though it is not execution admission.
Waiters must release short locks and permit admitted executions to finish. The permanent
`kworkng` BH adapter is bound after service publication, before current runtime
console-input and device registration. Earlier queues remain retained and are
kicked at installation. Executor progress is required for pending service and
eventual release of queued owners, not for an aggregate-idle synchronous wait.
Retaining a Line allocation does not stop a source or authorize release of
device resources; those remain separate owner obligations.

Fasteoi replay is not the policy for Edge, PerCpu, NMI or threaded/oneshot
sources. It proves neither hardware quiescence nor physical redelivery.
The [per-CPU source lifecycle](#per-cpu-source-lifecycle)
records current timer/IPI/PPI consumers, the distinction between Line-wide
disable depth and CPU-local enable state, and required shutdown acknowledgements.
PerCpu has independent local admission; completed remote
shutdown and shared-action retirement still require their lifecycle owners.
Validation results must be recorded separately; this description adds no claim
of unperformed hardware tests or complete Linux IRQ-flow parity. The fixture
`explicit_level_waits_for_final_enable_then_relies_on_hardware` checks pending
retention through nested disable, clearing at final enable, an empty replay queue
and no software callback; it does not demonstrate hardware reassertion.

## Controller Flow Contracts

Paths below are relative to the repository root. Entries describe code-visible
ordering, not a claim that every controller has equivalent EOI semantics.

| Provider / source | Before generic fanout | Completion after fanout | Control and remaining boundary |
| --- | --- | --- | --- |
| GICv2: `drivers/platform/irq/src/gic/gicv2.rs`, `claim_irq` | Ack, reject spurious, EOI priority drop | DIR using the claim cookie unless this CPU's `FORWARDED_PPIS` delegates deactivation to a guest | Ordinary SPIs select Fasteoi; SGIs and PPIs, including raw timer 27 and explicitly tagged private sources, select PerCpu. Flow selection does not permit normal-IRQ invocation of an NMI owner. Explicit GIC SPIs tagged PER_CPU are rejected during flow selection. GICv2 does not provide NMI preparation. The resident guest HW LR owns delegated deactivation; detached sources return to host completion. |
| GICv3: `drivers/platform/irq/src/gic/gicv3.rs`, `ack_irq` | Ack, classify NMI, EOI priority drop | `complete_irq` issues DIR | Ordinary SPIs select Fasteoi independently of claim classification. SGIs and PPIs, including raw timer 27 and explicitly tagged private sources, select PerCpu; explicit GIC SPIs tagged PER_CPU are rejected during flow selection. NMI-class claims enter generic NMI. Normal-IRQ claims for NMI-owned Lines complete without handler invocation. Guest forwarding is not supported by this backend. |
| x86 IO-APIC level: `drivers/platform/irq/src/x86/mod.rs`, `dispatch_regular_irq` | Decode vector, retain nonzero completion cookie | Deferred prepare before LAPIC EOI, then finish under the same Line exclusion after Fasteoi handlers | Selected Fasteoi gates admission; Remote IRR still set keeps the affinity request pending without changing destination; validated hardware level reasserts after unmask. |
| x86 IO-APIC edge and MSI-X: same provider | Transfer nonzero EOI cookie to generic Edge | For IO-APIC affinity, deferred destination transaction completes before LAPIC EOI; MSI has no IO-APIC destination transaction | IO-APIC edge migration follows Linux ACK-before-EOI order; overlapping arrivals latch pending for the running owner. Re-enable retriggers using the recorded backend target/vector, not an arbitrary self-IPI. MSI device masking/release remains owner-controlled. |
| x86 local timer/IPI vectors: same provider | LAPIC EOI before generic dispatch | Zero cookie: no further EOI | PerCpu controls local admission independently; remote producer shutdown remains an owner obligation. |
| RISC-V PLIC: `drivers/platform/irq/src/riscv/mod.rs`, `dispatch_irq` | Claim under PLIC lock; release before generic admission | Complete original claim on its claiming context under the retained Fasteoi Line lock | Validated standard PLIC uses Fasteoi with KIRQ pending/replay; fixed effective target and priority masking remain provider-owned. |
| RISC-V local timer/software causes: same provider | Software cause clears SSIP; timer service handles its own deadline | Skip cookie: no PLIC completion | PerCpu classification permits concurrent handlers with independent local SIE gating. |
| LoongArch: `platforms/kplat-loongarch64/src/irq.rs`, `dispatch_irq` | Timer clears TICLR; EIOINTC ISR is consumed before child dispatch | No-op | Ordinary PCH sources use Level mask/ack and conditional unmask. Parent W1C and leaf CLR are separate; edge is rejected. Local timer uses PerCpu. |

### GIC Configuration And Retrigger

Ordinary SPIs must be below the discovered Distributor's exclusive INTID limit
and below 1020. A numeric SPI range does not exclude forwarding: adding
forwarded SPIs requires an explicit deactivation ownership contract.
GICv2 checks masked trigger readback. GICv3 additionally uses the maintained
vendor's bounded `Gic::wait_for_distributor_rwp` after requesting masking.
Controller locking covers configuration and conditional delivery restoration.
Fallible configuration reports `ConfigurationFailed { hwirq }` on readback
or RWP failure without restoring delivery. The infallible mask path logs RWP
timeout but cannot certify masking completion. Partial writes are not rolled
back, and neither readback nor software drain proves hardware quiescence.

SPI mask/unmask preserves routing and priority. Retrigger writes ISPENDR
without polling pending, which another CPU may already have consumed, or RWP,
which does not track that write. It creates no software replay owner.
ACK, priority-drop and DIR ownership remain with the original claim, including
rejected arrivals. Vendor provenance and licensing are in `vendor/README.md`.

### PLIC Resend Rationale

Linux revision `8d3ae59288f1e7d58d76558a6ee96d533bc5019f` provides the comparison:
`drivers/irqchip/irq-sifive-plic.c` selects fasteoi with `IRQ_TYPE_NONE`;
`kernel/irq/resend.c::check_irq_resend` relies on hardware reassertion only
for explicitly level-marked sources. Standard PLIC has no hardware retrigger
callback, and RISC-V selects `HARDIRQS_SW_RESEND`.

The [PLIC interrupt gateway specification](https://github.com/riscv/riscv-plic-spec/blob/master/riscv-plic.adoc#interrupt-gateways)
distinguishes level reassertion from edge gateways: completion permits a new
request but does not recreate a consumed edge. Fasteoi selection alone cannot
justify dropping pending for an unknown-trigger source. KIRQ uses its
retained-Line queue and the permanent kworkng BH executor, not a second owner
of `SoftirqVec::Tasklet`. Hardware-active queries, dynamic retargeting,
device shutdown and safe MSI vector reuse remain separate capabilities;
software drain is not a hardware reclamation certificate.

## Per-CPU Source Lifecycle

Local hardware control, software admission and shared handler retirement are
different capabilities. `IrqFlags::PER_CPU` currently records metadata; it does
not itself select a per-CPU flow. Explicit provider selection chooses `PerCpu`
for supported ordinary local sources, with CPU-local enable/configuration
slots distinct from `IrqDescRuntimeState`'s ordinary Line-wide state. The legacy
boolean enable bridge retains ordinary Line depth handling, but sets only the
current slot for PerCpu flow without depth bookkeeping. Kernel-lifetime timer/IPI services use
this control without claiming a completed runtime retirement protocol.
GIC descriptor constructors also do not infer `PER_CPU` from an SGI/PPI INTID.
Therefore neither the flag nor its absence is a complete local-source classifier.

### Source Owners

| Owner and source | Current lifecycle | Remaining migration obligation |
| --- | --- | --- |
| `core/kruntime/src/lib.rs::init_interrupt`, timer and IPI | Publishes shared handlers once before local activation; AP activation is in `core/kruntime/src/mp.rs`. | Kernel-lifetime startup is not a runtime unregister protocol. Preserve startup ordering while identifying each local source explicitly. |
| `drivers/platform/timer/src/arm_generic.rs::rearm_local_timer_irq` | Programs the current CPU timer and calls the legacy `kirq::enable` bridge, now updating its PerCpu slot. | Local disable updates only that CPU; timer stop/rearm exclusion remains with the timer service. |
| `arch/kipi/src/notification.rs::enable_fixed_notifications` | Enables the local IPI line, publishes readiness and drains retained notification reasons during CPU-bound boot. | No shutdown/hotplug protocol exists here. Do not use an IPI callback to disable its own transport before pending shutdown work has completed. |
| `platforms/kplat-aarch64/src/peripherals/pmu.rs::pmu_if_impl` | Registers an NMI action only when the selected mechanism supports it; supported GICv3 NMI delivery uses generic NMI dispatch. Normal-IRQ claims for that NMI-owned Line complete without invoking its handler. Without NMI delivery, a PPI uses disabled PerCpu registration and explicit local activation; an SPI retains ordinary registration, not an NMI-owner fallback. | `pmu/lifecycle.rs` closes starts and completes synchronous local stop/teardown before NMI owner removal. It rejects shutdown in mode None; the ordinary-action configuration has no corresponding runtime unregister path. SPI PMU support additionally needs CPU-to-source association and enforced routing; the current first-IRQ firmware lookup does not establish those guarantees. |
| `drivers/platform/irq/src/gic/gicv3.rs::prepare_local_irq_work`, SGI15 | Uses `try_register_percpu_startup` once, disabled; each CPU prepares and explicitly enables its local SGI. | Kernel-lifetime registration; no stop/free or CPU-hotplug protocol is added. |
| `virt/kvmm/src/vdev/aarch64/irq_route.rs` and `timer_residency.rs` | Uses `try_register_percpu` to prepare one disabled route in sleepable context, then acquires forwarding capability. Task schedule transitions save/restore CNTV and transfer this CPU's forwarding state under IRQ exclusion. | Stop disables timer output, revokes forwarding and clears local ownership. The permanent shared route is not unregistered per vCPU. See `virt/kvmm/docs/vcpu-residency.md` for the supported GICv2 lifecycle and WFI drain. |

The vtimer route's `try_free_percpu_irq` call is preparation rollback: forwarding
capability acquisition failed before route publication or local activation.
The route preparation gate retains pending cleanup responsibility when removal
fails or returns no handler; subsequent preparation retries cleanup and cannot
register a new route until it succeeds. It is not evidence of a live per-CPU
route-removal protocol. Driver-provider
`release_irq` uses generic action withdrawal/drain and does not acquire CPU
membership or stop device producers on behalf of the driver.
Rejected normal-IRQ delivery is not an NMI that interrupted IRQ-enabled execution:
GICv3 classifies that claim as NMI and calls `generic_handle_nmi`, bypassing
ordinary running admission. Claim classification remains the irqchip's job.
This enforces KIRQ's separate entry/context and callback-argument contracts,
not a claim that Linux rejects every NMI action in a shared flow function:
Linux `handle_percpu_devid_irq` can service both IRQ and NMI contexts.
No supported production NMI delivery path currently requires the generic
former normal-IRQ handler compatibility behavior; it existed only for software/test
compatibility. Normal-IRQ delivery to an NMI-owned Line now completes without a
handler call. PMU mode `None` installs an ordinary PerCpu action, not an NMI
owner using this rejection path. True NMI, including IRQ-on NMI, is unchanged.

### Retirement Responsibilities

At Linux revision `8d3ae59288f1e7d58d76558a6ee96d533bc5019f`, `kernel/irq/manage.c::enable_percpu_irq` and
`disable_percpu_irq` operate on the current CPU under descriptor exclusion.
`kernel/irq/chip.c::irq_percpu_enable` and `irq_percpu_disable` update the
corresponding bit of `desc->percpu_enabled` after the chip operation.
`handle_percpu_devid_irq` permits callbacks on different CPUs without one
ordinary running-state gate. `__free_percpu_irq` refuses to unlink an action
while its affinity intersects the enabled CPU set. `free_percpu_irq` requires
the caller to disable the source on each relevant CPU first; it does not hide
a broadcast shutdown inside handler removal.

Complete runtime local-source retirement requires all of the following, not
just the implemented flow and CPU-local control state:

- Source classification and CPU-local enable state owned by KIRQ, separate
  from ordinary Line-wide disable depth.
- CPU pinning from target selection through the chip operation and state
  update, plus serialization against final removal and concurrent re-enable.
- Caller-owned shutdown on all relevant CPUs, with evidence that the local
  callback has finished rather than merely been queued or notified.
- A stable relevant-CPU set and a defined failure path when a target cannot
  participate. Firmware-present CPUs are not automatically a shutdown barrier.
- Shared handler removal only after local shutdown and every admitted dispatch
  has returned, including its handler, original claim completion and tail work.
  Hardware and guest activity remain separate.

`kipi::run_on_cpu` and broadcast callback APIs report submission, not callback
completion. `kipi::run_on_cpu_sync` supplies the completed-call capability
for boot-static CPUs with enabled IPI services. The source owner retains its
management mutex across the stop transition, each relevant CPU's completed
device stop/local mask, and final `try_free_percpu_irq`. Re-enable paths must participate
in that owner's shutdown exclusion. They need not take the sleepable mutex:
PMU uses a closed-start gate checked within pinned local setup, then synchronous
CPU calls drain starters admitted before closure. The stop callback itself must
not acquire the management mutex. Failed target validation or callback preparation keeps
the owner in stopping state with the action retained; it is not permission to
free a partially stopped source.

KIRQ refuses withdrawal while any local slot is enabled. Unsafe
`try_free_percpu_irq` additionally requires the caller, before entering removal,
to stop every participating CPU's source, exclude restart and new dispatch
through removal, and wait for every admitted dispatch to return, including its
handler, original claim completion and tail work. This includes software dispatch.
Failed shutdown retains the action
and resources for retry. It does not
broadcast, choose CPU membership, stop hardware producers or wait for PerCpu
execution. The enabled-mask check is necessary, not a lifetime proof.

Different CPUs may execute the shared action concurrently. A handler may clear
its own enabled slot before returning, so source-owner exclusion, not that slot
or a generic drain, protects its raw action borrow. PerCpu dispatch has no
execution count or ordinary running gate. This follows the source-owned shutdown
distinction in Linux `handle_percpu_devid_irq` and `__free_percpu_irq`.
Full dispatch return is stronger than handler return or completion callback
return: the PerCpu guard retains the Line and records unhandled diagnostics
after original completion. The source-stop proof must cover the entire dispatch
within its outer IRQ-masked interval, including that tail, before unsafe free.
Rejected normal-IRQ claims for NMI-owned Lines complete under the Line lock
without borrowing an action. True NMI owner reclamation retains its separate
local revocation protocol.

The production caller of `try_free_percpu_irq` is currently VMM route preparation
rollback in `virt/kvmm/src/vdev/aarch64/irq_route.rs`. Its preparation mutex
serializes attempts, and forwarding is unpublished until preparation succeeds,
so this rollback precedes local activation. A successful route is permanent;
vCPU stop revokes local forwarding rather than freeing the shared action.
This caller does not require a new runtime retirement protocol. Permanent
timer/IPI services likewise do not acquire an unload requirement from this API.

`kipi::run_on_cpu_sync` already supplies a completed local callback. For an
ordinary handler that keeps local IRQs masked, a later ordinary IPI callback
cannot run until the earlier dispatch's entire outer IRQ-masked interval has
ended, including the handler, original completion and post-completion tail.
Together with producer stop and restart exclusion this can establish the
source-owned shutdown barrier. It does not wait for its own surrounding IPI
completion, so this argument must not be used to retire that same IPI action;
nor does it apply unchanged to NMI or a handler that enables nested IRQs.

The unsafe removal boundary requires completed source-owned execution exclusion,
not merely stopped producers and clear enabled slots. No production caller currently
requires dynamic removal of a running ordinary PerCpu service. Public software
dispatch entries must also preserve their existing pinned, IRQ-masked hardirq
contract; test-only synthetic admissions are not hardware shutdown evidence.
No stop token, counter or action grace period substitutes for this caller proof.
Synthetic fixtures must wait for each admitted dispatch to return fully
before invoking unsafe removal; they cannot use ordinary drain as a PerCpu barrier.

Boot CPU membership is stable after runtime service startup in the supported
configuration; present CPUs must not be substituted before that rendezvous.
Dynamic CPU hotplug still requires CPU-lifecycle exclusion, and terminal stop
is excluded. Neither readiness nor callback completion supplies that missing
lease. Forwarded vtimer and NMI retirement retain separate owner obligations.

Linux also separates these guarantees: `kernel/smp.c` with `wait=true` waits
for selected callbacks to return, while CPU-hotplug exclusion belongs to the
CPU lifecycle machinery. For example, `drivers/perf/arm_spe_pmu.c` removes its
CPUHP instance (including applicable local teardown callbacks) before calling
`free_percpu_irq`. A callback-return acknowledgement alone does not prohibit
another startup/enable path from reopening a source afterward.

## Scope

```text
arch/kirq/src/
├── lib.rs              # crate-level public surface and stable re-exports
├── irq_work.rs         # permanent CPU-local NMI-to-hardirq work bridge
├── model/
│   ├── mod.rs
│   └── desc.rs         # OS-visible IRQ descriptor and virq/hwirq vocabulary
├── runtime/
│   ├── mod.rs
│   ├── action.rs       # internal regular IRQ action and return classification
│   ├── action_set.rs   # private stable-node action chain and admission ceilings
│   ├── index.rs        # owning XArray indexes, prepared publication and retirement
│   ├── state.rs        # single-Line state, lifecycle gates and internal facade
│   ├── state/registry.rs # namespace, mapping and MSI publication transactions
│   ├── state_local.rs  # Line-locked CPU-local enable/configuration slots
│   ├── dispatch.rs     # normal IRQ action fanout
│   ├── entry.rs        # trap entry, generic claim routing and IRQ-exit ordering
│   ├── rcu.rs          # interrupt watching and scoped NMI read bridge
│   ├── unhandled.rs    # descriptor-lifetime unclaimed-event diagnostics
│   ├── resend.rs       # retained-Line pending replay queue and executor binding
│   ├── nmi.rs          # Line-owned NMI publication, scoped dispatch and retirement
│   ├── nmi/action.rs   # Sole owner and quiescent CPU-local publication
│   ├── nmi/shutdown.rs # Retained close identity and local participant snapshot
│   └── manager.rs      # mapping, registration, control and removal transactions
├── domain/
│   ├── mod.rs          # per-domain RCU XArray indexes
│   └── fw.rs           # firmware controller-node to domain association and xlate
├── backend/
│   ├── mod.rs
│   ├── platform.rs     # IntrManagerIf bridge and PendingIrq claim ownership
│   └── msi.rs          # MSI/MSI-X allocation and message composition bridge
└── bottom_half/
    ├── mod.rs
    ├── context.rs      # IRQ execution-context tracking and diagnostics
    ├── deferred.rs     # internal softirq-owned IRQ-tail handoff
    ├── lifecycle.rs    # hardirq entry/exit extension hooks
    └── softirq.rs      # fixed-vector softirq pending bits and bounded runner
```

`kirq` deliberately does not depend on `khal`, `kcpu`, `kplat`, `driver_os_api`,
any firmware parser, or any controller driver. Its only firmware-facing
dependency is the allocation-free `fwnode` vocabulary crate, which names a node
and its raw binding cells but owns neither parsing nor policy. It uses
`kbuild_config::NR_CPUS` for bounded CPU-local control,
NMI publication/participant storage and softirq CPU iteration; this is not an
online-CPU membership or shutdown guarantee. Architecture trap registration and x86 APIC
trap adapters remain in `khal::irq`; x86 APIC MSI vector allocation is an
irqchip/backend detail reached through `kirq::MsiBackendIf`, not through
`khal`. `IPI_IRQ` stays at Kconfig call sites such as `kipi`, not in `kirq`.
Driver IRQ integration is owned by the `driver_os_api_xkernel` provider crate, which translates
`driver_os_api` IRQ resources into `kirq` descriptors and handlers.

## Architecture

The crate root explicitly exports APIs from their owning modules; `manager`
does not aggregate backend, NMI, lifecycle or deferred exports. `runtime/entry.rs`
owns trap-facing orchestration, generic claim consumption and common exit order.
`manager.rs` owns management transactions, while `dispatch.rs` owns flow-specific
admission, borrowed action traversal and completion tails. These remain internal
modules of one IRQ core: separating execution from control does not create a
second descriptor or action owner.

`state.rs` owns single-Line state and exposes registry operations through explicit
internal re-exports. Its private child `state/registry.rs` owns logical-number
allocation, namespace membership and mapping/MSI transactions. Child placement
permits access to private Line identity and lifecycle fields without exposing
them to sibling modules. Metadata validation, flow binding and paired index
publication must commit under the existing registry and original-Line exclusion;
splitting those writes into separately locked public setters would break that
transaction boundary. This access does not transfer handler execution, action
reclamation or source-quiescence responsibility to the registry. No new crate
or root-level registry implementation API is introduced; the exported
`DYNAMIC_VIRQ_BASE` remains namespace vocabulary, not mutation authority.

`IrqLine` retains descriptor identity and locked state. `ActionSet` owns linked
action allocations; `RetiredAction` carries the unique detached reclamation
right. `PendingIrq` is the sole backend claim owner for normal IRQ and NMI, not
an action owner or a hardware-quiescence token. Directory RCU, ordinary running
drain and PerCpu source-owner exclusion retain their distinct lifetime duties.
`rcu.rs` centralizes interrupt-watching assumptions; `unhandled.rs` owns bounded
event diagnostics, not execution admission. `irq_work` owns its CPU-local work
handoff separately from softirq and from the pending IRQ replay queue.

```text
CPU trap entry
    │
    └── khal::irq adapter
            ├── NoPreempt
            ├── kirq::handle_irq(vector)
            │       ├── IrqLifecycleGuard::enter()
            │       ├── HardIrqContextGuard::enter()
            │       ├── IntrManagerIf::dispatch_irq()
            │       │       └── irqchip claim + classify
            │       │              ├── generic_handle_irq(PendingIrq)
            │       │              │      └── dispatch_claim(source, completion)
            │       │              │             └── PendingIrq::complete()
            │       │              └── generic_handle_nmi(PendingIrq)
            │       │                     ├── dispatch_nmi_handler()
            │       │                     └── PendingIrq::complete()
            │       ├── drop HardIrqContextGuard
            │       ├── deferred::run_hardirq_exit_deferred()
            │       └── lifecycle guard drop
            │
            └── kirq::handle_nmi(vector)
                    ├── IntrManagerIf::dispatch_nmi()
                    └── generic_handle_nmi(PendingIrq)
```

Registration/configuration paths:

```text
try_map/try_register/try_enable/unregister
    └── IRQ_STATE (lookup/publication; released before controller operations)
            ├── virq -> Arc<IrqLine> -> SpinNoIrq<IrqStateDesc>
            └── (domain, hwirq) -> the same Arc<IrqLine>

domain::IrqDomain
    └── XArray<Arc<IrqLine>>: RCU lookup for IRQ/NMI readers

configure_platform_irq() / set_platform_irq_enabled()
    └── IntrManagerIf::{configure, enable}

alloc_msix()
    ├── MsiBackendIf::alloc_msi_vector()
    ├── MsiBackendIf::compose_msi_message()
    └── publish_msi_reservation() publishes a fresh virq and retained reservation

register_nmi()
    |-- prepare one KBox<Handler> outside locks
    |-- original Line management -> registry -> Line state
    `-- install NmiAction owner, mark NMI ownership; reject non-PerCpu flow

prepare_percpu_nmi()
    |-- retain Line and exclude local IRQs/migration
    |-- complete local controller preparation under Line lock
    `-- publish current CPU's pointer to the installed owner
```

## Calling Constraints / Execution Context

- Mapping and control operations protect the directory with `IRQ_STATE` and
  the selected Line with its state lock. Registration/free additionally require
  sleepable task management admission (explicit startup registration is the
  nonwaiting exception). Controller configure/enable/disable runs under Line
  exclusion after directory release, without a global control lock. NMI writers
  use the same Line management boundary; Domain mappings distinguish hardware
  numbers belonging to different controllers.
- `disable_irq()`, `synchronize_irq()` and `free_irq()` wait for the retained
  Line's aggregate execution predicate to become idle; hardirq, softirq and
  BH-disabled callers are rejected. `disable_irq_nosync()` / `try_disable_irq_nosync()` perform a lookup-only disable on a descriptor that
  already exists, update the nested disable depth and mask the platform line;
  they do not create a descriptor, allocate a `virq` or publish a domain mapping, and do not wait for a running
  handler.
- The compatibility entries `enable()` and `register()` still exist. `enable(spec, false)` is
  the legacy boolean bridge of `disable_irq_nosync()`, usable from the hardirq path and never waiting;
  `enable(spec, true)` is the legacy enable bridge: when disable nesting exists it first decrements the depth,
  and starts the platform line when the depth reaches `0`; when the depth is already `0` it still issues the platform enable, to preserve the existing
  semantics of per-CPU/static IRQ bring-up. New code should prefer the more explicit `try_*` /
  `*_irq` entries to handle descriptor conflicts and enable/disable nesting.
- `handle_irq()` runs on the hardirq trap adapter call path. The caller must have masked local
  IRQs, disabled preemption and pinned the current CPU, and the function itself must not sleep.
- `handle_nmi()` runs on the IRQ-off NMI adapter call path (pseudo-NMI or hardware
  NMI); it does not touch the normal IRQ dispatch fanout, does not call lifecycle hooks, and does not acquire
  `IRQ_STATE`. If the exception comes from an IRQ-on context, the entry has already performed the normal IRQ entry; the GIC calls `generic_handle_nmi()` inside that outer
  lifecycle, consistent with the Linux IRQ-on NMI semantics.
- `translate_hwirq()` borrows the Domain XArray in IRQ/NMI context; boot/control
  queries use writer exclusion. Strict misses return `None`, not identity IDs.
- `IrqLifecycleHook` runs after the trap adapter has disabled preemption and while the IRQ-on exception lifecycle has not yet ended;
  that exception may later be classified by the irqchip as a normal IRQ, an NMI or spurious. It is not a
  hardirq-depth boundary; the hardirq/softirq/BH state is maintained by
  `kirq::context`. A hook must not sleep, must not wait for locks that an interrupted context may hold, and must not
  assume a process context exists.
- `DeferredExecutorHook` runs after the IRQ-on controller dispatch returns and the generic hardirq depth
  has been exited, and before the lifecycle exit. If a claim exists, the generic IRQ/NMI helper has already
  completed the controller completion. It is only the handoff point to the later deferred execution
  subsystem; it must not sleep, must not recursively call the deferred runner, and must not assume a process
  or task context exists.
- `alloc_msix()` / `free_msix()` must be called from a normal kernel context. They allocate, through the backend,
  a controller-local vector and return an OS-visible virq and a device-visible MSI message;
  the caller must not read the current APIC id or assume the virq equals the hardware vector.
- `IntrManagerIf::notify_cpu()` must satisfy the publish-before-notify order, which IPI/TLB
  shootdown depends on.

## Interrupt Controller Backend Contract

`IntrManagerIf` is the boundary between `kirq` and the concrete irqchip drivers. `kirq` is responsible for descriptors,
handler fanout, context and completion ordering; the irqchip driver is responsible for the concrete controller behavior.

### configure / enable

- `configure(desc)` is called before a normal non-MSI line is enabled. It consumes the already normalized
  `IrqDesc` metadata, including trigger, polarity, controller, source, affinity and
  flags. A backend should use only the fields it understands, and unknown or irrelevant metadata should not cause a wrong configuration.
- `enable(desc, on)` receives the selected Line's normalized descriptor,
  retaining domain and controller metadata across the backend boundary.
  Existing providers use `desc.hwirq` as their controller-local source id,
  not the dynamic virq; their register behavior is unchanged. This metadata
  transport does not add controller-instance routing or temporary flow masking,
  and a copied descriptor does not retain the hardware binding's lifetime.
- An MSI descriptor carries `IrqFlags::MSI` and does not go through the normal line configure/enable. MSI enablement
  comes from the bus/device layer writing the device-visible `MsiMessage`.

### dispatch / claim / ack

- After claim/ack, `dispatch_irq(vector)` classifies the source and invokes
  exactly one generic entry: `generic_handle_irq(PendingIrq)` or
  `generic_handle_nmi(PendingIrq)`. The boundary follows Linux irqchip calls to
  `generic_handle_domain_irq()` / `generic_handle_domain_nmi()`.
- The `Option<Virq>` returned by `dispatch_irq()` only gives the outer deferred context the resolved
  normal virq metadata; `None` may mean an NMI, a strict domain miss or spurious, and must no longer be
  treated as an IRQ/NMI classification result. Handler dispatch and completion have already finished before it returns.
- The `PendingIrq` of normal dispatch holds an `IrqRef` and a completion cookie; the resolution of the domain hwirq
  to an OS-visible virq is still done by `generic_handle_irq()` inside `kirq`.
- `dispatch_nmi(vector)` claims a hardware source, tags its Domain and calls
  `generic_handle_nmi(PendingIrq)`. Lookup borrows the Domain index without
  locking `IRQ_STATE`; this path never opens the normal IRQ window.
- GIC reads IAR, filters special IDs and retains the cookie for deactivate/DIR.
  x86 local vectors EOI in the backend; IO-APIC/MSI transfer EOI to generic flow:
  Edge acknowledges before handlers and level Fasteoi after handlers, under the
  Line lock. PLIC carries the claimed hwirq as its completion cookie. LoongArch
  still completes EIOINTC before generic dispatch and has a no-op completion;
  its source-specific flow migration is a separate delivery.

### complete / EOI / deactivate

- Both generic entries consume `PendingIrq`. Normal IRQ completion follows the
  selected flow's ordering; NMI completion follows the scoped handler callback.
  `PendingIrq` is the sole claim RAII guard: Drop consumes an outstanding host
  completion if normal control flow has not already done so. It is not `Send`;
  completion must occur on the CPU that claimed the interrupt.
- `complete_irq(cookie)` consumes the remaining host-owned obligation of the
  original claim. Cookie encoding and source-specific EOI/deactivation ordering
  belong to the backend, not to a generic trigger-type rule. Already completed
  operations must not be repeated. Supported guest forwarding may retain a
  separate deactivation owner; hook return is not hardware inactivity or device
  shutdown. See [Controller Flow Contracts](#controller-flow-contracts) for
  provider-specific ordering.
- Ordinary dispatch retains running admission through its required completion/tail work.
  Level/Edge/Fasteoi rejections instead keep the original Line lock through chip
  operations and original completion, without execution admission. Normal-IRQ
  claims for NMI-owned Lines also complete under the held Line lock without a
  credit or action access. PerCpu rejection likewise completes under the held
  Line lock without a count. Accepted PerCpu executions rely on source-owner
  lifetime protection until full dispatch return. Lookup misses do not acquire a
  later same-number Line. No registry lock spans completion; flow-specific Line
  exclusion and running ownership order ordinary drain without
  establishing hardware quiescence.

### notify_cpu publish-before-notify

`notify_cpu(id, target)` must guarantee that the request state the caller published in Normal memory is observed by the target CPU before the IPI.
The x86, GIC and RISC-V backends should provide an architecturally equivalent fence or
barrier in their own implementations; callers should not repeat a fence at every IPI call site.

## State Machine

### Descriptor / action lifecycle

```text
unknown
   -> try_resolve_and_publish(IrqSpec)
   -> descs[virq] = Arc<IrqLine { state: SpinNoIrq<IrqStateDesc> }>
   -> register() installs one regular action
   -> dispatch_claim(source, completion)
   -> disable_irq_nosync() masks without waiting
   -> synchronize_irq() drains admitted original claims and running replay, not queued entries
   -> unregister()/free_irq() removes regular action and synchronizes teardown
   -> retain the descriptor independently of its remaining actions
```

Domain association owns the mapped Line independently of its actions. Removing
the final regular/shared/NMI action or completing an empty claim preserves both
owning XArray entries and the same Line allocation.
Remapping the same source therefore reuses its existing identity and control
state instead of reconstructing a descriptor behind a surviving reverse map.
This also applies to domainless fixed descriptors: handler removal must not
undo explicit preparation or change the Line identity used for re-registration.

This follows Linux's separation between `free_irq()` in `kernel/irq/manage.c`
and mapping disposal in `kernel/irq/irqdomain.c`. Platform Domain mappings have
platform/Domain lifetime; action owners must not dispose them. There is no
general runtime Domain disposal entrance yet. MSI has its explicit resource
release transaction. Domainless descriptors retain registry lifetime too;
neither action removal nor IRQ completion disposes them. Registration requires
explicit descriptor preparation in both management modes. General non-MSI
descriptor disposal is not provided yet; the XArray cutover does not add a
generic platform-mapping destruction API.
`ut-tests/mapping_lifetime.rs` checks regular/shared removal, empty-claim
completion, repeated mapping and enrichment without using a hardware producer.
It also checks that neither an empty claim nor regular/shared action removal
can undo bare descriptor preparation before startup handler registration.

The logical directory and each Domain own an `XArray<Arc<IrqLine>>`.
Both entries retain the same allocation, whose logical number is immutable.
Normal Domain dispatch obtains that Line directly in an interrupt RCU scope;
only explicit identity fallback queries the logical directory. Strict misses
and unknown Domains remain unresolved. After retaining the Arc, dispatch exits
the RCU scope and checks retirement/admission under the Line lock, without
acquiring `IRQ_STATE`. The retained admission survives controller completion.

`IRQ_STATE` serializes writers, conflict checks and dynamic number assignment.
The descriptor and the actual Domain entry provide reverse-mapping validation;
there is no separate `virq_to_mapping` copy. Numeric-only IRQ/NMI translation
borrows the Domain entry without retaining an Arc; boot/control queries use
XArray's writer-locked read and need no initialized RCU service. Immutable
numeric snapshots and their intentional leaks have been removed.

Writers prepare worst-case insertion capacity with NOWAIT before registry/Line
exclusion. Under exclusion, identity and mapping conflicts are revalidated,
then descriptor/flow state and both indexes are published without allocation.
A reader may retain the first published entry before the second is visible,
but cannot acquire Line admission until the writer releases its Line lock.
Preparation failure returns `NoMemory` before publication;
dynamic number allocation may leave an unused number after a failed attempt.

MSI release preflights callback submission before exclusion and consumes the
two records retained since publication only after validating release. It closes
admission and withdraws both indexes under registry and Line exclusion, then
submits both records after restoring task context.
Only after submission does it release the backend reservation. Old borrowed
readers retain the withdrawn owners through the RCU grace period; independent
Arc holders may retain the Line longer. Reusing a number never retargets an old
reader or dispatch guard. Ordinary action removal still keeps its descriptor.

`IrqSpec::PlainVirq` contributes no hardware metadata. `IrqSpec::Desc`
participates in descriptor merge and mapping validation. `commit_resolution`
returns only after both required prepared insertions have completed.
`IrqDesc::try_merge()` allows a later descriptor to fill in trigger, polarity, controller,
domain, affinity and flags, but does not drop metadata that has already been confirmed. A conflict in hwirq, virq, domain,
trigger, polarity, controller or the `(domain, hwirq) -> virq` mapping returns
`IrqDescError`, and release builds do not silently overwrite metadata. Unknown trigger/polarity/
controller may be filled in by a later determined value; two different determined values never overwrite each other.

### Firmware interrupt mapping

Firmware-to-IRQ mapping has three owners and no overlap:

1. The firmware backend returns a controller `FwNode` plus the raw binding
   cells in `FwIrqSpec`. It does not interpret the cells, choose a domain, or
   allocate a logical IRQ.
2. The controller driver implements `IrqDomainXlate` next to the code that owns
   the controller. It converts its binding cells into `IrqHwSpec`
   (controller-local `hwirq` and trigger) and rejects malformed, unsupported or
   out-of-range declarations with `IrqXlateError`. It never names another
   controller or calls the mapping API.
3. `kirq` associates that exact controller node with one registered
   `IrqDomainId` and maps the resulting `IrqDesc` through the existing
   `try_map` transaction. `try_map_fwspec` returns `MappedIrq`, whose `desc` is
   the authoritative stored resource.

Association is keyed by `FwNode` identity: backend identity plus backend-local
node id, never compatible strings, family, or physical address. Two instances
of one controller family therefore remain distinct, and a node from another
backend with the same numeric id does not match. `register_irq_domain` takes a
`FwNode<'static>` and is boot-only; duplicate domain association, duplicate
controller association and unknown domains are explicit errors. The
association is stored inside the owning `IrqDomain` rather than in a second
fixed-capacity table; lookup copies it out and releases the domain lock before
translation and mapping. The controller family used in mapped descriptors is
domain metadata (`IrqDomain::kind`): an `Existing` target keeps the domain's
stored kind, while a `New` target initializes the new domain's kind once at
creation. No registration path can overwrite an existing domain's kind, so a
contradictory domain/kind pair cannot be created.

Registration places a controller instance in the domain chosen by its boot
wiring. `IrqDomainTarget::Existing(id)` associates with an existing well-known
root; the primary GIC and PLIC bring-up paths use this to keep platform-static
descriptors and legacy consumers on stable identifiers.
`IrqDomainTarget::New(kind)` allocates a fresh per-instance domain. The generic
core validates existence, controller uniqueness and allocation; it does not
infer root reuse from controller family, identity policy or registration order.
An existing domain keeps its own identity policy; a new domain is always
created without numeric identity fallback. Registration is boot control-plane
only and must not be called from IRQ, NMI or bottom-half handlers. `New`
requires an initialized allocator and uses a non-sleeping `NOWAIT` allocation
(reported as `NoMemory` on failure); `Existing` allocates nothing.

Because each additional instance receives its own domain, two same-family
controllers keep independent hardware-number namespaces. Numeric identity
fallback (`IrqDomain::is_identity`) belongs to an existing compatible root
only. A newly allocated domain is never identity-mapped: an unmapped hardware
number stays unresolved instead of resolving to a same-numbered logical Line
owned by another source. This keeps `(domain, hwirq)` the mapping identity for
every non-root instance and rejects unmapped claims rather than silently
borrowing a global number.

`IrqDomain::kind` remains the controller-family metadata used to fill
`IrqDesc.controller` and feed existing flow validation and legacy descriptor
paths. It is not an instance identity: a driver that owns one initialized
controller must admit only the domain identifier returned by its own
registration, never every domain whose `kind` matches.

The domain registry is append-only. The well-known roots stay in a static set;
dynamically allocated domains are linked into a singly-linked list through
`AtomicPtr` links and leaked for the kernel lifetime. Nothing is removed, so a
published domain, its identifier and its links remain valid; there is no
fixed-capacity dynamic table and no truncation point at which a controller
silently loses its namespace. A dynamically allocated domain has its firmware
association installed before the link that publishes it, so a reader that can
find the domain also finds its controller binding. Registry traversal takes no
lock and no RCU scope, which keeps domain lookup and numeric resolution usable
from interrupt context; control-plane lookup takes only each domain's short
association lock.

The intrusive list is deliberate rather than a substitute for an existing
index. `XArray` is the kernel's general sparse index, but its borrowed reads
require an RCU read scope and its retaining reads require `Arc` ownership. IRQ
dispatch resolves a domain before it enters the Line read scope, and early
control-plane mapping can run in contexts where a task read scope is not
available. Domain entries are created only at boot, are never replaced or
removed, and number at most a few controllers per platform, so an append-only
link walked with Acquire loads is sufficient and avoids introducing an RCU
service dependency or a fallible shared-ownership allocation into domain
lookup. The list is not used for Line dispatch; per-interrupt work still uses
the domain's own `XArray` after a single identifier lookup.

`try_map_fwspec` distinguishes the required failure classes: an absent
declaration is `Ok(None)` from the backend, a malformed declaration is
`MalformedSpec`/`InvalidHwirq`, an unsupported binding is `UnsupportedSpec`, an
unregistered controller is `ControllerNotReady`, and IRQ-core rejection
(capacity, mapping conflict, unsupported flow) is `FwIrqMapError::Map(...)`.
There is no fallback to a default controller or to family-based lookup.

The same `commit_resolution` transaction validates and stores the descriptor,
so `MappedIrq::desc` carries the trigger produced by translation rather than a
consumer default. Repeating the same valid declaration returns the existing
mapping; conflicting trigger/controller/domain metadata returns
`IrqDescError::TriggerConflict` / `ControllerConflict` / `DomainConflict`.

`actions` is the internal action list of `kirq`. `register(desc, handler)` remains the compatible
single-action API: it only allows installing one non-shared action on an empty line, and a second regular
registration keeps returning `false` and logs a warning. `register_shared()` /
`try_register_shared()` install a shared action carrying an `IrqActionToken`; the
shared actions on the same line are fanned out uniformly by `kirq` in dispatch, and `driver_os_api_xkernel` keeps only the
adaptation relation between the devres token and the `IrqActionToken`. A shared action currently supports only the default
auto-enable path and exposes no disabled shared registration API. If a later shared action
registration fills in more specific descriptor metadata, `kirq` re-issues the stale platform configure without changing the current enable/depth
state.
The normal IRQ call argument of a handler is the already resolved OS-visible `virq`; a shared IRQ handler
relies on this argument to identify the current line, and should not infer it back from device or platform private state. The action still reserves
a future threaded slot and an optional name, but in this phase the threaded slot never
produces scheduling, wakeup, softirq or workerqueue behavior.

`IrqDescRuntimeState` is the sole owner of descriptor/action runtime state. It currently holds:

- `actions`: the private ordinary action chain of stable KBox nodes. Regular registration admits one
  exclusive action; shared registration has no fixed four-action limit.
  Storage is prepared with NOWAIT before short locks. Exhausted publication
  sequences reject registration without wrapping or reusing removal keys;
- `generation`: the sequence number of descriptor/platform configuration metadata changes; actions and
  lifecycle do not increment it;
- `configured_generation`: the descriptor
  generation last successfully issued to the platform configure path, used to avoid configuring again on every enable;
- `is_enabled`: logical management enable state, distinct from Fasteoi source
  suppression and the provider's effective hardware priority;
- `disable_depth`: the nested disable depth; `try_register_disabled()` installs the
  handler at depth `1`, and the platform line is enabled only when a later `try_enable_irq()` decrements it to `0`;
- `is_running`: accepted Level/Edge/Fasteoi handlers and software replay own
  ordinary execution through required completion/tail work. Locked rejections
  own no running admission, and queued replay retains only an Arc and flag.
  Ordinary sync/disable/free drain this state under the Line lock. PerCpu has no
  execution counter; unsafe removal requires prior source-owned quiescence;
- `teardown_depth`: outstanding whole-Line teardown credits. Ordinary withdrawal
  adds a credit only for the last action; non-last shared removal relies on the
  management mutex for membership serialization and leaves survivors eligible.
  A nonzero value rejects new actions; descriptor lifetime is independent;
- `is_msi`: the MSI lifetime marker derived from the descriptor flags;
- `oneshot_mask_pending`: reserved for later threaded/oneshot semantics; this phase does not drive a hardware mask.

These fields are crate-private runtime state and do not constitute a public ABI. Later shared IRQ, oneshot mask,
and threaded IRQ work may only be extended within this owner, and must not bypass `IrqStateDesc` to create parallel state on the dispatch,
MSI free, or unregister paths.

The public return type of the primary handler is still `IrqEvent`. Internally, dispatch classifies it as:

- `NOT_HANDLED` -> `Unhandled`;
- `HANDLED` -> `Handled { sources: 0 }`;
- `from_sources(bits)` -> `Handled { sources: bits }`.

A future-only `WakeThread { sources }` classification is also kept internally. Current public handlers cannot return
this classification; even if an internal test constructs this value, it only represents future threaded IRQ semantics and does not trigger an IRQ
thread wake, softirq, or workqueue work. An action with a
future-only thread slot is not dispatchable in the current non-threaded core: it does not synchronously
run the primary handler.

### virq mapping

- An explicit `virq` directly uses the OS-visible IRQ number specified by the caller;
- A descriptor with a `domain` but no `virq` allocates a dynamic virq through `(domain, hwirq)`;
- New Domain bindings publish the retained Line through a prepared XArray slot;
- A domain not registered in the static registry in `domain/mod.rs` returns `IrqDescError::UnknownDomain`,
  and leaves no control-plane mapping nor returns a dead line that the data plane cannot resolve;
- Dynamic virqs start from `DYNAMIC_VIRQ_BASE`;
- A plain `usize` is interpreted at the API boundary as `IrqSpec::PlainVirq` and refers only to the `virq` itself;
- `enable(usize, on)` carries no new metadata but resolves to the stored identity descriptor;
  for a low-numbered platform-static line, this may still call the platform enable by the identity `hwirq`;
- `try_map()` accepts only `IrqDesc`, because establishing a domain mapping requires explicit
  `(domain, hwirq)` resource semantics, and a plain `virq` cannot take part in hardware mapping.
- The same `virq` cannot be the target of two different `(domain, hwirq)` mappings at once; an explicit `virq`
  must have its reverse consistency checked before the mapping is inserted.

The `driver_os_api_xkernel` provider is the boundary adapter from the devres IRQ vocabulary to `kirq`. An `IrqResource` without
`hwirq/domain/controller` metadata must be converted into
`IrqSpec::PlainVirq`; only a resource carrying hardware routing metadata is converted into
`IrqSpec::Desc`. This way the plain IRQ handle of a device resource never wrongly enters the descriptor
merge/mapping branch.

A devres IRQ request installs a shared action through `kirq::try_register_shared()` and wraps the returned
`IrqActionToken` into `driver_os_api::IrqHandlerToken`. On release, the `driver_os_api_xkernel` provider uses
`kirq::free_irq_action()` to delete a single action; only when the last action leaves,
KIRQ masks only for last-action removal. Every successful shared removal drains ordinary
execution before returning the owner, without disposing of the descriptor. Shared IRQ
fanout, teardown and later threaded/oneshot extension points are all concentrated in the IRQ core, not hidden in the
local aggregation table of the devres adapter. The `IrqEvent` returned by a handler is merged by `kirq`
after the fanout on the same line completes; data-ready notification is the responsibility of the device subsystem.
`kdriver` takes no part in the IRQ wake bridge; it is responsible only for adapting the devres IRQ
resource to the `kirq` action lifecycle.

### MSI/MSI-X

```text
alloc_msix(affinity)
   -> backend allocates controller-local vector
   -> backend composes MsiMessage { address, data }
   -> prepare both retirement records; failure returns the reservation for rollback
   -> kirq retains the records and non-cloneable MsiReservation in the new Line
   -> kirq publishes (MSI_DOMAIN, vector) to a fresh virq
   -> caller registers handler on virq and programs device with message

x86 MSI-X dispatch
   -> hardware vector
   -> PendingIrq::Domain(MSI_DOMAIN, vector)
   -> dispatch_claim(source, completion) retains the Domain's Line directly

free_msix(virq)
   -> preflight RCU submission (no retirement allocation)
   -> verify unused, logically disabled state and the allocation reservation
   -> consume the Line's two preallocated withdrawal records
   -> close admission and withdraw both owning indexes under registry exclusion
   -> release short locks and submit both RCU retirement records
   -> consume reservation in backend release
   -> drop the transaction's retained Line; old readers retain index owners
```

The registry lock spans validation and withdrawal, not retirement submission or
backend release. All ordinary rejection, including invalid submission context,
occurs before withdrawal. A caller-created MSI flag is not an allocation proof: a
mapping without the private reservation cannot release backend storage.
`MsiBackendToken` grants allocator access, while `MsiReservation` represents one
successful allocation and cannot be reconstructed or cloned by consumers.
The backend consumes that reservation and must not sleep, reenter KIRQ or
return without releasing it. An invalid owned reservation is an internal bug,
not a recoverable rejection after mapping withdrawal. The reservation has no
hardware-releasing Drop; an arbitrary lookup's final Arc is not a teardown owner.
A rejected release leaves the caller responsible for its allocation identity.
Device shutdown and ordering of already-issued MSI messages are caller-side
prerequisites, not consequences of the software unused check. IRQ action drain
does not stop DMA or device message production.

Allocation rollback is a different transaction: if message composition or
mapping fails, the fresh vector has not been returned to a consumer and is
released without holding the registry lock. The backend supplies its own
allocator exclusion. Allocation publication uses a fresh logical number rather
than adopting an existing domain binding. Publication failure returns the same
reservation for explicit rollback. The x86 allocator's release only clears its
owned bitmap bit and destination; a future fallible backend needs a separate
pre-withdrawal preparation protocol before integration.

Private-state tests in `ut-tests/line_admission.rs` exercise the same release
transaction as production with an injected backend: management admission,
installed actions, admitted dispatch, unfinished teardown, non-MSI identity and
missing/mismatched reservations reject without backend calls. Successful release
prevents a second backend call. `ut-tests/msi_reservation.rs` additionally checks
private ownership after publication failure, rejection of an existing binding,
and reverse-map withdrawal before the injected backend consumes ownership.
It also verifies that an actionless but logically enabled MSI retains its
descriptor, both indexes and reservation until explicitly disabled. The legacy
force-enable API can create this state without a handler; an empty action set and
idle running state alone do not authorize disposal. No controller operation is
issued by this rejection path.
These are software ownership tests, not hardware late-message validation.

`ut-tests/index_preparation.rs` injects rejection at either production insertion
preparation boundary. It verifies that neither index becomes visible and that
the same mapping can subsequently be published. This is not global allocator
OOM injection; allocation-level cancellation belongs to the XArray tests.
`ut-tests/index_transactions.rs` observes the exact interval between Domain and
logical publication or withdrawal. Software hardirq and nested NMI queries
check visibility while the Line lock still excludes admission. The lock probe
runs only when SMP lock exclusion is present, not with an elided UP lock.
The runtime lifecycle fixture additionally holds an interrupt read borrow on
another CPU across withdrawal and same-key reuse, then verifies reclamation
after reader exit and callback drain. These fixtures check context balance;
they do not claim physical IRQ/NMI delivery coverage.

MSI resources are edge-triggered and marked with `IrqFlags::MSI`. They do not
run through normal `IntrManagerIf::configure/enable` line configuration; the
device-visible state is the MSI message written into the PCI MSI-X table by the
bus/device layer. The x86 backend owns APIC destination selection and message
encoding, matching Linux's split between PCI MSI device domains and the x86
vector parent domain.

If an MSI-X vector fires without a matching `(MSI_DOMAIN, vector) -> virq`
mapping, normal dispatch reports an unhandled strict-domain miss. It must not
dispatch the raw APIC vector as an OS-visible IRQ.

`unregister()` / `free_irq()` do not delete MSI descriptors. The Line retains the
backend reservation; `MsiAllocation` is only the device-facing identity/message.
The caller's resource owner invokes `free_msix()` as the final cleanup point
for the descriptor, both owning XArray entries, and backend
vector. This keeps the documented device teardown order valid: unregister the
handler first, then free the MSI resource.
`free_msix()` withdraws the descriptor and published mapping before returning
the vector, preventing allocator reuse while the old binding remains published.
Existing pre-withdrawal failures leave ownership available for retry. Neither
allocation nor release accepts NMI context.

### Device-owned readiness

Regular IRQ dispatch invokes device actions, aggregates their handled result, and
completes the platform claim. It does not maintain a line/source subscription
table or notify generic IRQ waiters. Device subsystems own readiness and wakeup.

The native console action collects UART bytes into bounded per-port storage and
submits prepared kworkng work after releasing the port lock. TTY parses input,
echoes, delivers signals, and wakes readable waiters in normal-worker context.
Networking similarly owns its receive processing and socket readiness.

`driver_os_api_xkernel` and devres manage IRQ resource lifetime, not upper-layer
waiter registration. Descriptor-local ordinary execution completion remains part of
`synchronize_irq()` and `free_irq()`; it is not an event subscription. Hardirq
lifecycle hooks and deferred-executor handoff are unchanged.

`kirq` owns hardirq dispatch, softirq and IRQ-thread execution infrastructure.
`kworkng` owns general sleepable workqueues. Networking, TTY and block
subsystems own the decision to wake waiters when their data becomes ready.

### Lifecycle hook

```text
register_irq_lifecycle_hooks()
   -> handle_irq()
      -> IrqLifecycleGuard::enter()
         ├── snapshot current hooks
         └── call on_irq_enter
      -> dispatch and complete hardirq
      -> Drop guard
         └── call snapshot on_irq_exit
   -> clear_irq_lifecycle_hooks()
```

The guard saves a snapshot of the exit hook at enter time, so clearing the hooks midway through handling does not break the enter/exit pairing of this IRQ. When no hook is registered, IRQ entry skips the global hook lock directly via an atomic active bit; only when the active bit is set does it acquire the lock to copy the hook snapshot. The lifecycle hooks describe the trap/preempt-off lifecycle and are not equivalent to the Linux `hardirq_count` boundary.

### IRQ context

```text
handle_irq()
   -> HardIrqContextGuard::enter()
      -> platform dispatch / handler dispatch / controller completion
   -> drop HardIrqContextGuard
   -> deferred executor
```

`kirq::context` maintains the hardirq, serving-softirq and BH-disabled depths in per-CPU state.
The public query APIs use `NoPreemptIrqSave` to protect the read of the current CPU slot, so that when an ordinary task's query is interrupted by a local IRQ, it does not read the same per-CPU slot concurrently with the hardirq guard's writes.
The hardirq guard and softirq runner IRQ-tail hot paths run with local IRQs already masked and the CPU pinned by the caller, and use `*_irqoff` crate-local helpers. These helpers still create a nested `NoPreemptIrqSave` (or `IrqSave` for snapshots), a near-free nested save/restore on already-masked paths.

Context diagnostic semantics:

- hardirq, serving softirq and BH-disabled are all non-sleepable / interrupt-like.
- `interrupt_context_level()` gives a coarse-grained diagnosis by the priority order hardirq, softirq, BH-disabled, task; BH-disabled is not hardirq, but sleepable callbacks still cannot run in it.
- `is_in_interrupt_context()` is a bottom-half gating predicate, not a future IRQ-thread predicate. A future sleepable IRQ-thread context should be a separate execution context, belonging neither to softirq nor to the deferred executor, and should not be expressed through this predicate.
- Calling `local_bh_disable()` in hardirq increments a diagnostic counter and rate-limits the warning; underflow diagnostics are likewise counted and rate-limited, to avoid a log storm on error paths.

The `LocalBhGuard` returned by `local_bh_disable()` holds `NoPreempt` until drop, so the BH-disabled critical section does not migrate to another CPU. Enter/drop additionally mask local IRQs briefly while updating the per-CPU depth; the outermost drop attempts a direct softirq drain only when on the same CPU and not in hardirq/serving-softirq. `LocalBhGuard` is not `Send` and cannot be transferred across threads.

### Softirq foundation

```text
open_softirq(vec, action)
   -> SOFTIRQ_ACTIONS[vec] = action

raise_softirq(vec)
   -> current CPU SOFTIRQ_PENDING.fetch_or(bit, Release)
   -> if pending transitioned from empty to non-empty, wake current CPU ksoftirqd
      when not already in hardirq/serving-softirq/BH-disabled

run_pending_softirqs()
   -> pending.swap(0, Acquire)
   -> enable local IRQs while running action snapshot in vector order
   -> disable local IRQs before checking new pending bits
   -> optional handler context leak check when IRQ context debug is enabled
   -> if work remains, query the scheduler's needs_resched flag
   -> repeat up to restart limit while no scheduling request is pending
   -> preserve pending and wake current CPU ksoftirqd on either limit
```

`bottom_half/softirq.rs` implements a Linux-like fixed-vector foundation, using per-CPU pending bits.
Through `SoftirqDaemonIf`, `ktask` provides `ksoftirqd/N` for each online CPU. The daemon is
pinned to its CPU, blocks waiting on the `kirq` wake source, and drains
that CPU's pending softirqs in normal task context. When the restart limit is hit, or when a task-context raise of a softirq makes the current
CPU's pending mask go from empty to non-empty without a chance for a direct run at hardirq-exit/BH-enable, `kirq`
wakes the current CPU's daemon as the backoff executor.
If a softirq is raised in a hardirq, an active softirq, or a BH-disabled critical section, `kirq` only keeps the
pending bit and does not wake the daemon directly: hardirq exit or the outermost `local_bh_enable()` will try a direct drain on the same
CPU; only if the direct drain exceeds the restart budget is the work handed over to `ksoftirqd/N`.
If a task-context raise happens before that CPU's `ksoftirqd/N` is created, the pending bit is preserved;
the daemon's wait loop checks `local_softirq_pending()` first after it starts, so draining early pending work does not depend on another wake.

The entry and exit of the softirq runner keep local IRQs masked, so that IRQ-tail, BH-enable and
daemon drain share the same `*_irqoff` internal path; inside the action loop, local IRQs are temporarily enabled,
and disabled again before checking new pending bits and the restart budget. This semantics aligns with the key behavior of Linux
`__do_softirq()` while still keeping preemption disabled. M3 is still not a complete Linux
softirq/NAPI equivalent: it currently creates no workerqueue thread, tasklet or threaded IRQ
handler, and has no NAPI poll budget/repoll state machine.

Yielding by `ksoftirqd/N` is the responsibility of the `ktask` provider: after each time the daemon actually runs softirqs, it
voluntarily returns to a scheduling point, avoiding holding the same CPU for a long time under a persistent pending workload. Currently, when a batch of actions completes in direct
drain and work is still pending, the current task's scheduling request is queried through `kiface_task_api::TaskPreemptionIf::needs_resched()`;
if a request exists or the restart limit is hit, pending is preserved and the daemon is woken.
The query neither consumes the request nor schedules directly; it returns false if the task is not yet initialized or preemption is not enabled.
This check cannot interrupt a single action, and Linux's 2ms time budget is not introduced.

`open_softirq()` is only for init-time registration. There is no unregister API at this stage; the runner copies an action table snapshot
under `SpinNoIrq`, but does not hold the registration lock while executing actions.

### Workqueue Integration

Workqueue ownership belongs to `task/kworkng` and its `kworkqueue_ng` and
`kworkerpool_ng` subcrates. See their design and security documents for queue
state, prepared storage, cancellation and shutdown contracts. `kirq` does not
own work objects or normal worker tasks.

`kworkng::runtime::init` registers Tasklet and High actions after all CPU
services are ready. Fixed `kipi` reasons raise the target CPU's softirq bits;
local dispatch and ksoftirqd both enter the same bounded BH service. Pending
demand survives a budget-limited pass and requests another pass.

Normal callbacks run on pinned worker tasks and may sleep. BH callbacks run
inside softirq context and must not block. Prepared submission is allocation
free; borrowed Work requires the caller to retain pinned storage through drain.
Safe owned handles arrange deferred cleanup on the normal service. Synchronous
waits check sleepability and optional self-wait diagnostics. Neither IRQ context
checks nor self-wait detection prove the absence of arbitrary dependency cycles.

### IRQ diagnostics configuration

KIRQ diagnostics are controlled explicitly through Kconfig, rather than binding production diagnostics to `unittest`:

- `KFEAT_IRQ_CONTEXT_DEBUG` enables execution-context invariant checks, covering
  detection of IRQ-on misuse of `raise_softirq_irqoff()`, comparison of context snapshots before and after a softirq action,
  and diagnostics such as hardirq/softirq/BH depth underflow. This option
  is on by default under `BUILD_TYPE_DEBUG` and off by default in release, so that the high-frequency interrupt path does not bear the extra CSR
  reads and context snapshot cost.
- `KFEAT_IRQ_STAT` enables IRQ/softirq diagnostic statistics, recording counters such as softirq runs, restart limit
  hits, daemon wake/suppression, context-deferred runs, unhandled vectors and context
  debug warnings. Statistics on high-frequency paths must be collected in per-CPU counters and aggregated only when the diagnostics are read,
  so that a global atomic cacheline does not become a new bottleneck under multi-core interrupt pressure.

`unittest` can still enable test helpers and diagnostic read entry points, but it is only a validation mechanism, not the sole switch for production diagnostic
capability.

### Deferred executor

```text
softirq::init()
   -> internal register_deferred_executor()
   -> handle_irq()
      -> IntrManagerIf::dispatch_irq()
         -> generic IRQ/NMI helper (when a claim exists)
      -> run_hardirq_exit_deferred(ctx)
         └── call on_hardirq_exit(ctx)
```

The deferred executor is a single-owner hook. `softirq::init()` installs the
softirq runner before local IRQs are enabled by `kruntime::init_interrupt()`.
Every IRQ-on entry reaches the exit hook, including an entry subsequently
identified as NMI or spurious; the dedicated IRQ-off NMI path does not.
Registration is internal to KIRQ. Clearing is available only under
`cfg(unittest)`, not as a production shutdown API. Both the hook record and its
atomic publication are updated under the registration `SpinNoIrq`, including
test clearing/restoration. The IRQ-tail hot path reads an atomic function-pointer
slot and returns immediately when empty, without a global lock. Test replacement
does not supply a runtime callback-drain or owner-unload protocol.

Workqueues use explicit submission through `kworkng::DefaultQueue::Normal`.
Tasklets and IRQ threads must use their corresponding execution infrastructure,
not install a competing implicit deferred-executor owner.

## Algorithm Flow

### Unclaimed action fanout diagnostics

For a descriptor with ordinary action admission, dispatch visits the current
action chain up to its captured publication ceiling and ORs the visited actions'
claim results. Concurrent unlink can bypass unseen actions; this is not an
exact admission-time fanout snapshot. Only a completely unclaimed
fanout increments the descriptor's unhandled count. `HANDLED` with no source
bits is still a claim; a claim is not proof that a device deasserted its IRQ.

The count and pending suppressed-report count are saturating `u64`s owned by
`IrqDescRuntimeState`. Accounting uses the retained Line's state critical section
at execution-admission completion, without acquiring `IRQ_STATE`. CPUs sharing a
Line, including a per-CPU IRQ descriptor, share this diagnostic count. Accounting
adds no lock beyond those used by cleanup, nor an atomic, allocation, clock read,
feature switch or controller call.
This warning budget remains active without `KFEAT_IRQ_STAT`; it records only
unclaimed fanout, rather than adding general per-CPU IRQ activity statistics.
The existing guard and claim completion ordering is retained; an owned summary
is logged after releasing the Line lock and waking synchronization waiters.
Consequently reports can appear out of order across CPUs, or after a later
descriptor lifetime has begun. A virq and count are not a unique lifetime id.

Reports occur at counts 1, 2, 4, ... and once at `u64::MAX`, at most 65 reports
per descriptor lifetime. Each report gives its cumulative count and the number
of earlier events suppressed since the previous scheduled report. This is
event-count sampling, not a time-based rate limit. Reaching `u64::MAX` marks a
lower bound; later events never wrap or restart logging. Logging filters may
hide a scheduled report, as with other `warn!` calls.

Handled dispatches, action additions/removals, descriptor metadata updates,
and enable/disable do not reset the counters. Existing execution ownership
prevents an old snapshot from being charged to a newly created descriptor.
Actual descriptor removal discards its counters; recreation starts at zero.
No public counter-query API or final teardown summary is provided, so an
unreported tail is not preserved after removal and report spacing can grow
large. These counts are not per-action attribution, a failure ratio, a hardware
mask policy, or a recovery detector.

Unresolved sources, resolved sources without descriptors, admission with no
actions, and unsupported threaded-state warnings remain separate diagnostic
paths and can still log on each occurrence. If an admitted ordinary traversal
visits no actions because of concurrent unlink, its merged result is unhandled
and participates in the sampled count. Dedicated NMI dispatch remains outside
`IRQ_STATE`.

### `register(desc, handler)`

1. Parse `IrqSpec` and validate the requested management context.
2. Prepare the action node with NOWAIT allocation before management or short
   locks. Allocation failure leaves membership and metadata unchanged.
3. Retain an already prepared Line, acquire management admission and revalidate
   identity. Missing descriptors are not created by registration.
4. Under Line exclusion, reject occupied or sequence-exhausted membership before
   merging descriptor metadata or publishing mappings. Release registry exclusion
   before controller application; retain the original Line guard.
5. Generic registration rejects resolved PerCpu flow. Dedicated PerCpu
   registration requires disabled local slots and never auto-enables delivery.
   Accepted registration applies stale configuration before publishing the node.
   Ordinary default registration then enables delivery; disabled registration
   retains a disable depth of at least one and waits for explicit `enable_irq()`.
   If compatibility bring-up already enabled an otherwise empty ordinary Line,
   disabled registration requests masking under the same Line exclusion before
   committing disabled state. Recording disable depth alone would leave that
   pre-existing delivery gate open. This does not stop the device producer.
6. Release management exclusion before dropping unpublished storage or the
   caller's retained handler owner. Dispatch supplies the resolved `virq` to
   `handler.handle(virq)`.

`ut-tests/action_allocation.rs` injects preparation failure for one selected
handler in unittest builds. It checks regular, startup and shared registration
without changing membership, metadata, delivery state or the next publication
sequence, and verifies terminal handler destruction outside management/short
locks. This exercises error handling, not physical allocator exhaustion.

### `enable_irq(desc)`

1. Resolve the `IrqSpec` to an already existing descriptor, without creating a new descriptor or publishing a new domain
   mapping.
2. Reject a whole-Line teardown with `TeardownInProgress`, preventing re-enable
   after last-action withdrawal. A non-last shared removal adds no such gate;
   surviving actions retain their normal enable/disable rules during its wait.
3. If the line has no registered action, return `NoIrqAction`; an ordinary explicit enable serves only the action lifecycle of
   disabled registration / nested disable.
4. Decrement `disable_depth`; only when the depth returns to `0` is the platform line configured and unmasked as needed.
   The legacy `enable(spec, true)` still keeps the force-enable semantics used for platform bring-up, but likewise must not
   pass through the teardown gate; hitting an existing teardown descriptor must be rejected before resolve/publish,
   to avoid merging metadata or inserting a domain mapping on the failure path.

### `free_irq(desc)`

1. Reject waiting teardown in hardirq, softirq and BH-disabled contexts.
2. Retain the Line under `IRQ_STATE`, acquire its management mutex outside short
   locks, then revalidate identity and record admission under `IRQ_STATE`.
3. Validate sleepable execution before withdrawal. Action removal prepares no GP
   marker and allocates no retirement storage.
4. Unlink the regular action under the Line lock. Mask the last action's source
   and set disable depth to at least one before releasing that lock.
5. Outside short locks, drain the retained Line's aggregate execution through
   controller completion. Keep detached node storage and its successor link
   intact until the drain finishes; only then recover the action owner.
6. Finish teardown and admission under the original Line lock; no directory
   lookup or removal occurs. MSI resource owners use `free_msix()` for final
   release, independently of action removal.

Free does not cancel pending service or reserve an action-registration epoch.
After later registration on the same live Line, delayed replay may run the new
eligible action; it cannot snapshot the removed owner.

The public `IrqDescError::RetirementUnavailable` variant was deliberately removed:
action removal no longer prepares retirement storage or a grace period, so this
error has no producer. Downstream exhaustive matches must remove that arm rather
than map it to another action-free error. This does not remove XArray directory
retirement or its separate submission/failure contract.

### `free_irq_action(desc, token)`

1. Reject waiting teardown in hardirq, softirq and BH-disabled contexts.
2. Retain the Line under `IRQ_STATE`, acquire its management mutex outside short
   locks, then revalidate identity and record admission under `IRQ_STATE`.
3. Unlink only the matching token under the
   Line lock. An unmatched token changes neither membership nor delivery state.
4. Only last-action removal adds whole-Line teardown and masks the source.
   Non-last removal leaves surviving actions dispatchable under normal flow
   eligibility; the management mutex still serializes membership changes.
5. Release short locks and drain aggregate execution before recovering the detached
   action. The management mutex excludes other membership writers throughout
   unlink and drain. No action RCU read scope or GP is involved. This software
   execution drain does not establish hardware source quiescence.
6. Finish management admission and any teardown credit this transaction owns
   on the original Line. Non-last removal must not issue a cleanup unmask over
   a survivor's running flow. Directory membership remains retained.
   The Line-wide `IrqDrain` remains required: continued survivor traffic
   can prolong the drain. Removal does not promise bounded completion latency.

### `synchronize_irq(desc)`

1. Reject waiting synchronization in hardirq, softirq and BH-disabled contexts.
2. Resolve the `IrqSpec` to an existing `virq`, without creating a new descriptor.
3. Wait for retained-Line `has_active_execution()` to become false, including original-claim
   completion and admitted running replay, not just callback return. Pending
   queue entries are excluded and may execute later if still eligible.
4. This API does not mask the platform line; to prevent new handlers from entering, call `disable_irq()` first.
5. The caller must not hold a lock the handler may acquire or a resource lock that prevents the target CPU's handler from exiting, otherwise
   a deadlock may form in which the handler waits for the lock and teardown waits for the handler.

Execution waits use the descriptor-local `execution_idle` completion, obtained
through `execution_idle_completion()`. Under the retained Line lock,
`begin_running_dispatch()` reinitializes it only on ordinary idle-to-busy.
Finishing running admission publishes completion when running ownership is
absent. Ordinary flow tails
finish admission under the same lock as final chip/completion work, marking the
guard Finished before unlock; guard Drop cannot finish it twice. Deferred wakes
run after unlocking. The completion path neither acquires `IRQ_STATE`
nor looks up or deletes a descriptor. A delayed wake from a prior dispatch
cannot mark a newly reinitialized completion done, and wake callbacks never
run under the Line lock. `kcompletion::DeferredWake` owns a completion `Arc`,
not an escaping stack waiter. The task provider implements scoped pinned
waiters through `CompletionWait` and the shared park latch, without allocating
poll registrations or Wakers; kirq does not depend on `ktask`/`ksync`.
`kiface_task_api::TaskBlockingContextIf::is_sleepable_context()` checks sleepable task
context before synchronization; kirq rejects invalid callers with
`IrqDescError::InvalidContext`. `free_irq*()` checks before removing actions,
then calls `kiface_task_wait_api::wait_for_completion()` after releasing control locks.
The task provider returns `KError::InvalidInput` for invalid direct calls;
registration has no allocation-failure branch. IRQ error mapping stays in kirq.
The manager validates before side effects and restores all temporary IRQ locks
before waiting, without leaving state that changes sleepability. Its post-removal
wait therefore asserts that site-specific invariant rather than treating a
failed wait as successful teardown.
`Completion` serves only as a wake source;
The authoritative ordinary condition is `!is_running`, rechecked under
the retained Line lock after every wait.

Each synchronous operation captures an `IrqDrain` under `IRQ_STATE` at its
lookup, disable decision or action withdrawal. The directory now owns
`Arc<IrqLine>` entries. Each Line contains its descriptor/action state behind
a short IRQ-safe lock, and the drain retains that same Line. Predicate rechecks
lock the original Line directly: neither a missing directory entry nor a reused
number implies that the original dispatch is complete. Completion remains only
a wake mechanism; the retained Line's aggregate execution predicate, observed under its lock,
is authoritative. That lock also serializes Level/Edge/Fasteoi/PerCpu rejections through
chip operations and original completion. Such rejections need no execution
credit: a drain cannot acquire the lock and observe idle until they release it.
This is not a barrier against future arrivals or proof of source shutdown.

Dispatch snapshots borrow their execution guard; the guard owns the retained
Line, while snapshots own neither the Line nor handler references. Guard
completion and teardown finish update that instance directly without looking
up or removing a directory entry. Short-lock ordering
is registry lock then Line state lock. NMI writers use the same Line management
mutex before these short locks; there is no global NMI publication/table lock.
Never acquire the registry or management mutex while holding a Line state lock.
Wait and wake callbacks run after all short locks are released.

Each Line also owns management exclusion, acquired before short locks and
retained across request/free transactions, including dispatch drain. Directory
removal requires no admitted management transaction as well as aggregate-idle
execution. Candidate Line and completion storage is prepared outside
short locks and retained by its preparer until those locks have been released.
The controller flow boundaries and prerequisites are recorded in
[Controller Flow Contracts](#controller-flow-contracts). Selected PLIC and ordinary GIC
SPI Fasteoi Lines additionally implement running/pending admission, with retained
software replay for PLIC and hardware-only retrigger for GIC. Unsupported sources
are rejected rather than assigned a fallback. x86 IO-APIC level sources select Fasteoi with hardware
reassertion. IO-APIC edge and allocated MSI sources select `IrqFlow::Edge`.
LoongArch PCH levels select `IrqFlow::Level`; unsupported edges are rejected.

Fasteoi and software replay complete their running interval and decide pending
service under one uninterrupted Line lock: clear running/replaying, complete the
original claim, then retrigger or publish the queued Line if eligible. The
permanent BH notifier is returned by this locked preparation and invoked only
after unlock. A shutdown between publication and notification is valid; the
executor rechecks eligibility rather than treating notification as admission.
Execution accounting remains held through this tail in the current model.

### Edge Flow

`EdgeChip` supplies mask/unmask and hardware retrigger. Admission sets running
and consumes the original claim under the Line lock before handlers execute.
An overlapping or disabled arrival sets pending, requests masking and consumes
its own claim without disturbing the running owner. That owner loops over
coalesced pending work while eligible, unmasking before the next action snapshot;
disable, configuration failure or teardown ends the loop without unmasking.
Only the accepted owner takes running admission before snapshot/unlock;
rejected arrivals retain the Line lock through masking and completion without a
credit. Additional iterations
create no hardware claim and issue no extra EOI. Running ends before the outer
admission is released. This follows Linux `handle_edge_irq`, independently of
Fasteoi's running-arrival policy.

After final enable or surviving shared-action withdrawal, pending edge work
requests hardware delivery to the backend's retained routing. Acceptance clears
pending; rejection retains it and the x86 provider diagnoses the failure. A
later management retry can retry after the routing fault is corrected. Hardware
request acceptance is not a remote completion or an in-flight software replay.
MSI mask/unmask remain device-owner operations; the x86 chip's no-op mask does
not stop a device, even though generic disabled admission suppresses handlers.
Ordinary control has no global serialization lock. Accepted dispatch admission
survives handler return through its required completion/tail work. Empty
Level/Edge/Fasteoi arrivals complete under the Line lock without a credit;
Accepted PerCpu executions rely on source-owner lifetime protection until full dispatch return. PerCpu
rejections complete under the retained Line lock without a count. Normal-IRQ claims for
NMI-owned Lines complete under the held Line lock without a count or action access.
A drain checks aggregate idle under that same lock, so it cannot skip a locked rejected
completion already in progress.
An unresolved or missing Line still completes its claim, but acquires no Line
obligation and must not attach cleanup to a subsequently published replacement.
Both synchronous and asynchronous disable now use one retained-Line transaction.
They acquire registry then Line state exclusion, release the registry,
and update disable depth plus the bounded controller mask operation while still
holding Line state exclusion. Synchronous disable waits on that retained Line
only after all short locks have been released. Disable does not acquire the
global control lock: its Line guard serializes against enable, registration and
withdrawal state/controller updates. An outer local IRQ/preemption guard remains
necessary because the registry-to-Line handoff releases the nested IRQ-saving
lock guards out of order; it restores the entry context after both locks end.
Enable and registration retain the same outer local protection, not a global
mutex. Directory transactions and controller-internal register locks may still
serialize independent Lines; no throughput improvement is claimed without measurement.
Normal and legacy enable follow the same registry-to-Line handoff: the selected
Line remains owned while configuration and unmask run under its state lock,
without registry exclusion. Normal enable remains lookup-only and requires an
action; legacy enable also requires a prepared Line, but can force the
hardware enable write at depth zero for per-CPU bring-up. Neither path creates
a missing descriptor. Absence returns `UnknownIrq` before number allocation,
mapping publication or controller operations. Fixed timer/IPI preparation in
`kruntime` precedes local source activation; SGI15/PMU and vtimer route setup
likewise prepare their mappings before enable. Restore reuses that identity.
Nested enable still
decrements disable depth without unmasking until the depth reaches zero.
The final enable admission checks teardown and prepares the enable plan under
the same Line guard that covers controller application. Normal enable has no
separate unlocked teardown observation. Legacy enable additionally prechecks
before descriptor merging and passes the same guard to the locked resolver.
Resolution selects an identity, validates descriptor and mapping compatibility,
then commits metadata and prepared index entries without reacquiring
the Line lock. Ordinary map and registration reuse the same validation/commit
implementation. Registry exclusion ends before controller application; Line
exclusion remains continuous. NMI publication uses that same original Line,
without a second global publication lock or raw-hwirq namespace.

`ut-tests/enable_preparation.rs` checks missing plain/Domain identities with
IRQs initially enabled and masked, plus rejection of an orphaned reverse mapping.
Existing failure/locking tests prepare their Line explicitly before exercising
configuration and force-enable. All regular/shared/NMI registration, including
the explicit startup variants, requires a prepared descriptor. Absence is
rejected before preflight, controller configuration or action publication.
Primary timer/IPI, SGI15 and PMU setup
prepare mappings first. Startup acquisition still never waits, and descriptor
identity is rechecked after acquisition before metadata changes. Task-mode
registration also performs lookup only: console and virtual timer routes map
before requesting actions, and devres consumes mapped or MSI-allocated resources.
Missing descriptors are not allocated or published by registration. Ordinary
registration prepares a NOWAIT action node before descriptor lookup: allocation
failure can return `NoMemory` first; after successful preparation, a missing
descriptor returns `UnknownIrq` and the unpublished node is dropped. Requesting
an action does not allocate the IRQ descriptor, as in Linux, but this local
action-storage preparation order differs from Linux's lookup-first ordering.
Regular, shared and NMI registration retain one Line guard from preflight
through descriptor resolution, configuration, action installation and delivery
restoration.
The resolver and installation bodies borrow that locked state instead of
reacquiring the Line lock. Management admission pins membership after the
directory guard is released, before entering installation/controller code.
Preflight rejection precedes metadata mutation. Disabled registration leaves
the source disabled; later shared
actions preserve disable depth and only reconfigure stale metadata. Caller-owned
handler references outlive the management transaction, including rejection.
Regular and NMI registration both serialize publication through the original
Line's management mutex and state lock, without a global publication lock.
No sleepable lock acquisition occurs under the state lock. Registration and
removal separately retain an outer NoPreemptIrqSave across their non-LIFO
registry-to-Line handoff.
Ordinary regular/shared withdrawal likewise relies on management admission rather than
reacquiring the directory in its body. Action unlink and, only for the last
action, teardown marking and masking form one Line-locked transaction, without
global control exclusion. Same-Line registration/removal is serialized by the
management mutex. Enable/disable use the state lock; a last-action teardown
rejects re-enable, while non-last removal leaves normal survivor eligibility.
The detached node leaves that transaction before the caller drains the original
Line, with all short locks released. Only after the drain may the caller recover
the action and free node storage. Admission completion uses only the retained
Line lock, and releases management before returning the handler
to the caller. Removing a non-last shared action does not mask the remaining
actions.
NMI registration and withdrawal retain the original Line state lock across
bounded controller configure/mask operations, after directory release.
The handler is installed in the Line's `NmiAction`, not in the ordinary fanout
array. Successful local preparation publishes access on that CPU. A state
marker reserves ownership for both dispatch paths. Removal requires completed
local revocation and an empty publication array, not an action grace period.
The returned owner is destroyed only after management and short guards release.
Outer IRQ-off protection outlives both directory and Line guards, so releasing
the directory first cannot restore ordinary IRQ delivery inside this transaction.
Synchronous disable captures this identity in the same transaction as its
disable plan; it cannot mask one instance and then synchronize another.

Per-Line controller exclusion does not guarantee independent progress for all
unrelated control operations. Directory-to-Line acquisition can still retain
`IRQ_STATE` while waiting for a busy Line, delaying other directory users.
Controller-internal register locks may also serialize distinct Lines. Releasing
the directory before controller application removes that particular global
critical section. Normal IRQ lookup separately uses RCU and does not take
the registry lock; these control-plane serialization limits do not apply to it.

`synchronize_irq()` does not mask the line; a persistent IRQ may still cause the wait condition to repeatedly become non-zero, so teardown
paths should use `disable_irq()` or `free_irq()`.

### normal IRQ dispatch

1. `khal::irq::irq_handler()` creates the `NoPreempt` guard.
2. `kirq::handle_irq()` creates the lifecycle guard and triggers `on_irq_enter`.
3. `HardIrqContextGuard` enters the generic hardirq context.
4. The platform backend claims and classifies; a normal IRQ constructs
   `PendingIrq { IrqRef, completion cookie }` and directly calls
   `generic_handle_irq()`.
5. `generic_handle_irq()` delegates source resolution and original completion
   to `dispatch_claim`. An RCU lookup retains the original Line without
   `IRQ_STATE`; its lock checks retirement and flow eligibility. Accepted ordinary
   Level/Edge/Fasteoi handlers acquire running ownership before capturing actions
   or unlocking.
   Fasteoi checks running and action eligibility before capturing the list ceiling;
   rejected disabled/action-less arrivals become pending and are masked before
   completion. Level/Edge/Fasteoi rejections retain the Line lock through chip
   operations and completion without a credit or clearing the active owner.
   Accepted PerCpu executions rely on source-owner lifetime protection until full dispatch return;
   PerCpu rejection completes under the retained Line lock without a count. Normal-IRQ
   claims for NMI-owned Lines complete under the held Line lock with no count or
   action invocation. Missing mappings/Lines complete the
   original claim without attaching to a replacement. The return is resolved
   IRQ metadata, not a statement that an action ran or handled the interrupt.
6. Without directory or Line locks, traverse the ordinary chain while borrowing
   the live dispatch guard, up to the captured publication ceiling. Each visited primary receives the
   resolved `virq`; results are combined and classified. Concurrent unlink may
   skip an unseen action. An unclaimed pass records diagnostics, without generic
   waiter notification, threaded-handler scheduling or per-action statistics.
7. Ordinary running execution ends only after its required completion/tail work. The Fasteoi owner
   clears running and completes under its original Line lock; accepted PerCpu completion
   stays unlocked. Edge completion occurs before handlers as described above,
   but admission survives the whole pending loop. Level completes after mask/ack
   before handlers; it acquires admission after that locked completion and before
   snapshot/unlock, retaining it through conditional tail unmask. Rejected
   Level/Edge/Fasteoi arrivals return after locked completion without admission.
   Admission release updates only the retained Line and wakes drain waiters
   outside its lock. It does not acquire the directory lock or delete even an
   empty domainless descriptor; an empty claim must not undo preparation.
8. `HardIrqContextGuard` exits the generic hardirq context.
9. `run_hardirq_exit_deferred()` calls the currently registered deferred executor;
   `DeferredRunContext::resolved_irq()` carries the IRQ identity resolved in the previous step, and is `None` for NMI, strict
   domain miss or spurious, and does not express whether a handler actually ran.
   The default owner is `softirq`, which drains the current CPU's pending softirqs when the context allows;
   pending work not covered by the restart budget is handed to the current CPU's `ksoftirqd/N`.
10. The lifecycle guard is dropped and triggers `on_irq_exit`.
11. After returning to the `khal::irq` adapter, `NoPreempt` is released.

### NMI Dispatch

Both dedicated NMI entry and controller-classified IRQ-on NMI delivery pass a
typed `PendingIrq` to `generic_handle_nmi`. GIC identifies its root Domain
and raw hwirq; the original cookie retains completion responsibility.

1. Hold CPU/IRQ exclusion and establish effective NMI context and RCU watching.
2. Open one interrupt read scope before borrowing the Domain's XArray Line;
   obtain the logical `Virq` from that retained Line's published identity.
3. Use `NmiAction::with_current` to borrow the local publication and invoke
   the handler with that logical `Virq`, uniformly with normal IRQ handlers.
   No action borrow escapes the callback. Domain/hwirq selects the Line, not
   the callback argument; no second lookup by number supplies its identity.
4. Complete the original claim, including when lookup or action is absent.
5. End the directory read scope, restore watching and NMI context, then
   release the outer CPU/IRQ guard.

The true NMI read path takes no registry, Line or management lock, clones no
Arc, runs no destructor and emits no potentially locking diagnostic.
It does not fall back to the logical directory on a Domain miss.
Normal-IRQ delivery to an NMI-owned Line is rejected regardless of local enable
state. Original claim completion runs under the already-held Line lock without
execution admission. No local publication is borrowed and no handler receives a
logical-IRQ callback. This branch adds no diagnostics or locks. Drain acquires
the same Line lock and cannot skip that completion; this does not prove hardware
quiescence. There is no second regular action.

IRQ-off NMI entry does not invoke ordinary lifecycle hooks. IRQ-on entry has
already entered the normal IRQ lifecycle before classification; GIC completes
the NMI before reopening NMI delivery and continuing the ordinary IRQ exit.

Tests cover borrowed dispatch under held writer locks, single ownership,
logical callback identity distinct from the controller hwirq, original completion within the read scope,
and refusal to withdraw a locally published owner. They do not prove physical
NMI delivery, source quiescence or remote shutdown completion.

## Concurrency Model

### Ordinary Action Lifetime Proof

The private chain in [action_set.rs](../src/runtime/action_set.rs) stores each
action in a stable KBox allocation. Head/tail updates and head capture are
Line-locked, so lock release/acquire publishes the initial head's payload.
Release publication of atomic next links pairs with Acquire traversal.
These memory orderings publish initialized payloads;
they do not by themselves protect lifetime. For ordinary Level/Edge/Fasteoi
actions and replay, that protection has four parts:

1. [dispatch.rs](../src/runtime/dispatch.rs) grants ordinary running admission under the
   original Line lock before capturing a cursor. `IrqDispatchGuard::snapshot_actions`
   binds the snapshot's lifetime to the guard. Its unsafe contract requires the
   caller to establish exact-Line lock identity and live admission; the returned
   lifetime enforces only that the snapshot cannot outlive the guard.
   The guard retains that same Line
   through callbacks and original claim completion; a numeric IRQ match alone
   is not a lifetime proof.
2. [manager.rs](../src/runtime/manager.rs) serializes membership with the Line's
   management mutex through unlink and `IrqDrain::wait`. Unlink and snapshot
   capture share the Line lock. Later captures cannot reach the detached node;
   earlier captures retain running admission. A reader already on the detached node may
   still follow its next link, which must not be cleared or repurposed yet.
3. The management mutex prevents a second writer from removing/reclaiming that
   successor during the first removal's drain. [state.rs](../src/runtime/state.rs)
   publishes aggregate execution state under the same Line lock used by
   admission. Observing `!has_active_execution()` after unlink proves all potentially old borrowers
   have finished. Later admissions can only traverse the remaining chain.
4. Only after that observation does `RetiredAction::reclaim` recover the detached allocation.
   Node reclamation is shallow, not recursive through next. The returned handler
   remains owned until management cleanup and unlock, so final client destruction
   cannot run under the Line or management locks. Abandoning detached ownership
   must not free storage before drain.

Publication ceilings bound each traversal against later append; they are not a
reclamation epoch and do not promise exact admission-time membership.
Level/Edge/Fasteoi and eligible replay use exclusive running ownership, acquired
under the Line lock before snapshot/unlock and retained through all borrows
and required tail work. A running tail may clear `is_running` before its final
chip/completion transaction only after consuming snapshots and under continuous
Line exclusion through admission finish. Drain cannot observe that intermediate
state. PerCpu has no execution count and is outside this ordinary drain proof.
Neither ordinary idle nor a node drain reports the irqchip's
active/pending hardware state or stops a device producer.

For PerCpu sources, Linux `handle_percpu_devid_irq` does not use the ordinary
running flag, and `__free_percpu_irq` relies on caller-completed per-CPU shutdown
before unlinking. A cleared enabled bit alone is not equivalent: a callback can
disable its own source while its execution and original completion are still
outstanding. Different CPUs may execute the shared PerCpu action concurrently;
a single Line-wide running bit cannot represent those independent lifetimes.
The source owner must therefore wait for every admitted dispatch to return fully
before calling unsafe removal, and exclude restart and new dispatch through removal.
This protects the raw action borrow without running ownership or an execution
count. Rejected PerCpu completion remains under the Line lock and borrows no
action. An empty enabled mask alone does not discharge accepted-execution
obligations. `PER_CPU` metadata does not establish
a flow: tests select PerCpu or an explicit fake ordinary chip, never Legacy.

The dedicated unsafe `try_free_percpu_irq` entry checks resolved PerCpu flow under
the retained Line's management and state locks before withdrawal. Wrong flow
returns `UnsupportedFlow`; any enabled local slot, shared/NMI ownership or
absence returns no handler without changing delivery or action membership.
Only a regular action is unlinked. Prior source-owner quiescence permits
reclaiming its storage in a separate removal closure without `IrqDrain` or a
PerCpu execution wait;
the descriptor remains installed. Generic free APIs reject PerCpu flow before
withdrawal, including token-based removal.
The source owner must stop sources on all participating CPUs and wait for all
admitted dispatches to return before the call, including their post-completion tails, excluding restart
across removal. The enabled-slot check cannot verify this unsafe precondition.

Generic synchronous disable and synchronize also reject PerCpu before changing
disable depth or waiting. Local enable/nonsynchronous disable remain usable on
the calling CPU. This is an explicit KIRQ interface boundary, not a claim that
Linux's `synchronize_irq` has the same rejection check: Linux separates per-CPU
source shutdown and action ownership from ordinary running-state synchronization.
Dedicated removal supplies no PerCpu execution drain. No broadcast stop or
new source-owner protocol is implemented by these checks.

NMI retirement has a separate proof: the source owner closes starts, completes
local stop/teardown on every participant, and only then withdraws the immutable
action owner. Normal-IRQ rejection for that Line completes under its held lock,
without execution admission or action access. Line exclusion serializes that
completion with management; it does not replace local NMI publication revocation or establish hardware
quiescence. The PMU owner supplies synchronous
per-CPU shutdown; permanent timer/IPI registrations have no general runtime
removal protocol. The only production PerCpu free caller is VMM preactivation
route-preparation rollback, serialized before forwarding publication. A new
runtime caller must supply its own complete source/entry exclusion proof,
not another action grace period or merely an empty enabled mask.

This follows the Linux-style distinction between action unlink plus IRQ execution
synchronization and descriptor RCU retirement (see `kernel/irq/manage.c` and
`kernel/irq/irqdesc.c`). It is not a copy of Linux's descriptor flags or threaded
IRQ synchronization: X-Kernel uses original-Line running ownership for ordinary
actions and source-owner lifetime protection for PerCpu raw borrows, and does
not claim chip-active synchronization.
The generic [krcu-list](../../../util/krcu-list/docs/design.md) prepared-GP API is
unchanged and is no longer the ordinary action container. XArray directory RCU
and the separate CPU-local NMI owner protocol remain unchanged.

The standalone [action-chain models](../tests/action-chain-models/README.md)
check finite publication, preserved-link and execution-drain scenarios with
Loom, including deliberately broken ordering and reclamation controls. Their
README maps each model to production operations and records the assumptions.
They supplement actual-code kernel tests; they do not execute the production
chain or establish raw-pointer validity, Completion wake/reset correctness,
hardware quiescence or performance. Loom stays outside the kernel dependency
graph and default unit tests.

- `runtime/action_set.rs::ActionSet` owns a private stable-node chain, its count
  and monotonic publication sequences. The Line serializes topology updates;
  node storage is prepared before short locks. Admission captures a borrowed
  cursor and sequence ceiling bound to its dispatch guard, not an Arc to a list,
  independent handler owners or an immutable membership snapshot. Traversal
  uses Acquire next loads paired with Release publication and stops above the
  ceiling. Concurrent unlink may bypass an unseen node; a reader that already
  reached it can still follow its preserved successor link. Each edge pass
  captures a new ceiling, bounding that pass against later append. NMI ownership
  stays separate from ordinary membership.
- `IRQ_STATE` serializes control-plane directory and mapping transactions;
  each Line lock protects descriptor/action state. Normal dispatch retains a
  Line through RCU and takes only its Line lock for admission. Completion uses
  that retained Line, without directory lookup or mutation.
- Both XArray indexes own the same allocation. Numeric IRQ/NMI queries borrow
  it through RCU. MSI release closes admission and withdraws both entries before
  submitting their independent retirement records outside registry exclusion.
  Each index owner remains alive until its callback's grace period completes.
- Ordinary dispatch captures the action cursor and publication ceiling and
  establishes running admission under the Line lock, then borrows handlers through its
  dispatch guard after unlocking. A lookup racing withdrawal may
  retain a retired Line, but cannot admit service on it. A replacement with the
  same number cannot redirect the retained completion owner.
- Running ownership covers ordinary Level/Edge/Fasteoi and replay execution.
  Accepted PerCpu execution has no running gate or count; unsafe removal requires
  prior source-owner quiescence through full dispatch return. PerCpu rejections
  complete under the retained Line lock without execution admission. Rejected normal-IRQ
  claims for NMI-owned Lines complete under the held Line lock without admission.
  Queued entries and locked ordinary rejections own neither. Finishing the last
  ordinary execution publishes `execution_idle` under the Line lock and wakes
  outside it. Ordinary drain observes `!is_running` under the same lock.
  Control-plane owners, not dispatch completion, decide descriptor withdrawal.
  `free_irq()` unlinks the action, masks the last action's source, then waits
  outside short locks for aggregate execution to become idle before reclaiming the node.
  It retains the descriptor and returns the handler only after management unlock.
  The last action must mask even while another teardown owns a credit.
  Last-action `teardown_depth` excludes registration and re-enable while draining.
  Non-last shared withdrawal adds no whole-Line gate; survivors may dispatch
  during the Line-wide drain, potentially prolonging that wait. The
  management mutex still serializes membership until removal completes. Waiting
  APIs reject interrupt-like contexts; callers must not hold locks needed by
  handlers or otherwise prevent their completion.
- NMI reads borrow the Domain index under RCU and the locally published action
  under CPU/IRQ exclusion. Only task/startup writers acquire management and
  state locks. Action removal requires completed local teardown on every
  published CPU, not an RCU wait; readers never acquire the management mutex.
- `IRQ_LIFECYCLE_HOOKS` uses `SpinNoIrq` and copies the function
  pointers briefly at entry only when the active bit is set. The no-hook IRQ hot path skips the global lock through an atomic active bit; the hook body
  does not run inside that lock.
- `IRQ_CONTEXT_STATE` is per-CPU state accessed through `NoPreemptIrqSave` (or `IrqSave` for snapshots). Public queries and the hardirq/softirq IRQ-tail `*_irqoff` helpers all create this guard; on paths where the caller already masked local IRQs and pinned the CPU, the nested save/restore is near-free.
- `SOFTIRQ_PENDING` is a per-CPU atomic bit mask. `raise_softirq*()` publishes the pending bit with
  `fetch_or(Release)`, and the runner takes the batch with `swap(0, Acquire)`,
  so that a bit re-raised while a handler is running is not overwritten by clearing pending.
- `SOFTIRQ_ACTIONS` uses `SpinNoIrq` to protect init-time registration and the runner snapshot; actions
  are executed outside the lock.
- `SoftirqDaemonIf` is a one-way `kirq -> scheduler` capability: `kirq` only requests a wakeup of the
  current CPU's daemon; it does not create tasks, does not block, and does not depend on `ktask`. The provider must be callable from
  IRQ-disabled context. `ktask` creates and pins
  `ksoftirqd/N` before local IRQs are enabled on each CPU, so what the daemon drains is the per-CPU pending mask of the same CPU.
- `kworkng` owns service publication; `kworkerpool_ng` uses ktask task/wait/timer
  services and kirq's BH execution context. IRQ-safe submission does not create
  tasks or block. No scheduler execution-accounting provider is needed.
- `kworkqueue_ng` owns work lifecycle predicates. Wake notifications are hints;
  synchronous waits recheck their predicate after waking.
- softirq diagnostics record context leak, restart-limit, unhandled vector, action run,
  daemon wake request, context-suppressed wake, context-deferred run and
  IRQ-enabled misuse of `raise_softirq_irqoff()`. They are used to find problems such as "a softirq action too heavy
  causing daemon fallback", "mistakenly believing sleeping is possible inside BH/hardirq", and "callers of `*_irqoff` helpers not having
  established the IRQ-off precondition". The daemon wake/request counters are recorded per event:
  `raise_softirq*()` requests or suppresses a daemon wake only once when the pending mask goes from empty to non-empty;
  a direct drain rejected in an interrupt-like context records a context-deferred run.
- `DEFERRED_EXECUTOR_HOOKS` uses `SpinNoIrq` to protect the registration/clearing control plane; the runner hot path
  reads the hook only from an atomic function-pointer slot and does not call the owner while holding a lock. A global
  reentry guard is not currently used, to avoid the hardirq-exit handoff of one CPU wrongly suppressing another CPU on SMP;
  softirq already uses a per-CPU pending/reentry model.
- `PendingIrq` owns the remaining host completion obligation and uses
  `PhantomData<*mut ()>` to prevent cross-thread transfer. The entry integration
  must separately exclude CPU migration through completion; the marker does
  not pin execution or validate a claim.

## Design Decisions

### Why it is split into a separate crate

IRQ descriptors, state, dispatch, NMI actions and lifecycle hooks form an
ownership boundary shared by architectures, platforms and drivers. Keeping
them in `khal` would couple softirq, workerqueue and threaded IRQ semantics
to HAL trap-entry details.

### Why `khal::irq` still exists

`khal::irq` is now only responsible for trap handler registration and the HAL execution constraints before entering `kirq`.
It no longer re-exports the generic `kirq` API, and no longer exposes the x86 MSI-X/APIC helpers; drivers,
the runtime, IPI, timer and irqchip backends should depend on `kirq` directly.

### Why devres is adapted only to `kirq`

`driver_os_api` is the OS-neutral resource model of the driver framework. It describes the IRQ resources a driver needs,
but does not own kernel interrupt handling semantics. The x-kernel provider lives in `driver_os_api_xkernel` (`drivers/adapters/xkernel/provider`),
and converts the trigger/controller/domain/event/handler of `driver_os_api` into `kirq`
types. This way the driver framework gains kernel IRQ capability while `kirq` does not depend back on the driver framework.

### Why lifecycle hook and deferred executor both use a single owner

Lifecycle hooks provide hardirq enter/exit notification. The deferred executor
is instead softirq's internal hardirq-exit handoff, not a public extension-owner
registry. Keeping one owner preserves exit ordering; additional deferred work
uses the softirq/workqueue interfaces rather than replacing that handoff.

### Why generic IRQ subscriptions were removed

After native TTY input moved to device-submitted work, no production consumer
needed the generic line/source waiter table. Removing both registration APIs
and dispatch-time notification keeps device readiness with its owning subsystem
and removes unrelated global subscription bookkeeping from IRQ processing.
Softirq and IRQ-thread semantics belong to `kirq`; sleepable workqueues belong
to `kworkng`.

NetRx direct progress is a separate milestone after the workerqueue foundation. For now the `NetRx`
softirq can only serve as a wake source for the network fallback worker; a later milestone will decide whether the softirq
delivers `system_wq` work, or an audited nonblocking ingress path advances
RX directly according to a budget.

### Why NMI Reads Avoid Mutable IRQ State

NMI may interrupt a writer holding registry or Line exclusion. Its read path
therefore borrows the RCU-protected index and acquire-loads its CPU-local action
publication, not mutable Line state. Management/state locks serialize owner
installation and withdrawal. Local teardown revokes publication only after
handler and claim completion; withdrawal refuses any remaining published slot.
Management from NMI is rejected before allocation or locks. Unrelated NMI
sources can continue during task-context retirement.

### Why Normal IRQ Claim Uses `PendingIrq`

Hardware claiming and logical resolution are separate steps. `PendingIrq`
retains the raw source and completion cookie. LAPIC timer and LoongArch EXTIOI
claims use explicit logical identities; GIC/PLIC/IO-APIC/MSI Domain claims
retain the mapped Line directly. A lookup miss still completes the original
hardware claim, without inventing an identity fallback for strict Domains.

## Drop / Resource Release

- `PendingIrq::Drop` consumes outstanding host completion on an early return;
  it does not repeat acknowledgement or completion already consumed by the flow.
- The `Drop` of `IrqLifecycleGuard` runs the exit hook saved at entry.
- Deferred handoff registration is internal and permanent during normal runtime.
  `cfg(unittest)` clearing/restoration is fixture support, not owner shutdown.
- `unregister()` removes the regular action and disables the last action's
  source. It retains all descriptor identities, including domainless entries.
  MSI owners release resources with `free_msix()`. Sleepable context is validated
  before withdrawal. After unlink, the task drains retained-Line claim/replay
  admission before node recovery, without action GP preparation or allocation.
  Directory/index retirement continues to use its independent RCU protocol.
- `unregister_nmi()` closes admission and rejects removal while any CPU is
  enabled, prepared or locally published. Once local shutdown is complete,
  it recovers the sole action owner without an action grace-period wait.
  An outstanding `NmiShutdown` claim rejects ordinary unregister.
  Normal/token-based free cannot remove this action. Client destruction occurs
  after management unlock; mapping ownership remains independent.

Management map/enable/disable/register/free/synchronize checked entry points
reject NMI context before ordinary lock acquisition. Their compatibility wrappers
silently return the failure value in NMI rather than entering a potentially
locking logger. `descriptor()` is still an ordinary locked query, not an NMI
observation API. Ordinary and NMI writers share the Line management boundary.
Source stop, restart exclusion and completed local teardown remain mandatory.

Linux `kernel/irq/manage.c::free_percpu_nmi` uses `__free_percpu_irq` to check
the enabled CPU mask and detach the action before freeing it, without an action
RCU grace period. Its source owner completes per-CPU disable/teardown first.
KIRQ follows that ordering but does not copy the C storage layout: one shared
owner plus local publication slots encode the scoped Rust borrow lifetime.
These slots are not Linux's `percpu_dev_id` payloads or separate action owners.
Directory RCU protects Line identity separately; neither Linux's enabled-mask
check nor KIRQ's empty slots alone stop a producer.

## Line Management Transactions

### Storage Lifetime and Destruction

Directory withdrawal, service admission and final storage destruction are
separate events. An `Arc<IrqLine>` preserves allocation identity; it does not
grant permission to install an action or dispatch after withdrawal. Management
must revalidate directory identity after acquiring its potentially blocking
mutex. Registry exclusion still provides that control-plane identity check;
normal dispatch instead checks retirement under the RCU-retained Line's lock.

`IrqLine::virq` uses `klazy::Once<Virq>` to bind the logical number once,
independently of mutable controller metadata. A private candidate starts
unassigned. Mapping commit validates all recoverable conflicts, assigns the
number under registry and Line exclusion, then publishes the indexes. Reusing
that Line for a different number is rejected before publication. Metadata
enrichment and retirement preserve the assigned number.

`IrqLine::logical_irq` uses only non-waiting `Once::get`, so observation needs
neither the mutable Line lock nor an owning reference clone. An assigned number
does not prove directory membership or grant service admission. The initializer
runs only in the serialized commit path and returns an integer; it cannot
allocate, reenter or wait for another initializer. This separates identity
observation from admission. Direct Domain lookup uses this immutable number
without reacquiring the Line lock. Mapping lifetime and MSI reservation tests cover alias
rejection, observation while the Line lock is held, failed preparation,
metadata enrichment and retirement.

`IrqStateDesc::is_retired` is a one-way state under the Line lock. Explicit
disposal sets it before unlinking either index, while registry exclusion still
serializes publication. It is not `teardown_depth`: removing an action leaves
the prepared descriptor available for another registration, whereas retiring
the descriptor permanently closes that allocation. A stale owner cannot become
a new publication candidate. A fresh Line can use the logical number only
through the normal mapping checks; it does not reset the old Line's state.

Registration and removal check retirement before management admission, and
ordinary dispatch checks it before taking running admission or invoking a
flow operation. A retired lookup still completes its original claim, but does
not use the old Line's chip callbacks. Action-install and execution predicates
also reject retired state, including software replay eligibility. Existing
registry identity checks remain necessary; this flag does not replace index
consistency, reader lifetime protection, or device shutdown.

The retirement regression in `ut-tests/msi_reservation.rs` retains an old
owner across disposal, rejects its republication and action installation, and
accepts a fresh identity at the same logical number. `ut-tests/line_dispatch.rs`
injects a retired lookup and verifies exactly one claim completion with no old
flow mask/unmask calls or in-flight credit. This models stale-reference admission.
The runtime paired-index reuse test separately exercises actual RCU publication,
withdrawal, retained borrowed readers and grace-period reclamation.

The final release chain is bounded by the following ownership rules:

- Management lookup retains the Line before acquiring its mutex. The guard
  and its scoped event listener finish before that retained owner is dropped.
  A waiter not yet admitted can outlive directory withdrawal, but cannot
  resurrect the withdrawn descriptor.
- Dispatch and queued replay retain the original Line. Queue membership owns no
  execution credit; popping transfers the Arc to the executor, which admits
  execution only after locked eligibility checks. Resource release rejects
  outstanding aggregate execution or admitted management work. A queued owner may outlive
  withdrawal; retired admission prevents it from accessing handlers or the chip.
- Ordinary actions are removed and drained by the management path. PerCpu
  removal instead requires source-owner quiescence before the unsafe call. Removed handlers
  leave management and short-lock exclusion before final destruction. A
  disposable Line must contain no actions: arbitrary client destructors are
  not part of the empty-Line storage release contract.
- `IrqDrain` retains the Line and clones its `Completion` while waiting.
  Scoped completion registrations retain their borrows until unlinking;
  `DeferredWake` independently retains the completion until notification.
  Dropping the Line's completion owner neither signals nor cancels waiters.
- MSI reservation consumption is explicit and precedes final Line release.
  The reservation's destructor does not stop hardware or return a vector.

`kmutex::RawMutex` destruction does not acquire the mutex or call its unlock
operation. With the pinned no-std `event-listener` 5.4.1 implementation, Event
destruction releases its inner Arc; the listener slab and pending-operation
queue are storage, not a worker that may be run during destruction. Listener
cleanup and queue draining must already have finished with the last mutex
operation. The current allocator releases this storage through IRQ-safe locks
and per-CPU caches, not a sleepable reclamation operation. These dependencies
must be rechecked if mutex internals or allocation backends change.

This permits reasoning about empty storage separately from driver teardown;
it does not authorize final Arc release in NMI context. Nor does it establish
an RCU grace period or prevent stale admission by itself. Any future directory
replacement must preserve these ownership rules and provide both guarantees.

Linux makes the same separation in `kernel/irq/irqdesc.c`:
`irq_desc_free_rcu` schedules `delayed_free_desc`, which releases the descriptor's
kobject; `irq_kobj_release` frees descriptor-owned storage. That callback is not
driver action teardown. X-Kernel's retained Arc and completion machinery differ,
so the Linux callback alone is not proof that every Rust field is safe to drop.

### Transaction Admission

Each Line embeds a `kmutex::Mutex<()>`, built on `lock_api`. KIRQ holds the
ordinary RAII guard on the acquiring task's stack across its transaction.
`kmutex` owns the single sleeping-mutex algorithm also reexported by `ksync`;
`ktask` provides task identity, yield, wait and optional diagnostic operations.
There is no IRQ-specific mutex factory or runtime-held transaction callback.
KIRQ owns transaction policy and context admission. `kiface-task-api` supplies
sleepability and current-task presence observations; `kiface-task-wait-api`
supplies completion waiting. Neither owns the IRQ management mutex.

Ordinary acquisition requires sleepable task context. Explicit startup
try-acquisition requires an installed current task and rejects interrupt/BH/NMI
context, but permits masked IRQs and the startup idle task. Contention performs
no transaction. The guard is not Send: migration of its owning task is allowed,
but transfer to a different task is not.
The caller must not recursively wait on this mutex or acquire it under short
IRQ locks; a startup transaction must not sleep.

After acquiring a retained Line's mutex, management rechecks that the
directory still names that allocation. Registration retries against the current
Line if its selected identity was retired; removal never redirects an old request
to a replacement sharing the same number. Registration never creates a Line.
Explicit startup registration performs try-acquisition only:
contention must not install an action or change hardware/descriptor metadata.
Identity retries allocate no candidate. If withdrawal leaves the directory
empty, registration returns UnknownIrq rather than recreating the retired Line.
Replacement retries remain progress-dependent.

Boolean NMI registration wrappers report rejected admission and concrete
registration errors after releasing management locks, except in NMI context,
where logging itself is unsafe. Startup SGI and PMU callers likewise distinguish
`Ok(false)` rejection from `Err` diagnostics without changing startup policy.

Ordinary registration also checks action admission before descriptor merging
or mapping publication. The preflight borrows the retained Line's locked state
under management, registry and Line exclusion and inspects the same predicates
used by installation. Regular actions require an empty Line and an available
publication sequence; shared actions require compatible ownership and checked
sequence/count increments. Storage has already been prepared; there is no
fixed fanout-capacity gate. Line exclusion remains held through the body, so a
competing control transaction cannot invalidate admission after preflight.
Rejected richer descriptors leave existing
metadata, generations, mappings and action owners unchanged. Diagnostics occur
after helper locks are released and are suppressed in NMI context.

Resolution errors release management admission and locks through the existing
RAII cleanup, allowing a subsequent startup try-registration on the same Line.
Fresh dynamic identity allocation may still advance the allocator. This is not
hardware rollback: configuration now returns `Result<(), IrqDescError>`, and
failure skips the prepared control commit while retaining resolved metadata and
mappings. Read-only preparation and post-apply commit do not recover partial
hardware writes. NMI configuration failure precedes publication of its sole
Line-owned action; source shutdown remains a separate owner responsibility.

`runtime.is_configuration_failed` defaults to false and is set only on a
configuration error, not by metadata-only merges. It leaves configured
generation, enabled state and disable depth unchanged while gating
`can_run_actions`. Queued replay is discarded without execution admission, chip
access, running actions or consuming
pending, and withdrawal cannot unmask the Line. The latch forces configuration
on retry and clears only on successful configuration commit. Recovery enable
forces a hardware enable write even if logical enabled state is already true.
Shared registration captures restoration eligibility before configuration and
forces enable after owner publication only for a logically enabled Line at zero
disable depth. NMI errors latch failure and successful configuration commit
clears it before publication. These recovery paths are implemented; validation
remains pending.

Freeing a missing action or unmatched token returns `Ok(None)` before preparing
disable, beginning teardown or waiting for drain. An existing empty Line keeps
its enable state, disable depth and directory identity; missing Lines remain
absent. A successful removal keeps the existing mask-and-drain sequence.

Admission is recorded under the registry/Line locks so explicit MSI release
cannot remove a Line while its management transaction is active. Free
withdraws the action and masks the last action's source under short locks, then
releases those locks before waiting on the original Line. Management exclusion
remains held through that wait. Removed handlers retain
an owner outside the transaction so their final destructors do not run under
the management mutex or short locks.

An unmatched removal returns without applying a controller plan or changing
enable state, disable depth or teardown state, including on an empty Line
enabled through the legacy startup bridge. Only a successfully withdrawn last
action triggers masking; callers that intend to disable a source use the
explicit disable APIs.

After drain, teardown completion, admission release and conditional surviving
action unmask share one original-Line critical section. No descriptor is
withdrawn, so completion needs neither the registry lock nor an outer IRQ guard
for a registry-to-Line handoff. Enable/disable and dispatch serialize through
that same Line lock. The management mutex still excludes registration through
completion, and ordinary running ownership protects explicit resource release.
NMI duplicate checking and publication use the same retained Line transaction.
Distinct Domains can contain the same hardware number without sharing an action.

Linux `kernel/irq/manage.c` uses `request_mutex` outside descriptor and chip-bus
locks for request/free serialization, releasing the short locks before
`__synchronize_irq` while retaining `request_mutex`. The RAII guard expresses
that ownership directly in Rust. XArray capacity is also prepared before short
locks; commits revalidate identity and publish without allocating nodes.

The IRQ-work kernel unit tests use NOWAIT `KBox` allocation, including fixtures
prepared while local IRQs and preemption are disabled. Nodes retain their heap
addresses through publication and drain. Self-freeing callbacks reconstruct
only pointers transferred by `KBox::into_raw`; chained callbacks cast the
first-field IRQ-work pointer back to its original `repr(C)` owner before
reconstruction. The local queue fixture outlives both parent and child callbacks.
The separate host IRQ-work test keeps its standard allocator ownership model.
