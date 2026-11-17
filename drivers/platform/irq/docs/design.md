# irq-driver — Design

## IO-APIC Deferred Affinity

The x86 provider exposes `IntrManagerIf::deferred_affinity` only for an
IO-APIC descriptor that resolves to a configured instance/pin and is not an MSI
source. KIRQ owns requested/effective/pending; the provider validates a
physical Fixed RTE, selects the first ready logical CPU whose raw APIC id fits
the 8-bit destination field, and performs the register transaction under the
IO-APIC registry lock.

Acceptance writes no RTE and masks no line. Edge deferred preparation masks,
writes and verifies destination and restores the incoming mask before EOI; KIRQ
commits effective before the EOI callback. Level preparation masks and returns
an opaque local token containing destination, selected CPU and the original
mask state. After EOI, finish reads Remote IRR under the same lock: still set
leaves destination unchanged and keeps the request pending; clear writes and
verifies destination and restores the mask. The provider stores no pending
table and never calls KIRQ lifecycle code.

Replay and completions without a matching Line never reach the provider. A
failed mask/write/readback becomes `HardwareFailure`; KIRQ latches the routing
fault, while the original claim still completes. Logical destination,
non-Fixed delivery, an out-of-range CPU/APIC id, or an unknown instance/pin is
rejected before writes. Ordinary disable retains pending; last-action
withdrawal cancels it and reconciles requested with effective.

## GICv2 Runtime SPI Affinity

The GIC provider exposes `IntrManagerIf::immediate_affinity` for an ordinary
standard SPI belonging to its initialized GICv2 instance. Private sources,
foreign domains, unsupported flags and IDs outside the discovered standard
distributor range expose no capability. `gic/gicv2/affinity.rs` implements the
transaction; KIRQ retains the only Line mapping, requested/effective policy and
routing-fault admission.

Each CPU's local GICv2 initialization discovers its own banked
`GICD_ITARGETSR` capability before resetting private state or letting runtime
publish interrupt readiness. The immutable logical-to-raw CPU map supplies the
hardware identity, and the target map records logical CPU, hardware identity
and the discovered CPU-interface bit. Repeated identical discovery is
idempotent. A conflicting association, an out-of-range logical CPU or an
ambiguous discovery fails CPU bring-up instead of silently advertising a
routable target. A uniprocessor GIC records its implicit target only for the
boot CPU; zero target bits never mean that every CPU is reachable.

Candidate selection consumes runtime's interrupt-ready set and chooses the
first candidate with a recorded target. Linux `gic_set_affinity` selects an
online CPU and writes that CPU's `gic_cpu_map` byte; it does not mask for a
GICv3-style RWP. This backend follows the GICv2 protocol: `Preserve` writes the
single target byte and verifies its readback without touching the enable
register, trigger, priority, action ownership or disable depth. `KeepMasked`
first establishes and verifies suppression, then writes the route and leaves
delivery masked. For an implicit uniprocessor target, `GICD_ITARGETSR` is
RAZ/WI, so `Preserve` performs no MMIO and `KeepMasked` only verifies
suppression through the implemented enable register.

A pre-write rejection leaves state unchanged. A readback mismatch requests
masking and reports `HardwareFailure`; it does not claim rollback, hardware
quiescence or a successful route. KIRQ's existing fault recovery still requires
a disabled-source routing retry before explicit enablement. Attaching the target
map does not change `send_sgi` or the existing IPI transport, which keeps its
own target-list path.

### GICv2 Affinity Validation

`ut-tests/gicv2_target.rs` covers CPU-identity lookup, duplicate/conflicting
discovery, out-of-range and non-one-hot rejection. `ut-tests/gicv2_affinity.rs`
drives the production routing transaction with injected registers. It covers
non-identity logical CPU to target-bit mappings, candidate filtering, implicit
uniprocessor behavior, mask/readback failure ordering, and the narrow
register-operation surface (enable and target only). Lifecycle, action ownership
and disable-depth invariants remain covered by the existing KIRQ affinity tests;
this register fixture does not reimplement them. Register fakes model only
hardware and contain no alternate routing algorithm.

`ut-tests/gicv2_affinity_delivery.rs` is an opt-in QEMU integration test that
proves actual delivery and migration. It is skipped unless
`KIRQ_TEST_QEMU_GICV2_SPI` names an absolute INTID reserved exclusively by the
operator. For the stock QEMU 10.0.11 `virt` machine, INTID 240 is outside the
default device wiring described for the GICv3 fixture below; recheck the exact
machine and passed-through devices before use. The test requires at least two
ready CPUs by default; `KIRQ_TEST_QEMU_GICV2_MIN_CPUS=1` explicitly selects
single-runtime-CPU coverage. The boot target type is not derived from the
ready-CPU count: a multi-interface GIC can expose `Explicit` with only CPU0
started. By default the fixture accepts either `Explicit` or
`ImplicitUniprocessor`. `KIRQ_TEST_QEMU_GICV2_EXPECT_TARGET` explicitly requires
`implicit`, `explicit`, or `any`, and only that declaration makes the fixture
assert a specific discovered type. A single-CPU run of an SMP-enabled image is
still not a true UP kernel build. An opted-in source with invalid prerequisites
fails rather than silently skipping.

The unittest boot also needs the configured root block device. The commands
below use the Makefile's default `disk.img`, which must already exist.

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
KIRQ_TEST_QEMU_GICV2_SPI=240 KIRQ_TEST_QEMU_GICV2_EXPECT_TARGET=explicit \
  timeout 600 make unittest UNITTEST_CRATE=irq_driver VSOCK=n NET=n SMP=2 \
  QEMU_ARGS="-machine virt,gic-version=2"

KIRQ_TEST_QEMU_GICV2_SPI=240 KIRQ_TEST_QEMU_GICV2_MIN_CPUS=1 \
  KIRQ_TEST_QEMU_GICV2_EXPECT_TARGET=implicit timeout 600 make unittest \
  UNITTEST_CRATE=irq_driver VSOCK=n NET=n SMP=1 \
  QEMU_ARGS="-machine virt,gic-version=2"
