# x86-apic — Design

## MSI Allocation Range

The backend allocates vectors in the half-open range from MSIX_VECTOR_BASE
to APIC_TIMER_VECTOR. KIRQ's sparse XArray needs no pre-sized table or range
query. Before consuming the reservation, KIRQ withdraws both lookup entries
and submits their RCU retirement records. Backend release then returns the
owned vector to its allocator; it cannot reject a valid reservation.

## Local Timer Mask

`set_local_timer_enabled` changes the current LAPIC timer mask through the
existing CPU-local handle guard. KIRQ uses it for PerCpu enable/disable; it does
not change the timer mode, deadline or initial count. Clockevent programming
remains with the timer driver. There is no per-vector hardware mask for IPIs.

## Purpose

LAPIC handles use kernel-lifetime KBox storage. BSP and AP initialization return
KResult and reserve this owner with NOWAIT because IRQs and migration are disabled
before scheduler startup. A failed reservation returns NoMemory without publishing
the CPU-local pointer or enabling error delivery. The platform forwards this error
to the existing fatal boot boundary; this is not retryable hardware initialization.
MMIO mapping errors also propagate through this boundary via IoMapError's KError
conversion. IO-APIC mapping can fail after LAPIC publication and error unmasking;
the published owner is retained and boot cannot continue. Hardware preparation
is not rolled back. On success,
the owner becomes a raw pointer retained for kernel life, as before.

`x86-apic` is the x86_64 platform's interrupt-hardware adapter: it brings
up the Local APIC on every CPU, owns IO-APIC input-line routing, sends
IPIs, provides the MSI-X vector backend that `kirq` consumes, and exposes
end-of-interrupt and raw-APIC-id helpers to the rest of the kernel.

## Responsibilities

- Local APIC bring-up: `init_local_primary` (boot CPU — masks the legacy
  PIC, selects x2APIC vs xAPIC from CPUID, programs timer/error/spurious
  vectors, maps the xAPIC MMIO page when needed) and `init_secondary`
  (application CPUs).
- IO-APIC bring-up: `probe_io_apic(paddr, gsi_base)` maps the controller
  and reads its implemented pin count without programming it;
  `install_io_apic(probed, policy)` masks every hardware redirection entry,
  writes policy defaults, and publishes the instance with its probed
  `pin_count`. The firmware `gsi_base` is consumed by the caller's topology
  validation before installation. Every entry starts with vector zero and
  masked; a delivery vector is assigned per configured route later, so two
  instances never compete for the same vector by pin arithmetic. Pins above
  the 32-entry backend window stay masked and are not routable. Production
  boot calls the split steps through `irq_driver::x86::init`, which probes
  the whole topology and validates addresses, capabilities, GSI ranges, and
  explicit routes before installing any instance.
- GSI resolution: `pin_for_gsi_in_range(gsi, gsi_base, pin_count)` performs
  the checked subtraction `gsi - gsi_base` and bounds the result by the
  probed pin count. The result is a controller-local `IoApicPin`, never the
  GSI. A GSI is a firmware route number; it is not a pin and never becomes
  `IrqDesc::hwirq`. The IRQ integration layer performs this resolution while
  validating the boot topology; after installation, dispatch resolves the
  dynamically assigned delivery vector instead of the GSI.
- IO-APIC routing: `configure_pin_checked(instance, pin, policy)` (masked
  vector installation with readback and per-route vector assignment),
  `set_pin_enabled(instance, pin, ..)` (mask/unmask),
  `pin_trigger_mode(instance, pin)` (query), and
  `retrigger_pin(instance, pin)` (physical-route delivery through the
  assigned vector). `IoApicTriggerPolicy::Program` also rewrites
  trigger/polarity; `KeepHardware` retains the programmed bits while still
  assigning and verifying a delivery vector. `IoApicInstanceId` and
  `IoApicPin` together distinguish a source from a GSI, a CPU vector, or an
  equal pin on another controller.
- Inter-processor interrupts: `send_ipi_self`, `send_ipi_raw`,
  `send_ipi_all_but_self`.
- MSI-X backend (`kirq::MsiBackendIf` provider): a bitmap allocator over
  the `0x40..0xf0` vector window, `compose_msi_message` building the xAPIC
  `0xFEE0_0000 | (apic_id << 12)` message form, and affinity resolution
  through `kcpu_id_map`.
