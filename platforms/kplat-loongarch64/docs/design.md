# kplat-loongarch64 — Design

## Purpose

`kplat-loongarch64` is the LoongArch64 platform implementation behind
the `kplat` contracts: it wires boot-time initialization, the stable-
counter clock source and local timer, the EIOINTC + LS7A PCH PIC
interrupt hierarchy, SMP CPU bring-up, and GED-style power-off for
LoongArch machines (QEMU virt-class platforms).

## Responsibilities

- Boot handoff: the `PlatformBootIf` provider runs
  `early_driver_init` (clock, interrupts, console from the device tree,
  RTC sample seeding `ktime::initialize_realtime` with feature `rtc`) and per-CPU
  `activate_local_interrupts` (timer enablement after handler installation).
- Clock: the architectural stable counter (`Time::read`, `TCFG`
  registers) exposed as the `CounterSourceIf` / `ClockEventIf` providers,
  with a once-computed `NANOS_PER_TICK` conversion.
- Interrupts: the `kirq::IntrManagerIf` provider mapping logical IRQs to
  the LoongArch hierarchy — local timer interrupt 11, EIOINTC line 3,
  and external lines routed through EIOINTC + PCH PIC with range
  gating.
- SMP: `start_secondary_cpu` sends the entry point and stack top through
  CSR mailboxes and kicks the AP with an IPI (feature `smp`).
- Power: the `PlatformPowerIf` provider implements `power_off` (device-tree
  `syscon-poweroff` declaration written to the GED sleep-control
  register), `halt` (park without powering off), and an unsupported
  `suspend_to_ram`. The separate `CpuStartIf` provider implements `boot_ap`
  when SMP is enabled.
- Default platform contracts: `kplat::default_dma_if_impl!` and
  `default_mmio_if_impl!` are instantiated here.

## Non-Responsibilities

- No device-tree parsing policy: firmware descriptions are consumed
  through `of` / `rtc_driver` / `console_driver`; this crate owns no
  parser.
- No interrupt policy: which IRQ a device uses and how it is shared is
  `kirq`'s decision; this crate only gates and dispatches lines.
- No wall-clock ownership: the RTC sample is taken here, but realtime
  correlation belongs to `ktime`.
- No console implementation: UART drivers live in `console-driver`;
  this crate only calls its initialization entry points.
- No wakeup/suspend support: S3 has no path and is reported as
  unsupported.

## Scope

```text
platforms/kplat-loongarch64/
├── src/
│   ├── lib.rs        # module wiring, default platform contracts
│   ├── init.rs       # PlatformBootIf provider (early init, IRQ preparation/activation)
│   ├── time.rs       # stable counter, CounterSourceIf/ClockEventIf
│   ├── irq.rs        # IntrManagerIf provider, IrqType routing
│   ├── irq/
│   │   ├── eiointc.rs  # EIOINTC (256 vectors, IOCSR)
│   │   └── pch_pic.rs  # LS7A PCH PIC (64 lines, MMIO)
│   ├── mp.rs         # AP bring-up (CSR mailbox + IPI)
│   └── power.rs      # CpuStartIf/PlatformPowerIf providers, GED S5 power-off
├── ut-tests/
│   ├── irq.rs         # boot parent-gate check (non-SMP only)
│   ├── irq_eiointc.rs # recording-backend EIOINTC tests
│   └── irq_pch_pic.rs # recording-backend PCH PIC tests
└── Cargo.toml
```

## Guest Test Layout

`unittest::path_mod_test!` mounts the three private `tests` modules at their
original parents in `src/irq.rs`, `src/irq/eiointc.rs`, and
`src/irq/pch_pic.rs`. Module paths, parent-private access, and guest
registration are unchanged. All tests inherit the crate's LoongArch64 gate
and require `cfg(unittest)`.

