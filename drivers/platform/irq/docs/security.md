# irq-driver — Security And Reliability

## GICv2 Runtime Routing Boundary

The affinity selector in `src/gic/mod.rs` and the transaction in
`src/gic/gicv2/affinity.rs` are covered by this analysis. KIRQ supplies
interrupt-ready candidate logical CPUs under Line exclusion, outside NMI; the
backend validates its own GIC domain, standard SPI range and discovered target
before any register write. The immutable logical-to-raw CPU map supplies the
hardware identity used to look up the banked CPU-interface target; a logical CPU
number is never written as a target bit.

Per-CPU GICv2 initialization discovers the banked target before resetting local
private state and before runtime publishes readiness. The target map rejects
out-of-range logical CPUs, non-one-hot targets and conflicting associations
without replacing an established route. Discovery or registration failure
aborts CPU bring-up instead of silently advertising routing capability. An
implicit uniprocessor target is recorded only for the boot CPU, and its RAZ/WI
`GICD_ITARGETSR` is never read back as if it encoded a destination.

The nested GIC lock excludes competing ordinary IRQ register transactions and
migration for the whole update, below the caller's Line lock. No new unsafe
boundary is introduced. The GICv2 target byte does not have the GICv3 RWP
protocol: `Preserve` writes and reads back the target without changing
enablement, while `KeepMasked` first verifies suppression and leaves delivery
masked. A pre-write rejection has no MMIO side effect. A readback mismatch
requests masking and reports uncertain hardware state; it is neither rollback
nor proof of hardware quiescence. An already claimed interrupt still completes
on its original CPU through its original completion cookie. The opt-in delivery
fixture drains before moving, so it does not exercise an in-flight claim racing
a target update; that remains an unverified acceptance item. There is no CPU
hotplug or multi-controller inference in this contract.

## GICv3 Runtime Routing Boundary

The affinity selector in `src/gic/mod.rs` and the transaction in
`src/gic/gicv3/affinity.rs` are covered by this analysis.
KIRQ supplies interrupt-ready candidate CPUs under Line exclusion, outside NMI;
the backend validates its own source namespace, standard SPI bounds and target
encoding before writes. CPU masks are policy input, not permission to target a
different controller instance. The nested GIC lock excludes competing ordinary
IRQ register transactions and migration for the entire hardware update.
The safe `IntId::spi` construction follows discovered standard SPI range and
exact GIC binding validation before converting to an SPI-relative number.
No new unsafe boundary is introduced. Target reachability relies on the
singleton controller's successful per-CPU redistributor initialization before
runtime readiness publication, and on the immutable logical-to-MPIDR map.
Firmware-present CPUs alone are not accepted as evidence of readiness. There
is no CPU-hotplug or multi-controller inference in this contract.

Successful routing preserves actual enablement, or verifies suppression for
`KeepMasked`. It does not modify priority, trigger, action membership or the
completion cookie. A readback mismatch or disable timeout is an uncertain
hardware outcome: best-effort masking is not certified quiescence. KIRQ's
separate routing fault closes software admission until recovery, but source
owners must still stop generation and drain admitted handlers before releasing
resources. Routing readback alone does not prove delivery to the selected CPU.

## GIC firmware resource boundary

Discovery rejects malformed status, untranslated/malformed reg and ranges,
missing required register roles and address-width overflow before hardware init.
Disabled supported nodes are skipped; a malformed selected node is not skipped
in favor of a later controller. Mapping and register-size admission remain in
the existing GIC initializer. This change adds no MMIO unsafe block, fallback
registry or readiness state. DT address metadata is not proof of controller
presence; hardware setup can still fail at the existing fatal-boot boundary.

## Firmware interrupt-cell boundary

Controller translation treats interrupt specifiers as untrusted firmware input.
`gic/xlate.rs` accepts only exactly three cells, types 0/1, in-range SPI/PPI
numbers below 1020/32, and the four defined sense nibbles or zero. It returns
malformed for other shapes and values, unsupported for ESPI, and invalid-hwirq
for out-of-range numbers; it never clamps a cell. The PLIC translator accepts
exactly one nonzero source no greater than the driver's supported 1023 and
reports level-high. Neither translator allocates IRQs, reads controller state,
or retries through another controller.

