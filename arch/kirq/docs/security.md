# kirq — Security and Reliability Analysis

## Runtime Routing Boundary

This analysis includes `model/affinity.rs`, `backend/affinity.rs` and
`runtime/affinity.rs`, plus the Line and management admission checks they use.
Routing updates validate sleepable context before acquiring management exclusion;
queries reject NMI before taking any ordinary IRQ lock. Both retain the selected
Line and recheck retirement. The query returns values, not a mapping lease.

The controller is trusted to validate source identity, destination encoding and
reachability before writes. An immediate transaction runs bounded and
non-sleeping under Line exclusion; controller locks nest below Line and must not
reenter KIRQ. Success must preserve actual suppression or establish
`KeepMasked`, and return a nonempty effective subset of the supplied ready
candidates. An invalid success is treated as uncertain programming, not
committed as a usable destination.

Deferred requests are accepted only after the backend validates a nonempty
ready candidate set; accepting one writes no RTE and masks no line, so the old
route remains usable. `requested` is the pending target and no second pending
mask exists. `Ok` means accepted; `is_pending` is the only observation that the
request is not yet effective. A replacement request cannot clear a latched
routing failure.

The generic flow layer invokes deferred preparation only for a real hardware
claim and only with the Line lock held. Hardware completion and software-only
replay use separate `IrqDispatch` methods: replay cannot pass an empty closure
into the EOI path. Edge migration completes before EOI. Level preparation masks
before EOI and finish runs after EOI; Remote IRR still set is a retry condition,
not a programming failure. Replay, no-Line completion and software simulation
never advance pending. A pending request may remain indefinitely; there is no
worker, poll, allocation or in-flight counter.

Post-write failure does not imply rollback, hardware masking or quiescence. The
separate routing fault blocks action admission, registration and enable; ordinary
trigger configuration cannot clear it. Disabled-source routing recovery may clear
only that fault after confirming the new route and suppression. Existing disable
depth still prevents flow tails or software replay from reopening the source
before explicit enable. Device shutdown and old-claim completion remain separate
owner/controller obligations.

Ordinary disable retains pending. Last-action withdrawal cancels pending and
sets `requested = effective` (or `None` if no route was confirmed) without
clearing a hardware failure. A retained Line that is re-registered does not
restore the cancelled request, and an old completion cannot advance it.
Retirement clears the Line's routing state; a later Line is independent.

A failed route can accept a deferred retry only while the source is disabled.
Activation and registration preflight admit that exact `pending + disabled`
state without relaxing the general fault gate, but action publication remains
after the masked recovery result: a failed pair returns no action and no token.
Configuration/enable runs the masked prepare/finish pair without a real EOI;
only verified completion clears the fault before unmask. If no confirmed old
route exists and the pair cannot complete, activation fails closed rather than
using `effective == None` as proof that hardware was never active. A confirmed
old route may resume while the request stays pending.

The `cfg(unittest)` helper `tests_irq::retire_reserved_test_line` is solely for
an explicitly reserved ordinary hardware fixture. The fixture must stop its
producer, mask and drain delivery, withdraw its action and exclude other mapping
users before removing both test indexes. The helper rejects busy/enabled, MSI,
private and NMI-owned Lines and releases the returned owner outside short locks.
It uses quiescent fixture removal, not GP-delayed retirement, so the caller must
exclude all borrowed readers of the removed key, not only active actions.
It is not a production dynamic mapping-disposal API or proof of source vacancy.

## Initial Policy And Retirement Notification

An ordinary first action may establish a requested policy only after the CPU
present map is ready. `present_mask()` is present, not ready; it does not write
hardware. If the map is unavailable, registration returns `NotReady` before
publishing an action or history. Fixed owner affinity is independent of that
default. No procfs read or user setter supplies the initial policy.

Action-history publication is a separate core fact: successful regular/shared/
NMI publication sets it, failures do not, and action removal does not clear it.
It is not a filesystem visibility flag; procfs applies its own display policy.

Static user-routing source capability is a separate classification used for
initial inode permissions. It is independent of action presence, per-action
restriction and temporary routing state; every actual write still revalidates
the complete checked setter.

Retirement observers are weak, per-subscription intrusive slots allocated
outside the Line lock. Subscription verifies the original Line is current under
`IRQ_STATE -> Line`. Retirement moves the list under the Line lock and invokes
callbacks only after KIRQ locks are released. A cancellation after that move
does not access links and does not wait for a captured callback. Callback code
does not hold KIRQ locks; dropping a namespace record may indirectly acquire
the Line lock through subscription Drop, which is permitted outside the callback's
KIRQ operation. Only real Line retirement notifies, not ordinary action removal.

## Sparse Handle Identity Boundary

`runtime/handle.rs` is an identity and enumeration boundary, not a new routing
authority. The XArray operation returns the least present Arc at or after the
inclusive start while holding only the array writer lock; KIRQ checks
retirement after releasing it. An entry can be inserted behind the cursor or
retire immediately after observation, so callers must treat successive results
as weakly consistent and revalidate every handle.

Enumeration rejects NMI, hardirq, serving-softirq, BH-disabled task and active
RCU reader before acquiring the array lock. It does not require sleepable task
context, does not allocate and cannot be used from a read scope. A stale handle
query or set is rejected as `UnknownIrq`; it never operates on a same-number
successor. Query holds `IRQ_STATE -> Line`, validates the retained Arc and
retirement, and copies only the locked snapshot. It does not take management
exclusion because it programs no hardware.

Setter admission acquires the retained Line's management mutex before
`IRQ_STATE`, validates the original Arc, checks retirement, and calls
`begin_management`. It releases the Line before `IRQ_STATE`, runs the affinity
transaction under a fresh Line guard, and ends admission before releasing the
management mutex. The `is_managed` membership bit, not the mutex alone, prevents
eligible retirement between short-lock scopes. The controller transaction
remains the existing `apply_locked` path with all source, ready-CPU, failure and
deferred rules unchanged. The handle API provides no credentials and no
capability bypass; procfs and source-owner policy remain outside this module.

## User Routing Protection Boundary

User routing protection is a change-admission policy, not an information-access
rule. A protected ordinary Line remains readable through `irq_affinity`; a
future procfs consumer may expose that snapshot read-only. Only
`check_user_affinity_change` and `set_user_affinity` apply the action policy.

The restriction belongs to the regular action and cannot outlive it. Before
action publication, lookup/configuration failure leaves no policy. Once the
action is published, its existing lifetime is authoritative; a post-publication
platform failure does not create a second policy state. Last-action removal
removes the policy with the existing pending cancellation. Re-registration uses
fresh options. The policy scan runs under Line exclusion and is not a dispatch
path check.

The trusted kernel setter is intentionally not blocked by this user-only
policy. It still enforces fixed, PerCpu, NMI, MSI, source and context rules, and
is not proof of caller ownership. Future generic balancing adapters must use
the checked user path or the user-only promise is incomplete.

`check_user_affinity_change` is a sampled eligibility observation and never a
capability token. The setter repeats the checks after management admission.
Denial changes no affinity snapshot, requested/effective/pending state, disable
depth, action membership or hardware. No-action and teardown reject the user
change entrance; read observation remains unchanged.

PMU ordinary SPI and unavailable-NMI fallback declare the restriction through
their existing registration options. PPI and real-NMI lifecycle paths are
unchanged. PMU initial SPI-CPU association and real delivery remain owner
integration obligations; no CPU0 fallback or fake fixed affinity is introduced.

## NMI Action Lifetime Boundary

`runtime/nmi/action.rs::NmiAction` owns one stable `KBox<Handler>`.
Per-CPU `AtomicPtr<Handler>` slots publish access to that immutable owner;
they are not independent action owners or reader counters. Only PerCpu NMI
registration is supported. Unbound and other flows are rejected before action
publication, so their different shutdown rules cannot bypass this lifetime.

`prepare_local_action` publishes the current CPU's slot only after successful
controller preparation, under continuous Line and CPU/IRQ exclusion.
`NmiAction::with_current` establishes its own `NoPreemptIrqSave` guard and
acquire-loads that slot. Its callback cannot return or retain the borrowed
handler. The true NMI path takes no registry, Line or management lock, clones
no owner, updates no reference count and performs no allocation or wait.

The unsafe boundary is local teardown, not an unchecked remote-stop boolean.
`teardown_percpu_nmi` requires stopped generation, excluded restart and no
outstanding claim spanning the transition. Only after backend success does
`clear_current` revoke the calling CPU's pointer under Line/CPU exclusion.
Failure retains publication and the cleanup obligation. No remote CPU may clear
another CPU's slot. A nested NMI returns before the interrupted local revoker
resumes; ordinary local revocation cannot overtake the guarded read.

For true NMI, `with_nmi_claim` owns CPU/IRQ exclusion across dispatch and original
claim completion. Its directory RCU scope retains the Line through completion.
Normal-IRQ claims for NMI-owned Lines instead complete under the already-held
Line lock without execution admission, a CPU-local owner borrow or handler
invocation. This rejection adds no diagnostics or locks. Drain takes the same
Line lock, serializing with rejected completion without proving hardware
quiescence. Directory RCU is not action reclamation.
Under management and Line exclusion, `take` refuses every nonempty local slot
before recovering the owner. Final destruction occurs outside all management,
Line and CPU/IRQ guards. No action grace period or running RCU service is
required for removal; directory retirement retains its own RCU requirements.

The PMU owner must still stop every boot-static CPU permitted to start, including
unprepared CPUs. A start can pass its gate and pause in its pinned IRQ-off
region before publishing preparation, escaping a participant-only snapshot.
Completed target calls order those old regions before restoration while Closing
remains in force. Empty enabled/prepared/publication slots do not independently
establish that ordering or clear pending hardware. Other source owners must
provide equivalent exclusion. Concurrent CPU hotplug, system stop and general
shared PMU event ownership are outside this protocol.

## CPU-local Control Boundary

NMI removal retains the handler and persistent close admission when a tracked
local slot is enabled. A failed close must not reopen enable admission or clear
another CPU's state. Only a completed local disable updates that slot. This
check does not drain true-NMI readers or replace producer shutdown. Successful
local teardown revokes publication only after claim completion; withdrawal also
checks all publication slots before recovering the owner without an action GP.
Unrelated NMI sources need not be excluded. Local preparation is retained across backend failures;
it cannot be replaced with a different backend or discarded by free.

`runtime/nmi.rs::teardown_percpu_nmi` is an unsafe boundary: its caller stops
local producer activity, prevents restart through teardown/reuse, and excludes
claims spanning the transition. Backend teardown can clear pending state.
Line/CPU exclusion protects the control transaction, not arbitrary device state
or remote handlers. The GIC backend waits for local RWP, rejects active delivery
and checks pending/attribute state; timeout never authorizes reclamation. PMU
terminal quiescence intentionally retains resources and is not this teardown.

`runtime/manager.rs::PerCpuForwarding::set_local_state` is an unsafe boundary.
The retained Arc prevents Line storage reclamation, but is not a producer or
hardware lifetime lease. Callers must keep the action registered, exclude
mapping/configuration/teardown and competing ownership changes, execute on the
owning CPU outside NMI with IRQs masked, and stop/mask the source before Host.
Guest transitions reconstruct logical activity while delivery remains masked.
The callback must not allocate, sleep, reenter KIRQ or retain a borrowed vCPU.
Backend guest-interface metadata is trusted firmware input validated for range,
alignment and non-overlap by the provider; the VMM validates register capacity
after device mapping. Missing capability must not fall back to guessed MMIO.

`runtime/state_local.rs` is protected by the retained Line lock. CPU selection,
controller programming and commit must retain IRQ/preemption exclusion; plans
cannot move to another CPU. Configuration failure closes only that CPU's action
admission. Local callbacks may overlap across CPUs, so handlers must synchronize
shared data themselves. Rejected PerCpu claims complete under the retained Line
lock without execution admission; accepted executions borrow the action through
unlocked completion under the source owner's lifetime contract, without a count
or ordinary running gate. Enabled slots block removal and flow replacement;
shutdown owners must stop and disable every participating source and wait for
every admitted dispatch to return fully before removal.

Generic regular/startup/shared registration, free, synchronous disable and
synchronize reject resolved PerCpu flow with `UnsupportedFlow` before hardware,
action or disable-depth mutation. Local nonsynchronous disable still masks the
calling CPU's source before committing its disabled slot. The Line lock and
IRQ/preemption exclusion cover apply and commit; other CPU slots remain intact.
This transition is neither producer shutdown nor a drain.

The dedicated `try_register_percpu` and `try_register_percpu_startup` contracts
are the ordinary local-action publication entrances: the resolved flow must
be PerCpu (`UnsupportedFlow` otherwise), the Line must have no action, and all
local enabled slots must be clear. Occupancy or an enabled slot rejects with
`Ok(false)` before hardware configuration or handler publication. Post-resolution
rejection may retain merged descriptor metadata; callers must not infer metadata
rollback from a failed registration. Success configures only the calling CPU
and leaves delivery disabled; activation is an
explicit operation on each participating CPU. Task registration requires
sleepable context; the startup variant retains nonwaiting startup requirements.
Existing allocation/configuration error and rejected-owner destruction rules apply.

An empty enabled set is not a quiescence certificate: previously admitted claims
may still exist and source producers may still need stopping. No PerCpu execution
counter or generic drain establishes this missing proof.
These registration checks grant neither NMI/forwarding capability nor source-owned
stop/free exclusion or runtime removal of permanent handlers.