```

The fixture stops its source, synchronizes and drains admitted handlers, removes
its action and test mapping, restores target/priority/trigger state, and never
clears an active bit to fabricate a drain. Because it drains each claim before
moving the route, it does not cover an in-flight claim whose completion races a
target update. The routing transaction does not read or rewrite the completion
cookie and GICv2 completion remains on the claiming CPU, but that
completion-during-migration property is not exercised here and remains an
unverified acceptance item. Emulated delivery does not validate physical GICv2
fault behavior.

## GICv3 Runtime SPI Affinity

The GIC provider exposes `IntrManagerIf::immediate_affinity` for an ordinary
standard SPI belonging to its initialized GICv3 instance. Private interrupts,
foreign domains and unsupported controller instances expose no capability.
The selector does not access registers. `gic/gicv3/affinity.rs` implements the
transaction without changing the claim/completion path. KIRQ owns requested/effective masks,
management serialization and routing-fault admission; this driver owns target
encoding and the controller transaction. Neither layer creates another mapping
or changes action ownership during a routing update.

Candidates come from runtime's interrupt-ready set. On this singleton backend,
each CPU publishes readiness only after its local GICv3 initialization has
located and prepared the matching redistributor. The immutable logical-to-raw
CPU map supplies the full MPIDR affinity, not a logical-CPU bit used as a
hardware destination. This reachability argument relies on that one-controller
boot contract; a future multi-controller or CPU-hotplug implementation must
replace it with an explicit per-controller reachability contract.

The transaction follows Linux `drivers/irqchip/irq-gic-v3.c::gic_set_affinity`:
select a ready destination, preserve the actual enable state, mask if needed,
write IROUTER, and restore only previously enabled delivery. A `KeepMasked`
request instead keeps delivery disabled. One GIC lock covers this sequence,
below the caller's Line lock; no registry lock, allocation, scheduling or KIRQ
callback occurs within it. Priority, trigger configuration and disable depth
are not routing state and remain unchanged. An already claimed interrupt still
completes through the original CPU's DIR path.

This driver additionally checks distributor-disable completion, routing readback
and enable restoration. RWP is evidence of the disable write's completion, not
of IROUTER completion or handler drain. A timeout or readback mismatch requests
masking and reports `HardwareFailure` without claiming rollback or quiescence.
KIRQ then prevents action execution and enablement until a disabled-source
routing retry succeeds. Later trigger configuration preserves IROUTER.
This is immediate routing, not IO-APIC-style deferred migration, CPU hotplug,
or a guarantee that a device has stopped generating interrupts.

### Affinity Validation

`ut-tests/gicv3_affinity.rs` exercises register ordering and injected failures
without hardware or timing assumptions. The separate opt-in integration module
`ut-tests/gicv3_affinity_delivery.rs` exercises actual GIC delivery and records
the receiving logical CPU. It is skipped unless `KIRQ_TEST_QEMU_VIRT_SPI` names
an absolute INTID that the operator has reserved exclusively for the fixture.
Absence of a KIRQ mapping is not proof that a hardware source is unused.

For the stock QEMU 10.0.11 `virt` machine, INTID 240 is outside the devices
connected by [`a15irqmap`](https://github.com/qemu/qemu/blob/v10.0.11/hw/arm/virt.c):
the platform bus occupies SPI offsets 112..175 (INTIDs 144..207). This choice
must be rechecked for another machine, QEMU version, custom wiring or passed
through devices. The opt-in is not suitable for an arbitrary physical board.

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
KIRQ_TEST_QEMU_VIRT_SPI=240 timeout 600 make unittest \
  UNITTEST_CRATE=irq_driver VSOCK=n NET=n
```

Callback acknowledgement, not elapsed time, drives each delivery phase; an
external QEMU timeout diagnoses missing progress. SMP execution moves the source
through every CPU in the sampled ready set. The fixture requires at least two
ready CPUs by default;
`KIRQ_TEST_QEMU_VIRT_MIN_CPUS=1` explicitly selects single-CPU coverage, which
cannot prove cross-CPU routing. An opted-in source with invalid prerequisites
fails rather than silently skipping the test. The
fixture restores its hardware state and removes its action and test mapping.
An emulated delivery pass does not validate physical hardware fault behavior.

## GIC register discovery

`gic::firmware` is a private binding module, not a new crate. It selects the first
available supported controller in tree order through the backend-neutral
`fwnode` contract and consumes translated register intervals. GICv2 assigns
GICD/GICC and optional GICH/GICV; GICv3 retains GICD, every declared
redistributor region (`#redistributor-regions`, default 1) and
`redistributor-stride` (default 0). The vendored GIC driver only maps one
contiguous redistributor frame array with the architectural 0x20000 stride, so
`gic::init` rejects disjoint or custom-stride layouts until that driver grows
support; discovery still describes them faithfully. Malformed selected nodes
fail instead of falling through to a later controller.

The DT backend supplies structural views and status/address interpretation,
and irq-driver owns register meaning and hardware initialization, matching Linux
OF resource interpretation and irqchip bindings. `GicConfig::from_fwnode`
returns a `GicSelection` carrying both the configuration and the selected
`FwNode`; `init_from_fwnode` collects it after allocator initialization,
invokes the existing `init(config)` path, then registers the exact node with
KIRQ. `from_madt` for ACPI and `from_static` for board tables are future peers
that produce the same selection.

## PLIC register discovery and context identity

`riscv/firmware.rs` is a private binding module. It selects the first available
PLIC-compatible node that the backend also reports as an interrupt controller,
consumes its translated first `reg` interval, reads `riscv,ndev`, and walks the
raw `interrupts-extended` entries into `PlicConfig`. It preserves raw context
indices and skips placeholder arguments, M-mode entries, unavailable CPUs and
non-supervisor-external contexts. Malformed references and missing CPU
identities are errors; firmware never maps a hardware CPU to a logical CPU,
builds a runtime context table or defines register geometry.

`riscv/context.rs` owns the immutable logical-CPU-indexed context table. It
maps firmware declarations through the existing CPU map, skips valid hardware
identities outside the runtime CPU set, enforces logical/hardware/index
uniqueness and rejects an empty result. `riscv/registers.rs` owns the register
layout: priority bytes, the `0x80` per-context enable stride, the `0x1000`
context-local stride, the `15872` context bound and the required MMIO aperture.
Every register access is a bounds-checked, 32-bit-aligned volatile access; the
driver does not construct a whole-block Rust reference, so a shorter
firmware-mapped window does not imply access to bytes outside that mapping. The
register-window constructor rejects a base that is not four-byte aligned or a
length that is not a whole number of registers. An internal offset that bypasses
the validated layout is a diagnosed invariant failure, never zero, `false` or a
silently discarded write.

`riscv/legacy.rs` is the explicit no-firmware platform adapter. It supplies the
historical `hart_id * 2 + 1` declarations to the same context builder and
register validator; it is selected only when firmware describes no PLIC node.
`PlicConfig::from_fwnode` returns the configuration and raw declarations, and
primary initialization builds and validates the context table before mapping the
register window and publishing the IRQ domain.

## x86 IO-APIC initialization

`x86::init(X86IrqInitConfig)` is the single boot-CPU entry for the x86
interrupt hardware. The configuration carries up to 32
`IoApicDeclaration`s (`id`, `address`, `gsi_base`) in installation order,
one `kirq` domain per instance, the initial redirection policy, the
declaration's `FwSource`, and the effective ISA -> GSI route table for
lines `0..=15`. The first instance reuses `IO_APIC_DOMAIN`; each additional
instance gets a distinct, non-identity domain allocated without a firmware
namespace node (`kirq::alloc_irq_domain`). Equal local pin numbers on two
controllers therefore never denote the same source.