After initialization, the driver registers the selected node's exact identity
with KIRQ. A duplicate domain or controller association is a fatal boot error,
not a replacement. Registration failure aborts the boot hook because consumers
would otherwise retain an unmapable firmware description; there is no fallback
to a compatible-string search. The returned domain identifier becomes this
driver instance's admission boundary: GIC and PLIC selection reject any other
domain, including a same-family domain registered by a second controller, since
register access still targets the single initialized controller. Flow selectors
still enforce the runtime `STANDARD_INTID_LIMIT` and `riscv,ndev`, so successful
translation is not proof that hardware implements the source. The legacy PLIC
fallback registers no firmware domain and cannot be reached through firmware
interrupt mapping.

## Local NMI Boundary

`src/gic/nmi.rs` accepts only standard PPIs in the GIC domain. Its sole unsafe
conversion constructs an IntId after the 16..32 check, preventing truncation or
out-of-range register indexing. KIRQ supplies retained Line exclusion and CPU
pinning; the GIC lock serializes the full register transaction. Redistributor
RWP completion is not active-handler completion: active state is checked and
never forcibly cleared. A teardown caller must stop the producer before pending
state is cleared. Timeouts, active claims, stuck pending state and attribute
readback failures return errors without enabling delivery. Higher layers retain
the preparation/handler on failure and must not treat the error as shutdown.

## PLIC Source State Reservation

The validated PLIC source bound limits the NOWAIT KBox reservation to 1024
entries including reserved source zero. Reservation precedes MMIO admission
and PLIC publication. Allocation failure is fatal boot because this early IRQ
initialization API has no recovery channel; it is not a NOFAIL allocation.

## Local Source Control

`gicv2::set_forwarding_state` validates descriptor identity and the PPI range
before constructing the vendor IntId. Its GIC guard covers banked active writes
and the per-CPU delegation bit. The caller owns producer stop and local IRQ
exclusion across the wider transition; the backend does not infer quiescence
from an inactive bit. Ordinary GICv2 completion consults only explicit local
delegation. `gic::guest_interface` validates all firmware register ranges before
exposing GICH/GICV; its tests do not prove physical MMIO accessibility.

PerCpu selection includes GIC private sources (including forwarded PPI 27 and
NMI-tagged PPIs), RISC-V timer/software causes, and x86 timer/IPI
vectors. KIRQ pins the CPU across configure/enable/commit. RISC-V controls the
local SIE bit, and x86 timer control changes only the LAPIC timer mask. An x86
IPI vector has no independent hardware mask: disabled software admission is not
producer shutdown or completed remote quiescence. Completion timing is unchanged.

## Scope

This analysis covers the entire crate: `src/lib.rs` (architecture
dispatch), `src/gic/mod.rs`, `src/gic/xlate.rs`, `src/gic/spi.rs`,
`src/gic/gicv2.rs`, `src/gic/gicv2/affinity.rs`, `src/gic/gicv3.rs`,
`src/gic/nmi.rs` (AArch64 GIC),
`src/riscv/mod.rs`, `src/riscv/firmware.rs`, `src/riscv/context.rs`,
`src/riscv/registers.rs`, `src/riscv/affinity.rs` and `src/riscv/legacy.rs`
(PLIC), and
`src/x86/mod.rs` and `src/x86/discovery.rs` (IO-APIC glue). No modules are
excluded; the backend crates (`arm_gic_driver`,
`x86-apic`) encapsulate their own register access and are audited
separately — values they return are treated as hardware facts here.

## Trust Model

The device tree and platform constants select and locate the interrupt
controller; the kernel trusts those boot-time descriptions after
validation. Controller MMIO contents are hardware state programmed only
by this crate and `kirq`. There is no user-space input and no
network/file data path in this crate.

## External Boundaries

- **Device-tree controller description** (`gic/firmware.rs`): the node the backend
  reports as an interrupt controller contributes its compatible strings and
  `reg` regions, which determine version and MMIO windows. Checked properties: compatible
  must match a known GIC list, and each detected version must supply
  its required regions (v2: GICD+GICC; v3: GICD+GICR). Failure:
  `init_from_fwnode` returns `Err` ("invalid GIC firmware configuration" or
  "no available supported GIC controller in firmware description"), and the
  aarch64 platform init stops with `primary GIC initialization failed`.
