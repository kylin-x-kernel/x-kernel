# irq-driver — Design

## Purpose

`irq-driver` is the platform interrupt-controller integration layer: it
implements the `kirq::IntrManagerIf` contract for each supported
architecture (GIC v2/v3 on AArch64, PLIC on RISC-V, IO-APIC glue on
x86_64) and owns controller discovery, MMIO mapping, and per-backend
configuration so that `kirq` stays architecture-neutral.

## Responsibilities

- Discover and map the controller: GIC regions from the device tree
  (`config_from_device_tree`), PLIC from its fixed platform address with
  DT confirmation, each mapped through `memspace::iomap_device`.
- Provide `kirq::IntrManagerIf` per architecture: `configure` (trigger
  and polarity), `enable`/disable, `dispatch_irq` (claim-and-handle),
  `dispatch_nmi` (GICv3 PMR-protected path), `complete_irq`,
  `notify_cpu`, and `set_prio`.
- Expose descriptor constructors (`irq_desc`, `level_irq_desc`,
  `edge_irq_desc`, `plic_irq_desc`, `legacy_irq_desc`) so platform init
  builds correctly rooted `IrqDesc`s (`GIC_ROOT_DOMAIN`, `PLIC_DOMAIN`,
  `IO_APIC_DOMAIN`).
- Expose GIC-specific NMI capability queries (`supports_hardware_nmi`,
  `set_nmi_attr`).

## Non-Responsibilities

- No IRQ policy: vector allocation, priorities, and handler registries
  belong to `kirq`; backends only program what `kirq` decides.
- No device discovery: the controller nodes are found by this crate, but
  device IRQ wiring is platform init's job.
- No x86 I/O-APIC register access: `src/x86.rs` delegates to the
  `x86-apic` crate and only translates `IrqDesc` trigger/polarity.
- No software interrupt routing between CPUs beyond the backend's
  `notify_cpu`.

## Scope

```text
drivers/platform/irq/
├── src/
│   ├── lib.rs      # architecture dispatch (cfg per target)
│   ├── gic.rs      # AArch64: discovery, mapping, IntrManagerIf, NMI
│   ├── gicv2.rs    # GICv2 backend (arm_gic_driver)
│   ├── gicv3.rs    # GICv3 backend (arm_gic_driver)
│   ├── riscv.rs    # RISC-V PLIC backend
│   └── x86.rs      # x86_64 IO-APIC glue (x86-apic crate)
└── Cargo.toml
```

## Architecture

```text
kirq (policy) ──IntrManagerIf──> irq-driver
                                   |
             +---------------------+----------------------+
             | aarch64           | riscv64              | x86_64
             v                   v                      v
        gic.rs               riscv.rs               x86.rs
   config_from_device_tree  PLIC @ 0x0c00_0000    x86-apic crate
   (DT: interrupt-controller     (DT confirm)      (trigger/polarity
    + reg regions; v2: GICD+GICC,                    translation)
     v3: GICD+GICR)
   iomap: gicd/gicc/gicr      iomap: "plic"
             |                   |
        gicv2/gicv3.rs (arm_gic_driver)   riscv_plic
```

## Execution Context

- `init` / `init_from_device_tree` run once during platform bring-up,
  before device interrupts are enabled; `init_current_cpu` runs per CPU.
- `dispatch_irq` runs in IRQ context with interrupts enabled; the GICv3
  NMI path (`dispatch_nmi`) runs with interrupts disabled and no NMI
  window, relying on PMR protection.
- Controller configuration uses backend `SpinNoIrq` locks; claim/completion
  and the prepared GICv3 irq_work notifier use register operations directly.
- `prepare_local_irq_work` runs once per CPU during runtime interrupt-service
  initialization, after allocator and local GIC initialization, with migration
  excluded. It may allocate and take locks, but does not sleep. It must not be
  called from NMI, IRQ handlers, or early platform initialization.

## Local irq_work Notification