Pure validation runs before any hardware is touched: an empty set, zero
address, duplicate address, or unknown provenance is `InvalidInput`.
Initialization then runs `init_local_primary`, probes every controller
with `probe_io_apic` (mapping and reading pin count only), and validates
the returned `IoApicCapabilities` (pin count in `1..=32`, no
`gsi_base + pin_count` overflow), every pair of half-open GSI ranges
`[gsi_base, gsi_base + pin_count)` for overlap, and every explicit
firmware route against the probed owner. Only after all of that does
`install_io_apic` program any redirection table, in declaration order; the
returned instance identifier must match that order. A duplicate address or
overlapping range is fatal before programming; an explicit route whose
probed owner differs from the owner selected when its descriptor was built
is also fatal, because consumers may already retain that descriptor. An
explicit route outside every probed range is `InvalidInput` after MMIO
mapping, so the platform must abort boot; mappings are not rolled back.
Identity defaults that no probed controller can resolve are skipped rather
than failing boot. `x86::init_secondary()` is the per-AP counterpart.
Platform boot converts discovery output, or its logged static fallback when
discovery reports `Absent`, into this configuration; the unified entry
itself does not read firmware. No second IRQ registry is created.

The effective route table starts with `ISA n -> GSI n`, edge/high.
Firmware overrides are applied in declaration order, matching Linux
`mp_override_legacy_irq`: before installing an override, a target GSI `g`
below 16 that still maps to itself is invalidated; the override then sets
`ISA s -> GSI g`. Order is significant: `9 -> 9` followed by `4 -> 9`
leaves ISA 9 invalid, while `4 -> 9` followed by `9 -> 9` restores it as
a real explicit route. Shared GSIs are therefore not rejected outright.
`config.legacy_irq_desc` resolves a route's GSI to the instance with the
highest declared base at or below it (and an offset below the 32-pin
backend bound), then stores that instance's domain and the local pin in
the descriptor. Boot validation repeats the resolution with probed pin
counts, using `x86_apic::pin_for_gsi_in_range` against each instance's
declared base and probed `pin_count`. No per-GSI map is added at runtime and
the installed backend retains only the pin count: descriptor operations carry
the domain (which selects the instance) and the controller-local pin, never a
GSI.

After installation `init` publishes one immutable topology entry per instance,
`{ domain, gsi_base, pin_count }`, indexed by backend instance id. This table
is the only source of truth for instance domains and GSI ranges; dispatch uses
it to resolve a descriptor domain back to an instance. The allocation-free
`x86::gsi_desc(gsi, trigger)` scans it with a checked `gsi - gsi_base`
subtraction, bounds the offset by the probed pin count and the backend's
32-pin window, and returns a descriptor naming the owning domain and local pin
(`source = Acpi`). It returns `None` before publication and for any unowned or
out-of-window GSI. Accepting a bare `IoApic` route through the generic mapping
path is therefore unnecessary and rejected.
Platform boot builds the configuration once in `early_driver_init`,
derives the console descriptor through `config.legacy_irq_desc(4)`, and
passes the same configuration to `prepare_interrupts`; it no longer
hard-codes IRQ 4.

Delivery vectors are not derived from the pin number. Each instance's
initial redirection entries use vector zero and stay masked, and the first
successful configuration of an `(instance, pin)` route allocates one
vector from a shared 32-slot pool. The backend records
`vector -> (instance, pin)`; dispatch resolves a delivered vector through
that table, then maps the instance to its domain and uses the local pin as
`IrqRef::Domain(domain, pin)`. This is what allows two 24-pin controllers
to coexist in the 32-vector window.

Vector installation and trigger policy are separate decisions. A descriptor
that states a trigger programs trigger and polarity
(`IoApicTriggerPolicy::Program`); one whose trigger is unknown keeps the
firmware-programmed bits (`KeepHardware`) but still receives and verifies a
delivery vector. Unknown therefore means "do not override the hardware
configuration", not "no delivery route". A pin is marked configured only after
that transaction succeeds, and unmasking an unconfigured pin is a no-op, so a
failed configuration cannot expose a null-vector entry.

`x86::discovery::from_acpi()` is the hardware-free MADT decoder behind
that platform wiring; its `to_init_config()` converts every retained
controller declaration and the overrides into the unified entry's
configuration. It walks the MADT twice: the
first pass retains at most 32 IO-APICs; the second interprets overrides
against the complete controller set, so controller/override interleaving
does not matter and skipped declarations consume no capacity. Override
declarations themselves are processed in order, as described above. At most 16 legacy overrides
are retained, one per ISA source. Zero-address, duplicate-address, and
duplicate-`gsi_base` controllers are skipped; overrides must use the ISA
bus, a source below 16, and non-reserved polarity/trigger flags. An
override is accepted only when a retained controller's `gsi_base` is at
or below its GSI, and duplicate ISA lines are last-wins. The superseded
mapping is captured before target invalidation, so a duplicate's
`OverriddenIsaMapping` diagnostic is recorded even when removing the previous
route is what made the duplicate visible. Overrides update
the effective map in declaration order: an accepted target below 16 that
still maps to itself has its identity route invalidated, while a later
explicit override for that line restores it. A level-triggered
IRQ0 override is decoded as edge, matching Linux's faulty-timer
workaround. All retained controllers are representable; GSI-range overlap
and pin counts need hardware, so the unified entry validates them after
probing. Skips are surfaced as `X86DiscoveryIssue`s; the first malformed
MADT entry returns `X86DiscoveryError::Malformed` and ends the walk, and a
MADT without a usable controller returns `Absent`. Only `Absent`
authorizes the platform's static fallback; a malformed stream is fatal.

## Firmware interrupt binding translation

After controller initialization, `init_from_fwnode` calls
`kirq::register_irq_domain(GIC_ROOT_DOMAIN, controller, IrqController::Gic,
&GIC_XLATE)` and `init_primary` does the equivalent for `PLIC_ROOT_DOMAIN`.
Registration is boot-only and keyed by exact firmware-node identity, so two
instances of the same controller family remain distinct and a second
registration is an error. The controller node must outlive the kernel; both
entry points therefore take `FwNode<'static>`. These singleton boot paths
explicitly select `IrqDomainTarget::Existing(GIC_ROOT_DOMAIN)` or
`Existing(PLIC_DOMAIN)` to preserve legacy identifiers. A future second
hardware instance must call with `IrqDomainTarget::New` on its own
initialization path; KIRQ does not select a legacy root from controller family
or registration order.

The registered domain is also the driver's admission boundary. This driver is
the singleton backend for one initialized controller: all programming goes
through that controller's registers using `desc.hwirq`, not through per-map
`host_data` as in Linux `irq-gic.c`. Admitting every domain of the GIC family
would therefore let a second instance's sources program the first controller.
`select_gic_flow` and the GICv3 NMI path accept only the domain recorded for
the initialized instance (`is_gic_domain`), and PLIC selection accepts only
`current_plic_domain()`. A second controller of the same family can register
its own domain in KIRQ, but this driver rejects it with `UnsupportedFlow` until
an instance-aware register backend exists.

The controllers own the meaning of their binding cells through
`kirq::IrqDomainXlate`:

- `gic/xlate.rs` requires exactly three cells. Type 0 is an SPI at `32 + N`
  below the architectural 1020 limit; type 1 is a PPI at `16 + N` below 32.
  The SPI/PPI identifier ranges come from the vendored GIC driver;
  type 2 (ESPI) is unsupported; other types and cell counts are malformed.
  Flag values 1/2/4/8 map to rising/falling/high/low, zero stays unknown, and
  any other sense nibble is malformed. It does not allocate IRQs or call back
  into KIRQ.