- **PLIC region and contexts**: `PlicConfig::from_fwnode` selects the first
  available `sifive,plic-1.0.0` / `riscv,plic0` node that the backend reports
  as an interrupt controller, and supplies its translated `reg` resource,
  `riscv,ndev` and raw `interrupts-extended` declarations. Firmware parsing
  preserves raw indices, skips the binding's `-1` argument placeholder, M-mode
  entries, unavailable CPUs and non-supervisor-external entries, and requires a
  firmware-decoded hardware identity for each S-mode CPU. The controller-owned
  context table then maps hardware identities to the runtime CPU set and owns
  logical/hardware/index uniqueness. Malformed or dangling references, missing
  identities, duplicate associations, no usable S-mode context, out-of-range
  context indices or a mapped window too small for the used context registers
  abort initialization instead of being guessed or falling back to the fixed
  layout. The register layer owns the `0x80` enable stride, `0x1000`
  context-local stride and 15872-context bound. Without firmware, the legacy
  fixed address `0x0c00_0000`, register-layer source bound and explicit
  `hart_id * 2 + 1` declarations are retained.
- **Controller MMIO windows**: mapped once through
  `memspace::iomap_device`; all later access stays inside the mapped
  apertures.

## Unsafe Code

RISC-V source activation changes only the current hart's SIE source bits.
Platform startup must retain the global SSTATUS.SIE mask across preparation
and activation until runtime handlers are installed. PLIC context preparation
does not itself provide that CPU-wide exclusion.
Context preparation clears only the current context's supported source enable
bits before lowering its threshold. It must precede source registration on that
context and must not be reused to reset a running CPU. Global source priorities
and other contexts are not changed. This establishes a disabled local baseline,
not effective-affinity ownership or proof that the device has stopped producing.

On x86, fixed LAPIC spurious/error vectors are consumed before ordinary
claims are constructed. Spurious delivery must not acknowledge any ISR entry;
error delivery captures controller status and acknowledges once in x86-apic.
Neither path may fall through to generic completion or Action dispatch.

Ordinary x86 IO-APIC/MSI claims transfer a single EOI cookie into KIRQ. Edge flow
consumes it before handlers under the Line lock; level Fasteoi consumes it after
handlers. Local timer/IPI vectors keep early EOI and a no-op cookie. A failed
mapping still consumes the original claim once. Explicit IO-APIC/MSI descriptors
are range/identity checked; MSI admission also requires retained allocation
routing. Callbacks do not derive an MSI target from the replay CPU. Device-owned
MSI masking, posted deliveries and vector-reuse safety are not supplied by these
flow operations. Generic disable suppresses handlers but cannot stop MSI traffic.

Production selectors reject unsupported identities rather than assigning a
Legacy flow. x86 retains only explicit IO-APIC/MSI bindings and fixed raw
timer/IPI PerCpu identities. A KIRQ candidate with no selected flow does not
authorize configuration or dispatch. PCI legacy INTx and direct legacy
`set_trigger` calls retain their existing contracts; neither is a fallback
flow or gains new validation from this change.

GICv2 local preparation constructs only architected SGI/PPI IDs (0..32),
clears their banked active state and requests disablement before CPU-interface
setup. The caller must keep CPU IRQ delivery masked and run this once before
local source activation. GICv2 SGI enable writes may be ignored; they are not
a substitute for CPU masking. This path neither clears pending state nor
changes shared SPI state, and must not be reused on a live CPU interface.
The common local entry rejects IRQ-enabled calls and atomically claims a
per-CPU attempt before hardware setup. A failed attempt remains claimed:
there is no rollback/retry guarantee. This flag is separate from PMR readiness
and also protects GICv2 and builds without pseudo-NMI support.

Every unsafe site is in a backend wrapper with a `SAFETY:` comment; the
common precondition is that hwirq/interrupt identifiers were validated
by the caller (`kirq` dispatch or platform init):

- `gic/gicv2.rs::init` -- `Gic::new(gicd_base, gicc_base, None)`: the bases are
  the iomapped GICv2 MMIO frames from platform discovery.
- `gic/gicv2.rs::{checked_config,set_trigger,enable,set_prio,retrigger_spi}` --
  `IntId::raw(..)` wraps a validated hardware IRQ number. `init_current_cpu`
  enumerates the fixed private range; `set_forwarding_state` checks the PPI
  range/domain and asserts IRQ/NMI exclusion before changing banked ownership.
  The unittest PPI state/pending helpers receive range-checked IDs from `gic.rs`.