- Common helpers: `with_local_apic` (per-CPU LAPIC access),
  `end_of_interrupt`, `raw_apic_id`.

## Non-Responsibilities

- No generic IRQ policy or dispatch: IRQ objects, actions, flow
  execution, and synchronization belong to `kirq`; `irq-driver` connects
  those flows to this crate's operations. The IO-APIC delivery-vector
  pool, the Local APIC vector layout, and delivery destination encoding
  stay here, and this crate programs registers it is told to program.
- No interrupt-line discovery: the IO-APIC address and initial policy
  arrive from `irq-driver`/platform policy; this crate never parses ACPI
  or MADT and owns no fallback address.
- No firmware or bus semantics: pin numbers are not interpreted as ISA or
  PCI lines. The historical "pins `>= 10` are level/low" rule is an
  explicit `IoApicInitPolicy::platform_legacy()` input, not backend
  inference.
- No timer policy: the local APIC timer vector is configured here, but
  the clock-event semantics live in the platform timer wiring.
- No legacy PIC ownership beyond bring-up masking: the 8259 is masked
  during `init_local_primary` and never touched again.

## Ownership and Host Resources

| State | Owner and lifetime |
| --- | --- |
| PIC masking, x2APIC/xAPIC mode, LAPIC vectors | This crate; boot CPU during `init_local_primary` |
| Per-CPU `LocalApic` handle | This crate; one leaked NOWAIT `KBox` in the CPU-local slot, installed by `init_local_primary`/`init_secondary` |
| IO-APIC MMIO mappings and register lock | This crate; every installed controller, indexed by `IoApicInstanceId` |
| IO-APIC probed `pin_count`, per-pin vector and configured mark | This crate; retained per instance under the registry lock. `pin_count` is immutable after publication; vector assignment happens on first configuration, and the configured mark tracks the latest configuration transaction |
| IO-APIC delivery vectors | This crate; one 32-entry route table shared by all instances, assigned on first configuration |
| GSI -> (instance, local pin) resolution | Boot integration; `pin_for_gsi_in_range` arithmetic over declared and probed ranges, never a per-GSI table |
| IO-APIC pin count support policy | `irq-driver`; validates the returned capability |
| MSI/MSI-X vector bitmap and destinations | This crate; exposed through `kirq::MsiBackendIf` |
| ISO semantics and ISA -> GSI table | Not owned here; firmware discovery and IRQ integration layers |

Host-resource management is separate from hardware operations:
`memspace::iomap_device` maps the xAPIC page and IO-APIC window, and
NOWAIT `KBox` allocation plus CPU-slot publication own the per-CPU LAPIC
handle. Hardware operations are PIC port writes, LAPIC register/MSR
access, IO-APIC redirection register access, EOI, and IPI issue.

## Scope

```text
arch/x86-apic/
├── src/
│   ├── lib.rs                    # Shared types, constants, module wiring
│   ├── local_apic.rs             # Per-CPU LAPIC lifecycle, EOI, timer, IPIs
│   ├── msix.rs                   # MSI-X allocator and kirq backend provider
│   ├── io_apic.rs                # Registry, probe/install, pin ops, routing
│   └── io_apic/programming.rs    # Register transactions and route vectors
├── ut-tests/                     # Pin/instance bounds, GSI ranges, vectors, init
└── Cargo.toml
```

## Architecture

```text
init_local_primary()
  ├─ mask legacy PIC (ports 0x21/0xA1)
  ├─ CPUID: x2APIC ? enable x2APIC : map xAPIC MMIO page ("lapic")
  └─ LocalApicBuilder: timer 0xf0 / error 0xf2 / spurious 0xf1

probe_io_apic(io_apic_paddr, gsi_base)
  ├─ map IO-APIC window ("ioapic")
  └─ read pin_count from the hardware table size (no writes)

install_io_apic(probed, policy)
  ├─ mask every hardware redirection entry with vector zero
  ├─ apply policy trigger defaults
  └─ publish ──> IO_APICS: LazyInit<SpinNoIrq<IoApicRegistry>>
                instances[id] = { io_apic, pin_count, pin_vectors, configured_pins }
                return IoApicInstanceId

pin_for_gsi_in_range(gsi, gsi_base, pin_count): checked gsi - gsi_base,
                  bounded by pin_count -> IoApicPin (used by boot validation)
io_apic_route(vector): shared VectorTable -> (instance, pin) for dispatch
configure first use: allocate lowest free vector in 0x20..0x40 for the route
unmask: only after configure_pin_checked recorded a verified vector

per-CPU: LOCAL_APIC_PTR (kcpu_slot::cpu_slot_cell!) holds a leaked LocalApic handle
         installed by init_local_primary / init_secondary

kirq::MsiBackendIf ──> MsixVectorAllocator (bitmap, 0x40..0xf0)
                   ──> compose_msi_message (xAPIC 0xFEE0_0000 form)
```

