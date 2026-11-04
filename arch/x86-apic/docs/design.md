# x86-apic — Design

## MSI Reverse Map Range

`MsiBackendIf::msi_hwirq_range` declares the fixed half-open range from
MSIX_VECTOR_BASE to APIC_TIMER_VECTOR without locking or allocating. KIRQ uses
it to prepare permanent reverse-map slots. Release clears lookup before this
backend returns a vector to its allocator; rejection retains ownership and
allows KIRQ to restore lookup without allocation.

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

- Local APIC bring-up: `init_primary` (boot CPU — masks the legacy PIC,
  selects x2APIC vs xAPIC from CPUID, programs timer/error/spurious
  vectors, maps the xAPIC MMIO page when needed) and `init_secondary`
  (application CPUs).
- IO-APIC routing: `configure_irq_checked` (masked trigger/polarity transaction
  with readback), `set_irq_enabled` (mask/unmask), `irq_trigger_mode` (query),
  and `retrigger_io_apic` (fixed physical-route delivery).
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
arch/x86-apic/
├── src/
│   ├── lib.rs        # LAPIC bring-up, IPIs, IO-APIC access, MSI-X backend
│   └── io_apic.rs    # Checked configuration and fixed-route retrigger
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
- `configure_irq_checked` / `set_irq_enabled`: called by `kirq` (through
  `irq-driver`'s `IntrManagerIf`) with a hardware IRQ number; this
  crate translates it to an IO-APIC redirection entry.

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
registration is aborted or torn down: it validates `u8` convertibility,
clears the bitmap bit and destination binding only for in-window vectors, and
returns `false` otherwise. The same allocator lock protects both allocation bits
and each vector's optional APIC destination. The first successful composition
binds that destination; recomposition accepts only the same target until free.
Route admission validates this binding, and retrigger uses it rather than the
CPU performing the request. There is no other creation or cleanup path.

## Execution Context

- `init_primary` runs once on the boot CPU during platform bring-up,
  before interrupts are routed through the APIC; `init_secondary` runs
  on each application CPU before it enables interrupts.
- `set_irq_enabled` / `configure_irq` / `irq_trigger_mode` take the
  IO-APIC spin lock and may be called from task or ordinary IRQ context through
  `kirq`. They must not be used from NMI. Checked configuration and retrigger
  have the same restriction.
- `with_local_apic` masks local interrupts for the borrow duration, so
  LAPIC accesses cannot re-enter from an interrupt handler on the same
  CPU.
- `msi_route_matches` takes the MSI allocator spin lock. It permits task and
  ordinary IRQ context, but not NMI or callers already holding that lock.
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
- `MSIX_VECTOR_ALLOCATOR`: `SpinNoIrq` protects the allocation bitmap
  (`0x40..0xf0`, 176 vectors) and the per-vector destination bindings together.
  Free clears both; an allocated vector cannot be rebound to another target.

## Error And Panic Model

- LAPIC owner allocation failure returns `NoMemory` through the platform's
  fallible interrupt preparation hooks to the runtime's fatal boot boundary.
- MSI vector exhaustion returns `None` to `kirq` (`alloc_msi_vector`);
  free with an out-of-window vector returns `false`; `compose_msi_message`
  returns `None` for an out-of-window vector or an unresolvable affinity
  target (logged).
- Panics mark bring-up ordering violations: `with_local_apic` without a
  per-CPU handle, `boot_ap` with a logical CPU that has no raw id
  mapping, or duplicate local initialization.

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

`io_apic.rs` bounds pins against the physical table and the fixed IO-APIC vector
window below 0x40. `configure_irq_checked` masks, reads back the mask, programs
trigger/polarity, verifies those bits and restores prior delivery only on success.
It preserves vector/destination and refuses changing an active remote-IRR route.
A failed hardware readback is reported, not treated as a completed barrier;
mask failure cannot guarantee physical suppression. Generic KIRQ admission
provides an independent failed-configuration gate. The older `configure_irq`
entry remains a low-level unchecked operation and is not used by KIRQ flow.

`retrigger_io_apic` reads the fixed vector and physical destination under IO_APIC
exclusion and issues the request before releasing that lock. Level, logical
destination or mismatched-vector entries are rejected. No new vector is created.
`MsixVectorAllocator` additionally retains the destination first committed by
message composition; conflicting recomposition fails. `Any` is resolved once
when producing the message, not again on the replaying CPU. `retrigger_msi`
holds the allocation lock through IPI issuance and free clears both the bit and
target. Admission uses `msi_route_matches`, which rejects explicit CPU affinity
that disagrees with the message target. This is a snapshot, not a lease.
KIRQ retains Line/control identity while invoking these operations.

Lock ordering is KIRQ Line -> IO_APIC or MSIX_VECTOR_ALLOCATOR -> CPU-local
LAPIC borrow. These operations never call back into KIRQ. Retrigger returns
request acceptance, not remote completion. The existing MSI API still requires
the owner to stop/drain the producer before release; target retention does not
solve posted interrupts, vector reuse or CPU hotplug. Those lifetime protocols
are deliberately not changed here.

Private register-model tests cover masked programming/readback failure, route
preservation and exact target/vector selection. Allocation tests cover unbound,
released, conflicting and reused routes. QEMU boot tests complement, rather than
replace, those tests; they do not constitute physical-controller timing proof.