The EIOINTC and PCH PIC cases use recording backends instead of live
IOCSR/MMIO. Register-transaction tests observe IRQ exclusion at every access,
probe the same control lock with `try_lock` when SMP is selected, and check
different-bit updates in each bank. A recorder-only completion test starts with
IRQs enabled and checks that the W1C write observes them still enabled. An
accidental control `SpinNoIrq` acquisition therefore fails an assertion even in
UP, where the spinlock's CAS flag is absent. This detects acquisition spanning
the write, not arbitrary locking elsewhere in the function; real IRQ dispatch
still invokes completion in its own pinned context. The single boot parent-gate
case reads live CPU-local ECFG and retains `#[cfg(not(feature = "smp"))]`:
external vectors target CPU0, while an unpinned SMP test could run elsewhere.
These fixtures do not establish physical multi-node routing or concurrent
hardware behavior. No case uses user-mode flags.

## Architecture

```text
PlatformBootIf (kiface)
  early_driver_init ─> time::early_init, irq::init (EIOINTC+PCH PIC),
                       console from DT, RTC sample -> ktime
  activate_local_interrupts ─> per-CPU timer enablement (TCFG + IRQ 11)

kirq::IntrManagerIf (kiface)
  enable:   Timer -> ECFG.LIE | Ex -> range gate -> EIOINTC + PCH PIC
  dispatch: Io -> EIOINTC claim; Timer -> TICLR clear; Ex -> complete

CpuStartIf / PlatformPowerIf (kiface)
  power_off -> of::syscon_poweroff -> GED S5 byte write
  boot_ap   -> mp::start_secondary_cpu (CSR mail + IPI)
```

## Execution Context

### Ordinary External Flow

Only external `LevelChip` sources enter PCH configuration. The CPU-local timer
uses PerCpu flow; the EIOINTC parent IRQ 3 cannot own an action and returns
`UnsupportedFlow` from selection and checked configuration. Neither configures
a PCH pin even though their numeric IDs overlap the external namespace. A recording
configuration callback test verifies that exclusion, parent rejection and external failure
propagation without touching live registers.

The supported PCH level sources select KIRQ `LevelChip`, not Fasteoi. Linux
`irq-loongson-eiointc.c::eiointc_irq_dispatch` clears parent ISR before child
dispatch; `irq-loongson-pch-pic.c` selects `handle_level_irq` for level inputs.
This backend preserves that split: `claim_external` completes the EIOINTC W1C
once, including rejected leaves. KIRQ then masks the PCH leaf before the parent,
runs actions unlocked, and conditionally unmasks. PCH level ack is empty.
Unmask writes PCH CLR, opens EIOINTC, then installs HTVEC and opens PCH MASK.
Hardware reassertion services pending levels; no synthetic IPI is issued.

`configure_level` holds the PCH register lock across mask, trigger/polarity
programming, readback and old-mask restoration. Failure returns
`ConfigurationFailed` and does not intentionally unmask; failed mask readback
cannot guarantee physical suppression. Other source bits remain unchanged.
Unknown firmware trigger means the platform's established level default;
unknown polarity follows explicit LevelLow, otherwise defaults high.

Edge requests, per-CPU external flags, multi-node retargeting and external pins
outside 0..64 are unsupported. The flat binding reserves 3 and 11 for local
sources; explicitly external descriptors colliding with them are rejected.
Local timer IRQ 11 uses PerCpu control state; enabling it changes only the
current CPU's local gate. Parent IRQ 3 is demultiplexed before generic dispatch,
not registered as a Line action. Non-platform domains and unsupported software
identities return `UnsupportedFlow`; there is no Legacy flow fallback.
The bounded flat Unknown-controller external binding remains supported as Level.
An unpublished KIRQ candidate with no selected flow is not an executable fallback.
HTVEC here is the PCH vector
table, not a separate supported HTVEC parent-controller driver. This platform
provides only a fixed single-node EIOINTC route transaction; multi-node
routing, CPU-encoded/virtual EIOINTC and edge retrigger require separate
mechanisms.

