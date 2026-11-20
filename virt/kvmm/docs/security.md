# KVMM Security

## MMIO Ownership Safety

This analysis covers prepared MMIO owners, GICD register storage and page-table
storage lifetime transfers. It does not cover the entire hypervisor, guest
isolation or world-switch assembly.

The MMIO bus consumes KBox owners prepared by the control plane before acquiring
the bus lock. Runtime self-tests likewise allocate before locking and report
preparation failure as false. GICD consumes a prepared register array and clears
it before exposing register reads, without allocating in its constructor.

Control-plane NOWAIT allocations must precede resource admission that lacks
rollback, particularly host GICH mapping. Nested Vec/Arc allocation, MMIO mapping
cleanup and vCPU hook creation retain their previous behavior. Hook factories
still run under their registry lock and return Option rather than Result;
changing their failure model is deferred rather than hidden behind a panic.
In particular, `handle_psci_cpu_on` marks the target CPU on before constructing
its Vcpu and hooks. A future fallible hook constructor must define rollback of
that state and partially prepared hooks before it can be used on this path.

Tests exercise MMIO dispatch and owner destruction, reject invalid allocation
flags, and check GICD register storage reset and read-back without host GICH
access. They do not force global OOM or validate hardware-backed boot rollback.

## Page-table storage lifetime transfers

The unsafe `free_pt_page` boundary in `src/mm/mod.rs` reconstructs a one-page
`PageRef` with `PageRef::from_raw((pa as usize).into(), 1)` and drops it. The
physical address must identify exactly one outstanding, unconsumed
`PageRef::into_raw` transfer of
length one from the same allocation incarnation. The caller must end hardware
use of that table page and complete all required translation retirement before
calling; this boundary consumes the transfer exactly once. An address lookup
alone cannot establish ownership or hardware quiescence.

The EPT, Stage-2 and G-stage constructors create a typed partial mapper before
allocating children. Every linked parent descriptor carries one length-one raw
transfer; every unlinked candidate remains a local typed owner. The private
allocation callback must return fresh zeroed exact backing with the requested
page count and alignment. This is a trusted internal contract, not a driver or
guest allocation interface. Zero roots make partial destruction safe: EPT
`destroy` reads its owned root but does not follow an absent PDPT; Stage-2 and
G-stage skip invalid descriptors and distinguish child tables from data leaves.
Stage-2 links each L2 before requesting the next, so no raw unlinked ownership
escapes cleanup. Failed construction has never published its root to an observer.

The same raw-transfer rule applies to runtime ensure-table helpers. They prepare
an unpublished typed child, publish its parent link, and transfer the hold in an
infallible commit window. Allocation failure leaves that slot unchanged. Earlier
successful live updates remain owned if a later allocation fails; false does not
mean the whole operation made no change. Existing mapping-lock and caller
quiescence requirements still apply. Empty G-stage L0 removal now clears the
parent and executes the existing `hfence_gvma` before `free_pt_page`, as required
by the release boundary. The final range fence is retained. This does not establish
new remote-vCPU synchronization or replace the caller's hardware-use obligations.

Guest RAM reservation validates nonzero page-aligned host-representable size and
a checked guest end before admission. Its exact `VirtMem` PageRef keeps every
reserved base page alive and zeroes backing before publication. Drop releases
only those holds, allowing another holder to delay reuse. `VmGuestMemory` field
order destroys its mapper before the reservation; construction failure unwinds
table children before the caller's reservation drops. vCPU-held VM Arcs and the
existing stop/join/architecture operations retain access-lifetime responsibility.
A physical reference does not certify guest, translation or device completion.
Externally supplied memslots remain outside this ownership conversion.

Serial kernel fixtures force each constructor allocation ordinal through a local
callback, verify exact page geometry/accounting and late VM-build backing rollback,
and exercise ensure-table failure, reuse and empty-table release. Guest-reservation
tests check a three-page exact allocation, zeroing, last-holder release and rejected
geometry. Inactive fixtures establish ownership cleanup and descriptor behavior;
they do not prove hardware TLB completion, remote-vCPU safety or guest-entry support.

## Timer Residency Security

### Scope

This analysis covers the AArch64 timer lifecycle in
`src/vdev/aarch64/{timer_residency,vtimer,irq_route,vgic}.rs` and its integration
with `src/vcpu.rs` and `src/arch/aarch64/mod.rs`. It is not a complete audit of
guest memory, other virtual devices or the VMX/RISC-V backends.