- `riscv.rs` requires exactly one nonzero PLIC source within the supported
  `1..=1023` range and reports level-high, matching the binding's lack of a
  trigger cell. The legacy no-firmware PLIC fallback has no node and therefore
  registers no firmware domain; it only serves legacy descriptor paths.

Actual hardware range admission (`STANDARD_INTID_LIMIT`, `riscv,ndev`) remains
in the existing flow selectors; translation is a binding decoder, not a
readiness or hardware-presence claim.

Before the DT backend migration, the extraction based on main f5f3cf4a1 passed parser host tests (24),
AArch64 OF/irq-driver guest tests (54/54, coverage exported), and UNITTEST-enabled
Clippy on AArch64, RISC-V, x86 and LoongArch. Guest boot exercised GICv3; GICv2
register selection is fixture coverage, not a GICv2 hardware acceptance claim.
Runs used a private disk with NET=n, VSOCK=n and ACCEL=n.

## Local NMI Transactions

`IntrManagerIf::percpu_nmi` selects `PerCpuNmiChip` only for a standard GICv3
PPI with a platform-validated NMI mechanism. Unsupported controllers return
None. `gic/nmi.rs` owns bounded local attribute transactions under the GIC
lock; KIRQ retains Line exclusion and CPU pinning around these operations.
The backend uses the selected CPU interface's Redistributor RWP wait, checks
disabled/inactive state, then programs and reads back priority or NMI attributes.
Teardown clears and checks stale pending state before restoring normal delivery.
It never enables the source or writes active state. PMU producer control and
remote rendezvous remain outside the controller. Register fixtures validate
failure ordering; they do not prove physical NMI quiescence.

## Purpose

PLIC source-state storage uses a KBox slice reserved with NOWAIT after firmware
source-count validation and before MMIO mapping/controller publication. This is
early boot, before ordinary task scheduling; KERNEL is not permitted. Failure
terminates at the existing fatal-boot boundary rather than publishing a partial
controller. The source-zero slot is retained for hardware-number indexing.

`irq-driver` is the platform interrupt-controller integration layer: it
implements the `kirq::IntrManagerIf` contract for each supported
architecture (GIC v2/v3 on AArch64, PLIC on RISC-V, IO-APIC glue on
x86_64) and owns controller discovery, MMIO mapping, and per-backend
configuration so that `kirq` stays architecture-neutral.

## Responsibilities

- Discover and map the controller from the backend-neutral firmware root:
  `GicConfig::from_fwnode` for GIC and `PlicConfig::from_fwnode` for PLIC.
  Both select the exact controller node, consume firmware-translated CPU
  addresses, and map them through `memspace::iomap_device`. The PLIC keeps its
  fixed platform address as the no-firmware fallback, preserving the legacy
  layout.
- Decode x86 MADT IO-APIC and interrupt-source-override declarations in
  `x86::discovery`; platform boot turns the retained declarations into the
  unified entry's configuration, and only `Absent` permits a static
  fallback.
- Retain the selected controller node and register its exact identity with
  KIRQ after hardware initialization, together with a binding-specific
  `IrqDomainXlate`. Consumers map firmware interrupts through that identity;
  the driver never selects a controller or allocates a logical IRQ itself.
- PLIC `riscv,ndev` is read through the same node and still bounded to
  `1..=1023` before the source-state allocation.
- Provide `kirq::IntrManagerIf` per architecture: `configure` (trigger
  and polarity), `enable`/disable, `dispatch_irq` (claim-and-handle),
  `dispatch_nmi` (GICv3 PMR-protected path), `complete_irq`,
  `notify_cpu`, and `set_prio`.
- Expose descriptor constructors (`irq_desc`, `level_irq_desc`,
  `edge_irq_desc`, `plic_irq_desc`) so platform init builds correctly
  rooted `IrqDesc`s (`GIC_ROOT_DOMAIN`, `PLIC_DOMAIN`). x86 ISA consumers
  instead call `X86IrqInitConfig::legacy_irq_desc`, which applies the
  effective firmware route and yields an `IO_APIC_DOMAIN` descriptor.
- Expose GIC-specific NMI capability queries (`supports_hardware_nmi`,
  `set_nmi_attr`).

## Non-Responsibilities

- No IRQ policy: vector allocation, priorities, and handler registries
  belong to `kirq`; backends only program what `kirq` decides.
- No device discovery: the controller nodes are found by this crate, but
  device IRQ wiring is platform init's job.
- No x86 I/O-APIC register access: `src/x86/mod.rs` wires the provider to
  the `x86-apic` crate. `src/x86/config.rs` owns boot/AP initialization
  ordering, declaration validation and domain publication;
  `src/x86/runtime.rs` owns post-boot descriptor operations, flow selection
  and dispatch, translating `IrqDesc` trigger/polarity.
  `src/x86/discovery.rs` only decodes firmware and records issues; it
  maps no MMIO and programs no registers.
- No software interrupt routing between CPUs beyond the backend's
  `notify_cpu`.

## Scope

```text
drivers/platform/irq/
├── src/
│   ├── lib.rs      # architecture dispatch (cfg per target)
│   ├── gic/
│   │   ├── mod.rs  # AArch64: discovery, mapping, IntrManagerIf, NMI
│   │   ├── xlate.rs # GIC firmware interrupt-cell translation
│   │   ├── gicv2.rs # GICv2 backend (arm_gic_driver)
│   │   ├── gicv3.rs # GICv3 backend (arm_gic_driver)
│   │   ├── gicv3/affinity.rs # immediate ordinary SPI routing transaction
│   │   ├── nmi.rs  # GICv3 NMI window
│   │   └── spi.rs  # ordinary SPI flow operations and transaction adapters
│   ├── riscv/
│   │   ├── mod.rs       # RISC-V PLIC backend and provider wiring
│   │   ├── firmware.rs  # PLIC firmware discovery and binding declarations
│   │   ├── context.rs   # controller-owned logical-CPU context table
│   │   ├── registers.rs # register geometry and bounds-checked MMIO
│   │   ├── affinity.rs  # immediate ordinary-source routing transaction
│   │   └── legacy.rs    # explicit no-firmware context declarations
│   └── x86/
│       ├── mod.rs       # provider wiring, public surface, test attachment
│       ├── config.rs    # boot topology, validation, install, domain publication
│       ├── runtime.rs   # descriptor ops, flow selection, vector dispatch
│       └── discovery.rs # MADT IO-APIC/ISO decoding, no hardware access
└── Cargo.toml
```

## Architecture

```text
kirq (policy) ──IntrManagerIf──> irq-driver
                                   |
             +---------------------+----------------------+
             | aarch64           | riscv64              | x86_64
             v                   v                      v
        gic/                 riscv/                 x86/{config,runtime}.rs
   GicConfig::from_fwnode   PlicConfig::from_fwnode x86-apic crate
   (DT: interrupt-controller  (legacy.rs fallback) (multi-instance GSI
    + reg regions; v2: GICD+GICC,                    ranges; domain+pin)
     v3: GICD+GICR)
   iomap: gicd/gicc/gicr      iomap: "plic"
             |                   |
        gicv2/gicv3.rs (arm_gic_driver)   riscv/{registers,context}.rs
```

