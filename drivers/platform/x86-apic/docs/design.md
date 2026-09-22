# x86-apic — Design

## Purpose

`x86-apic` is the x86_64 platform's interrupt-hardware adapter: it brings
up the Local APIC on every CPU, owns IO-APIC input-line routing, sends
IPIs, provides the MSI-X vector backend that `kirq` consumes, and exposes
end-of-interrupt and raw-APIC-id helpers to the rest of the kernel.

## Responsibilities

- Local APIC bring-up: `init_primary` (boot CPU — masks the legacy PIC,
  selects x2APIC vs xAPIC from CPUID, programs timer/error/spurious
  vectors, maps the xAPIC MMIO page when needed) and `init_secondary`
  (application CPUs).
- IO-APIC routing: `configure_irq` (trigger mode and polarity),
  `set_irq_enabled` (mask/unmask), `irq_trigger_mode` (query).
- Inter-processor interrupts: `send_ipi_self`, `send_ipi_raw`,
  `send_ipi_all_but_self`.
- MSI-X backend (`kirq::MsiBackendIf` provider): a bitmap allocator over
  the `0x40..0xf0` vector window, `compose_msi_message` building the xAPIC
  `0xFEE0_0000 | (apic_id << 12)` message form, and affinity resolution
  through `kcpu_id_map`.
- Common helpers: `with_local_apic` (per-CPU LAPIC access),
  `end_of_interrupt`, `raw_apic_id`.

## Non-Responsibilities

- No IRQ policy or dispatch: vectors, priorities, and dispatch decisions
  belong to `kirq`; this crate programs registers it is told to program.
- No interrupt-line discovery: the IO-APIC physical address arrives from
  platform init (ACPI MADT via `acpi`/`khal`), not from this crate.
- No timer policy: the local APIC timer vector is configured here, but
  the clock-event semantics live in the platform timer wiring.
- No legacy PIC ownership beyond bring-up masking: the 8259 is masked
  during `init_primary` and never touched again.

## Scope

```text
drivers/platform/x86-apic/
├── src/
│   └── lib.rs        # LAPIC bring-up, IPIs, IO-APIC routing, MSI-X backend
└── Cargo.toml
```

## Architecture

```text
init_primary(io_apic_paddr)
  ├─ mask legacy PIC (ports 0x21/0xA1)
  ├─ CPUID: x2APIC ? enable x2APIC : map xAPIC MMIO page ("lapic")
  ├─ LocalApicBuilder: timer 0xf0 / error 0xf2 / spurious 0xf1
  └─ map + init IO-APIC  ──> IO_APIC: LazyInit<SpinNoIrq<IoApic>>

per-CPU: LOCAL_APIC_PTR (kcpu_slot::cpu_slot_cell!) holds a leaked LocalApic handle
         installed by init_primary / init_secondary

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
- `send_ipi_raw(interrupt_id, target_raw_apic_id)`: called by kernel
  subsystems (e.g. TLB shootdown, rescheduling); the interrupt id comes
  from the caller, the raw id from `kcpu_id_map` resolution, and the
  direction is this crate -> the target CPU's LAPIC.
- `configure_irq` / `set_irq_enabled`: called by `kirq` (through
  `irq-driver`'s `IntrManagerIf`) with a hardware IRQ number; this
  crate translates it to an IO-APIC redirection entry.

## Resource Lifecycle

The MSI-X vector window (`MSIX_VECTOR_ALLOCATOR`) is created statically
(`SpinNoIrq::new(MsixVectorAllocator::new())`) — no runtime init step.
`alloc_msi_vector` is invoked by `kirq` when a device registers an
MSI-X interrupt and returns a vector in `0x40..0xf0` or `None` on
exhaustion; `compose_msi_message` uses the still-held vector for the
device message. `free_msi_vector` is invoked by `kirq` when the
registration is aborted or torn down: it validates `u8` convertibility,
clears the bitmap bit only for in-window vectors, and returns `false`
otherwise. There is no other creation or cleanup path.

## Execution Context

- `init_primary` runs once on the boot CPU during platform bring-up,
  before interrupts are routed through the APIC; `init_secondary` runs
  on each application CPU before it enables interrupts.
- `set_irq_enabled` / `configure_irq` / `irq_trigger_mode` take the
  IO-APIC spin lock and are called from task context through `kirq`.
- `with_local_apic` masks local interrupts for the borrow duration, so
  LAPIC accesses cannot re-enter from an interrupt handler on the same
  CPU.
- IPI senders and `end_of_interrupt` may run in interrupt context; they
  touch only the current CPU's LAPIC registers.

## Concurrency Model

- `IO_APIC`: `LazyInit<SpinNoIrq<IoApic>>` — one lock serializes IO-APIC
  MMIO access; initialized during `init_primary` before runtime masking
  is used.
- `LOCAL_APIC_PTR`: a per-CPU slot holding a deliberately leaked
  `LocalApic` handle; local IRQs stay masked while a `&mut LocalApic` is
  borrowed, excluding overlapping borrows on the same CPU.
- `IS_X2APIC` / `XAPIC_BASE`: set once at `init_primary`, read with
  `Relaxed` afterwards.
- `MSIX_VECTOR_ALLOCATOR`: `SpinNoIrq` bitmap allocator
  (`0x40..0xf0`, 176 vectors).

## Error And Panic Model

- Allocator exhaustion returns `None` to `kirq` (`alloc_msi_vector`);
  free with an out-of-window vector returns `false`; `compose_msi_message`
  returns `None` for an out-of-window vector or an unresolvable affinity
  target (logged).
- Panics mark bring-up ordering violations: `with_local_apic` without a
  per-CPU handle, `boot_ap` with a logical CPU that has no raw id
  mapping, and a failed LAPIC MMIO mapping in `init_primary`.

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
