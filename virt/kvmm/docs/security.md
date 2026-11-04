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

The EPT, Stage-2 and G-stage page-table builders transfer child-page holds into
raw addresses stored in parent entries. Their destruction paths call
`free_pt_page` for those child pages, while root-page fields retain their own
`PageRef` holders. These ownership holders protect storage lifetime; they do not
perform translation retirement or prove that a vCPU has stopped using a table.

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
publication or retries after a failed attempt has rolled back its action.
Preparation must not reenter the gate or hold IRQ/spin guards.

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
