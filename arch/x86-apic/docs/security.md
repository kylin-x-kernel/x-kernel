# x86-apic — Security And Reliability

The MSI range declaration matches the allocator's admission bounds and never
changes after initialization. Invalid/unallocated vector release returns false
without transferring ownership to a new caller; KIRQ restores the previous
lookup on rejection. Device masking and interrupt drain remain prerequisites,
not consequences of reverse-map clearing or allocator reuse.

## Local Timer Mask Boundary

`set_local_timer_enabled` requires a prepared current-CPU LAPIC. The existing
handle guard excludes migration and local interrupt reentry; the timer owner
must serialize competing clockevent programming. Masking alone does not cancel
a deadline, drain an admitted handler or stop remote IPI producers.

## Scope

LAPIC owner reservation uses NOWAIT, never KERNEL, on IRQ-disabled BSP/AP boot
paths. Allocation failure propagates through the platform interrupt preparation
hooks to fatal boot handling. No per-CPU pointer is published and error delivery
is not enabled after failure. No reserve, latency bound or NMI safety is promised.
The pointer is leaked only after checking that its CPU-local slot is empty.
Tests inject owner preparation failure while pinned with IRQs disabled and check
NOWAIT selection and unchanged CPU-local storage; they do not force global OOM
or simulate recovery from partially prepared hardware.

This analysis covers the entire crate: `src/lib.rs` (shared types and
constants), `src/local_apic.rs` (Local APIC bring-up and IPIs),
`src/msix.rs` (MSI-X backend), `src/io_apic.rs` (instance registry and pin
operations), and `src/io_apic/programming.rs` (register transactions). No
modules are excluded; every unsafe access site is enumerated below. The
vector and dispatch policy layer above this crate belongs to `kirq` and is
audited there.

## Trust Model

Device and platform firmware describe the IO-APIC location and the CPU
topology; the kernel trusts those boot-time facts. Everything the crate
programs afterwards is kernel-controlled register state. There is no
user-space input, no network/file data, and no device-callback entry
into this crate.

## External Boundaries

- **IO-APIC MMIO windows**: mapped once per platform-provided controller
  address (`probe_io_apic`). All redirection-table accesses name an
  `IoApicInstanceId` and are bounds-checked against that instance's
  `max_table_entry()` before MMIO is touched; values outside
  `IoApicPin::MAX` cannot be constructed, and pins beyond the hardware
  table are rejected or no-ops.
- **MSI message composition**: the composed address
  (`0xFEE0_0000 | apic_id << 12`) is written by devices into system
  memory and triggers a physical interrupt; an unrepresentable affinity
  target makes composition fail (`None`) instead of retargeting.
- **Legacy PIC ports (0x21/0xA1)**: written once at bring-up to mask the
  8259; no other port access exists.

## Unsafe Code

All unsafe blocks carry `SAFETY:` comments and fall into four groups:

- Legacy PIC masking (`Port::<u8>::write(0xff)` on 0x21/0xA1) — part of
  APIC bring-up before interrupt routing changes.
- xAPIC MMIO page — mapped through `memspace::iomap_device`
  (`"lapic"`) before the builder consumes the address; mapping failure
  propagates to the runtime's fatal boot boundary.
- IO-APIC volatile register access — serialized by the crate's
  `SpinNoIrq`, bounds-checked per redirection entry.
- The per-CPU `LocalApic` borrow in `with_local_apic` — soundness rests
  on one leaked handle per CPU, installed before interrupts are enabled,
  and local IRQ masking for the borrow duration.

## Protected Resources

The APIC error-vector handler owns ESR capture and exactly one EOI. It must
only be called for an in-service APIC error, not for the dedicated spurious
vector: an EOI there could retire an unrelated ISR entry. The IRQ backend
excludes both fixed vectors from ordinary claims, preventing a second generic
completion. Captured error flags are diagnostic input, not a device IRQ number.

LAPIC preparation uses the vendored `enable_masked()` interface: it masks the
timer and LINT pins, installs a stopped one-shot timer, then software-enables
the controller without unmasking these sources. LVTERR is masked with a valid
vector before any timer/LINT mask write. After handle publication, error
delivery is explicitly enabled with ESR refreshes before and after the LVTERR
write; the fixed error handler is already available. It does not drain interrupts
already pending from firmware, or reset other LVT sources. This avoids
starting the dependency's default periodic event before timer-handler readiness.
Boot callers keep ordinary IRQ delivery disabled throughout preparation;
clock-event activation remains the timer driver's responsibility.

- `IO_APICS` (`LazyInit<SpinNoIrq<IoApicRegistry>>`): every installed
  IO-APIC handle, its probed pin count, assigned vectors, and configured-pin
  flags; every redirection access goes through the registry lock.
- `LOCAL_APIC_PTR` (per-CPU): the exclusive Local APIC handle slot.
- `MSIX_VECTOR_ALLOCATOR` (`SpinNoIrq` bitmap and destination bindings): the MSI-X vector window
  `0x40..0xf0`; double allocation is structurally impossible (bits are
  set under the lock), and `free` clears bits only for in-window
  vectors together with their APIC destination bindings. First composition binds
  the live vector's target; conflicting rebinds are rejected without changing
  the original binding. Route admission and retrigger read the same protected
  target. This is not device shutdown or a hardware-delivery drain guarantee.