Unsafe `try_free_percpu_irq` validates resolved flow under the retained Line lock and
management admission, before any withdrawal or controller operation. It reuses
the enabled-slot rejection and regular-action-only removal path: shared and
NMI owners cannot be removed through it. It requires sleepable task context.
Before calling, the source owner must stop every participating CPU's source,
exclude restart and new dispatch through removal, and wait for every admitted
dispatch to return, including its handler, original claim completion and tail
work. This includes software dispatch. A failed
shutdown must retain the action and resources for retry. Only that prior exclusion permits recovery of the
detached raw action storage; its separate removal closure uses no `IrqDrain`
and does not wait for PerCpu execution. An empty
enabled set is necessary, not proof that a handler which disabled itself has
returned. Mapping and device-resource lifetime remain separate responsibilities.

The PerCpu guard retains the Line and records unhandled diagnostics after the
original completion callback returns. Neither handler return nor completion
callback return proves that this tail has finished. Source-stop exclusion must
cover full dispatch return within its outer IRQ-masked interval before unsafe free.

The production caller is VMM route-preparation rollback before local activation,
serialized by its preparation mutex. Successful routes and other permanent
timer/IPI registrations have no runtime removal protocol. This boundary follows
the source-owned shutdown distinction of Linux `__free_percpu_irq`; it does not
infer shutdown from enabled bits or provide a broadcast stop operation.

## Source Binding And Dispatch Entry

Mapping, callback readiness, registration/configuration and source production
are separate lifecycle boundaries, in that order. A mapped Line alone cannot
justify starting a device producer. Ordinary disabled registration masks a
pre-enabled empty Line before committing disabled state; a software disable
depth alone would not close the existing controller gate. This is delivery
control, not proof of producer shutdown or drained device activity.

Shared detach must first stop this consumer's producer and exclude restart,
then unlink/drain its action before freeing callback resources. Surviving
shared owners keep their delivery policy. Descriptor mappings remain independent
of action ownership; neither this ordering nor action drain resolves MSI device
quiescence and allocation-release obligations. PerCpu and NMI keep their
separate source-owner exclusion requirements.

`runtime/entry.rs` owns the trap-facing and generic IRQ/NMI entry sequence;
`runtime/manager.rs` owns mapping, registration, control and removal transactions.
Explicit crate-root exports expose these APIs from their owning modules rather
than through a manager export aggregator. This is a visibility and responsibility
boundary, not a new execution-context proof.

The public `handle_irq` and `handle_nmi` raw entries are unsafe. The HAL adapter must
establish the required current-CPU, IRQ-mask and claim conditions before calling
them; the functions do not create hardware-entry provenance or mask IRQs.
Ordinary claim processing retains IRQ exclusion. Only after completion may the
deferred tail temporarily enable IRQs; it stays CPU-pinned and restores the mask
before watching exit. Dedicated NMI entry does not permit those windows.
`runtime/rcu.rs` is the unsafe watching/read bridge and relies on initialized CPU
storage, exclusion of migration and ordinary IRQs, balanced same-stack exit,
and completion of protected work before exit. `PendingIrq` is the sole claim
owner, but its non-Send marker does not itself establish these entry conditions
or certify that an arbitrary completion cookie is a valid controller claim.

`kcpu::excp::{IRQ, NMI}` store unsafe callback pointers and HAL implements that
same contract. Architecture call sites justify entry masks and captured state.
`IntrManagerIf::{dispatch_irq, dispatch_nmi, complete_irq}` preserve unsafe
signatures through the `kiface` facade and provider signature checks. This
makes the cross-crate proof obligation explicit without adding locks or a
second interrupt-entry token. A safe wrapper cannot silently invent it.

`PendingIrq::new` is unsafe: construction transfers a unique acknowledged claim
or a backend-defined no-completion sentinel. The constructor's caller must
retain controller validity and same-CPU IRQ/migration exclusion through explicit
completion or Drop, within the initialized CPU's watching hardirq/NMI entry.
Its non-Send marker prevents thread transfer, not migration
or the release of an outer guard; those remain explicit unsafe obligations.
Completion consumes the original cookie once. No guard can establish that a
fabricated cookie belongs to a real hardware claim. Kernel fixtures instead use
a crate-private, unittest-only simulated claim with no hardware obligation;
their entry helper establishes CPU/IRQ and hardirq context. They do not validate
physical EOI/deactivation by writing fabricated cookies to a controller.

`IrqStateDesc::flow: Option<IrqFlow>` starts at `None` for an unbound candidate.
Only backend selection establishes a production flow: fixed local sources use
PerCpu, and supported external sources use explicit Level, Edge or Fasteoi
capabilities. Unknown or foreign sources return `UnsupportedFlow`. There is no
production Legacy flow or dispatch fallback. An unbound descriptor cannot invoke
actions; an arriving claim still requires its original completion.

`generic_handle_irq` and `generic_handle_nmi` are safe consumers of an owned
claim. The constructor's entry-scope obligation supplies CPU/IRQ exclusion and
watching through consumption, without another token or public unsafe call.
Normal dispatch rejects effective NMI context before taking flow locks; NMI
dispatch establishes its own effective NMI scope. Neither entry authorizes
fabricating a claim or ending its required entry scope early. PerCpu action
borrows require source-owned exclusion of reclamation until the entire dispatch
returns, including when a handler disables its own source.

`cfg(unittest)` fixtures use `IrqStateDesc::test_new` with an explicit fake
Fasteoi chip or another explicit test flow. Those fixtures do not establish a
real backend binding or a production fallback. The legacy boolean enable bridge
and force-enable semantics remain separate control APIs and do not bypass
backend source validation.

## Ordinary Level Admission

`src/runtime/dispatch.rs` retains the original Line through mask/ack, actions
and conditional unmask. Level hooks run under its short lock and must be bounded,
non-sleeping, non-reentrant and ordinary-IRQ-safe; lock order is Line then chip.
Actions run unlocked. A rejected overlapping claim cannot clear another owner's
running state. Disable or teardown during a handler prevents tail unmask.
`src/runtime/resend.rs` never queues level replay: the provider must guarantee
that an asserted source can reassert on unmask. Neither pending nor in-flight
proves device quiescence, posted-delivery drain or completed hardware shutdown.

## Configuration Failure Boundary

`src/backend/platform.rs` propagates the controller's configuration result.
`src/runtime/manager.rs` publishes new regular/shared action ownership only
after configuration succeeds, and commits enable/depth/configured generation
only after successful application. `src/runtime/nmi.rs` similarly excludes
the sole Line-owned NMI action publication on configuration failure. Existing
actions remain owned; failed configuration is not permission to destroy them.
Resolved mapping/metadata and partial hardware writes are not rolled back.
The provider must not restore delivery on failure, and a caller must treat
the error as requiring recovery even when logical enable state is unchanged.
This is not hardware-active drain or a guarantee that every provider
performs readback; those guarantees remain controller-specific.

## irq_work Ownership And NMI Boundary

The trusted platform must return a permanent notification function that never
locks, allocates, logs, lazily initializes or invokes callbacks inline. It must
notify only the prepared current CPU at ordinary IRQ priority. Configuration
and CPU-slot prerequisites are established before NMI consumers are exposed.
Ready is release-published after provider setup; NMI readers acquire readiness
without ordinary IRQ context locks. Failure leaves the node caller-owned.

Unsafe queue callers own the node's stability, initialization and callback
contract. A successful claim serializes producers; the next pointer is private
until release publication. The acquire-detaching local consumer alone reads
links afterward. Duplicate callers need independent external lifetime protection:
AlreadyQueued is not a lifetime lease, and a freed pointer cannot be retried.
Neither producer nor dispatcher may access storage after its ownership handoff.

The static CPU template contains only null pointers, false atomics and None;
its StaticSlotValue implementation introduces no shared owned allocations.
UnsafeCell notification storage is immutable after initialization; backlog is
accessed only by the IRQ-off, non-reentrant local consumer. NMI writers modify
only the atomic incoming list. Callbacks are trusted not to enable IRQs, sleep,
unwind, or wait for work whose execution requires this CPU. A callback failure
is fatal kernel behavior, not a recoverable queue transaction.

Do not run draining from NMI exit after merely clearing software depth: the
interrupted execution may still hold an ordinary lock. The pending ordinary
IRQ waits for restoration of the original mask. Long IRQ-off sections, stopped
CPUs, permanent boot failure and hot-unplug have no reclamation-time guarantee.
Budget 64 bounds callback count, not callback cost or total interrupt utilization.
Malicious or unbounded callbacks can still monopolize the CPU; this is an
internal kernel capability, not an interface for untrusted code.

Tests separate deterministic CAS/ownership/budget interleavings from live
platform delivery. Passing a mock is not evidence of hardware NMI safety; live
tests must include NMI publication while the actual GIC lock is held.

## Interrupt Watching Boundary

`krcu_core::RcuContextIf` is a static contract implemented by `krcu`, including
before GP service startup. Watching lookup must not lock, allocate, look up a
task or schedule; initialized CPU identity is required. It preserves the caller's
IRQ mask, never enables previously disabled IRQs and does not change NMI exclusion.
It is not a consumer read interface or a declaration of service readiness.
The provider's CPU watching reference has permanent storage but does not pin the
caller. Architecture IRQ adapters must exclude ordinary IRQs and migration
until all controller, deferred and lifecycle work finishes. Explicit RCU entry
exit occurs before the HAL can schedule. NMI scopes retain effective NMI state
through watching exit, including nested dedicated/classified paths. Their atomic
CPU-slot count requires initialized CPU storage and strictly nested same-CPU
execution; it takes no lock and never forms an exclusive reference shared with
NMI. Available stack space bounds nesting far below the address-sized counter.
Only coarse classification bypasses the ordinary context lock in NMI; detailed
snapshots are explicitly excluded. Watching alone grants no consumer borrow.
`runtime::rcu::with_nmi_claim` separately enters `krcu::with_interrupt_read`
after NMI context and watching are established, and leaves that read scope only
after original claim completion. Both callbacks must remain on the claiming CPU
without scheduling. An outer `NoPreemptIrqSave` spans both callbacks.
The typed Domain index is RCU-borrowed; the action is locally borrowed through
`NmiAction::with_current`, without locks or refcount updates. Watching alone
never permits reclamation. Directory retirement needs its grace period, while
action withdrawal requires completed local revocation. Synthetic tests verify
pairing, the read scope across completion, and lock bypass, not
hardware NMI entry or actual user instructions. The idle host separately brackets its
architecture wait; its preemption exclusion must span the IRQ return tail,
which must not access protected objects after watching exit. The
controller-classified claim path and its hardware completion remain source-audited;
the synthetic nested-scope test does not execute a real controller claim.
The user-return host restores watching before IRQ dispatch, rather than keeping
an unwatched CPU across the HAL scheduling tail. This does not supply x86 vector-2
NMI dispatch or repair its SWAPGS window; those architecture capabilities are not
advertised by the user-boundary integration.

## Softirq Group Registration Boundary

`open_softirqs` accepts trusted kernel enum/function pairs, not raw or external
data. The protected asset is `SOFTIRQ_ACTIONS`, the fixed action table. Partial
registration could leave a service exposing only one required BH priority; the
mitigation is validation and publication under the same IRQ-safe table lock.
Duplicates, occupied slots and excessive length return false without mutation.
No callbacks run under the lock, and no allocation or new unsafe operation is
introduced. The caller must prepare action dependencies before registration and
publish its own readiness separately. There is no runtime removal/drain guarantee.
Tests cover conflict rollback, successful group installation, repeated registration,
duplicate vectors, empty input and oversized input.

## Effective Context Boundary

`context::is_softirq_context` adds no raw-pointer or external-data input. It uses
the existing context snapshot and hardirq-first classification. This mitigates
executing BH-only callbacks inside a hardirq that interrupted softirq: an active
softirq nesting count alone is insufficient. CPU pinning and pass serialization
remain execution-provider obligations; the query changes no context state.

## Test Audit Scope

The test audit scope covers the guest cases and fixtures under `ut-tests/`, as well as the four host tests in
`tests/host/unhandled.rs`. The guest modules keep their original visibility and
`cfg(unittest)`; the host module uses `cfg(all(test, not(unittest)))`.
The two unhandled-IRQ counting tests use a local `UnhandledIrqState` and do not register real IRQs.
The migration preserves all test bodies, serial markers and cleanup sequences, and does not change production state or unsafe contracts.
`ScopedHardIrqContext` is located in `ut-tests/bottom_half_context_test_support.rs`.
The `with_nmi_context` fixture in the same file requires the caller to initialize CPU/current, exclude ordinary
IRQs and migration, and guarantee that the callback returns normally and does not schedule.
`ut-tests/bottom_half_context.rs`
retains the regression for NMI queries made while a lock is held; `ut-tests/rcu_irq_entry.rs` and
`ut-tests/rcu_context.rs` retain the mainline RCU entry balance regression.
The manager's two hardirq rejection tests use `ScopedHardIrqContext` and also
keep the CPU pinned and local IRQs masked; they must not fall back to a guard that merely increments the context count.
The scoped fixture for softirq is located in `ut-tests/bottom_half_softirq_test_support.rs`;
both still provide test-time access through the original `test_support` module.