## Execution Context

Reproducible boot checks and their coverage limits are in [testing.md](testing.md).

- `init` / `init_from_fwnode` run once during platform bring-up,
  before device interrupts are enabled and include primary CPU-interface
  initialization. Each AP calls `init_current_cpu` once before activating local
  sources. The primary interrupt-preparation hook must not repeat it: local initialization
  resets priority/interface state and private IRQ enable/active bits.
  A per-CPU attempt flag rejects repeated local setup before destructive MMIO,
  including retries after failure. IRQ-enabled entry is rejected without
  consuming the attempt. This is not CPU hotplug or reset support.
  GICv2 banked SGI/PPI state is reset on each CPU before CPU-interface setup,
  as in Linux `gic_cpu_config`; this does not clear pending notifications or
  modify shared SPI state. SGI enable bits can be read-as-one/write-ignored,
  so preparation relies on CPU IRQ masking rather than disabled SGI readback.
  The existing CPU-interface driver resets all private priorities to `0xa0`;
  that per-CPU operation runs after releasing the global GIC lock.
- GIC initialization returns local CPU-interface errors to the platform caller
  as `GicInitError`, whose `Display` text names the failed check.
  GICv3 EOI-mode setup and PMR readiness publication occur only after successful
  local initialization. Global hardware may already be configured on failure;
  there is no rollback guarantee. Mandatory platform bring-up stops on error
  before activating sources rather than logging and continuing.
- In pseudo-NMI builds, successful GICv3 setup is followed by
  `pmr::initialize_masked` after releasing the controller lock. It installs the
  ordinary-IRQ blocking mask before publishing readiness and leaves DAIF.I
  set. CPU-interface setup's permissive PMR must not become the saved state of
  subsequent IRQ guards before runtime activation.
- `dispatch_irq` runs in IRQ context with interrupts enabled; the GICv3
  NMI path (`dispatch_nmi`) runs with interrupts disabled and no NMI
  window, relying on PMR protection.
- Controller configuration uses backend `SpinNoIrq` locks; claim/completion
  and the prepared GICv3 irq_work notifier use register operations directly.
- `prepare_local_irq_work` runs once per CPU during runtime interrupt-service
  initialization, after allocator and local GIC initialization, with migration
  excluded. It may allocate and take locks, but does not sleep. It must not be
  called from NMI, IRQ handlers, or early platform initialization.

## RISC-V Local Preparation And Activation

`init_current_cpu_context` selects the current CPU's context from the
controller's authoritative table, clears the supported source enable bits in
that context, then sets its PLIC threshold to zero under the same IRQ-safe
controller lock. Like Linux PLIC context setup, this removes inherited routes
before activation. It leaves global priorities, pending state and claim
completion unchanged. It returns an error when the CPU has no context, and
rejects a repeated preparation because that would disable live routes. Only
after the register transaction succeeds is the CPU recorded in the prepared
mask. Primary context failure is fatal boot; an AP failure aborts that CPU's
bring-up and prevents runtime readiness publication.
`activate_local_interrupts` only sets SSIE/STIE/SEIE, without repeating PLIC
preparation or opening the global SSTATUS.SIE gate. Runtime owns that final
gate and must install handlers before permitting delivery. Timer programming
remains a separate timer-driver operation. Claim, enable and completion read
the same context table, so the firmware and legacy paths cannot disagree about
the current context.

## Local irq_work Notification

GICv3 reserves ordinary Group 1 SGI15 for irq_work. Preparation rejects an
out-of-range SGI, a conflict with `IPI_IRQ`, or MPIDR Aff0 >= 16 (the sender
does not implement the optional range selector). A global `LazyInit<bool>`
serializes mapping preparation through `kirq::try_map`, followed by one
`kirq::try_register_percpu_startup` call, disabled by contract; the permanent
handler invokes `kirq::irq_work::handle_irq`. Registration and enablement share
a descriptor marked `IrqFlags::PER_CPU`, identifying the SGI's CPU-local hardware state.
The platform owns this mapping for kernel lifetime; APs reuse it. Mapping
preparation does not configure or enable hardware. If action installation fails,
the prepared mapping remains reserved rather than being reclaimed during boot.
Preparation or registration failure is retained and yields
`None`, without replacing an existing handler. No GIC lock is held while
entering the generic IRQ registration/control plane.

The dedicated registration requires resolved PerCpu flow, no installed action
and no enabled local slot. It configures only the calling CPU, never enabling
delivery automatically. Post-resolution rejection may retain descriptor
metadata, but does not publish the handler or configure hardware. Disabled slots
do not prove hardware quiescence or completion of previously admitted claims.
SGI15 remains an ordinary, kernel-lifetime IRQ; PerCpu registration grants no
automatic NMI or forwarding capability.

Each CPU disables its SGI, clears the hardware NMI attribute when supported,
sets edge configuration and ordinary priority `IRQ_THRESHOLD`, then enables
through `kirq::try_enable(desc, true)` so generic descriptor state and hardware
agree. The typed SGI15 descriptor selects `PerCpu`, including with `PER_CPU` set,
so enable state belongs to the calling CPU's slot, not a global enabled bit.
Every CPU must perform its own preparation and enablement; the shared handler's
registration does not enable another CPU's SGI. The existing boolean API forces
the local hardware enable write even when that CPU's slot is already enabled.
`try_enable_irq` is depth-aware rather than a force-enable replacement.
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

## x86 Vector Dispatch

The dedicated LAPIC spurious vector is consumed without EOI. The LAPIC error
vector delegates ESR capture and one EOI to `x86-apic`. Neither produces a
`PendingIrq` or enters generic Action dispatch; both retain the surrounding
`kirq::handle_irq` exit bookkeeping. This follows Linux's dedicated spurious
and error handlers, not its separate unused-vector fallback.

Timer/IPI vectors acknowledge early and use the local PerCpu flow. MSI-X
and IO-APIC claims transfer EOI to generic flow using a nonzero cookie. Edge flow
acknowledges under the Line lock before handlers; IO-APIC level Fasteoi completes
after handlers. Disabled and overlapping edge arrivals are masked where possible,
acknowledged and recorded pending for the existing running owner or later enable.

`prepare_regular_delivery` owns regular-vector classification and early EOI;
its result transfers only the outstanding completion cookie into `PendingIrq`.
Production classification resolves an IO-APIC vector through the backend's
`vector -> (instance, pin)` table and then to that instance's domain; an
in-window vector with no live route is acknowledged and dropped rather than
left in service. Tests exercise the production decision path with injected
classifiers and EOI operations: both IO-APIC range boundaries, routed and
unrouted vectors, MSI/local boundaries and exception vectors. They verify the
selected domain, classifier call count and early EOI count without issuing a
physical EOI.
Each accepted model delivery also passes through `complete_delivery`, proving
one EOI across preparation and completion, rather than only checking a cookie.
The dedicated spurious/error tests separately verify regular/error delegation.
A separate spurious test uses the real backend
through `kirq::handle_irq` to check return-context balance. Register sequencing
is tested in the vendored x2apic host suite.