- `IS_X2APIC` / `XAPIC_BASE`: bring-up-era mode selection, read-only
  afterwards.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | IO-APIC access beyond the table | Medium — MMIO to unmapped register offsets | Caller supplies a pin outside the 32-entry window or beyond the hardware table, or names a different instance | `IoApicPin` rejects pins above 31; `set_pin_enabled`, `configure_pin_checked` and `retrigger_pin` resolve the instance and bound-check against its `max_table_entry()`, and `pin_trigger_mode` returns `None`. |
| T-02 | MSI vector reuse while a device still holds it | Medium: spurious interrupts, cross-device delivery | Unbalanced lifecycle or missing device quiescence | KIRQ retains one non-cloneable reservation per successful allocation and withdraws mappings before consuming it in backend release. The bitmap and destination are cleared under the allocator lock. Device shutdown and late-message ordering remain caller obligations. |
| T-03 | IPI to a CPU id wider than the xAPIC destination field | Medium — misdirected or dropped IPI | Logical CPU whose raw id exceeds `u8` on the xAPIC path | `apic_id_for_affinity` rejects and logs the target instead of truncating; MSI composition returns `None`. |
| T-04 | LAPIC access before per-CPU bring-up | Medium — null-pointer panic | `with_local_apic` on a CPU without `init_local_primary`/`init_secondary` | Explicit `assert!` on the per-CPU handle with a named message; fails loudly at the call site. |
| T-05 | Spurious interrupt through the masked legacy PIC | Low — stray IRQ 8259 line | Legacy device raising a line after masking | 8259 masked at `init_local_primary` before APIC routing takes over; spurious lines cannot reach the kernel path afterwards. |
| T-06 | Wrong IO-APIC instance programmed | Medium — equal pin on another controller | Instance domain ignored or misresolved at integration | The backend indexes instances by checked `IoApicInstanceId`; every pin operation names one. Non-primary instances receive distinct non-identity domains, and dispatch resolves the delivered vector through the route table before selecting a domain. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | APIC MMIO mapping fails at `init_local_primary`/`probe_io_apic`/`install_io_apic` | KError returned | Boot fails | 2 | IoMapError converts to InvalidInput, Io or NoMemory; existing hardware and any published LAPIC owner are not rolled back. |
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
- Controllers with more than 32 inputs have all hardware entries masked
  during init, but the route-vector pool and `IoApicPin` cover only pins
  `0..=31`. Higher pins are not routable by this backend until a later
  slice widens the vector window.

## Audit Checklist

- New unsafe blocks stay within the three audited groups (PIC ports,
  iomap-backed MMIO, per-CPU LAPIC borrow) with `SAFETY:` comments.
- New IO-APIC accesses keep the `max_table_entry()` bounds check.
- MSI-X allocation stays under the spin lock and inside the
  `0x40..0xf0` window.

## Flow Programming And Retrigger

`src/io_apic/programming.rs::Registers for IoApic` contains the unsafe
register accesses.
The private adapter is used only with the permanent mapped handle under registry
exclusion. `IoApicPin` bounds the 32-entry route window, and helpers validate
the pin against the owning instance's max_table_entry before read/write. The
vector is assigned from the shared route table on first configuration and
read back with the trigger/polarity bits. Configuration retains that exclusion
across mask, readback, vector assignment, optional trigger/polarity writes and
optional restore. A descriptor without trigger metadata still installs a
vector (`IoApicTriggerPolicy::KeepHardware`); it does not skip configuration.
The configured mark describes the most recent configuration transaction: a new
attempt clears it first and only success sets it again. Unmasking a pin without
that mark is a no-op, so neither a never-configured pin nor a pin whose last
reconfiguration failed can enable a null-vector entry. Failure does not
authorize unmask, but a hardware refusal to mask cannot be converted into a
physical quiescence guarantee. Changing remote-IRR trigger/polarity is refused.

Deferred destination changes validate a physical Fixed RTE before any write.
Edge changes mask, write/readback destination and restore the incoming mask
before EOI. Level prepare masks and returns only a local mask-state token;
finish after EOI refuses to change destination while Remote IRR is still set,
then writes/verifies and restores suppression. A failed mask/write/readback is
reported as failure with the entry left masked where possible; it is not
rollback, hardware quiescence or a successful route. The controller owns no
affinity pending table, and no register write occurs during request acceptance.

Retrigger validates fixed physical edge routing and issues an IPI to the stored
destination/vector while the route owner is locked. MSI composition retains its
target in the same locked allocator that owns the vector bit; free clears both.
Unbound/released vectors cannot retrigger, and a reused allocation does not
inherit the previous destination. The notification is not an acknowledgement:
an already issued interrupt may still be pending in a LAPIC after the lock is
released. Device shutdown and safe vector reuse remain caller/system lifecycle
obligations, not properties proven by a bitmap or by KIRQ software in-flight.
