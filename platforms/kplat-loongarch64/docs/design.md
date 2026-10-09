# kplat-loongarch64 — Design

## Purpose

`kplat-loongarch64` is the LoongArch64 platform implementation behind
the `kplat` contracts: it wires boot-time initialization, the stable-
counter clock source and local timer, the EIOINTC + LS7A PCH PIC
interrupt hierarchy, SMP CPU bring-up, and GED-style power-off for
LoongArch machines (QEMU virt-class platforms).

## Responsibilities

- Boot handoff: the `BootHandler` provider runs
  `early_driver_init` (clock, interrupts, console from the device tree,
  RTC sample seeding `ktime::initialize_realtime`) and per-CPU
  `final_init` (timer enablement).
- Clock: the architectural stable counter (`Time::read`, `TCFG`
  registers) exposed as the `ClockSourceIf` / `ClockEventIf` providers,
  with a once-computed `NANOS_PER_TICK` conversion.
- Interrupts: the `kirq::IntrManagerIf` provider mapping logical IRQs to
  the LoongArch hierarchy — local timer interrupt 11, EIOINTC line 3,
  and external lines routed through EIOINTC + PCH PIC with range
  gating.
- SMP: `start_secondary_cpu` sends the entry point and stack top through
  CSR mailboxes and kicks the AP with an IPI (feature `smp`).
- Power: the `SysCtrl` provider implements `power_off` (device-tree
  `syscon-poweroff` declaration written to the GED sleep-control
  register), `halt` (park without powering off), `boot_ap`, and an
  unsupported `suspend_to_ram`.
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
│   ├── init.rs       # BootHandler provider (early/final init)
│   ├── time.rs       # stable counter, ClockSourceIf/ClockEventIf
│   ├── irq.rs        # IntrManagerIf provider, IrqType routing
│   ├── irq/
│   │   ├── eiointc.rs  # EIOINTC (256 vectors, IOCSR)
│   │   └── pch_pic.rs  # LS7A PCH PIC (64 lines, MMIO)
│   ├── mp.rs         # AP bring-up (CSR mailbox + IPI)
│   └── power.rs      # SysCtrl provider, GED S5 power-off
└── Cargo.toml
```

## Architecture

```text
BootHandler (kiface)
  early_driver_init ─> time::early_init, irq::init (EIOINTC+PCH PIC),
                       console from DT, RTC sample -> ktime
  final_init(_ap)   ─> per-CPU timer enablement (TCFG + IRQ 11)

kirq::IntrManagerIf (kiface)
  enable:   Timer -> ECFG.LIE | Ex -> range gate -> EIOINTC + PCH PIC
  dispatch: Io -> EIOINTC claim; Timer -> TICLR clear; Ex -> complete

SysCtrl (kiface)
  power_off -> of::syscon_poweroff -> GED S5 byte write
  boot_ap   -> mp::start_secondary_cpu (CSR mail + IPI)
```

## Execution Context

- `early_driver_init` runs on the boot hart before the driver model;
  the RTC path maps its MMIO aperture through `memspace` and must run
  after `memspace` is available.
- `final_init_ap` runs per application CPU; `time::init_percpu` touches
  only CPU-local registers and enables the local timer IRQ.
- `dispatch_irq` runs in interrupt context: it must not sleep or
  allocate, and external lines are claimed/completed inside the
  controllers before the generic handler runs.
- `power_off` runs at the power-down terminal; after the S5 write
  returns it halts rather than assuming success.

## Concurrency Model

- Platform state uses `LazyInit` one-shot slots (clock conversion, PCH
  PIC mapping); pre-init access panics with named messages, since all
  call sites follow the boot order.
- EIOINTC IOCSR and PCH PIC MMIO accesses are serialized by the
  `kirq`-driven single-threaded interrupt path; the backends add
  bounds checks but no locks of their own.
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