`select_x86_flow` validates explicit IO-APIC/MSI domain and controller identity,
flags, source range and supported targeting. For IO-APIC it resolves the
descriptor's domain to an installed instance (the primary domain maps to
instance zero even before the published table exists), then reads that
instance's trigger for the local pin when firmware left it unspecified; a
requested trigger is configured before action publication. Level uses `FasteoiResend::Level`, edge uses hardware
retrigger. IO-APIC affinity changes and per-CPU flags are rejected. MSI requires
an allocated, message-bound vector; its mask belongs to the PCI device owner.
The public chip callbacks recheck source identity before hardware access.
Checked IO-APIC configuration propagates mask/readback failure without publishing
a new action. Invalid/changed retrigger routing is diagnosed and pending retained;
retry requires later management progress after the fault is corrected.

Only the fixed raw LAPIC timer/IPI identities select PerCpu. Other unbound or
foreign identities return `UnsupportedFlow` before register access. None of
the production GIC, PLIC or x86 selectors provides a Legacy flow fallback.
KIRQ's unpublished candidate may have no selected flow; that is not a runnable
software interrupt or a backend capability. Synthetic fixtures must arrange
their test binding explicitly.

This removal does not remove PCI legacy INTx, the boot-configuration route
lookup `X86IrqInitConfig::legacy_irq_desc`, or the direct infallible
`set_trigger` interface. Those names describe existing signaling or APIs, not
the removed KIRQ flow.

Linux references are `kernel/irq/chip.c::handle_edge_irq/handle_fasteoi_irq` and
`arch/x86/kernel/apic/vector.c::apic_retrigger_irq`: retrigger targets the recorded
CPU/vector under routing exclusion, not the CPU that happens to request replay.
Local timer/IPI state and MSI shutdown/vector-release ownership remain separate
work; request issuance is not proof that a remote handler has completed.

## GIC Source Admission

GICv2 and GICv3 initialization cache the exclusive standard INTID limit from
`GICD_TYPER.ITLinesNumber`: `min(32 * (ITLinesNumber + 1), 1020)`. This includes
the SGI/PPI prefix; it is not an SPI count to add to 32. GICv3's IDbits-based
`max_intid()` describes a wider identifier space and is not used for this bound.
Both backends read the existing driver's `typer_raw()` once during global setup;
admission needs neither a controller lock nor another MMIO read.

`select_gic_flow` validates resources that declare GIC through their controller
or root domain. It rejects conflicting controller/domain identity, unavailable
initialization, special IDs and IDs beyond the discovered standard range before
KIRQ publishes a mapping or installs an action. Checks precede narrowing from
`usize`; extended IDs and LPIs are not supported by this provider path.
Accepted ordinary SPIs use Fasteoi. Explicit GIC SGI/PPI sources select PerCpu,
including SGIs marked `PER_CPU` and virtual timer PPI 27. An explicit GIC SPI
marked `PER_CPU` returns `UnsupportedFlow`; this also
rejects SPI NMI registration, which adds that flag. Range
admission is separate from hardware trigger readback, masked configuration and
RWP synchronization; direct low-level calls still require validated hardware IDs.

The limit follows the standard-line distinction used by Linux's GIC drivers
(`GIC_LINE_NR` versus `GIC_ID_NR` in `irq-gic-v3.c`, and the 1020 clamp in
`gic_init_bases` in `irq-gic.c`). It does not prove every private ID is wired or
that firmware describes the right controller instance.

The raw identity compatibility path means no domain and an Unknown controller.
Raw private INTIDs in `0..32`, including 27, select PerCpu both with and without
the `PER_CPU` flag. The flag alone does not promote raw IDs outside that range.
This classification does not validate hardware range or wiring. Other raw and
foreign identities return `UnsupportedFlow`; a foreign domain is not
reinterpreted as GIC.

PerCpu flow selects CPU-local execution/control accounting, not an NMI or guest
forwarding capability. `percpu_nmi` still requires an explicit GIC root-domain
PPI, GICv3 and an available NMI mechanism. `percpu_forwarding` still requires an
explicit root-domain PPI on GICv2. Raw private IDs, SGIs and the `PER_CPU` flag
alone satisfy neither capability contract.

Within that explicit GIC range, standard SPIs (INTID >= 32) reject
`EdgeFalling` and `LevelLow` with `UnsupportedFlow` before resolution publishes
or updates a Line. The old edge/level-only programming must not silently turn
those requests into rising-edge or high-level configuration. This follows the
SPI restriction in Linux `gic_set_type` in both `irq-gic.c` and `irq-gic-v3.c`.
`EdgeRising`, `LevelHigh` and `Unknown(_)` select ordinary Fasteoi for eligible
SPIs. `Unknown(_)` preserves its unspecified-metadata behavior; it is not proof of supported hardware
configuration. Checked backend configuration adds the private-source rules
below; direct legacy `set_trigger` callers retain their infallible interface.
A private-registry regression checks that rejected SPI metadata
neither publishes a new mapping nor changes an existing Line's state.

### Checked GIC Configuration

`gicv2::configure` and `gicv3::configure` call `select_gic_flow` before
register access. They also bound admitted raw private IDs before narrowing to the
vendor `IntId`. Bounds checks precede the `Unknown` trigger no-op: a direct
oversized descriptor is rejected even when its trigger is unspecified. Priority
and NMI attributes remain caller policy; configuration changes neither.
The old infallible `set_trigger` entry points remain available for legacy callers
and do not acquire this checked contract.

For GICv2, an already matching PPI/SPI trigger succeeds without writes. A
change captures hardware enablement, temporarily masks an enabled source,
checks mask readback, writes the trigger and checks trigger readback, then
restores and checks enablement only on success. An initially disabled source
stays disabled. One existing `GIC` lock covers the entire transaction, including
readback and restoration. Failure requests disablement and returns
`ConfigurationFailed { hwirq }`; neither earlier writes nor hardware trigger
state are rolled back. Fixed rising-edge SGIs are accepted without programming;
other known SGI trigger requests are rejected before access. PPI polarity is
integration-defined; readback verifies edge/level only, not electrical polarity.

GICv3 private configuration is intentionally read-only: matching current-CPU
trigger readback succeeds, preserving fixed SGI/PPI and private NMI startup;
a mismatch returns `UnsupportedFlow` before mutation. The vendor global
`Gic::set_cfg` writes all redistributors for private IDs, and this adapter has
no public GICR RWP completion primitive. Neither behavior can be hidden behind
a local mask. Unknown triggers still make no readback guarantee.

GICv3 SPI changes follow the same locked transaction but wait for the vendor's
bounded `wait_for_distributor_rwp` before checking mask readback and programming
the trigger. This wait also runs when the initial enable read is clear, since
an earlier disable can still be pending. It covers distributor-tracked writes,
not ICFGR completion, private registers, handlers or active interrupts. Timeout
and readback failures return the single `ConfigurationFailed` variant. The
failure-path disable request has no completion wait: hardware state can remain
partial, and even successful trigger programming is not undone.

These functions do not sleep or allocate. They require initialized hardware,
must not run in NMI context or with `GIC` already held, and need no current
process. Private sources address the executing CPU; callers select that CPU
before entry, and the lock excludes local IRQs and migration during access.
Caller lifecycle serialization and device/handler quiescence remain separate
obligations. Restoring a hardware enable snapshot does not establish generic
flow-suppression or disable-depth correctness by itself.