### Boot And Runtime Context

The EIOINTC routes external vectors through HWI1. Preparation first closes
HWI1, clears all four EIOINTC enable banks before programming routing, and
masks both PCH PIC banks before setting each bank's edge/polarity state.
After initializing both controllers, `irq::init` enables HWI1 in the boot
CPU's `ECFG.LIE`, preserving other enabled CPU lines. Each AP opens its own
HWI1 from `prepare_local_interrupts`, before KIRQ readiness publication.
Individual device lines remain gated by EIOINTC and PCH PIC; enabling a device
alone cannot open the CPU parent gate. PCH activation writes the HTVEC mapping
before unmasking the source.

Linux `drivers/irqchip/irq-loongson-pch-pic.c::pch_pic_reset` likewise writes
every PCH mask bank explicitly. Linux's EIOINTC router initialization enables
all vectors; this backend instead retains its existing per-device EIOINTC
gate and starts those gates closed. No pending/active bits are cleared by
this preparation: masking is not an acknowledgement or device-source drain.
The supported external route is the 64-line PCH PIC with an identity HTVEC
mapping. EIOINTC vectors 64..255 have no platform activation path, and generic
MSI-X allocation returns `None` on LoongArch. Those unsupported vectors stay
masked rather than inheriting firmware enables. Supporting them requires a
separate allocation/routing/activation lifecycle, not a wider PCH range check.

- `early_driver_init` runs on the boot hart before the driver model;
  the RTC path maps its MMIO aperture through `memspace` and must run
  after `memspace` is available.
- `activate_local_interrupts` runs on each CPU; `time::init_percpu` touches
  only CPU-local registers and enables the local timer IRQ.
- `dispatch_irq` runs in interrupt context: it must not sleep or
  allocate, and external lines are claimed/completed inside the
  controllers before the generic handler runs.
- `power_off` runs at the power-down terminal; after the S5 write
  returns it halts rather than assuming success.

## External IRQ Affinity

The platform exposes `IntrManagerIf::immediate_affinity` only for the existing
external PCH level namespace and only after the EIOINTC capability gate is
open. The transaction belongs to EIOINTC: it captures and masks the target
vector's ENABLE bit, rewrites the 8-bit route field while preserving the other
three vectors in the 32-bit ROUTE register, verifies readback, and restores the
incoming ENABLE state or keeps it masked. PCH PIC, PCH CLR/HTVEC, BOUNCE,
NODEMAP, IPMAP, trigger/polarity and KIRQ disable depth are not changed by this
transaction.

The platform selects `EiointcRouteMode::FixedNodeCoreSingleNode` explicitly.
This is a fixed-platform contract, not hardware detection: it does not identify
or automatically reject CPU-encoded/virtual EIOINTC variants. The gate also
requires EIOINTC initialization with ENABLE/ROUTE readback confirmation, and a
present CPU map whose raw IDs all fit node0 (0..3). Readback proves register
content, not node identity; the single-node fact is the explicit platform
contract, not a runtime probe. Missing CPU maps and multi-node topologies
simply close affinity capability; they do not turn into a boot failure. Each
apply additionally checks the executing CPU and target raw IDs locally, because
IOCSR route access is node-local and a target check alone is insufficient.

The route byte is `1 << raw_cpu_id` for this four-core node. No logical CPU
number is used as a route bit. `KeepMasked` verifies the EIO vector remains
suppressed and does not touch PCH. `Preserve` restores the actual incoming ENABLE
bit. A restore readback failure immediately requests suppression again and
returns HardwareFailure even if the new route was written; cleanup failure is
recorded as unconfirmed, never described as safe.

`prepare_local_external_parent()` opens only this CPU's ECFG.LIE HWI1 bit while
preserving other LIE bits. The boot CPU calls it after EIOINTC/PCH initialization;
each AP calls it from `prepare_local_interrupts` before KIRQ readiness is
published. The existing readiness set is therefore sufficient for target
admission; no second online/prepared mask is added.