## Trust Model

The test helper `ScopedHardIrqContext` holds `NoPreemptIrqSave` itself: it first pins the CPU and
disables local IRQs, then increments the hardirq depth; on drop it first restores the context counters, then restores IRQs and
preemption. Setting only the per-CPU counters is not enough to simulate hardirq, otherwise a task switch triggered by a lock release may
cause other tasks to inherit a wrong interrupt context. This helper is compiled only in the unittest configuration.

Tests that replace the deferred executor use `ScopedDeferredExecutor` to save and restore the original entry,
covering both the registration state and the IRQ-tail atomic fast path. Merely clearing the entry at the end of a test would permanently cut off the already started
softirq execution, leaving completion events that real devices have already set unhandled; the scope restores the entry before local IRQs and preemption are
re-enabled, and does not change the production registration policy. The `NoPreemptIrqSave` held by this helper
masks interrupts only on the CPU running the test, while the hook slot is globally visible, so real IRQ tails on other CPUs
may still concurrently call the registered test entry; therefore assertions on call counts must tolerate extra calls, and only verify
that the calls triggered by the test itself did happen.

`kirq` does not expose a user-space ABI directly. It trusts the kernel callers to provide handlers and descriptors that
conform to the execution-context constraints, and treats the following boundaries as external dependencies:

```text
Kernel drivers / subsystems
   │  IrqDesc + Arc<dyn IrqHandler>
   v
kirq
   │  IntrManagerIf
   v
Platform interrupt controller backend
   │  MMIO / CPU priority / EOI / IPI
   v
Hardware interrupt controller
```

Default assumptions:

- The platform backend correctly implements the configure, enable, dispatch, complete and
  notify ordering of `IntrManagerIf`;
- The platform backend maps normal dispatch to an OS-visible virq, and does not dispatch a raw vector as a virq
  on an MSI mapping miss;
- The MSI backend correctly implements the vector allocation, free and message
  composition of `MsiBackendIf`;
- IRQ handlers may run in hardirq context;
- NMI handlers satisfy the stricter pseudo-NMI execution constraints;
- The lifecycle hook and the deferred executor are each installed by exactly one explicit owner; the current deferred
  executor owner is softirq.
- `driver_os_api` / devres is the driver-framework adaptation layer and must not become a dependency of `kirq`. The conversion from driver resources to the
  kernel IRQ core is the responsibility of the `driver_os_api_xkernel` provider.
- `kbuild_config::NR_CPUS` bounds CPU-local control, NMI publication and
  participant storage. Configured capacity is not proof of CPU readiness,
  online membership or completed source shutdown.
- `IrqAction`, `IrqDescRuntimeState` and `IrqActionSnapshot` are `kirq`-internal state
  boundaries. Drivers, devres and irqchip backends must not reuse or modify these structures directly.
- Generic IRQ line/source subscriptions are not provided. Device subsystems
  own readiness and waiter wakeup; they may submit prepared deferred work.

## External Boundaries / Attack Surface

1. **Device interrupt input**
   Real hardware or emulated devices can trigger IRQs. `kirq` does not trust device behavior; it performs descriptor lookup and handler dispatch only by the interrupt id
   claimed by the platform backend.

2. **Firmware/platform metadata**
   The device tree, ACPI or platform static tables generate `IrqDesc`. Incorrect metadata may cause
   misconfigured trigger, polarity, domain or hwirq.

3. **Platform controller backend**
   GIC/APIC/PLIC backends are responsible for MMIO, priority, EOI/deactivate and IPI sending. `kirq`
   constrains them through the trait contract, but does not directly verify MMIO side effects.

4. **MSI backend**
   Backends such as x86 APIC are responsible for encoding the controller-local vector and the CPU/APIC destination into a
   device-visible MSI message. `kirq` only stores the virq mapping and the message carrier, and does not parse the specific
   architecture format.

5. **Kernel handler / hook callbacks**
   The regular handler, NMI handler, lifecycle hook, waiter wake and deferred
   executor are all callbacks provided by the kernel. The IRQ core only manages the call order and does not verify whether a callback sleeps internally
   or takes the wrong lock.

6. **Trap adapter boundary**
   `kirq` does not register a CPU trap handler. `khal::irq` must establish the correct trap-entry execution constraints before entering `handle_irq()` /
   `handle_nmi()`.

This module does not directly handle user pointers, DMA buffers, file contents, network packets or FFI.

## Unsafe Code Inventory

The private `handle_level_irq`, `handle_edge_irq`, `handle_fasteoi_irq` and `handle_percpu_irq`
boundaries in [dispatch.rs](../src/runtime/dispatch.rs) require `entry` to lock
the exact supplied `line`, with `virq` identifying that same Line. Their callers
derive both identity and owner from lookup and obtain `entry` from `line.lock()`;
numeric IRQ equality alone is insufficient. Accepted ordinary execution establishes
running admission on that Line before snapshot capture or unlock. The separately unsafe
`IrqDispatchGuard::snapshot_actions` requires that exact retained-Line lock and
either live ordinary running admission or a PerCpu execution protected from
withdrawal by the source owner. Every cursor access must precede the end of that
execution; the returned cursor borrows the guard, which alone does not prove
source shutdown. Rejected execution keeps the lock through completion and
captures no action cursor or execution credit.

`runtime/manager.rs::try_free_percpu_irq` is an unsafe reclamation boundary.
Before calling, the owner must stop every participating CPU's source and wait
for all admitted dispatches to return, including software dispatch, their handlers,
original completions and post-completion tails;
restart and new dispatch must remain excluded until return. Its enabled-slot
check is necessary but cannot validate this lifetime proof. Failure to complete
shutdown requires retaining the action. No PerCpu execution wait or GP supplies
the missing exclusion.

The index boundaries are in `runtime/index.rs` and `runtime/state/registry.rs`:

The registry is a private child of `state`, so its atomic metadata/index
transactions can access Line identity and lifecycle fields without widening
those fields to sibling modules. The state facade re-exports required operations
internally, not a root-level registry mutation API. Child visibility is not
synchronization: registry exclusion and the original Line lock still protect
identity checks, flow/descriptor commit and paired index updates. This boundary
does not establish action-reader quiescence or hardware shutdown.

- Prepared XArray insertion runs in allocator-initialized boot/task context,
  never IRQ/NMI/BH context. NOWAIT preparation precedes registry/Line exclusion.
- `withdraw_msi` requires successful RCU submission preflight before exclusion.
  MSI publication allocates both empty retirement records before publishing the
  Line; failure returns the unpublished reservation for backend rollback.
  Release consumes those records only after validation, without allocating.
  Every returned retirement record must be submitted after restoring that same
  context, before invoking arbitrary backend callbacks.
- Both owning indexes retain the original Line. Withdrawal marks it retired
  before removing either entry, so a racing old lookup cannot gain admission.
- IRQ readers clone their Arc within an interrupt RCU scope. Numeric NMI queries
  only borrow and return a scalar; they cannot run a final Line destructor.
- Retired tree nodes and index-owned Arcs are released by RCU, not leaked.
  Remaining owned Arc holders extend lifetime beyond the grace period.
- The unittest-only quiescent fixture removes a synthetic slot without pruning
  radix nodes. It requires exclusion of same-key borrowed readers and writers;
  it is not used to prove the production retirement protocol. The runtime
  paired-index reuse test instead exercises actual withdrawal and RCU callbacks.

`arch/kirq/src/bottom_half/context.rs` uses `kcpu_slot` cells. Its unsafe items are the `SpinNoIrq::new` constructor in the `IRQ_CONTEXT_STATE` slot template and `NmiContextGuard::enter`: CPU slots must be initialized, and the caller must stay on this CPU with ordinary IRQs excluded until the guard is dropped; nested entries must unwind first.

- `current_state_snapshot()` contains no unsafe code. It uses `IrqSave` plus a `NoPreempt` that is released without a scheduling check, and reads the context snapshot through `SpinNoIrq`.
- `with_current_state_irqoff()` contains no unsafe code. It updates the current CPU's context depth through `SpinNoIrq`.

Object exclusivity comes from `SpinNoIrq`; `NoPreemptIrqSave` only pins the CPU and masks local IRQ reentrancy. The hardirq guard and softirq IRQ-tail hot paths use crate-local `*_irqoff` helpers and expect the caller to have already masked local IRQs and pinned the CPU. The helpers still create a nested `NoPreemptIrqSave` (or `IrqSave` for snapshots); on already-masked paths this nested save/restore is near-free. Ordinary task queries therefore cannot read or write the same per-CPU slot concurrently with the local hardirq guard.

`arch/kirq/src/bottom_half/softirq.rs` accesses the per-CPU atomic pending mask `SOFTIRQ_PENDING` through safe accessors such as `with_current_irq_disabled` and `current(&guard)`; it has no unsafe pending-mask access. The mask is an `AtomicUsize` used to publish and acquire pending bits between IRQ and softirq.

Other related unsafe boundaries are located outside this module:

- The architecture trap entry enters the `khal::irq` adapter through `kcpu` assembly and the trap dispatch macro;
- The platform IRQ backend performs MMIO, priority mask and the necessary assembly barriers in `drivers/platform/irq`;
- `kiface` binds the platform implementation to `IntrManagerIf`.

Therefore the main security responsibility of this module is not local memory safety, but keeping the context, lock order and completion
semantics correct.

## Memory Safety Invariants