### Ownership And Unsafe Boundaries

The task observer owns an Arc-backed timer snapshot, never a pointer into a
mutably borrowed Vcpu. The assembly-visible architecture layout is unchanged.
Registers are read/written only at EL2 by the resident task with IRQ exclusion.
Actual task put saves CNTV_CTL/CVAL/OFFSET and disables output with ISB before
CPU reuse. Load reconstructs destination state before restoring control. Final
stop prevents later load and re-entry. Hardware failure closes guest-entry
admission; scheduler callbacks do not log, allocate, wait or wake tasks.
Timer snapshot state uses `SpinNoPreempt`; safe hardware-mutating methods
acquire `IrqSave` before that lock and release the lock before restoring IRQs.
Scheduler callbacks inherit IRQ exclusion from their unsafe caller contract.

`irq_route::set_local_forwarding` consumes KIRQ's unsafe ownership contract:
the host route is permanently registered, guest MMIO changes only the current
resident's enable state, and puts stop the producer before revoking delegation.
Local delivery is tracked separately from forwarding authority to avoid
unbalanced nested disables. IRQ entry cannot overlap task ownership transition.
Wake routes use per-CPU weak references, not task-observer reference cycles.

Route preparation serializes sleepable task callers with `ROUTE_REGISTRATION`.
`FORWARDING` publishes only a fully prepared capability through `LazyInit`;
IRQ-side readers never acquire the preparation mutex. A contender waits for
publication or retries pending rollback before attempting another registration.
The mutex's pending-rollback flag clears only after `try_free_percpu_irq` returns
an owner, or after successful forwarding publication. Errors and no-action
results retain cleanup responsibility and leave forwarding unpublished. The
returned handler is destroyed after releasing the gate, allowing destructor
reentry; preparation rechecks publication and pending state after reacquiring it.
This relies on exclusive VMM ownership of the route, not identity protection
against unrelated callers replacing its action. Preparation callbacks must not
reenter the gate or hold IRQ/spin guards.
The dedicated `kirq::try_register_percpu` entrance rejects a wrong resolved flow
or enabled local slot before configuration/action publication. This is an
admission check, not hardware quiescence. The unsafe `try_free_percpu_irq` call
in `src/vdev/aarch64/irq_route.rs` discharges caller-owned execution exclusion:
rollback runs under the preparation gate before `FORWARDING` is published,
and local activation requires that published capability. No action execution
has been admitted, and competing preparation is excluded. Rejected claims
borrow no action and complete under KIRQ's Line lock. KIRQ does not supply a
counted drain. Successful permanent routes
never use this rollback path; vCPU stop revokes local forwarding instead.

The vGIC's LR snapshot is protected by a per-vCPU `SpinNoPreempt` lock, including
safe queries and final-stop calls through distinct hooks for the same vCPU.
Hardware world switches still require the owning task's IRQ exclusion.
Logical active state survives physical-source revocation.
HW LRs never combine pending and active; inactive level-pending state is
resampled against timer output before re-entry. Physical active state alone
does not certify device quiescence or guest completion.

### Blocking And Failure

WFI publishes wake eligibility before querying queued interrupts. Task wake
registration retains an interrupt arriving before park. The existing ktask
timer future owns a deadline callback and drains it in sleepable context on
return; the scheduler observer does not cancel synchronously. Expiry only
wakes the task, and entry rechecks the guest counter before injecting an IRQ.
Disabled/masked guest timers have no periodic polling alarm. VM stop remains
an interruptible wake source. Unsigned comparisons and wide conversion prevent
large guest compare values from wrapping into an immediately due signed delta.
The WFI regression observes actual blocked schedule-outs before issuing each
wake, including repeated disabled/masked re-block and a final VM-stop wake;
it uses no wall-clock pass deadline. This does not exhaust timer-callback races.

Firmware capability/range checking belongs to the irqchip provider; GICH
capacity checking and device mappings belong to kvmm-api. This implementation
supports only its existing GICv2 virtualization interface, not GICv3 guests or
generic arbitrary-device forwarding. QEMU validation does not certify hardware
timing, all firmware layouts, or adversarial long-duration migration stress.