Linux reference: `drivers/irqchip/irq-gic.c::gic_set_type`,
`irq-gic-v3.c::gic_set_type`, and `irq-gic-common.c::gic_configure_irq`, inspected
at local linux-stable commit `8d3ae59288f1e7d58d76558a6ee96d533bc5019f`.
Both chips declare `IRQCHIP_SET_TYPE_MASKED`. Linux accepts fixed rising SGIs
without writes and checks configuration readback, but tolerates PPI mismatch;
the checked GICv2 provider reports that mismatch instead of claiming success.

`ut-tests/gicv2_config.rs` and `ut-tests/gicv3_config.rs` use private fake
register adapters to test the production transaction's access ordering, enabled
and disabled success, ignored masks, trigger mismatch, restore failure and
private-source behavior. GICv3 tests inject distributor-wait failure; actual
poll-budget behavior belongs to the vendor driver's tests. Fake observations
cannot validate MMIO accessibility, physical delivery or write completion.

### Ordinary SPI Flow

GICv2 PPI forwarding is a separate, explicit capability. A per-CPU slot records
which PPIs delegate deactivation to a guest. Completion skips DIR only for those
bits, not because VMM is compiled or the INTID equals 27. Under the GIC guard,
`set_forwarding_state` reconstructs the banked active bit and changes the local
delegation bit. Host return clears both after the consumer stops/masks output.
This does not add SPI forwarding. Optional GICH/GICV regions are obtained from
the third/fourth DT `reg` entries; discovery rejects absent, overlapping,
unaligned, too-small and overflowing regions before exposing their addresses.
The `cfg(unittest)` PPI probe reads enabled/active/delegated state under the
existing GIC lock for consumer lifecycle tests; it is not a production API.
The accompanying unsafe `pend_local_ppi_for_test` admits only GICv2 PPIs and
sets their banked pending bit under that same lock. Consumer fixtures retain
source ownership and CPU/IRQ exclusion across injection, then drain real
delivery. This tests late events without substituting a synthetic handler call.

Validated standard SPIs select the static `gic_spi::CHIP` Fasteoi capability.
Shared and wakeup flags are accepted; MSI, unknown flags, explicit affinity and
low polarity are rejected. Ordinary SGI/PPI INTIDs, including forwarded virtual
timer PPI 27 and NMI-tagged PPIs, use PerCpu. Explicit GIC SPIs marked `PER_CPU`
are rejected with `UnsupportedFlow`, without an alternative flow.
The publicly reachable chip callbacks revalidate this binding before MMIO:
invalid mask/unmask inputs are ignored and invalid retrigger returns false.
NMI registration adds PER_CPU before selection. The only current VMM HW-LR
route is virtual timer PPI 27 (`kvmm::vdev::aarch64::irq_route`); it is outside
this scope. Adding SPI forwarding requires an explicit ownership contract and
selection exclusion first, not merely skipping DIR for a guessed INTID.

Mask/unmask use the source enable bit without changing priority or routing.
GICv3 waits for Distributor RWP after every SPI disable, including management
disable, under its existing GIC lock. Unmask does not wait. A bounded timeout
is logged after releasing the controller lock, and the infallible mask hook
returns, matching Linux `gic_mask_irq`/`gic_do_wait_for_rwp`. This is distinct
from checked configuration, which returns an error and suppresses publication.
Neither behavior proves hardware inactivity; KIRQ still suppresses ineligible
actions and completes any claim arriving after a failed hardware mask. GICv2
has no RWP wait. The maintained vendor accessor and its unresolved attribution
record remain documented in `vendor/README.md`.

The hardware-only retrigger writes ISPENDR through the vendor `set_pending` API
under the GIC lock. Acceptance means issuance, not completed delivery. It does
not read pending back (a remote CPU may already have acknowledged it), wait for
RWP (which does not track ISPENDR), change enablement, or complete a claim.
Explicit level sources rely on hardware reassertion; unknown-trigger and edge
pending use this hardware path, never BH software replay. Linux's GIC drivers
likewise require hardware IRQ-context service through `handle_enforce_irqctx`.

ACK and early priority-drop EOI remain in each backend. Generic Fasteoi retains
the original Line until the original claim's DIR completes, including rejected
arrivals. DIR uses the claim cookie on the claiming CPU, without the GIC lock;
software retrigger never creates or consumes that completion obligation.
Configuration and final delivery restoration remain serialized by Line control,
including the configuration-failure eligibility latch. An unchanged hardware
trigger does not bypass logical disable depth or authorize flow unmasking.

`ut-tests/gic_spi.rs` checks callback input rejection, the production helpers' disable/wait order, timeout
without restoration, enable-only unmask and pending-only retrigger.
`ut-tests/gic.rs` checks source/flag/polarity/routing selection, including private
and NMI exclusion. Core dispatch/resend tests check completion exactly once,
pending retention and configuration-failure suppression. These deterministic
models are not physical delivery, MMIO fault or hardware-quiescence proofs.

## Concurrency Model

- GIC backend state lives in `LazyInit<SpinNoIrq<Gic>>`; each backend's
  global `init` returns if the GIC is already initialized.
- PLIC state lives in `LazyInit<PlicController>`, which publishes the immutable
  source bound and logical-CPU context table together with its
  `state: SpinNoIrq<PlicState>` lock. `PlicState` contains the register window,
  per-source routing state and the prepared-CPU mask. Primary initialization
  installs this controller before per-CPU context preparation.
- `remember_config` asserts that a re-supplied GIC configuration is
  byte-identical to the initialized one, catching inconsistent platform
  init instead of silently reprogramming.
- PLIC claim and completion transactions use the same lock as enablement;
  the lock is not held across handler execution. Completion selects the current
  hart's context under that lock, checks the source enable bit, and temporarily
  enables a disabled source before completing and restoring its disabled state.
  This follows Linux `plic_irq_eoi`: the PLIC specification's Interrupt
  Completion rule ignores completion for a source disabled on that target.
  Logical KIRQ disable depth, priority and unrelated source bits are unchanged.
  The private `PlicCompletion` adapter tests this transaction against a model
  that rejects disabled completion; it does not emulate actual device delivery.

## Error And Panic Model

- Absence is `Option`: no available supported controller node makes
  `GicConfig::from_fwnode` yield `None`, and too few `reg` regions for the
  detected version are a `GicConfigError::MissingRegion`; `init_from_fwnode`
  returns both as an `Err` so the platform can fall back or abort with its own policy.
- MMIO mapping failures panic with the region name
  (`map_mmio_region`), and `remember_config` panics on a changed config —
  both are bring-up ordering/configuration bugs, not runtime conditions.
- PLIC access before initialization panics via the named `expect` on the
  global handle.

## Design Decisions

- One crate per architecture via `cfg` dispatch instead of trait
  objects: each target compiles only its own backend, and `kirq` sees a
  uniform `IntrManagerIf` provider either way.
- GIC discovery walks the device tree for a node the backend reports as an
  interrupt controller whose compatible strings map to a known
  version, then takes as many `reg` regions as that version requires
  (v2: GICD+GICC; v3: GICD+GICR) — matching the hardware layout instead
  of hard-coding addresses.