- `gic/gicv3.rs:25` — `Gic::new(gicd_base, gicr_base)`: bases are the
  iomapped GICv3 MMIO frames.
- `gic/gicv3.rs:43/52/60/83/113` — `IntId::raw(..)`: same wrapping
  precondition; the `:113` site re-wraps an already-acknowledged
  interrupt id for NMI completion.
- `gic/gicv3.rs:117` — `asm!("isb")` after NMI-window register sequences:
  instruction synchronization only.
- `gic/gicv3.rs:136` — `asm!("mrs ICC_RPR_EL1")`: read-only system
  register read used to open the NMI window (feature `nmi-pseudo`).
- `riscv/registers.rs` — `PlicRegisters::new` plus each bounds-checked
  `read_volatile`/`write_volatile`: the base and size come from the successful
  `iomap_device` of the PLIC MMIO window. Construction rejects an unaligned base
  and a non-register-aligned length; every access is checked against the mapped
  length before the volatile operation. The driver never forms a whole-block
  `PLICRegs` reference. Invalid internal offsets are explicit invariant failures,
  not zero reads, false state or discarded writes.
- `riscv/mod.rs` `send_ipi_to_raw_hart` — `asm!("fence rw, rw")`: publishes
  prior memory writes before the SBI IPI request.
- `riscv/mod.rs` `activate_local_interrupts` and `IntrManagerIf::enable` —
  `sie::set_ssoft`/`set_stimer`/`set_sext`/`clear_stimer`/`clear_ssoft`: toggle
  only the current hart's supervisor interrupt-enable CSR bits.
- `riscv/mod.rs` — `sip::clear_ssoft()`: clears this hart's software
  interrupt pending bit.

`src/x86/` contains no unsafe of its own: register access is
delegated to the `x86-apic` crate. There is no user-pointer access
anywhere in this crate.

## Protected Resources

- `GIC_INFO` / `ACTIVE_GIC` (`LazyInit`): the validated GIC version and
  regions; immutable after init, and re-init with a different config is
  rejected (`remember_config` assert).
- `GIC` (`LazyInit<SpinNoIrq<Gic>>`) and `TRAP_OP`: the GICv3 backend
  handle and its trap operations, installed once.
- `IRQ_WORK_HANDLER_REGISTERED`: one-time registration outcome for reserved
  SGI15. Its handler is never removed; the notifier does not read this state.
  Mapping is prepared before action installation without enabling delivery.
  Either preparation or installation failure prevents notifier publication;
  an already prepared mapping remains platform-owned for kernel lifetime.
- `PLIC` (`LazyInit<PlicController>`): the source bound, immutable context table
  and PLIC register handle; access before init panics with a named `expect`.
  Its inner `SpinNoIrq<PlicState>` lock protects registers, effective targets and
  the prepared-CPU mask, excluding local IRQ delivery and migration while
  selecting the claiming context and executing enable/check/complete/restore. A
  disabled source is temporarily enabled only to satisfy the controller
  completion contract, then restored before unlocking. Concurrent source-control
  updates use the same lock, so restoration cannot overwrite a newer enable
  decision. This does not imply device shutdown or hardware quiescence after
  software drain.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | DTB describing a wrong GIC version or truncated regions | Medium — wrong MMIO windows mapped, later accesses fault | Malformed or hostile device tree | Version/region requirements checked per version before any mapping; missing regions yield `None` (no mapping, no access). |