## Cross-Crate Entry Interactions

- `compose_msi_message(token, hwirq, affinity)`: called by `kirq` when a
  device registers an MSI-X vector. `kirq` is the caller and owns the
  hwirq; this crate asks `kcpu_id_map` (via `raw_cpu_id`) to translate
  the affinity target into a raw APIC id, forms the xAPIC message
  (`0xFEE0_0000 | id << 12`, vector as data), and hands the
  `MsiMessage` back to `kirq` for device programming — `None` when the
  id exceeds the xAPIC destination width or the vector is out of
  window.
- `free_msi_vector(reservation)`: KIRQ consumes its exclusive allocation owner
  after withdrawing the published mapping, or during unpublished rollback.
  The backend clears the bitmap and destination under its allocator lock.
  The non-cloneable reservation replaces a caller-supplied numeric free request.
- `send_ipi_raw(interrupt_id, target_raw_apic_id)`: called by kernel
  subsystems (e.g. TLB shootdown, rescheduling); the interrupt id comes
  from the caller, the raw id from `kcpu_id_map` resolution, and the
  direction is this crate -> the target CPU's LAPIC.
- `configure_pin_checked` / `set_pin_enabled` / `pin_trigger_mode`: called
  by `kirq` (through `irq-driver`'s `IntrManagerIf`) after it resolves the
  descriptor's domain to an instance and its hardware number to
  `IoApicPin`; this crate programs the corresponding redirection entry.
  The per-instance domain identifies the controller and `hwirq` is already
  its local pin; GSI translation happened when the integration layer built
  the descriptor.

## Fixed Local Vectors

`irq-driver` distinguishes the dedicated spurious and error vectors from
ordinary Action delivery. A dedicated spurious vector has no ISR entry and
must not write EOI. For an in-service error vector, `handle_error_interrupt`
uses the dependency's `acknowledge_error`: refresh ESR, read status, then EOI
exactly once. The ESR write is guarded by maximum LVT index > 3, matching
Linux `sysvec_error_interrupt` and its Pentium 3AP exception. Diagnostics use
the captured status after releasing the local APIC borrow.

These two vectors produce no generic claim. They still return through
`kirq::handle_irq`'s common context/RCU/deferred-work exit. This does not change
completion ordering for timer/IPI, MSI-X, or edge/level IO-APIC sources, nor add
Linux's catch-all handling for arbitrary unused vectors.

## Resource Lifecycle

Controller preparation does not start a clock event. The vendored
`LocalApic::enable_masked()` owns one-shot/zero-count initialization; builders
do not duplicate that policy. It first writes a valid masked LVTERR, since
masking another LVT with vector zero can itself raise an APIC error (Linux
`clear_local_APIC`). It then masks timer/LINT0/LINT1 before
configuring vectors and software-enabling the controller. It never writes an
unmasked value to those sources. LINT delivery/polarity settings are preserved,
but neither pin is activated: X-Kernel has no LINT NMI consumer. In particular,
Linux's default BSP LINT1 NMI policy is not copied without its handler support.
Register programming belongs to the dependency; kernel source-activation
policy stays here and in the timer driver. See `vendor/README.md` for provenance
and the write-sequence regression test. The timer driver owns subsequent
mode selection, unmasking and event programming. This follows the Linux
`lapic_timer_shutdown` principle of stopping the count as well as masking the
LVT; masking alone is not the preparation contract.

After publishing the per-CPU handle, the fixed error handler is ready and
`enable_error()` explicitly refreshes ESR, enables LVTERR, and refreshes ESR
again, following Linux `lapic_setup_esr`. LVTERR is now unmasked, but BSP/AP
startup keeps EFLAGS.IF clear until runtime enables local IRQs; installing the
IDT does not enable delivery. This does not start the timer or unmask LINT.
Error delivery no longer inherits firmware's
LVTERR mask. Kernel unittest builds check actual preparation registers on each
booting CPU before publication/activation, so a call-site regression to
`enable()` fails even if vendor-only tests still pass.

The MSI-X vector window (`MSIX_VECTOR_ALLOCATOR`) is created statically
(`SpinNoIrq::new(MsixVectorAllocator::new())`) — no runtime init step.
`alloc_msi_vector` is invoked by `kirq` when a device registers an
MSI-X interrupt and returns a vector in `0x40..0xf0` or `None` on
exhaustion; `compose_msi_message` uses the still-held vector for the
device message. `free_msi_vector` is invoked by `kirq` when the
registration is aborted or torn down: it consumes `MsiReservation`, validates
the allocation invariant, and clears the bitmap bit and destination binding.
There is no recoverable release failure; invalid or already-free ownership is
an internal bug. The same allocator lock protects both allocation bits
and each vector's optional APIC destination. The first successful composition
binds that destination; recomposition accepts only the same target until free.
Route admission validates this binding, and retrigger uses it rather than the
CPU performing the request. There is no other creation or cleanup path.

## Execution Context

- `init_local_primary`, `probe_io_apic`, and `install_io_apic` run once on
  the boot CPU during platform bring-up, before interrupts are routed
  through the APIC; `init_secondary` runs on each application CPU before it
  enables interrupts. `irq_driver::x86::init` calls them in that order.
- `set_pin_enabled` / `configure_pin_checked` / `pin_trigger_mode` /
  `retrigger_pin` take the IO-APIC registry spin lock and may be called
  from task or ordinary IRQ context through `kirq`. They must not be used
  from NMI.
- `with_local_apic` masks local interrupts for the borrow duration, so
  LAPIC accesses cannot re-enter from an interrupt handler on the same
  CPU.
- `msi_route_matches` takes the MSI allocator spin lock. It permits task and
  ordinary IRQ context, but not NMI or callers already holding that lock.
- IPI senders and `end_of_interrupt` may run in interrupt context; they
  touch only the current CPU's LAPIC registers.

## Concurrency Model

- `IO_APICS`: `LazyInit<SpinNoIrq<IoApicRegistry>>` — one lock serializes
  MMIO access across all instances and protects the immutable per-instance
  `pin_count` plus the lazily filled route-vector and configured-pin
  tables. Instances are published in installation order; the integration
  layer verifies the returned `IoApicInstanceId` matches that order. A
  second registry publication is a programming error and panics.
- `LOCAL_APIC_PTR`: a per-CPU slot holding a deliberately leaked
  `LocalApic` handle; local IRQs stay masked while a `&mut LocalApic` is
  borrowed, excluding overlapping borrows on the same CPU.
- `IS_X2APIC` / `XAPIC_BASE`: set once at `init_local_primary`, read with
  `Relaxed` afterwards.
- `MSIX_VECTOR_ALLOCATOR`: `SpinNoIrq` protects the allocation bitmap
  (`0x40..0xf0`, 176 vectors) and the per-vector destination bindings together.
  Free clears both; an allocated vector cannot be rebound to another target.

## Error And Panic Model

- LAPIC owner allocation failure returns `NoMemory` through the platform's
  fallible interrupt preparation hooks to the runtime's fatal boot boundary.
- Allocator exhaustion returns `None` to `kirq` (`alloc_msi_vector`);
  releasing an invalid owned reservation panics on an internal invariant;
  the private bitmap helper still returns false for invalid/double-free inputs.
  `compose_msi_message`
  returns `None` for an out-of-window vector or an unresolvable affinity
  target (logged).
- Panics mark bring-up ordering violations: `with_local_apic` without a
  per-CPU handle, `boot_ap` with a logical CPU that has no raw id
  mapping, duplicate local initialization, or a missing xAPIC base when
  building the local APIC in xAPIC mode.

## Unsafe Code

All unsafe blocks are volatile/port access with `SAFETY:` comments:

- legacy PIC masking via `Port::<u8>` (bring-up ordering).
- xAPIC MMIO page mapped through `memspace::iomap_device` before the
  builder consumes it.
- IO-APIC register access under the crate's spin lock.
- The per-CPU `LocalApic` borrow in `with_local_apic` (exclusion via
  masked local interrupts + one handle per CPU).

## Design Decisions

- x2APIC vs xAPIC chosen from CPUID at runtime: one kernel image serves
  both shapes; the only visible difference is the destination-id
  encoding in `raw_apic_id`.
- MSI-X vectors allocated from a dedicated window below the local APIC
  timer vector (0x40..0xf0): device vectors can never collide with the
  fixed LAPIC vectors, and the xAPIC message form carries the vector
  directly as the delivery data.
- The MSI message targets the affinity CPU's raw APIC id when
  representable in 8 bits (xAPIC form) and otherwise refuses the
  compose, rather than silently retargeting.
- The Local APIC handle is leaked once per CPU and reused forever:
  LAPIC registers are never unmapped, so per-call allocation would be
  pure overhead on the IRQ path.

## Checked Flow Operations

`IoApicPin` bounds a pin below the 32-entry IO-APIC window, and `io_apic.rs`
further bounds it against the physical table. `configure_pin_checked` masks,
reads back the mask, assigns the route's vector on first use, optionally
programs trigger/polarity, writes the vector, verifies those bits, and
restores prior delivery only on success. A descriptor without trigger
metadata uses `IoApicTriggerPolicy::KeepHardware`: it still receives and
verifies a delivery vector, so "unknown trigger" never means "no route".
Every configuration attempt clears the pin's configured flag before hardware
access and sets it only on success, so the flag describes the latest
transaction rather than an older success. `set_pin_enabled(.., true)` refuses
pins without it; a failed (re)configuration therefore cannot unmask a
null-vector entry. The vector is drawn from one
shared 32-slot table keyed by `(instance, pin)`, so equal pin numbers on
different controllers receive distinct vectors. It preserves destination and
refuses changing an active remote-IRR route. A failed hardware readback is
reported, not treated as a completed barrier; mask failure cannot guarantee
physical suppression. Generic KIRQ admission provides an independent
failed-configuration gate.

`retrigger_pin` reads the route's assigned vector and physical destination
under the `IO_APICS` registry lock and issues the request before releasing it.
Level, logical destination or mismatched-vector entries are rejected. No new
vector is created, and a pin that was never successfully configured has no
assigned vector to retrigger.
Deferred affinity reuses the same registry lock and RTE protocol. `retarget_supported`
pre-writes only validates a physical Fixed entry. `retarget_edge` masks the RTE,
writes and verifies the destination and mask, and restores the incoming mask
unless `KeepMasked` is requested. `prepare_level_retarget` masks and returns the
original mask state as a local transaction token; `finish_level_retarget` reads
Remote IRR after EOI. While Remote IRR is set it leaves the destination
unchanged, restores the mask and reports Pending; once clear it writes/verifies
the destination and restores suppression. No controller-side pending table is
created: KIRQ owns the request, while the token is passed through one
completion. Logical destination, non-Fixed delivery and writes that fail
readback are rejected/failed without claiming rollback or quiescence.

`MsixVectorAllocator` additionally retains the destination first committed by
message composition; conflicting recomposition fails. `Any` is resolved once
when producing the message, not again on the replaying CPU. `retrigger_msi`
holds the allocation lock through IPI issuance and free clears both the bit and
target. Admission uses `msi_route_matches`, which rejects explicit CPU affinity
that disagrees with the message target. This is a snapshot, not a lease.
KIRQ retains Line/control identity while invoking these operations.

Lock ordering is KIRQ Line -> `IO_APICS` or `MSIX_VECTOR_ALLOCATOR` -> CPU-local
LAPIC borrow. These operations never call back into KIRQ. Retrigger returns
request acceptance, not remote completion. The existing MSI API still requires
the owner to stop/drain the producer before release; target retention does not
solve posted interrupts, vector reuse or CPU hotplug. Those lifetime protocols
are deliberately not changed here.

Private register-model tests cover masked programming/readback failure, route
preservation and exact target/vector selection. Allocation tests cover unbound,
released, conflicting and reused routes. QEMU boot tests complement, rather than
replace, those tests; they do not constitute physical-controller timing proof.

## Sleep routing snapshot

`SavedIoApics` reserves complete redirection storage before critical entry.
With CPUs parked it captures and masks every installed pin without allocation,
reconstructs routes masked after reset, then restores original masks only after
device/time recovery. Local APIC context reconstruction remains per-CPU platform
work; this object does not stop producers or save remote execution contexts.

The x86 platform SystemSleepIf provider is the consumer: prepare reserves the
snapshot before the barrier; enter captures/masks, wake rebuilds masked routes,
and finish restores the exact pre-sleep routing snapshot. Route mutation must
remain excluded through finish; device callbacks cannot reroute these pins.
