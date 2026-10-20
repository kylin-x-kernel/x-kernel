# x86-apic — Security And Reliability

## Scope

This analysis covers the entire crate — the single `src/lib.rs` (Local
APIC bring-up, IPIs, IO-APIC routing, MSI-X backend). No modules are
excluded; every unsafe access site is enumerated below. The vector and
dispatch policy layer above this crate belongs to `kirq` and is audited
there.

## Trust Model

Device and platform firmware describe the IO-APIC location and the CPU
topology; the kernel trusts those boot-time facts. Everything the crate
programs afterwards is kernel-controlled register state. There is no
user-space input, no network/file data, and no device-callback entry
into this crate.

## External Boundaries

- **IO-APIC MMIO window**: mapped once from the platform-provided
  physical address (`init_primary`). All redirection-table accesses are
  bounds-checked against `max_table_entry()` before MMIO is touched;
  out-of-range `irq` values are no-ops.
- **MSI message composition**: the composed address
  (`0xFEE0_0000 | apic_id << 12`) is written by devices into system
  memory and triggers a physical interrupt; an unrepresentable affinity
  target makes composition fail (`None`) instead of retargeting.
- **Legacy PIC ports (0x21/0xA1)**: written once at bring-up to mask the
  8259; no other port access exists.

## Unsafe Code

All unsafe blocks carry `SAFETY:` comments and fall into three groups:

- Legacy PIC masking (`Port::<u8>::write(0xff)` on 0x21/0xA1) — part of
  APIC bring-up before interrupt routing changes.
- xAPIC MMIO page — mapped through `memspace::iomap_device`
  (`"lapic"`) before the builder consumes the address; mapping failure
  panics.
- IO-APIC volatile register access — serialized by the crate's
  `SpinNoIrq`, bounds-checked per redirection entry.
- The per-CPU `LocalApic` borrow in `with_local_apic` — soundness rests
  on one leaked handle per CPU, installed before interrupts are enabled,
  and local IRQ masking for the borrow duration.

## Protected Resources

- `IO_APIC` (`LazyInit<SpinNoIrq<IoApic>>`): the only IO-APIC handle;
  every redirection write goes through the lock.
- `LOCAL_APIC_PTR` (per-CPU): the exclusive Local APIC handle slot.
- `MSIX_VECTOR_ALLOCATOR` (`SpinNoIrq` bitmap): the MSI-X vector window
  `0x40..0xf0`; double allocation is structurally impossible (bits are
  set under the lock), and `free` clears bits only for in-window
  vectors.
- `IS_X2APIC` / `XAPIC_BASE`: bring-up-era mode selection, read-only
  afterwards.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | IO-APIC access beyond the table | Medium — MMIO to unmapped register offsets | Caller passes an out-of-range `irq` | `set_irq_enabled` bounds-checks against `max_table_entry()` and the vector-window checks; `irq_trigger_mode` returns `None`. |
| T-02 | MSI vector reuse while a device still holds it | Medium — spurious interrupts, cross-device delivery | Unbalanced alloc/free from `kirq` | Bitmap allocator under a spin lock: double alloc impossible, `free` only clears in-window bits; re-alloc requires an explicit free. |
| T-03 | IPI to a CPU id wider than the xAPIC destination field | Medium — misdirected or dropped IPI | Logical CPU whose raw id exceeds `u8` on the xAPIC path | `apic_id_for_affinity` rejects and logs the target instead of truncating; MSI composition returns `None`. |
| T-04 | LAPIC access before per-CPU bring-up | Medium — null-pointer panic | `with_local_apic` on a CPU without `init_primary`/`init_secondary` | Explicit `assert!` on the per-CPU handle with a named message; fails loudly at the call site. |
| T-05 | Spurious interrupt through the masked legacy PIC | Low — stray IRQ 8259 line | Legacy device raising a line after masking | 8259 masked at `init_primary` before APIC routing takes over; spurious lines cannot reach the kernel path afterwards. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | LAPIC MMIO mapping fails at `init_primary` | Panic | Boot fails | 2 | `unwrap_or_else` panic with the mapped range in the message. |
| F-02 | MSI-X window exhausted | `None` returned to `kirq` | Device stays on a shared line or probe fails | 3 | 176 vectors; exhaustion is logged upstream by `kirq`. |
| F-03 | `with_local_apic` before bring-up | Panic (named assert) | Offending caller aborts | 2 | Bring-up ordering bug surfaced loudly. |
| F-04 | Affinity target has no raw APIC id | `None` from `compose_msi_message` | MSI retained or probe degrades | 3 | Logged warning with the CPU number. |

## Known Limitations

- MSI composition only supports the xAPIC `0xFEE0_0000` message form;
  x2APIC systems still use the xAPIC MSI address in this
  implementation (matches QEMU/hardware compatibility, but limits
  APIC-id reach to 8 bits).
- No suspend/resume path: APIC state is not saved or restored around
  sleep transitions (`suspend_to_ram` is unsupported platform-wide).

## Audit Checklist

- New unsafe blocks stay within the three audited groups (PIC ports,
  iomap-backed MMIO, per-CPU LAPIC borrow) with `SAFETY:` comments.
- New IO-APIC accesses keep the `max_table_entry()` bounds check.
- MSI-X allocation stays under the spin lock and inside the
  `0x40..0xf0` window.