| T-02 | Out-of-range hwirq reaching a backend | Medium — out-of-window register access | `kirq` or platform init passing a bad source number | PLIC validates source bounds. Explicit GIC resources are checked by `select_gic_flow` before publication; only raw private IDs retain PerCpu classification, and other raw IDs are rejected. Direct platform calls still require caller validation. `x86.rs` translates but never invents numbers. |
| T-03 | Interrupt storm from a misconfigured trigger mode | Medium — CPU starvation | Wrong edge/level programming | Explicit GIC SPIs reject falling-edge and low-level requests before publication. Checked backends verify known trigger configuration and request masking on transaction failure; GICv3 private changes are rejected before mutation. `Unknown` leaves hardware unchanged. Misleading register reads and direct legacy writes retain the limitations below. |
| T-04 | Reinitialization resets live controller state | Medium — inconsistent controller state | Repeated configuration or local CPU initialization after source activation | `remember_config` asserts config equality. Global handle reuse does not make local GIC initialization idempotent; platform bring-up initializes the primary interface through discovery and each AP interface separately, without repeating primary local setup. |
| T-05 | Spurious PLIC claim | Low — wasted IRQ cycle | Level noise on a PLIC line | Claim returns `None`/skip sentinel for timer and IPI events; completion only for genuine device lines. |
| T-06 | NMI irq_work submission re-enters controller or logging locks | High — CPU deadlock | NMI interrupts a GIC/logging critical section | Prepared notifier uses only immutable MPIDR affinity, integer encoding, DSB, the public SGI1R write primitive and ISB; no generic IPI helper, CPU map, lazy state or logging. |
| T-07 | irq_work SGI reaches the wrong CPU or runs as NMI | Medium — work stranded or drained in forbidden context | Unrepresentable Aff0, SGI/IPI conflict, wrong priority or NMI attribute | Preparation validates SGI < 16, distinct IPI_IRQ and Aff0 < 16; encodes all affinity levels with RS=0/IRM=0; sets ordinary priority and clears supported hardware NMI attributes before enabling. |
| T-08 | Firmware interrupt cells decode to a different hardware source or trigger | High — interrupt delivered for the wrong device, or a line programmed with unstated semantics | Malformed or hostile device tree interrupt specifier | The owning controller translates with strict cell count, type, range, and sense-bit checks; malformed, unsupported, and invalid-hwirq are distinct errors with no clamping or controller fallback. Runtime flow selectors independently bound the actual hardware range. |
| T-09 | A second same-family controller's domain is admitted by the singleton driver | High — another instance's source programmed through the first controller's registers | Two firmware controllers of one family; driver selects by family metadata instead of its own instance domain | `select_gic_flow`, `gicv3_nmi::update` and PLIC flow selection compare against the domain recorded for the initialized instance; foreign domains return `UnsupportedFlow`. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | No usable interrupt controller in the firmware description | `init_from_fwnode` returns `Err` | Mandatory platform bring-up stops | 3 | Discover configuration before hardware initialization; propagate failure to the platform. |
| F-02 | GIC region MMIO mapping fails | Panic with the region name | Boot fails | 2 | Bring-up ordering bug surfaced loudly. |
| F-03 | PLIC accessed before `init` | Named-`expect` panic | Offending caller aborts | 2 | Initialization-order bug surfaced loudly. |
| F-04 | GIC config re-supplied with different values | `remember_config` assert panic | Platform init aborts | 2 | Inconsistent platform description surfaced loudly. |
| F-05 | Local irq_work preparation unsupported or fails | Returns `None`, no notifier published | Generic irq_work remains unavailable on this CPU | 3 | Reject invalid affinity/SGI, retain global registration failure, and propagate NMI-attribute or generic-enable failure. GICv3 logs the failed preparation step; runtime publishes ready only on success and rejects available NMI without a ready local handoff. |
| F-06 | Firmware controller node cannot be associated with its IRQ domain | `init_from_fwnode`/`init_primary` reports the registration error; PLIC aborts boot | Consumers cannot map firmware interrupts; boot stops rather than silently using another controller | 3 | Registration happens only after hardware initialization and rejects duplicate domain/controller associations. No compatible-string fallback or lazy replacement exists. |

## Known Limitations

- Checked GIC configuration reports observable failures but does not prove
  isolation. GICv3 failure-path masking has no completion wait; trigger writes
  can remain applied. `ConfigurationFailed` combines RWP timeout and readback
  mismatch. See the checked-backend boundary below for caller obligations.

- GICv3 local preparation failure returns before EOI-mode configuration and PMR
  readiness publication. Partial hardware initialization is not undone; callers
  must stop bring-up instead of enabling sources or retrying a live controller.
  Controller-wide backend failures and MMIO mapping failures can still be fatal.

- Local irq_work is provided only on GICv3 with Aff0 < 16; no optional SGI
  range-selector support is assumed. MPIDR and the initialized GIC interface
  must remain stable. Callers must exclude migration across notification.
- SGI15 is reserved for the kernel lifetime. External disabling, unregistering,
  reprioritization, CPU hot-unplug or controller reset would invalidate readiness;
  there is no revocation protocol. Normal IRQ masking delays delivery by design.
- Preparation is allocator-ready startup work and may acquire locks. Only the
  returned notifier has the NMI-safe contract. Its register primitives contain
  the backend's audited assembly; this path introduces no new handwritten unsafe.