- The GICv3 NMI path is a separate `dispatch_nmi` entry: claim-time
  IRQ/NMI classification happens in the backend, keeping the PMR-based
  NMI window logic out of `kirq`.
  Both GIC NMI entry routes pass `PendingIrq` with `GIC_ROOT_DOMAIN`, the raw
  hwirq and the original completion cookie. KIRQ borrows the Domain's Line and
  NMI action through completion; the driver neither selects a handler by a
  global raw number nor owns its lifetime.
- PLIC keeps Linux-style constant IRQ bases (`S_SOFT`/`S_TIMER`/
  `S_EXT` offsets above the `INTC_IRQ_BASE` sentinel) and a skip
  sentinel for completion, since timer and IPI events need no PLIC
  claim/complete.

## PLIC Source Bounds

At primary initialization a discovered PLIC node must provide exactly one
nonzero `riscv,ndev` cell, at most the register layer's 1023-source limit.
Missing or malformed metadata on a discovered node stops boot rather than
selecting the legacy address fallback. Only absence of a compatible node uses
that fallback, with the register-layer capacity as its source bound; this does
not discover which sources physically exist. A private `PlicController` owns
the immutable bound, the context table and the IRQ-safe register lock,
published together through one `LazyInit`. Its methods own external-source
enable, priority and completion transactions; CPU-local context selection
remains inside the register lock.
Enable, priority and completion paths validate source
ids without truncation before accessing indexed registers. Completion cookie
zero remains the local timer/IPI no-completion sentinel.

Source validation is independent of context admission: the context table and
register aperture are validated before controller publication, and the legacy
fixed layout is confined to the no-firmware path. Flow suppression is described
separately below.

## Source Enable Identity

The `IntrManagerIf::enable` provider receives a normalized `IrqDesc`, just as
configuration does. GIC, PLIC and x86 providers currently select the same local
source using `desc.hwirq`; this interface preserves metadata without changing
register ordering, routing or priority policy. It does not add multi-controller
instance routing or make logical enable equivalent to temporary flow unmask.

## PLIC Effective Target

The first external-source enable selects the current CPU's prepared context
from the same authoritative table unless immediate affinity has already selected
a target. A bounded target table is allocated at primary initialization, before
controller publication, and shares the controller lock with registers.
Subsequent enable/disable uses that recorded context even when the management
task has migrated. Disable before first enable has no route to clear and does
not select a target. Completion still uses the claiming CPU's context, not the
management target.

The target table is indexed by validated hardware source, not reusable logical
IRQ. There is no allocation on enable/disable, affinity or completion. Enable
still sets configured priority to 6; flow masking controls when that policy
becomes effective in hardware. Affinity itself never writes priority, changes
the mask policy or changes KIRQ disable depth.

### PLIC Runtime Affinity

The provider exposes `IntrManagerIf::immediate_affinity` only for an ordinary
PLIC Fasteoi source that passes the same flow selection as dispatch. The
transaction runs under the existing Line exclusion and controller lock. It
offers only interrupt-ready candidates, then requires the candidate to be in
the controller's prepared-CPU mask and to have an authoritative context. A
candidate set with no such target is rejected before any register read or
write.

The old target context's actual enable bit is sampled. If the target changes,
the old bit is suppressed and verified, the new target is recorded, and the
new context is suppressed to remove stale firmware/prior-route enablement.
`Preserve` re-enables the new context only when the old context was actually
enabled; `KeepMasked` always leaves the new context disabled. A same-target
`Preserve` does not create a needless disable window. A same-target
`KeepMasked` still verifies suppression.

Failure handling is deliberately state-specific:

- Failure while suppressing the old context leaves the recorded target on the
  old context, so a later recovery disable closes that context. No new-context
  enable write occurred.
- Once the new target is recorded, any later suppression or enable failure
  leaves the target on the new context. The old context was already suppressed
  when that recording happened; recovery disable therefore closes the context
  whose enable write may have failed, without claiming the old route is restored.
- A post-write failure returns `HardwareFailure`; it does not claim source
  quiescence, restore delivery or complete an old claim. KIRQ owns admission
  closure and recovery sequencing.

Affinity does not complete or migrate an outstanding claim. A claim made on
the old CPU still completes on that CPU's context; the new context's enable
bit and the recorded target remain independent of that completion. The
deterministic model tests cover target selection, old/new suppression,
Preserve/KeepMasked, failure-target retention and unchanged priority/mask
policy. They do not prove physical delivery, hardware quiescence or multi-CPU
claim overlap.

## PLIC Fasteoi Capability And Priority

`select_flow` is a pure metadata check: no allocation, register access, target
selection or BH-service dependency. Standard PLIC selection requires the
permanently initialized controller, its PLIC domain and controller family, an
in-range source, and no per-CPU/MSI or unknown flags. Shared and wakeup-source
metadata are accepted. Trigger unknown or level-high and polarity unknown or
high are supported; explicit edge/low configurations and explicit affinity are
rejected with `UnsupportedFlow`. This selection does not prove level persistence
or eliminate the generic pending/resend obligation.

Only descriptors declaring PLIC through their domain or controller enter this
validation. Conflicting or incomplete PLIC identity is rejected, never silently
downgraded. On the RISC-V provider, local timer/software causes use PerCpu;
other non-PLIC descriptors return `UnsupportedFlow`, including
synthetic descriptors and foreign-domain directory mappings. PLIC selection before controller publication
returns an error rather than touching the boot-only `plic()` invariant. The
static `FasteoiChip` supplies real source mask/unmask functions; KIRQ must retain
the validated original binding and serialize these calls with Line state.

The bounded boot-allocated source table owns the effective target, optional
configured priority and mask state under the register lock. The first mask
captures hardware priority only when no policy has yet been recorded. Masking
writes priority zero without changing routing; repeated mask never captures its
own zero. `set_prio` updates configured priority but keeps hardware at zero while
masked. Explicit priority zero is valid policy, not an initialization sentinel.
Management enable retains the existing policy reset to 6 but does not clear a
flow mask; disable leaves priority policy and flow masking unchanged. Only
explicit unmask restores the latest configured policy. Completion changes
neither configured priority nor mask state and still uses the claiming context.

Generic flow owns the decision to release suppression, including final enable
and action replacement; a normal non-oneshot tail must not unconditionally
unmask. Replay eligibility, retention and withdrawal remain KIRQ obligations,
not services provided by this capability. Model tests exercise the production
selection and priority/route helpers, including masked updates, zero policy,
enable-default writes and completion restoration; they do not validate physical
delivery or the generic replay lifecycle.

## Dispatch Contract

`IntrManagerIf` dispatch and completion methods are unsafe provider contracts.
KIRQ supplies initialized entry CPU, IRQ/migration exclusion and IRQ/NMI context;
the irqchip acknowledges and classifies the actual source, then transfers its
unique completion obligation through unsafe `PendingIrq::new`. The matching
generic entry safely consumes that owner without repeating the unsafe call.
The raw provider qualifier is preserved by `kiface`, not replaced with
additional controller locks. Private GIC completion is not a driver-facing API.