GICv3 reserves ordinary Group 1 SGI15 for irq_work. Preparation rejects an
out-of-range SGI, a conflict with `IPI_IRQ`, or MPIDR Aff0 >= 16 (the sender
does not implement the optional range selector). A global `LazyInit<bool>`
serializes one `register_disabled` call; the permanent handler invokes
`kirq::irq_work::handle_irq`. Registration and enablement share a descriptor
marked `IrqFlags::PER_CPU`, identifying the SGI's CPU-local hardware state.
Registration failure is retained and yields
`None`, without replacing an existing handler. No GIC lock is held while
entering the generic IRQ registration/control plane.

Each CPU disables its SGI, clears the hardware NMI attribute when supported,
sets edge configuration and ordinary priority `IRQ_THRESHOLD`, then enables
through `kirq::try_enable(desc, true)` so generic descriptor state and hardware
agree. This boolean API explicitly forces platform enable on every CPU even
when the global descriptor is already enabled; `PER_CPU` is metadata, not the
mechanism that bypasses enabled-state deduplication. Do not replace this call
with `try_enable_irq`, which does not force platform enable.
Runtime publishes generic irq_work readiness only after receiving the notifier.
GICv3 preparation failures log the rejected SGI/affinity, handler registration,
ordinary-IRQ classification or enablement step before returning `None`. These
startup diagnostics do not run on the NMI-safe notifier path.
GICv2, RISC-V and x86 return `None`; they do not fall back to the locking IPI path.

The notifier reads immutable local MPIDR affinity and encodes the SGI1R
target directly. It needs no CPU map, per-CPU allocation, or new dependency.
`DSB ST` completes preceding Normal-memory work publication before the public
`arm-gic-driver` `ICC_SGI1R_EL1` primitive writes the notification, and `ISB`
orders issuance before return. Its path contains no locks, allocations, logging,
or lazy initialization, including when interrupted execution holds the GIC lock.
The driver's free `send_sgi` helper is deliberately excluded because it logs.

Under `cfg(unittest)`, `gic::with_irq_work_gic_lock_for_test` runs a bounded
callback while holding the actual GICv3 lock. Runtime tests can submit work or
trigger an NMI there, then check ordinary-IRQ draining after the callback and
lock release. The callback must not wait for ordinary IRQ delivery while masked;
GICv2 returns `false` without calling it.

The handler and SGI reservation last for the kernel lifetime. Other users must
not unregister, disable, reprioritize, or reuse SGI15. CPU hot-unplug and GIC
reinitialization after readiness are not supported by this lifetime contract.

## Concurrency Model

- Backend state (`GIC`, `PLIC`) lives in `LazyInit<SpinNoIrq<..>>`;
  `init` is idempotent (a second call returns).
- `remember_config` asserts that a re-supplied GIC configuration is
  byte-identical to the initialized one, catching inconsistent platform
  init instead of silently reprogramming.
- PLIC claim/complete pairs are guarded by the same lock as enablement.

## Error And Panic Model

- Absence is `Option`: no controller node in the device tree, or too few
  `reg` regions for the detected version, yields `None` so the platform
  can fall back or abort with its own policy.
- MMIO mapping failures panic with the region name
  (`map_mmio_region`), and `remember_config` panics on a changed config —
  both are bring-up ordering/configuration bugs, not runtime conditions.
- PLIC access before initialization panics via the named `expect` on the
  global handle.

## Design Decisions

- One crate per architecture via `cfg` dispatch instead of trait
  objects: each target compiles only its own backend, and `kirq` sees a
  uniform `IntrManagerIf` provider either way.
- GIC discovery walks the device tree for an
  `interrupt-controller` node whose compatible strings map to a known
  version, then takes as many `reg` regions as that version requires
  (v2: GICD+GICC; v3: GICD+GICR) — matching the hardware layout instead
  of hard-coding addresses.
- The GICv3 NMI path is a separate `dispatch_nmi` entry: claim-time
  IRQ/NMI classification happens in the backend, keeping the PMR-based
  NMI window logic out of `kirq`.
- PLIC keeps Linux-style constant IRQ bases (`S_SOFT`/`S_TIMER`/
  `S_EXT` offsets above the `INTC_IRQ_BASE` sentinel) and a skip
  sentinel for completion, since timer and IPI events need no PLIC
  claim/complete.