- The x86 path delegates register access to `x86-apic` and supports only
  IO-APIC trigger/polarity translation; MSIs are composed by the
  `x86-apic` backend, not here. `x86::init` first validates the
  declarations (non-zero, unique address; known provenance), then runs
  `init_local_primary`, probes every controller without writing a
  redirection table, and validates capabilities (pin count and
  `gsi_base + pin_count` overflow), half-open GSI-range overlap, and
  every explicit firmware route against the probed owner. Only after
  that complete validation are controllers installed and programmed, in
  declaration order. A non-zero GSI base is supported; the range is
  consumed by boot validation rather than retained for runtime routing.
  Rejection returns `InvalidInput`/`Unsupported` and boot aborts without
  rolling back MMIO mappings.
- `x86::discovery` treats the MADT as untrusted input. The first
  malformed entry ends the walk and fails it with the offending table
  offset; invalid or over-capacity controller declarations are skipped
  and recorded as issues rather than programmed. A duplicate ISA line is
  recorded with its superseded mapping, captured before target
  invalidation, and the later mapping wins. Its fixed arrays bound memory
  use, and it performs no MMIO access. Only `Absent` authorizes the
  platform's static fallback; a malformed stream or an unsupported
  topology aborts boot instead of silently using another address.
- GICv3 `dispatch_nmi` semantics depend on the PMR-based NMI window; on
  platforms without FEAT_NMI/pseudo-NMI support the NMI path is
  inactive (`supports_hardware_nmi` reports `false`).

## Audit Checklist

- Backend hwirq bounds checks remain in place before any register
  access.
- New MMIO windows go through `memspace::iomap_device`, never manual
  address math.
- IRQ/NMI-context `IntrManagerIf` operations remain allocation-free and do not
  sleep. `prepare_local_irq_work` is an explicit normal-startup exception for
  allocation and locks; it runs only after the allocator and GIC are ready.
- Keep irq_work notification independent of `GIC`, `LazyInit`, CPU maps and
  logging, and keep SGI15 out of the NMI priority/attribute classes.
- Keep the SGI15 descriptor marked `PER_CPU`; typed SGIs now select PerCpu.
  Prepare and enable it on every CPU using the existing force-enable path.
  Shared registration does not enable remote slots, and even an enabled local
  slot does not replace the required local hardware preparation.

## PLIC Source Validation

The RISC-V provider treats `riscv,ndev` as untrusted firmware input and checks
its cell size and supported range before publishing the controller. External
operations require completed primary initialization. The source bound and
register lock belong to one published controller, not independently initialized
globals. Source validation and rejection diagnostics precede the register lock.
External enable, priority and completion ids must be nonzero, within the initialized
bound and representable without truncation. Invalid ids return before indexed
register access. The no-DT fallback enforces library capacity, not physical
source discovery. The context table and register aperture are validated before
controller publication: used raw indices must fit the register-layer model and
the declared window must cover every used enable, threshold and claim register.
Source validation does not replace those context checks, and a valid hardware
CPU identity outside the runtime set is not treated as a routing target.

The effective-target table is allocated before publication with one slot per
supported source plus reserved slot zero. Target selection, immediate affinity
and enable-bit writes are serialized by the same controller lock. Management-task
migration cannot redirect a later disable to another context. Context preparation
must precede first enable; bound CPUs must remain available for the controller
lifetime. CPU hot-unplug is not supported. Immediate affinity can retarget an
ordinary source only to a prepared, interrupt-ready context.

The affinity transaction validates the source/flow and selects a prepared
candidate before any register read or write; an unreachable candidate set is
`Rejected` with zero register writes. It samples the old context's actual enable
bit, suppresses the old context, records the new target and suppresses stale
enablement on the new context. `Preserve` re-enables only if the old context was
actually enabled; `KeepMasked` leaves the new context disabled. No priority, mask
policy or KIRQ disable depth is changed.

A failure while suppressing the old context leaves the recorded target on the
old context and performs no new-context write. Once the new target is recorded,
a later suppression or enable failure leaves the target on the new context; the
recovery disable therefore clears that context, while the old context was
already suppressed. `HardwareFailure` does not claim quiescence, restore the old
route or complete an outstanding claim. Claim completion remains local to the
claiming CPU and does not rewrite the recorded target or enable state.