## Concurrency Model

- Platform state uses `LazyInit` one-shot slots (clock conversion, PCH
  PIC mapping); pre-init access panics with named messages, since all
  call sites follow the boot order.
- Each controller owns a `SpinNoIrq` register lock. Initialization and each
  enable/disable transaction use that controller's lock, independently of
  kirq's global control serialization. PCH enable holds it from HTVEC setup
  through MASK read-modify-write; EIO enable holds it across both ENABLE and
  BOUNCE updates. Different IRQ lines sharing a word cannot overwrite each
  other's bits. Range rejection and logging precede acquisition.
- Lock order is IRQ line/control state -> one controller's register lock.
  PCH and EIO locks are never nested; neither calls back into kirq, waits or
  dispatches while locked. Task and ordinary IRQ control are supported, not
  NMI control. The guard excludes local IRQ/preemption for the full IOCSR
  transaction. EIO ISR sampling and single-bit W1C completion do not take the
  configuration lock; no read-modify-write is introduced for pending bits.
- The QEMU single-node route supports raw CPUs 0..3 after their local HWI1 is
  prepared. A lock does not retarget local IOCSR accesses to another node;
  multi-node routing and remote-node sends remain unsupported and close the
  capability rather than being approximated.
- Linux `drivers/irqchip/irq-loongson-pch-pic.c` serializes mask RMW under
  `pic_lock`; `irq-loongson-eiointc.c` serializes mask/route/unmask under
  `affinity_lock`. Its multi-node `csr_any_send` routing and no-op ordinary
  mask callbacks differ from this platform's per-vector ENABLE/BOUNCE gates.
  Reuse the controller ownership boundary, not Linux's full-word enable policy.
- `power_off`/`halt` are terminal: no locks are held across the final
  register write or CPU park.

## Error And Panic Model

- Bring-up panics name the failure: a failed RTC aperture mapping
  (`"failed to iomap ls7a rtc"`), a console parse failure, and a boot AP
  without a raw CPU id mapping all abort loudly.
- Absence degrades: a firmware without `syscon-poweroff`, or a declared
  sleep-control register that cannot be mapped, falls back to halting
  with power kept; a spurious external IRQ logs and returns `None` to
  `kirq`.
- `suspend_to_ram` reports `KError::OperationNotSupported`.

## Design Decisions

- Power-off values come from the firmware declaration
  (`of::syscon_poweroff`), not from machine constants: direct kernel
  boot carries no ACPI tables, and the DTB's `syscon-poweroff` node
  names both the GED register and the S5 byte.
- External IRQs are range-gated at the platform routing layer before
  either controller is touched: the PCH PIC's 64 lines are the binding
  constraint, and out-of-range numbers are logged and rejected rather
  than truncated or remapped.
- The clock conversion (`NANOS_PER_TICK`) is computed once from the
  counter frequency and reused; per-CPU setup only programs `TCFG` and
  enables the timer line.
- `notify_cpu` and `set_prio` in the LoongArch `IntrManagerIf` are
  `todo!()` placeholders — this platform has no priority routing or
  IPI-based interrupt steering wired into `kirq` yet.

## Local event restoration

ClockEventIf::restore_local_timer disables TCFG, selects one-shot mode and clears
the initial value, then restores timer IRQ delivery without enabling a
countdown. It does not initialize NANOS_PER_TICK or reset the stable counter.
PM first restores CPU/controller access and IRQ routing with local IRQs masked;
software deadline arbitration happens later after timekeeping is ready.

## Interrupt Entry Contract

The `IntrManagerIf` dispatch/completion provider uses unsafe methods matching
KIRQ's entry contract. Dispatch acknowledges the local or external source before
transferring a no-op completion cookie to generic dispatch. The caller retains
hardirq context and ordinary IRQ/migration exclusion throughout the call; this
interface change adds no register transactions or new controller lock.