The ordinary Level/Edge/Fasteoi action proof is traced in
[Ordinary Action Lifetime Proof](design.md#ordinary-action-lifetime-proof):
[action_set.rs](../src/runtime/action_set.rs) owns node/link invariants,
[dispatch.rs](../src/runtime/dispatch.rs) binds cursors to execution admission,
[state.rs](../src/runtime/state.rs) serializes admission and its zero predicate,
and [manager.rs](../src/runtime/manager.rs) retains writer exclusion through
unlink, drain and node recovery. A new action reader must participate in this
admission protocol; an unrelated RCU read scope is not sufficient protection.
Do not clear detached next links or reconstruct exclusive node ownership until
drain. Do not permit another membership writer to reclaim a reachable successor
during that interval. Acquire/Release ordering and publication ceilings alone
cannot discharge either lifetime obligation.

PerCpu uses the claiming CPU's local admission predicate. Rejected claims
complete under the retained Line lock without borrowing an action. Accepted
claims run and complete unlocked, with no execution count or running gate;
the source owner protects the raw action borrow until the entire dispatch returns.
Accepted Level/Edge/Fasteoi and replay own `is_running`. The ordinary drain
predicate is `!is_running` under the Line lock; pending queue entries own no
execution admission. Ordinary idle is not a PerCpu completion certificate, irqchip-active
query, hardware quiescence proof or substitute for producer shutdown. Removing
the action GP does not remove these obligations or alter XArray directory RCU,
the CPU-local NMI owner protocol, or the generic `krcu-list` contracts.

- Regular/shared handlers live in the retained Line's private stable-node action
  chain. Dispatch captures a cursor and publication-sequence ceiling under the
  Line lock, with snapshot lifetime bound to the dispatch guard. It clones
  neither a list Arc nor handler owners. Callbacks borrow actions after releasing
  short locks; action traversal requires no RCU read scope.
- Accepted Level/Edge/Fasteoi handlers and replay establish running ownership
  before snapshot capture and unlock. Rejections hold the Line lock through
  chip operations and original completion without execution admission. Normal-IRQ
  claims for NMI-owned Lines also complete under the held Line lock without
  admission or action access. PerCpu rejection also completes under the held
  Line lock without a count. Accepted PerCpu borrows instead rely on completed
  source-owner shutdown before unsafe removal, not ordinary execution drain.
  `free_irq()` drains admitted claim and running replay obligations before
  recovering the detached node and returning handler ownership. This execution
  drain covers every action borrow and original completion; directory RCU is a
  separate lifetime boundary, not the action reclamation mechanism.
- The internal `WakeThread` return classification is inert future state in the current milestone and must not schedule a task,
  raise a softirq or create workerqueue work.
- `PendingIrq` cannot be sent across threads. Its remaining host completion
  must be consumed on the claiming CPU, exactly once; the integration's CPU
  exclusion, not the non-Send marker alone, preserves that affinity.
- `IntrManagerIf::complete_irq(cookie)` must consume only the original claim's
  remaining host-owned completion obligation, on its claiming CPU. The backend
  must obey the source's acknowledgement/deactivation protocol rather than infer
  it from trigger type alone. A no-op tail or guest-owned deactivation is not a
  hardware-quiescence certificate; forwarding shutdown needs a separate proof.
- The `virq -> IrqStateDesc` and `domain -> hwirq -> virq` mappings in `IRQ_STATE` must stay consistent.
- A descriptor with a domain may only use a domain registered in the static registry of `domain/mod.rs`;
  an unknown domain must return `IrqDescError::UnknownDomain` and must not create a mapping the data plane cannot resolve.
- A firmware interrupt may only go through the controller/domain association registered in `domain/fw.rs` by exact
  `FwNode` identity. `FwNode` equality compares both the backend identity and the node id;
  it must not degrade to matching by compatible string, `IrqController` family or numeric id.
- `IrqDomainXlate::translate()` only decodes the binding cells into an `IrqHwSpec`; it must not allocate a virq,
  select another controller or call back into `kirq`. The registry lock must be released before calling translate and
  `try_map`.
- `register_irq_domain()` is boot-only and non-idempotent: a second association for the same controller,
  an `Existing` target that already has an association, an `Existing` target that does not exist, and a failed new-domain allocation must all report an error
  explicitly and must not silently overwrite or fall back to another root. The association is stored inside the owning `IrqDomain`; there is no
  separate fixed-capacity association table; during boot it is serialized by the registration lock.
- The controller family of an `IrqDomain` is domain metadata. `Existing(id)` in `register_irq_domain()`
  preserves the kind the domain already has; `New(kind)` initializes the kind exactly once only when a new domain is created;
  no path may overwrite the type of an existing domain. `try_map_fwspec()`
  fills the descriptor with this authoritative value, avoiding contradictory domain and controller-kind combinations.
  Each firmware controller instance gets its own domain; a second instance of the same family gets a newly allocated
  dynamic domain and no longer shares a namespace with the first instance. The family is only metadata and does not mean any
  driver is entitled to operate that instance: a driver that accesses a single initialized controller (currently GIC/PLIC) must admit only
  the domain id registered for that instance and must not substitute family equality for instance ownership.
  Truly supporting a second hardware instance also needs a backend that saves register bases/chip data per instance.
- `register_irq_domain()` may only be called by the boot control plane and must not be called from an IRQ/NMI/BH handler.
  `New` additionally requires the allocator to be ready and uses a non-sleeping `NOWAIT` allocation, returning
  `NoMemory` on failure; `Existing` allocates no memory.
- The choice of the old root is an explicit decision of boot wiring and is not inferred in generic registration from the controller family,
  registration order or a boolean parameter. `IrqDomainTarget::Existing(id)` only verifies that the target exists and has no
  firmware association yet; `IrqDomainTarget::New(kind)` allocates a new instance domain. The family and identity
  policy belong to the selected domain itself and are not override parameters passed separately by the caller.
- The identity fallback belongs only to explicitly selected well-known roots and is a compatibility policy for legacy numeric numbering.
  Dynamic domains created by `create_domain()` always have `is_identity = false`: a hwirq that is not explicitly mapped
  must fail to resolve and must not degrade into a same-numbered global virq that picks up a Line from another source.
- The two storages of `LineIndex` in `runtime/index.rs` must be distinguished: the borrowed view
  (`Storage::Shared`) is a stateless, countless view that may be constructed temporarily; only the test-private
  registry's `Storage::Private` holds `count`. Count updates on publish/revoke apply only to private
  objects, and a borrowed view must not increment or decrement the count because of a temporary query, otherwise deletion underflows or wraps around.
- `try_map_fwspec()` must distinguish "no declaration" (the backend returns `Ok(None)`), a corrupt declaration
  (`MalformedSpec`/`InvalidHwirq`), an unsupported binding (`UnsupportedSpec`),
  a controller not registered (`ControllerNotReady`) and rejection by the IRQ core (`Map(...)`);
  it must not fall back to a default controller or to family inference.
- `MappedIrq::desc` must come from the stored descriptor returned by `commit_resolution`, so that the
  trigger/polarity/controller obtained from translate are not overwritten by consumer defaults.
- `try_disable_irq_nosync()` must stay lookup-only; an unknown IRQ returns
  `IrqDescError::UnknownIrq` and must not create a descriptor or
  domain mapping on the hardirq-safe disable path.
- RCU protects borrowed Domain/directory entries across withdrawal. Writers
  prepare capacity before exclusion, publish initialized Lines under their lock,
  and submit withdrawn ownership only after restoring task context.
- MSI allocation must expose only the virq and `MsiMessage` to the caller; the backend-local vector,
  APIC id and affinity choice must not leak to `driver_os_api` or a generic driver.
- NMI dispatch must not depend on `IRQ_STATE`, otherwise a pseudo-NMI may interrupt a normal IRQ path holding the lock and
  deadlock on itself.
- The lifecycle exit hook must pair with the hook captured at this enter, and must not be lost mid-handling because the global hook
  was cleared. The lifecycle denotes the IRQ-on trap/preempt-off lifetime and is not a
  hardirq-depth boundary; an irqchip later classifying the claim as NMI or spurious does not change this pairing.
- The hardirq depth must be maintained in pairs by `HardIrqContextGuard` and exited after irqchip dispatch (including its
  generic IRQ/NMI helper and controller completion) returns and before the deferred executor
  runs.
- `LocalBhGuard` must hold `NoPreempt` for the entire BH-disabled lifetime and must not be moved across threads.
  This way enter/drop and the outermost drain are all bound to the per-CPU depth of the same CPU.
- The deferred executor may run only after irqchip dispatch returns and the generic hardirq depth has exited;
  if there is an IRQ/NMI claim, its generic helper must already have completed controller completion. The executor
  cannot retain `PendingIrq` or alter claim completion ownership.
- The softirq pending bit must not be acquired via `load + store(0)`; the runner must use atomic
  `swap(0, Acquire)`, and raise must use `fetch_or(Release)`, so that work raised again during a handler
  is not overwritten.

## Thread Safety

### normal IRQ state

`IRQ_STATE` serializes directory and Domain writers; each Line lock protects
descriptor, action and admission state. Normal dispatch retains the original
Line through an interrupt RCU lookup, then takes only its Line lock. Driver
handlers run after releasing that lock. Completion uses the retained identity,
never a fresh number lookup.

Domain and logical indexes own the same Line. Reverse conflict checks use its
descriptor and the actual Domain binding instead of a redundant metadata map.
A reader racing withdrawal either misses or retains the old Line and observes
its retired gate; number reuse cannot redirect that retained owner. Numeric-only
IRQ/NMI translation uses an RCU borrow. Early boot and control reads use the
XArray writer lock, without assuming a current task or a running RCU service.
Waiting teardown APIs (`disable_irq()`, `synchronize_irq()`, and `free_irq()`)
are valid only from ordinary kernel context. They reject hardirq, softirq, and
BH-disabled callers before waiting on aggregate execution, because those contexts may be
the same execution path that must exit to make progress.
Callers must also avoid holding locks or resources that their IRQ handler may
acquire while waiting; otherwise teardown can wait for a handler that is blocked
behind the caller's own lock.

All descriptors survive action removal and empty-claim completion, including
domainless fixed Lines. Domain-bound Lines retain both owning XArray entries.
Mapping ownership is not inferred from
handler count, and handler removal cannot undo preparation. General non-MSI
runtime disposal is not provided yet: these descriptors retain registry lifetime.
MSI resources retain their separate explicit release transaction.

Retaining a Line is not admission to use its hardware binding. Management
waiters retain the original Arc and recheck registry identity after locking;
dispatch retains identity and execution admission; queued replay retains identity
and queue membership only, acquiring admission at eligible execution.

Logical identity is assigned through `klazy::Once<Virq>` after recoverable
commit validation and before index publication. Only the registry/Line-locked
commit path initializes it, with a non-reentrant integer-only closure. Readers
use non-waiting `get`, never initialization or waiting operations; they must
still retain or borrow the Line through an appropriate lifetime owner.
The assigned number survives retirement and does not authorize controller or
action access. Reusing one Line under another logical number is rejected.
The stored integer adds no destructor work or client-owned resource.

Explicit disposal marks the Line retired under its short lock before removing
the indexes. This one-way state rejects republication of the same allocation,
new management/dispatch admission and action installation or execution. It is
not cleared by normal action teardown, and a failed release must not set it.
The original claim still requires completion when a retired lookup is rejected;
no old-Line flow operation may be invoked on that path. Retirement does not
itself provide RCU protection or physical interrupt drain.
Unittest-only checkpoints observe partial publication and withdrawal without
releasing the writer or Line lock. Their interrupt observers perform lookups,
not handler admission, controller operations or sleeping waits. Nested NMI
observers return scalar identities only. The cross-CPU interrupt-reader fixture
keeps borrowed storage alive until its read scope exits and drains callbacks
only after releasing the reader. These checks do not establish device shutdown
or hardware late-message ordering.
Final storage destruction must not contain a client action or an unconsumed
hardware-release obligation. The management path returns removed handlers
outside its locks instead of delegating their destructors to storage retirement.
Completion registrations and deferred wakes preserve their own borrow/owner
lifetimes; dropping a Line is not a wake or cancellation operation.

Empty-Line deallocation relies on the non-sleeping allocator and on completed
mutex-listener cleanup, not merely on the presence of an Arc. The pinned
no-std Event implementation releases its inner storage without acquiring the
management mutex. Changing that dependency, adding a destructor-bearing Line
field, or making allocation release sleepable requires a new release-context
audit. IRQ-safe allocator locks are not NMI-safe: a future NMI lookup must not
become the final owning release of a retired Line. Numeric NMI lookup therefore
borrows through RCU and returns only the logical number. Normal dispatch retains
an Arc and checks retired admission under the Line lock.

MSI allocation publishes a fresh `(MSI_DOMAIN, backend_vector) -> virq`
mapping under `IRQ_STATE`, retaining its exclusive reservation in the Line.
Ordinary `unregister()` does not delete MSI descriptors or mappings. Final
`free_msix()` first validates unused, logically disabled state and reservation
ownership, then withdraws the descriptor and reverse mappings before returning
the vector.
An actionless Line can still be enabled by the legacy force-enable API; that
state is rejected without changing its mappings, control state or reservation.
Logical disablement is necessary but does not prove device-message quiescence,
which remains the resource owner's obligation.
Pre-withdrawal rejection preserves the caller's cleanup identity. Backend
release consumes valid ownership and has no recoverable post-withdrawal failure.

### Line-owned NMI Action

`src/runtime/nmi.rs` installs one `KBox<Handler>` in
`IrqLine::nmi_action: NmiAction`; `src/runtime/nmi/action.rs` owns its raw-pointer
publication and recovery invariants. NMI registration stores no second Arc in
the ordinary fanout list. The locked ownership marker contributes one action to resource
accounting and excludes ordinary/token free and shared installation. Owner
allocation precedes short locks; rejected ownership is released after guards.

Only successful local preparation release-publishes the current CPU's pointer.
The private scoped reader retains CPU/IRQ exclusion and cannot leak a borrow.
True NMI lookup retains the typed Domain index through original completion and
cannot fall back to another namespace on a miss. Normal-IRQ claims for this Line
complete under the already-held Line lock without execution admission and never borrow
the local owner or invoke it with the logical-IRQ ABI, regardless of local enable
state. Neither path performs action-owner reference counting. True NMI entry,
including IRQ-on NMI, is unchanged; no supported production backend required the
removed software/test handler compatibility behavior.

Removal validates sleepable task context, enters Closing under Line exclusion
and refuses enabled, prepared or published local slots. The private unsafe
`NmiAction::take` checks publication under management/Line exclusion, preventing
concurrent installation or preparation. It recovers ownership only after each
local revocation has completed; no callback service or action GP is involved.
The management helper returns the owner outside its guards for destruction.

`src/runtime/nmi/shutdown.rs` retains the original Line and unique shutdown
claim. Ordinary unregister and replacement cannot consume its action while
the claim exists. Finish rechecks identity and local obligations; rejection
returns the handle for retry. Dropping an unfinished handle abandons only the
claim and leaves Closing and the action intact. Forgetting it prevents retirement.
Handle Drop takes the Line lock and must not run in NMI. Shared callback captures
must be destroyed before recovering unique ownership for finish.

The participant snapshot records enabled/prepared slots, not all possible
starters. The source owner must stop producers, exclude restart, complete
target-CPU disable/teardown and exclude pending/active claims across reuse.
A queued callback is not an acknowledgement, and its acknowledgement is not
completion of the surrounding IPI claim. The PMU owner's all-start-capable-CPU
stop pass also drains old pinned start transactions before restoration.
Local controller success does not prove remote shutdown. Conversely, an empty
slot is meaningful only because unsafe local teardown already established
quiescence. Unrelated NMIs need not stop. Synthetic fixtures do not establish
physical NMI delivery or hardware late-claim isolation.

Shared action removal unlinks the matching token from the chain; it does not
leave a vacant slot. Tokenless removal accepts only non-shared membership before
changing the action count or oneshot state. A rejected removal must preserve
the shared owner, its token, dispatch eligibility and the Line's enabled state.

NMI rejection in checked normal management APIs happens before any ordinary
IRQ lock. Compatibility wrappers do not log this rejection in NMI. This does
not make arbitrary handler destructors NMI-safe: passing ownership into an
invalid-context call can still drop the rejected input on the caller's stack.
The caller remains responsible for the destruction context of its inputs.

### Lifecycle hooks

`IRQ_LIFECYCLE_HOOKS` stores only two function pointers. `IrqLifecycleGuard::enter()` copies a snapshot of the hooks
and stores the exit hook in the guard. Callbacks are not executed under the lock. The hooks run within the trap/preempt-off
lifetime, but are not a source of hardirq-depth state.

### IRQ context state

`IRQ_CONTEXT_STATE` is a per-CPU plain counter state. Queries and guard updates both protect the current CPU slot with
`NoPreemptIrqSave` or an explicit IRQ-off calling convention. Public queries
establish their own guard; the hardirq and softirq IRQ-tail hot paths reuse the
local-IRQ-masked + CPU-pinned context already established by the caller.

Hardirq, serving-softirq, and BH-disabled are all treated as non-sleepable / interrupt-like
states. `interrupt_context_level()` explicitly distinguishes BH-disabled from ordinary task context, so that
diagnostics do not misreport BH-disabled as a sleepable context. A future IRQ thread is a sleepable context;
it must establish a separate execution-context boundary and must not reuse `is_in_interrupt_context()` as a thread-state marker.
Diagnostics for abnormal contexts use atomic counters and rate-limited warnings, so that erroneous states in hot paths do not cause a log storm.

### Deferred executor

`DEFERRED_EXECUTOR_HOOKS` stores only one hardirq-exit function pointer, and uses `SpinNoIrq`
to protect the register/clear control plane. The runner hot path reads the hook from an atomic function-pointer slot
and does not acquire the registration lock. The executor runs in the normal hardirq exit / NoPreempt context; it must not sleep,
must not depend on the current process/task context, must not recursively call the deferred runner, and must not acquire sleepable locks that may be held by the
interrupted context.

A global reentry guard is not added for now, because it would suppress legitimate hardirq-exit
handoff on other CPUs under SMP; softirq pending state uses a per-CPU pending mask and context gating.

### Softirq state

`SOFTIRQ_PENDING` is a per-CPU atomic bit mask. The softirq runner obtains a batch through an atomic exchange,
the action table is snapshotted under `SpinNoIrq`, and handlers execute outside the lock. The runner
keeps local IRQs masked at entry/exit, but temporarily enables local IRQs inside the action loop and disables them again before checking for new
pending bits. `ktask` provides a per-CPU `ksoftirqd/N`, but `kirq` only requests a wakeup of the current CPU's daemon through
`SoftirqDaemonIf`; it does not create tasks or block. Raising a softirq in hardirq, active
softirq, and BH-disabled contexts only leaves the pending bit set; after each batch of actions returns, the direct runner, if work is still pending and the current task needs to be rescheduled, or if the restart limit is reached,
keeps the pending bits and wakes `ksoftirqd/N`; only restart exhaustion records the restart-limit counter.
`SoftirqDaemonIf::needs_resched()` only reads the task request, does not clear it, and does not perform scheduling inside the IRQ-disabled
runner. It returns false when there is no task or preempt is not enabled.
`raise_softirq_irqoff()` expects the caller to have already disabled local IRQs; if a debug/UT path detects misuse,
it records a diagnostic and temporarily disables local IRQs before accessing the per-CPU pending/context state.
BH-disabled state is managed with the RAII `LocalBhGuard`; the guard holds `NoPreempt` and uses type
constraints to prevent dropping across threads, thereby preventing the wrong per-CPU depth from being decremented on a different CPU.

## Threat Analysis

| ID | Threat | Trigger | Impact | Mitigation |
|------|------|----------|------|------|
| T-01 | IRQ metadata misconfigured | Firmware or platform supplies wrong trigger/polarity/domain | Interrupts lost, retriggered, or unmaskable | `IrqDesc` carries metadata explicitly, and platform configure consumes only the normalized description |
| T-02 | handler executes under registry/Line locks | Dispatch invokes an action before unlocking | Callback reentry deadlocks | `dispatch_claim` and `replay` snapshot eligible actions, then release short locks before `run_actions` |
| T-03 | Mapping miss becomes identity dispatch | Strict Domain has no mapping | Wrong handler or completion | Direct Domain lookup preserves strict misses; only declared identity Domains may use the logical directory |
| T-04 | NMI interrupts an IRQ writer | A writer holds registry or Line exclusion | Same-CPU deadlock or premature handler destruction | NMI borrows the RCU-protected index and CPU-local action without locks; CPU/IRQ exclusion spans claim completion, local teardown revokes access, and removal refuses published slots |
| T-05 | claimed IRQ not completed | normal control flow returns early or forgets the explicit complete | The controller thinks the interrupt is still active and later interrupts misbehave | The claimed IRQ guard's Drop compensates with completion |
| T-06 | lifecycle enter/exit not paired | The hook is cleared/re-registered mid-IRQ | hardirq nesting or deferred work state corrupted | The guard saves a snapshot of the exit hook at entry |
| T-07 | hook/handler sleeps or holds the wrong lock | Caller violates the hardirq/NMI context constraints | deadlock, latency spike or wrong scheduler state | rustdoc and the design document state the callback context explicitly; the IRQ core provides no sleepable dispatch |
| T-08 | IPI request observed by the target CPU before the memory publication | Platform `notify_cpu` lacks ordering guarantees | Cross-core protocols such as TLB shootdown miss work | The `IntrManagerIf::notify_cpu` documentation requires publish-before-notify |
| T-09 | deferred executor runs before completion or within hardirq depth | executor work lengthens the active IRQ, affects EOI/deactivate, or is misjudged as hardirq | interrupt tail latency, controller state anomalies or wrong context diagnostics | The irqchip's generic helper completes before returning; `handle_irq()` then always runs drop hardirq context, deferred, lifecycle exit |
| T-10 | Global reentry guard suppresses other CPUs | On SMP, another CPU is skipped while one CPU's executor runs | lost handoff opportunity | No global guard is currently implemented; per-CPU pending/reentry will be used later |
| T-11 | per-CPU context non-atomic state accessed concurrently by a local IRQ | An ordinary task query is interrupted by an IRQ, which reads/writes the same CPU slot | inconsistent context snapshot or data race | Public paths use `NoPreemptIrqSave`; hardirq/softirq IRQ-tail paths use only the already masked/pinned `*_irqoff` helpers |
| T-12 | softirq pending acquisition overwrites a new raise | The runner clears pending with `load + store(0)` | softirq work lost | raise uses `fetch_or(Release)` and the runner uses `swap(0, Acquire)` |
| T-13 | BH-disabled guard dropped across CPUs | The guard lifetime does not pin the current CPU or can move across threads | per-CPU BH depth leak, underflow, or softirq drained on the wrong CPU | `LocalBhGuard` holds `NoPreempt` and is not `Send` |
| T-14 | driver reads the current APIC id to build the MSI message | The MSI message is encoded by the device layer rather than the IRQ backend | Interrupts delivered to the wrong CPU under SMP/affinity/x2APIC | `MsiBackendIf::compose_msi_message()` is the sole message composition boundary |
| T-15 | MSI vector and handler lifecycles mismatched | The handler is still registered when the device releases the MSI resource, or handler unregistration deletes the MSI mapping too early | After vector reuse: stale handler, stale mapping or backend vector leak | `unregister()` retains the MSI descriptor; `free_msix()` checks whether the descriptor is still in use and completes the final cleanup; driver teardown should revoke the handler before releasing the MSI resource |
| T-16 | descriptor conflict silently merged | hwirq/domain/virq/trigger/polarity/controller metadata inconsistent | Handler bound to the wrong IRQ or wrong platform configure | `try_resolve_desc()` / `try_merge()` return `IrqDescError` at runtime; two definite trigger/polarity/controller values never overwrite each other |
| T-16b | Firmware controller identity matched by guess | compatible, family or numeric node id used in place of `FwNode` | Interrupts routed to the wrong controller with multiple instances or backends | `domain/fw.rs` associates strictly by the `FwNode` backend pointer + node id; a duplicate association fails explicitly |
| T-16c | Firmware cell interpreted by the wrong owner | The IRQ core guesses the trigger, or the controller returns a logical IRQ | Wrong binding, duplicate decoders or controller overreach | `IrqDomainXlate` only produces `IrqHwSpec`; `kirq` owns domain association and logical mapping; translate errors are returned classified as `IrqXlateError` |
| T-16d | New instance inherits the old instance's numeric identity fallback | A dynamic domain still carries `is_identity = true` and the second instance's hwirq is unmapped | An unmapped hardware number resolves to the same-numbered global virq and hits a Line from another source | `create_domain()` always generates a non-identity domain; only the reserved root keeps numeric compatibility; an unmapped request returns `None` |
| T-16e | Driver admits another instance's domain by family | A singleton GIC/PLIC backend accepts any domain of the same family but operates the initialized controller through `desc.hwirq` | The second instance's interrupts are configured into the first controller's registers | `is_gic_domain()`/`current_plic_domain()` accept only the domain id held by the current instance; family equality does not mean instance ownership |
| T-16f | Borrowed index view treated as an object that holds a count | Each modification temporarily constructs `LineIndex::shared()`, and a successful delete runs `count -= 1` | panic under overflow checks; count wraps in release | The borrowed view is stateless and countless; only the test-private registry's `Storage::Private` holds a count; the global index has regression tests for MSI allocation and release |
| T-17 | Missing MSI mapping becomes a raw logical IRQ | No `(MSI_DOMAIN, vector)` entry | Wrong handler selected | MSI dispatch uses the strict Domain Line index; a miss never falls back to the raw vector |
| T-18 | Unregistered domain mapped successfully | The control plane accepts an unknown `IrqDomainId` | The driver gets a virq that is never reachable | `validate_resolution_domain()` (via `resolve_prepared_locked()`) returns `UnknownDomain` before changing state |
| T-19 | BH-disabled misjudged as ordinary task context | diagnostics distinguish only hardirq/softirq/task | A sleepable future callback may run in a BH-disabled interval | `InterruptContextLevel::BhDisabled` and the docs state its non-sleepable semantics explicitly |
| T-20 | context misuse log storm | underflow or repeated local_bh_disable in hardirq | Logs drown real faults and lengthen IRQ handling time | The warning counters are kept, while the log emits only the initial samples and power-of-two samples |
| T-21 | poll/future IRQ wake treated as a future IRQ thread target | A later threaded IRQ directly reuses the poll/future wake bridge | Missing thread lifecycle, dev-id teardown, oneshot mask and scheduler ownership, causing a dangling wakeup or wrong teardown | The docs state that poll/future IRQ wake is only a temporary waiter notification; threaded IRQ, softirq and workerqueue must form independent ownership inside `kirq` |
| T-21b | Duplicate work execution | Concurrent claims or cancellation race | Corrupt callback state or leak in-flight ownership | Transfer: `kworkqueue_ng` owns synchronized claim/cancel/finish; `kworkerpool_ng` owns worker occupancy. |
| T-21c | Blocking from BH context | A BH callback invokes a synchronous wait | Deadlock or invalid scheduling | Mitigate: kirq exposes context predicates; workqueue waits reject non-sleepable contexts. |
| T-21d | Work storage released while borrowed | Producer or callback outlives storage | Use-after-free | Transfer: caller-owned pinned Work requires explicit drain; safe owned handles defer reclamation to the normal service. |
| T-21e | BH callback executed in the wrong context | Direct service entry outside target CPU softirq | Invalid per-CPU access or blocking | Mitigate: BH service validates CPU and softirq context; kirq owns softirq entry/exit. |
| T-21g | Callback dependency cycle | Callback waits on work that cannot progress | Bounded pool starvation or deadlock | Accept residual risk: optional self-wait checks catch local cases, not arbitrary dependency cycles; callers must avoid those cycles. |
| T-22 | oneshot runtime field mistaken for implemented behavior | Later code changes only the flag without installing the mask protocol | A oneshot IRQ cannot mask reentry or is unmasked wrongly | Review requires the oneshot mask and thread identity to be implemented together within the `IrqDescRuntimeState` owner |
| T-23 | threaded slot saves a task/worker pointer early | Thread lifecycle, cancellation, CPU affinity and exit synchronization not established | After handler return a freed object is woken, or a sleepable path is triggered in hardirq | The current `IrqThreadSlot` carries no target, and an action with a slot is not executed synchronously by the current dispatch path; a later milestone must first define ownership and teardown ordering |
| T-24 | Incomplete action/admission snapshot | Dispatch re-resolves state across locks | Withdrawal redirects an old claim or callback | Directory RCU protects lookup while dispatch retains the original Line's Arc; its lock grants admission and captures the action cursor and publication ceiling together. Traversal borrows the dispatch guard after unlocking; concurrent unlink may bypass unseen nodes. |
| T-25 | Stale controller updates | Concurrent register/enable/disable/free after directory release | A late unmask reopens a disabled or retired Line | The original Line lock spans state changes and controller application; teardown rejects enable and management admission pins identity through drain |
| T-26 | Driver needs to clear pending before enabling the IRQ, but request enables immediately | A complex device's DMA ring/MMIO state is not yet initialized after the request handler | The interrupt handler observes uninitialized device state | `try_register_disabled()` installs the handler with disable depth `1`, and the driver explicitly calls `try_enable_irq()` after initialization |
| T-27 | Handler remains active after teardown | Free recovers a detached node before execution admission drains | A handler accesses released MMIO/DMA state | Free preserves detached storage and next links until retained-Line drain completes; only last-action removal gates and masks the whole Line. Survivors remain eligible during non-last removal. Completion is published under the Line lock and waiters are woken after unlocking; IRQ completion never withdraws directory membership. |
| T-28 | Waiting IRQ API called in interrupt-like context | `disable_irq()`, `synchronize_irq()` or `free_irq()` called in hardirq/softirq/BH-disabled | The current CPU waits for itself to exit, causing deadlock or long spinning | `try_*` APIs gate with `is_in_interrupt_context()` and return `InvalidContext`; compatibility wrappers log a warning and return a failure value |
| T-28b | IRQ sync wait provider has no sleepable current task | Scheduler not yet initialized, or the waiting API called from non-task context | Illegal scheduling or the waiting API falsely succeeds | `TaskBlockingContextIf::is_sleepable_context` checks for a sleepable current task before the action is removed and on failure returns `IrqDescError::InvalidContext`; after releasing the control lock, a failure-free wait is performed with a `CompletionWait` stack registration, eliminating the registration-failure branch after removal |
| T-29 | Wrong shared action removed | Adapter token mismatches action identity or is reused before synchronization | Wrong owner released or old handler uses freed state | Line-local tokens increase monotonically without reuse; free-by-token rejects PerCpu, unlinks first, then drains ordinary running ownership before returning the owner. |
| T-30 | shared and non-shared actions mixed | An ordinary `register()` followed by registering a shared action, or the reverse | dispatch/teardown semantics unclear, possibly closing the line wrongly | `try_register()` allows only an empty line, and `try_register_shared()` rejects a line that already has a non-shared action |
| T-31 | Device readiness published too early | Driver wakes readers before publishing data | Readers observe incomplete data | Device subsystems own data publication and wakeup ordering; IRQ core does not broadcast readiness |
| T-32 | Membership changes during removal wait | Another writer reclaims a successor still reachable from a detached node | A dispatch follows freed storage | The retained Line's management mutex serializes membership through unlink, execution drain and recovery. Detached next links remain intact until drain. Only last-action withdrawal adds whole-Line teardown; non-last survivors remain dispatchable. |
| T-33 | handler cannot identify the IRQ that fired | The shared handler callback has no IRQ parameter and can only infer from external state | Shared line, diagnostics or generic wrapper misjudge the event source | Normal IRQ and NMI handlers receive logical `Virq`. NMI obtains it from the retained Line's published identity; controller domain/hwirq selects the Line and is not the callback argument. |
| T-34 | Missing last-action mask | A teardown waiter is mistaken for a surviving action | The source remains unmasked without a handler | Mask the last action's source independently of teardown credits; retain descriptor ownership through and after drain |
| T-35 | Failed registration during teardown still modifies descriptor/mapping | The registration path resolves/publishes first and checks the teardown gate afterwards | Returns failure but leaves merged metadata or a new domain mapping | Registration performs a lookup-only teardown gate first, and on a hit does not enter resolve/publish |
| T-36 | Teardown wait window re-enables an action-less line | After the last action is removed and masked, another CPU calls `enable_irq()` / legacy force-enable | After the descriptor is deleted the platform line is unmasked again, leading to unhandled IRQs or a storm; or the failure path leaves merged metadata/domain mapping | `try_enable_irq()` resolves an existing descriptor lookup-only and rejects an ordinary enable when `teardown_depth != 0` or `action_count == 0`; legacy force-enable first checks for an existing teardown descriptor before resolve/publish |
| T-37 | Waiting teardown holds a lock the handler needs | A task context holding a driver lock calls `free_irq()` / `synchronize_irq()`, while an old handler snapshot on another CPU waits for the same lock | Teardown waits for the handler and the handler waits for the teardown caller, forming a deadlock | Public rustdoc and the design document state that while waiting one must not hold locks the handler may acquire or resource locks that prevent the handler from exiting |
| T-38 | Global event subscription slows IRQs | Unrelated global subscription queries added after fanout | timer/IPI/device IRQs contend on a global lock | The generic IRQ waiter table and its queries have been removed; each device subsystem manages its own readiness notification |
| T-39 | Hardware NMI acknowledged through the wrong path | Backend misses the special IAR1 INTID or fails to read `ICC_NMIAR1_EL1` | NMI is not acknowledged/dispatched or is treated as an unknown IRQ | Backend classification selects NMIAR1 acknowledgement and generic NMI dispatch, including IRQ-on NMI. Normal-IRQ claims for NMI-owned Lines complete without invoking the handler; NMI registration provides no ordinary-handler fallback. |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure mode | Local effect | System effect | Detection/mitigation |
|------|----------|----------|----------|-----------|
| F-01 | duplicate regular handler registration | The second action is rejected | Device initialization fails but the existing handler is not overwritten | `register()` returns `false` with a warning |
| F-02 | duplicate shared action token release | The token does not match the target line or has already been released | The target handler is not removed or the release fails | `free_irq_action()` deletes an action only by line-local token and returns `None` on failure |
| F-03 | unknown IRQ dispatch | No handler to call | The interrupt is completed, but the functional event is lost | A warning records `Unhandled IRQ` |
| F-04 | NMI action unavailable | No locally published handler | Source event is not serviced | Complete the original claim without invoking a handler or entering a potentially locking NMI logger |
| F-05 | platform backend does not complete or completes twice | Controller state abnormal | Possible interrupt storm or interrupt stall | The internal `completed` flag of the claimed IRQ guard prevents duplicate completion |
| F-06 | trap adapter does not disable preemption | The `handle_irq()` execution context does not meet the constraints | lifecycle and scheduling boundaries confused | The `khal::irq` adapter creates `NoPreempt` |
| F-07 | deferred executor recursively calls the runner | The executor re-enters itself | Stack growth or uncontrolled hardirq tail | The API documentation forbids recursion; the softirq runner gates with serving-softirq context |
| F-08 | hardirq context guard not exited in pairs | hardirq depth leak | softirq/BH gating misjudges for a long time | guard RAII + context diagnostic counter |
| F-09 | softirq handler leaks context state | The handler modifies hardirq/softirq/BH depth and returns | Later context decisions wrong | Compare snapshots before and after the handler, warn and restore |
| F-10 | `local_bh_disable()` misused in hardirq | BH depth increased in hardirq | The direct runner is delayed, possibly exposing a caller context error | Record `bh_in_hardirq_warnings`; the outermost drop does not drain in hardirq |
| F-10b | BH-disabled raise mistaken for being able to schedule the daemon immediately | After raising a softirq in a BH-disabled critical section, the daemon is woken preemptively | Same-CPU direct-drain semantics weakened, extra wakes and test races | `wake_softirqd_if_needed()` treats BH-disabled as an interrupt-like context and keeps only pending; the outermost BH enable or the restart fallback handles it afterwards |
| F-10c | `raise_softirq_irqoff()` misused by an IRQ-enabled caller | The caller does not meet the `*_irqoff` precondition | The precondition for `irq_context_snapshot_irqoff()` reading per-CPU context state is broken | Record `irqoff_misuse_warnings` and temporarily disable local IRQ before accessing pending/context state |
| F-11 | descriptor mapping conflict | `try_*` APIs return an error, or compatibility APIs warn/fail fast | Prevents wrong metadata from entering `IRQ_STATE` | `IrqDescError` carries the conflicting fields |
| F-12 | descriptor uses an unknown domain | `try_*` APIs return `UnknownDomain` | Device initialization fails but no unreachable IRQ is created | The domain registry boundary check precedes state changes |
| F-13 | controller completion cookie mismatch | Completing the wrong active IRQ or a no-op | Interrupt lost, retriggered, or priority/deactivate state leaked | `PendingIrq` owns the original opaque cookie; generic IRQ core returns only that cookie to the backend. The backend must establish that it represents a valid claim. |
| F-14 | spurious vector treated as a real claim | The generic IRQ/NMI helper is called without having claimed an interrupt | Wrong handler call or completion | The irqchip calls the generic helper only after a valid claim; the unified IRQ-exit deferred hook may run, but `resolved_irq` stays `None` |
| F-15 | Reintroducing generic IRQ wake broadcasts | New consumer subscribes to a line rather than its data source | IRQ core acquires unrelated global state and obscures readiness ownership | Consumers register device actions and publish readiness through their own subsystem |
| F-16 | future `WakeThread` executed synchronously | A `WakeThread` return treated as handled primary work | Wrong wakeup or execution of sleepable work when the future thread is unimplemented | The current `WakeThread` classification stays inert; the threaded IRQ milestone must implement thread ownership explicitly |
| F-17 | action runtime field drift | `generation`, MSI marker, action list or oneshot pending inconsistent with the descriptor | Wrong MSI free decision, shared/threaded diagnostics or cleanup decision | descriptor merge/update goes uniformly through `IrqStateDesc` methods, and newly added fields must be maintained synchronously in that owner |
| F-18 | disable nesting not paired | Multiple disables followed by a single enable, or unclear semantics of the legacy boolean API | IRQ line masked for a long time or unmasked too early | `disable_depth` is maintained explicitly in `IrqDescRuntimeState`; `enable(spec,false)` maps to nosync disable and `enable(spec,true)` maps to depth-aware enable |
| F-19 | Free overlaps dispatch | Another CPU borrows an action or retains claim/replay admission | Caller resources are reclaimed before the handler finishes | `free_irq()` waits for retained-Line admission to drain before node recovery and handler return; it does not delete the descriptor or wait for an action GP |
| F-20 | synchronize_irq without a prior disable | The device keeps firing new interrupts while waiting | The call may wait for a long time or observe only the current window | The documentation specifies that `synchronize_irq()` does not mask the line; teardown uses `disable_irq()` / `free_irq()` |
| F-21 | Releasing one action of a shared line shuts down the whole line | free-by-token does not check the remaining actions | Other shared devices' interrupts lost | `free_irq_action()` produces a platform disable plan only after the last action is removed |
| F-22 | enable_irq on an unknown or action-less descriptor | An explicit enable creates a descriptor or unmasks a line with no handler | Unhandled IRQs or wrong platform state | `try_enable_irq()` does not create a descriptor; an unknown IRQ returns `UnknownIrq` and no action returns `NoIrqAction` |
| F-23 | enable_irq overlaps free_irq wait | teardown has masked the line but the descriptor has not yet been cleaned up | The platform line is reopened after the action is deleted | `teardown_depth` makes `try_enable_irq()` return `TeardownInProgress` |
| F-24 | Runtime NMI mechanism unavailable | The initialized platform mode is `None`, including unsupported GICv2 configurations | An NMI action cannot obtain local preparation/publication | AArch64 PMU registers an ordinary PerCpu action instead and skips NMI preparation; periodic NMI retirement remains unsupported. This is ordinary registration, not normal-IRQ delivery to an NMI-owned Line, which is rejected without invoking its handler. |
| F-25 | Firmware controller not registered when a consumer maps | Controller driver has not completed initialization, or the wrong node was registered | Mapping fails or a node is attached to the wrong domain | `try_map_fwspec()` returns `ControllerNotReady`; association uses exact `FwNode` identity and registration is explicit after controller init |
| F-26 | Malformed or unsupported firmware binding | Truncated cells, unknown type, out-of-range hwirq | Wrong hardware source configured or silent fallback | `IrqXlateError` distinguishes malformed, unsupported and invalid hwirq; the consumer maps that to `MalformedSpec`/`UnsupportedSpec`/`InvalidHwirq` |
| F-27 | Reusing a mapping with conflicting attributes | Same `(domain, hwirq)` declared with a different trigger or controller | Handler configured for the wrong electrical mode | `try_merge()` returns `TriggerConflict`/`ControllerConflict`; `MappedIrq::desc` never overwrites stored metadata from translation |

## Fault Management

- Registration failure is returned as a `bool`, and duplicates are checked before changing state wherever possible.
- Query-style APIs use `Option` to express unmapped or strict domain miss.
- Unknown claims still complete. True NMI lookup/action misses do not enter
  the potentially locking diagnostic path.
- A handler panic is an unrecoverable kernel error path and does not rely on the claimed IRQ guard's Drop to keep
  maintaining the controller state.
- platform enable/disable is called only when a platform binding is needed; a plain dynamic virq does not forcibly touch the
  controller.
- A platform spurious vector does not run the generic handler fanout; it still goes through the unified IRQ-exit
  deferred hook, consistent with Linux IRQ entry/exit pairing, and `resolved_irq` is `None`.
- Context misuse diagnostics keep the complete counts in counters, while warning output is rate-limited.
- `disable_irq()`, `synchronize_irq()` and `free_irq()` return `InvalidContext` through the
  `try_*` APIs in interrupt-like context; compatibility wrappers log a warning and return a failure value.
  `disable_irq_nosync()` is the only non-waiting disable API usable on hardirq-like paths.
- `free_irq()` masks the last non-MSI action's source and waits for
  in-flight claims to finish before node recovery. All Lines remain owned
  independently of their actions,
  including domainless descriptors. During the wait,
  `teardown_depth` rejects registration and enable. `unregister()` is a
  compatibility wrapper. Waiters must not hold locks needed by the handler.
- MSI descriptors bypass ordinary platform configure/enable. Allocation failure
  returns `None`; composition/publication failure explicitly returns the private
  reservation to the backend. `free_msix()` validates unused state and reservation
  ownership before withdrawing both owning indexes. RCU retirement submission
  precedes backend consumption of the reservation. Backend release
  has no recoverable failure after withdrawal. NMI calls are rejected before locks.
- The lifecycle hook registration is single-owner; if multiple consumers are needed later, it should be explicitly upgraded to a
  notifier chain rather than letting multiple subsystems overwrite the same global hook.
- No global IRQ waiter table is consulted after action fanout. Descriptor-local
  in-flight completion is published under the retained Line lock and woken
  outside it. This path does not acquire IRQ_STATE or remove a descriptor,
  including an empty domainless Line prepared before handler installation.
  Explicit MSI release still checks aggregate-idle execution before directory
  withdrawal; action removal never disposes a descriptor.
- deferred executor registration has a single owner. `softirq::init()` installs the default owner on the boot path;
  duplicate registration returns `false`, and an empty executor also returns `false`; the runner returns
  `NoExecutor` when none is registered.
  Registration and execution handoff are crate-internal. Clearing is
  `cfg(unittest)` only; hook-record mutation and atomic publication share the
  registration lock. A reader may already have loaded a callback, so test
  clearing is not a callback-drain or production unload guarantee. Fixtures
  restore the boot owner rather than leaving device deferred work stranded.
- `kworkng` publishes per-CPU normal/BH services only after preparation and
  action registration. `kworkqueue_ng` owns queue/work state;
  `kworkerpool_ng` owns normal managers and workers. `kirq` supplies context
  predicates and BH dispatch, not work storage or scheduler accounting.
- Synchronous work waits require sleepable context. Before freeing device state,
  callers must stop producers and drain owned delayed timers and work. Queue
  closure alone is not timer shutdown. See `task/kworkng/docs/security.md` for
  safe-handle deferred reclamation and borrowed-storage obligations.

## Privacy Analysis

`kirq` does not process user data. The information it handles is mainly IRQ numbers, controller metadata,
handler references, and CPU targeting policy. Logs may expose IRQ numbers or hwirqs; before being made visible to ordinary users,
they should be controlled by procfs/syslog permission policy.

## Known Limitations

- Descriptor-backed unclaimed action fanout uses saturating event counters
  and power-of-two warning sampling. Accounting is serialized by the existing
  Line lock used for in-flight completion; formatting and logging occur after
  releasing that lock. No directory lookup or lock is needed for accounting.
  It never masks an IRQ or changes disable depth. Any visited shared action claiming
  an IRQ prevents an unhandled count, including a faulty always-claiming
  action; these diagnostics cannot establish that hardware was acknowledged.
- Unhandled reports are bounded per descriptor lifetime, not per unit of
  time or across repeated descriptor recreation. Counters combine CPUs and
  action-set changes within that lifetime. A suppressed tail has no public
  query or teardown report and is discarded with the descriptor. Unknown-source,
  missing-descriptor, no-action admission and unsupported-thread warnings remain
  separate, so this change does not contain all IRQ-related log storms. A
  traversal emptied by concurrent unlink merges no claim and is counted as
  unhandled; it does not promise exact admission-time membership.
- Ordinary shared actions use a private node chain bounded by allocation availability and
  checked publication-sequence/count arithmetic, without a fixed fanout cap,
  per-action statistics, procfs output, debug-shirq injection or a Linux-style
  dev_id ABI.
- MSI/MSI-X currently provides only a minimal allocation/message bridge and does not support interrupt remapping,
  managed IRQ, affinity rebalance or dynamic post-enable MSI-X table entry management.
- The lifecycle hook is currently a single-owner model, not a notifier chain.
- The deferred executor is softirq's single-owner hardirq-exit handoff, not
  a workqueue or IRQ thread. Workqueues use explicit publication through
  `kworkng::DefaultQueue::Normal`.
- `/proc/interrupts` still has no per-IRQ/per-CPU statistics output.
- Readiness wakeup belongs to the device subsystem, not generic IRQ dispatch.
- `PER_CPU` is metadata, not a per-CPU enabled set or a teardown barrier. Resolved
  PerCpu sources use non-nesting CPU-local enable state, including through the retained
  legacy boolean enable bridge. Before freeing
  shared handler resources, callers need a separate proven local shutdown on
  every relevant CPU, protected against re-enable. Remote IPI submission and
  original-Line software drain do not supply that proof. The current boot-only
  activation paths must not be interpreted as a runtime shutdown protocol; see
  the [per-CPU lifecycle gate](design.md#per-cpu-source-lifecycle).
- `synchronize_irq()` uses descriptor-local completion and `TaskCompletionWaitIf`
  to wait for aggregate idle. Completion is only a wake source: recheck
  `!is_running` under the original Line lock after waking. PerCpu is rejected.
  Future threaded execution must define its ownership within this boundary;
  existing context restrictions still apply.
- Synchronous waits and dispatch guards retain the original `Arc<IrqLine>` and
  update only its locked state. Number reuse or directory withdrawal cannot
  redirect them to a replacement. Cleanup compares Line identity before removing
  a directory entry. Ordinary removal requires idle running state; PerCpu removal
  requires completed source-owner exclusion before the unsafe call;
  retaining storage neither grants management admission nor establishes hardware
  quiescence. Management transactions acquire management exclusion before
  registry and Line locks. NMI publication uses the same order, without global
  table/publication locks. Wait/wake callbacks run outside all short locks.
- Ordinary running admission covers its required controller completion/tail work,
  not merely the handler snapshot. Empty Level/Edge/Fasteoi arrivals instead
  complete under the held Line lock without a credit, as do rejected normal-IRQ
  claims for NMI-owned Lines and rejected PerCpu claims. Accepted
  PerCpu executions rely on source-owner exclusion until full dispatch return.
  Ordinary drain and resource
  release check state under that same lock, so they cannot pass a locked rejected
  completion already in progress. Lookup misses still
  complete their original claim without acquiring a later Line by number.
  The private consuming `IrqDispatch::complete` centralizes this normal-return
  ordering; it is not a type-level ban on dropping an admission early or a
  recovery guarantee after a kernel panic. Synthetic completion tests exercise
  software ordering and retained identity, not real controller delivery.
  This is a software drain boundary, not hardware pending-state quiescence.
  Selected Fasteoi Lines use running ownership for admitted software replay,
  while pending queue entries own no execution admission.
  Supported providers and the outstanding flow
  prerequisites are enumerated in [Controller Flow Contracts](design.md#controller-flow-contracts).
- Disable releases registry exclusion before calling the controller, but keeps
  the original Line state locked through the mask update, without acquiring the
  global control lock. The backend must be bounded, non-sleeping and must not
  reenter KIRQ registry/control/dispatch.
  An outer local IRQ/preemption guard preserves the entry mask across the
  non-LIFO registry-to-Line handoff; removing global mutual exclusion must not
  remove this local context protection.
  Controller-register locks are below Line state in this path; providers must
  release those locks before calling a generic dispatch entry. Drain waiting
  and the retained Line's final release happen outside all short locks.
- Enable retains the selected Line before releasing registry exclusion and
  keeps its state locked through configuration and unmask. Both controller
  operations obey the same non-reentrancy and bounded-execution contract as
  mask. Both normal and legacy enable reject a missing descriptor before
  allocation, mapping publication or controller application. A surviving
  reverse mapping alone cannot authorize reconstructing its missing Line.
  Legacy force-enable may merge compatible metadata into an existing Line only
  while its binding is not in use; otherwise descriptor and flow must remain
  unchanged. It does not replace explicit descriptor preparation. Final owner
  release stays outside short locks.
  All registration paths, including explicit startup variants, reject missing descriptors before
  preflight, configuration or action publication. Rejection retains no handler
  owner and does not create a reverse mapping. Neither mode implicitly allocates
  a descriptor. An existing identity is revalidated
  after management acquisition. NMI source shutdown remains caller-owned;
  old handler borrows and claims must finish before successful local teardown
  revokes publication; action removal performs no RCU wait.
  The outer local NoPreemptIrqSave guard masks IRQs before registry and Line acquisition
  and restores context only after both guards are gone. It protects the non-LIFO
  registry-to-Line handoff independently of cross-CPU mutual exclusion.
- Final enable admission checks teardown under the same Line guard as disable
  depth changes and controller application. A stale earlier observation cannot
  authorize enabling a tearing-down Line. Legacy enable checks teardown before
  merging metadata and retains that guard through resolution and application.
  Resolution validates identity, descriptor compatibility and mapping conflicts
  before mutation. Rejection does not publish metadata or mappings, although
  selecting a fresh dynamic identity may advance the allocator.
- Regular/shared registration relies on management admission for membership
  stability, not a directory lock around the controller call. Registration
  preflight, metadata resolution, installation and configuration/unmask share
  one Line guard. Callbacks borrow the locked state and must not reacquire
  registry/publication/Line locks or wait. The caller retains
  the handler across the entire transaction so rejected or discarded cloned
  owners cannot run final client destructors under Line or management locks.
- Ordinary Level/Edge/Fasteoi action withdrawal validates sleepable context before unlinking, retains
  the detached node through last-action masking, then releases Line exclusion.
  It drains original claim/replay obligations before recovering the node and
  returns handler ownership only after management cleanup. No action retirement
  allocation or GP preparation occurs. The now-unproducible public
  `IrqDescError::RetirementUnavailable` variant was deliberately removed, not
  repurposed for another failure. Index retirement retains its separate failure
  rules and is not an action-lifetime prerequisite.
  Only last-action withdrawal adds whole-Line teardown. Non-last shared removal
  leaves survivors eligible under normal flow rules throughout the drain;
  continued survivor traffic may prolong the Line-wide drain. Membership remains
  serialized by the management mutex. `IrqDrain` and source-owner shutdown
  obligations remain in place; execution drain is not hardware shutdown.
  Shared ownership and rejection of ordinary removal for NMI-owned Lines are unchanged.
  An absent action or unmatched token cannot change the Line's enable state,
  disable depth or teardown state, or invoke a controller plan. Successful
  action removal is required before the last-action masking transaction.
- `runtime/action_set.rs` is the ordinary membership/count/token audit surface.
  Its writer holds the Line lock inside management serialization. Unlink preserves
  detached next links until ordinary execution drain. PerCpu removal instead
  requires every admitted dispatch to have returned fully before entry.
  A failed removal leaves membership
  unchanged. Publication sequences and shared keys never wrap or get reused.
  A snapshot borrows the dispatch guard, not a list Arc or individual handler
  owners. Acquire traversal pairs with Release publication and invokes callbacks
  without short locks. Detached nodes are recovered in the removing task after
  ordinary drain or prior PerCpu source-owner quiescence, never by an asynchronous
  destruction callback.
  Concurrent unlink can bypass an unseen action; exact admission-time membership
  is not guaranteed. A later append has a sequence above the captured ceiling
  and cannot extend that pass. Edge replay captures a fresh ceiling each pass.
  NMI ownership remains separate and contributes only its owner flag to the count.
- NMI management releases the directory before controller configure/mask,
  retaining the original Line lock through the bounded operation. It releases
  all guards before final owner destruction, without an action GP wait.
  Source shutdown and completed CPU-local teardown are prerequisites, not
  consequences of owner withdrawal.
- `flush_work()` / `cancel_work_sync()` cover ordinary work items. The legacy
  delayed-work product API and timer adapter have been removed. Automatic delayed
  retry, full Linux concurrent-flusher handoff, CPU hotplug drain and rescuer
  semantics remain unsupported.
- `WakeThread`, `IrqThreadSlot`, and oneshot pending are only foundation state. They are not yet bound to
  a scheduler task, workqueue, mask protocol, thread teardown, CPU affinity, or
  per-action statistics; an action with a future-only thread slot cannot be
  dispatched in the current core, and must not be downgraded to a synchronous hardirq primary.

## Audit Checklist

- Whether a newly added IRQ API explicitly states its hardirq/NMI/normal-context constraints.
- Whether a newly added dispatch path releases `IRQ_STATE` before calling the handler.
- Whether a newly added action return classification keeps `NOT_HANDLED` as unhandled, rather than as a threaded wake.
- New `WakeThread` handling must have independently defined IRQ-thread ownership.
- Whether a newly added threaded IRQ design first defines thread slot ownership, cancel/teardown ordering,
  oneshot mask/unmask, CPU affinity, and sleepable-context boundaries.
- Whether a newly added shared IRQ design maintains the action list, dev-id identity,
  and fanout result aggregation inside `IrqDescRuntimeState`, rather than merely adding a flag or counter.
- New wake notification must follow device-owned readiness rather than restore
  generic IRQ line/source subscriptions.
- Whether a new NMI feature avoids reading or writing normal IRQ shared state.
- Whether a new platform backend satisfies the `PendingIrq` completion and `notify_cpu`
  publish-before-notify contract.
- Whether a new platform backend performs the final EOI/deactivate of a level IRQ only after
  generic handler fanout.
- Whether a new context check treats BH-disabled as non-sleepable rather than as ordinary task context.
- Whether new diagnostics keep counters, avoid allocation, and do not flood warnings without bound on the hot path.
- Whether a new MSI backend exposes only virq/message and does not let drivers read the APIC id or a raw CPU vector.
- Whether new softirq deferred-execution features enter through the hardirq-exit handoff in `bottom_half/deferred.rs`;
  a standalone irq_work must be dispatched by a normal IRQ and must not be consumed at NMI exit.
- Whether the deferred executor runs after completion and before lifecycle exit.
- Whether the deferred executor still keeps a single owner and introduces no `ktask`/`kprocess` dependency.
## Management Mutex Contract

The management guard is held directly by KIRQ and is not Send; it must not
be unlocked by a different task. NMI is rejected before ordinary task-context
queries; waiting acquisition rejects every context rejected by the scheduler's
sleepability predicate. Startup try-acquisition never parks, including when
running on the initialized idle task. It is not permission to sleep inside a
masked or otherwise non-sleepable transaction. Preparing a Line must happen
outside short locks, and an unlocked mutex's final destruction must not wait.

These checks do not validate IRQ directory membership, hardware quiescence or
application dependency cycles. KIRQ revalidates retained Line identity after
acquisition and records admission before releasing the registry lock; explicit
resource release excludes admitted Lines. A stale remover cannot touch a
replacement. Ordinary admission completion releases only the membership pin,
regardless of the returned value. Only an explicit successful action-removal
completion releases admission and its teardown credit together under the
original Line lock. Error and rejected-removal paths cannot infer retirement
from a general transaction result. Only release of a teardown credit can
trigger flow restoration; non-last shared removal must not unmask over a running
survivor's flow. It never looks up or
withdraws a descriptor, and therefore needs no registry lock or non-LIFO guard
handoff. The caller retains the Line owner and management mutex until completion
returns. Handler owners are returned outside locks for final destruction.
Registration may retry, but only after releasing the old mutex. Waiting for
dispatch holds management exclusion, not publication/registry/Line spinlocks.
Caller-owned handler dependencies must not require the same management mutex
while a removal waits for that handler; serialization cannot resolve such a
dependency cycle.

## Rejected Management Transactions

Ordinary action admission is checked before merging caller-supplied descriptor
metadata or publishing domain mappings. Preflight and installation reuse the
same ownership, capacity and token predicates. The retained Line management
mutex and continuous Line state exclusion serialize both phases; registry
exclusion covers identity revalidation and metadata publication. Caller references keep rejected
handlers alive until all management and short locks have been released. NMI
context rejection does not invoke the locking logger.

Resolution-error cleanup releases admission and management exclusion without
retiring an existing idle Line. A free request that finds no owned action
returns before changing enable/depth state, masking hardware or beginning
teardown/drain. These guarantees cover software rejection and absence, not
rollback of hardware writes, allocator identity reuse or device quiescence.

## Normal Dispatch Tail Ownership

The private `IrqDispatch::complete` consumes the dispatch admission only after
the supplied original-claim completion returns. `runtime/entry.rs` moves that claim
into a once-only closure on the claiming CPU. Missing Lines still complete their
claim without attaching to a replacement. For selected Fasteoi Lines, only the
dispatch with running ownership clears it, under the original Line lock held
through completion. Level/Edge/Fasteoi/PerCpu rejections hold Line exclusion continuously
through chip operations and original completion, take no in-flight credit and
never clear another owner's running bit. Accepted ordinary handlers and replay
take running ownership before snapshot capture and unlock.
Rejected normal-IRQ claims for NMI-owned Lines complete under the held
Line lock without a count or action access. Accepted PerCpu executions rely on
source-owner lifetime protection until full dispatch return;
PerCpu rejection has no count and cannot borrow an action.
The drain's aggregate-idle observation takes the same Line lock: rejected
completion is serialized by that lock, while running ownership protects ordinary
executions that release it. Running ownership may be cleared transiently
for the final chip/completion transaction only under continuous Line exclusion,
after snapshots are consumed; drain cannot observe that intermediate state.
Ordinary running admission resets `execution_idle` only on idle-to-busy;
waiters obtain it through `execution_idle_completion()`.
Finishing running admission publishes completion when running ownership is
absent. Ordinary tails mark the guard Finished before unlocking, preventing a
second finish in Drop; deferred wakes run outside the lock. A stale wake cannot
mark a newly reinitialized completion done.
Removing rejection credits therefore does not let sync/free skip an ongoing
locked completion. Neither mechanism excludes future arrivals or stops a source.
Registry exclusion is released before hardware operations; the Line guard must
be released before admission completion reacquires the original Line. Handlers
never execute under these short locks. This does not establish hardware
quiescence or type-level prevention of dropping an internal admission early.

## Fasteoi Replay Ownership And Progress

Resolution rejects flow changes while an action, aggregate execution or queued
replay flag remains.
For Fasteoi, it also rejects source-description changes during that interval,
even after the last action is withdrawn. These checks precede mapping and
descriptor mutation, preserving the original operation table, generation and
source for admitted completion/replay. Private-registry regressions cover both
ownership gates; they do not prove hardware quiescence or affinity migration.

Only an explicitly software-capable chip may use BH replay. A hardware-only
retrigger executes under the original Line lock with local IRQs excluded and
must be bounded, non-sleeping, allocation-free and non-reentrant into KIRQ.
Success means the controller accepted a pending request, not that a handler
ran or hardware became inactive. Failure leaves pending intact, without a
software fallback or an automatic progress guarantee. Hardware acceptance
does not retain a software queue owner or add an in-flight credit; a later
real claim acquires its own admission and completion obligation. Callers still
must stop hardware sources before releasing device resources. Ordinary GIC SPI
providers select this capability and accept the pending write without readback;
this is not proof of a subsequent delivery. GICv3 mask timeout is diagnosed but
the infallible mask hook cannot establish hardware quiescence on timeout.

Flow suppression must not be coupled to the presence of a new management enable
write. `IrqPlatformPlan::flow_unmask` is selected independently using the plan's
proposed enabled/depth state and current action/teardown ownership, and applied
after configure and optional enable while Line state remains locked.
Preparation changes no runtime control fields; only the post-apply commit
updates configured generation, enabled state and disable depth. The same Line
exclusion must cover preparation, hardware application and commit without an
intervening descriptor/control-state change. Other CPUs may receive a source
enabled by the backend, but their generic admission cannot observe this Line
until the commit has completed and its lock is released. True NMI readers
instead observe only CPU-local action publication after preparation; source
owners must not activate delivery
before preparation. Neither path establishes hardware rollback on failure. This restores a
previously enabled, action-less Line
when its first action is installed even with `enable: None`, without reopening
disabled or teardown-blocked Lines. Pending scheduling follows the transaction;
source unmask and software replay remain separate obligations.
The [configuration failure gate](design.md#ordinary-flow-and-replay)
records the retained mapping mutation that precedes the fallible hook. Regular/shared
configuration now precedes acquisition of the new action owner; NMI configuration
precedes publication of the sole Line-owned action. Delivery restoration remains after
action installation, under continuous Line exclusion. Existing shared owners
and disable depth remain intact during the configuration-only stage.
Disabled regular registration also configures before publishing its owner, but
does not enable or flow-unmask the source. Configuration readiness does not
grant delivery permission; the explicit enable transaction releases that gate.
Configuration errors propagate before new action publication and prepared
control-state commit. `runtime.is_configuration_failed` starts false and latches
only configuration errors; metadata-only merges do not latch failure. Configured
generation, enabled state and disable depth remain unchanged. The latch gates
`can_run_actions`, so queued replay declines execution without admission or chip access
without consuming pending, and withdrawal cleanup cannot unmask the source.
Already running handlers are not cancelled by this gate.

Recovery forces configuration even with a matching generation; successful
configuration commit clears the latch. Enable recovery forces a hardware enable
write despite an already true logical enabled state. Shared registration saves
restoration eligibility before configuration and restores hardware enable after
owner publication only when logically enabled with zero disable depth. NMI
configuration errors latch failure and successful configuration commit clears
it before publishing the Line-owned NMI action, without replacing source shutdown.
The failure gate and recovery ordering are implemented; validation remains
pending. Neither this software gate nor successful plan ordering proves hardware
rollback or quiescence.

The static chip capability supplies code lifetime, not permission to rebind a
live Line. Selection validates PLIC and ordinary GIC SPI identity and capability;
fixed local sources select PerCpu explicitly. Unknown or foreign sources are
rejected with UnsupportedFlow, not admitted through a fallback flow.
Binding changes
are checked before publication while actions, aggregate execution or the explicit
queued flag retain the binding.

Replay insertion retains the original Arc and marks queue membership under Line
exclusion, without execution admission or an in-flight increment. Queue operations use Line then queue lock;
consumers drop the queue lock before acquiring Line. Notification runs outside
both. The queued Arc prevents numeric reuse from redirecting work and retains
Line storage until the consumer releases it. Queue membership deduplicates submissions
and explicitly blocks incompatible flow/source changes;
running ownership prevents overlapping action execution on the selected flow.

`is_replaying` distinguishes software service from an outstanding hardware
claim. A real claim overlapping software service retains pending before its
completion, preventing loss after the replay handler's final device read.
The owning replay clears both flags, completes its claim (a no-op for software
service), and prepares pending service under the same Line lock. Fasteoi hardware
completion uses the same ordering. Only normal enable/teardown eligibility permits
retrigger or queue insertion. The returned permanent executor notifier runs after
unlock; withdrawal may win before notification, so dequeue must still revalidate
eligibility. `resend::prepare_locked` requires the supplied state to belong to the
retained Line; its caller locks that exact Line directly.

The executor clears queue membership and checks retirement, pending,
enabled/depth, configuration, action presence, teardown and running state under
the Line lock. Only an eligible execution calls `begin_running_dispatch` before taking
a fresh action snapshot under that lock. Rejected retired, disabled or action-less
entries access neither handlers nor chip operations. Before enqueueing,
those eligibility conditions must hold before explicit LevelHigh/LevelLow may
clear pending in reliance on the declared hardware level contract. Unknown
trigger never acquires that exemption merely from PLIC/Fasteoi classification.
Incorrect level metadata can therefore lose software service; a state-only
fixture is not evidence of physical reassertion. Withdrawal invalidates
eligibility rather than physically unlinking queued nodes. Sync/free drains
admitted claims and running replay only; it may return before a pending executor
runs. That executor holds no old cursor and cannot snapshot a removed action.
Pending service is Line-scoped, not tied to an action-registration epoch: later
registration on the same live Line may make a new action eligible for that replay.
This does not authorize the removed owner to be borrowed again. KIRQ keeps lazy
queue removal; unlike Linux's `clear_irq_resend`, it does not eagerly remove a
still-queued entry. Neither policy can cancel an entry already popped for execution.
Deferred wakes and final
handler/Line owner releases must remain outside short locks. Software replay
does not construct PendingIrq or complete the old hardware cookie.

Non-last shared removal must neither stop surviving actions nor interfere with
their flow-owned mask interval. It adds no whole-Line teardown credit, so
management completion cannot use that removal as a reason to unmask. For a
transaction that did gate the Line, completion first releases its teardown
credit and checks `can_run_actions` before flow restoration under the original
Line lock. No directory lock or additional context guard is acquired. Pending
scheduling follows after unlocking and uses current eligibility, not a saved
pre-withdrawal enable flag. Last-action removal, disable depth or remaining
teardown prevents reopening. NMI teardown retains its separate existing protocol.

`resend::install` is unsafe because its permanent notifier must be IRQ-safe,
allocation-free, non-inline and capable of scheduling the BH runner without
cancellation or shutdown. `run` requires softirq context and rejects NMI; replay
itself excludes local IRQs/preemption. No sleepable management lock is acquired
by replay. Client handlers must support non-sleeping BH replay and must not wait
for management exclusion held by their own synchronous withdrawal.

Before executor binding, queued Arcs and membership flags remain live, without
in-flight obligations. Installation notifies the backlog. Pending service and
eventual queued-owner release require executor progress, but an aggregate-idle
sync/free does not. Already admitted executions must remain able to finish while
the caller waits. There is no eager queue cancellation. The retained Arc protects
descriptor storage, not device resources or permission to access a retired chip
binding; source owners still stop producers and exclude restart before resource
release. Current runtime installs work services before external device/console
IRQ setup. Hardware-active queries, device
shutdown, dynamic affinity and CPU hotplug remain separate proof obligations.
Neither synthetic fixtures nor this documentation establish physical delivery.

## Edge Delivery Boundary

`backend/platform.rs::EdgeChip` is a trusted controller contract. Its callbacks
run under the original Line lock with IRQ/preemption exclusion and must not
sleep, allocate, reenter KIRQ or wait for remote handler completion. The lock
order remains registry -> Line -> controller registers/allocation -> local APIC.
`runtime/dispatch.rs::handle_edge_irq` retains admission across all coalesced action
iterations; it never reuses an acknowledged claim or clears a concurrent owner's
running state. Disabled/tearing-down/failed-configuration Lines retain pending
without executing handlers. A failed retrigger retains pending without silently
falling back to another CPU or a software callback.

`FasteoiResend::Level` requires a provider-validated level source, not a guess
from an unspecified firmware trigger. It relies on hardware reassertion after
unmask and does not prove physical drain. MSI's device-owned mask and outstanding
APIC delivery remain owner shutdown obligations. Neither aggregate idle nor a
successful retrigger is a vector-reuse barrier; the existing MSI release API
does not gain a new shutdown guarantee from retained target metadata.

MSI release holds the registry lock across unused/ownership checks and both
index withdrawals. It restores task context and submits RCU retirement before
backend release, which provides its own allocator exclusion. A private
non-cloneable `MsiReservation` and both preallocated index retirement records
are attached before the Line is published; an MSI flag or raw IRQ number alone
cannot authorize release. The backend consumes
the reservation and must release it without sleeping or reentering KIRQ. It
cannot report an ordinary failure after withdrawal. Software rejection preserves the
mapping and its retirement records; callers must still retain a usable allocation
identity. Release no longer adds an OOM failure to otherwise valid cleanup.
This does not repair an adapter that consumes its retry handle on rejection. Private-state
release tests check software failure atomicity, not device shutdown or physical
late-message isolation.

Allocation rollback calls the backend without the registry lock using the same
fresh unpublished reservation returned by failed publication. Backend allocator
exclusion is required in both call contexts. The x86 release has no fallible
hardware work: an out-of-range/already-free owned vector indicates a violated
allocation invariant. A backend with recoverable release failures requires a
pre-withdrawal protocol before integration. Reservation Drop does not release
hardware, and final Line destruction occurs outside the registry lock.