The logical-CPU context table is immutable after controller publication and is
validated against the declared MMIO window before any register access. The
prepared-CPU mask is updated under the controller lock only after a CPU's
context transaction succeeds; a failed or repeated preparation cannot make the
CPU a routing target. Claim, enable and completion use the same table, so a
firmware-decoded context and the legacy no-firmware layout cannot disagree.

## GIC Admission Boundary

`src/gic/mod.rs::select_gic_flow` checks explicit GIC controller/domain identity
against `STANDARD_INTID_LIMIT`, initialized through the existing driver's
`typer_raw()` in `src/gic/gicv2.rs` and `src/gic/gicv3.rs`. The limit uses ITLinesNumber,
not IDbits, and excludes 1020..1023 plus extended/LPI identifiers. Comparison
in `usize` rejects oversized values before they could truncate into a valid ID.
Before controller setup, explicit GIC admission returns `UnsupportedFlow`.
The KIRQ resolution path checks this result before metadata/mapping publication;
registration therefore retains caller ownership of a rejected handler.

The same gate rejects explicit GIC SPI `EdgeFalling`/`LevelLow` requests before
new mapping publication or existing metadata updates. Raw private classification
does not validate wiring, direct controller calls, private-source trigger support, unspecified
trigger metadata, forwarding state or per-CPU shutdown. Eligible ordinary SPIs
select Fasteoi; private sources including PPI 27, tagged SGIs and NMI-tagged PPIs
select PerCpu. Explicit GIC SPIs marked `PER_CPU` return `UnsupportedFlow`
including NMI registration's added flag;
neither successful admission nor cached hardware capacity proves delivery,
configuration success, register-write completion or hardware quiescence.

Raw descriptors (no domain, Unknown controller) select PerCpu for `0..32`,
including 27, with or without `PER_CPU`. The flag alone does not select PerCpu
for raw IDs outside that range. This compatibility classification
is not hardware validation and does not relax checked register bounds. Other raw
and foreign identities return `UnsupportedFlow`; neither a low INTID nor the
flag overrides a foreign domain/controller identity.

Flow selection grants no automatic NMI or forwarding access. NMI capability
still requires an explicit GIC root-domain PPI on GICv3 with an available NMI
mechanism; forwarding requires an explicit root-domain PPI on GICv2. In
particular SGI15 remains an ordinary IRQ, and raw PerCpu classification cannot
authorize either capability or replace source-owner shutdown.

### Checked Backend Configuration

Both checked backends reuse `select_gic_flow` before hardware access and
independently bound register indices, including raw private IDs, to the initialized
standard range below 1020, before handling unknown trigger metadata.
Their new `IntId::raw` sites in `checked_config` therefore cannot truncate an
oversized ID into a valid register index. The existing `GIC` lock serializes the
complete transaction with other backend register writers and pins current-CPU
private access. No new raw MMIO layout, allocation, priority write or
NMI-attribute write is introduced.

GICv2 checks enable, trigger and restored-enable observations; an observed
failure requests disablement without restoring or rolling back. Fixed rising
SGIs need no writes. A read-only PPI whose trigger already matches succeeds;
a PPI write that fails readback returns `ConfigurationFailed`. Electrical PPI
polarity remains an integration assumption, not a verified register property.

GICv3 private sources are read-only in this checked path. Matching current-CPU
readback preserves startup, including private NMI setup; a requested change
returns `UnsupportedFlow` without modifying any redistributor. SPI changes
wait for bounded distributor RWP completion before programming, and return
`ConfigurationFailed` on timeout or mask/configuration/restore mismatch. That
single error variant does not distinguish timeout from readback failure.

The GICv3 failure path only issues a disable write, without an inline completion
wait. A failed transaction can therefore leave mask writes pending and trigger
state changed. Neither backend promises fail-safe isolation when registers
are inaccessible, read-as-zero, read-as-one, write-ignored or faulting: the
vendor accessors do not expose access errors, and matching readback can be
misleading. The code reports observable mismatches; it cannot recover MMIO
faults or infer physical delivery from register values. Callers must treat
failure as partial hardware state and must not publish configuration success.

The transaction does not clear pending or active state, synchronize handlers,
stop device production, deactivate forwarded guest interrupts or prove drain.
Normal task/IRQ callers must not hold `GIC`; NMI execution is forbidden because
the lock cannot exclude it. Private sources require execution on the intended
initialized CPU. Direct legacy `set_trigger` calls retain their existing
caller-validation and infallible-write limitations.

Ordinary SPI flow remains gated on the prerequisites in
[the design](design.md#ordinary-spi-flow-prerequisites). In particular, exhausting
a bounded RWP poll must not be treated as evidence that masking completed.
Successful RWP observation also does not drain an already admitted handler or
deactivate a forwarded guest interrupt. Configuration readback failure and RWP
timeout are distinct hardware conditions, currently represented by the same
checked error; neither implies that earlier hardware writes were rolled back.
A checked result supports failure propagation, but the prepare/apply/commit
split alone must not be used to claim hardware rollback or isolation.

## Ordinary SPI Flow Boundary

`src/gic/spi.rs` is a private controller-operation adapter, not a second IRQ
state machine. KIRQ retains the original validated binding and serializes
mask/unmask/retrigger with Line state. Each backend acquires its existing GIC
lock below that Line lock; no operation reenters KIRQ. No such operation runs
from NMI. Publicly reachable chip callbacks revalidate source identity/range and
flow eligibility before invoking backend helpers. Invalid mask/unmask inputs
do nothing; invalid retrigger returns false without MMIO. `retrigger_spi`
constructs an `IntId` only from that admitted standard SPI range, preventing
truncation and private-register access.

SPI pending acceptance means a W1S write was issued, not that hardware became
inactive or a handler ran. Readback cannot distinguish failure from consumption
by a remote CPU. No software fallback is permitted. GICv3 disable waits for
bounded RWP, releases the controller lock and diagnoses timeout; the void hook
cannot promise completed masking on timeout. Generic eligibility still prevents
disabled/tearing-down handlers from running and completes arriving claims.
Physical shutdown remains caller-owned. Configuration errors use the separate
fallible path; none of these operations clears active state to fabricate drain.

Private sources, including NMI-tagged PPIs and the current forwarded virtual
timer PPI, are not ordinary SPI flow. Explicit GIC SPI `PER_CPU` requests are
rejected without an alternative flow. A future SPI forwarding consumer
must establish exclusion and completion ownership before using this capability.
The `gic_spi` model tests cover access order and timeout reporting, not physical
hardware or firmware security ownership.

The `cfg(unittest)` helper `gic::pend_local_ppi_for_test` is an unsafe fixture
boundary in `src/gic/mod.rs`: it admits only GICv2 INTIDs 16..32 before a banked
pending MMIO write. The caller must own the source, stop its producer, exclude
local IRQs/migration during the write, and drain delivery and restore source
state before releasing ownership. This hook does not grant source ownership
and is absent from production builds.

## PLIC Flow Masking

The permanent source table also owns configured priority and flow-mask state.
All priority writers participate under the controller lock: neither `set_prio`
nor the management enable default can reopen a masked source. First-mask capture
uses `Option<u32>` so hardware priority zero is preserved as explicit policy;
repeated masking cannot overwrite saved policy with the effective zero. Unmask
restores the latest policy, not an entry-time snapshot. No hardirq allocation is
needed. Masking changes source-wide priority, not effective-target ownership.

For descriptors declaring PLIC by domain or controller, flow selection rejects
incompatible domain/controller, bounds, trigger, polarity, flags and affinity
before returning the static PLIC capability. Only fixed local timer/software
causes select PerCpu; other non-PLIC descriptors return `UnsupportedFlow`. Selection itself
does not write registers or require the replay executor to be ready. KIRQ must
retain and validate binding identity, serialize mask/unmask decisions, and supply
pending/replay cancellation and drain before activating this flow. Static
operation lifetime is not proof of descriptor or replay lifetime. Unknown trigger
acceptance is not a claim that hardware will replay a completed notification.

## Dispatch Boundary Safety

The unsafe `IntrManagerIf` providers in `src/gic/mod.rs`, `src/x86/mod.rs` and
`src/riscv/mod.rs` trust KIRQ's active-entry exclusion. GIC claim helpers retain this
contract through IRQ/NMI classification; x86 transfers the remaining EOI cookie;
RISC-V transfers a PLIC claim or a local no-completion sentinel. Only the original
claiming CPU may consume the cookie, once, before entry exclusion ends. Safe
generic claim construction is not exposed. Controller MMIO and hardware
quiescence obligations remain independent of this Rust call boundary.
