# of — Design

## Purpose

`of` is the kernel's device-tree front end. It initializes one global DTB
handle from the bootloader pointer and layers semantic, ready-to-use
accessors on top of the structural parser `rs_fdtree`: chosen data, CPU
nodes, memory and reserved-memory regions, interrupt specifiers decoded per
controller kind, generic PCI host bridge facts, the PMU IRQ, and the
`syscon-poweroff` register.

## Responsibilities

- Own the boot DTB: `init_device_tree_ptr` validates the blob with
  `rs_fdtree::LinuxFdt` and stores it in a crate-global slot; `fdt()` reads
  it back; `FirmwareInitError` reports a null pointer or a parse failure.
- Expose tree queries: `find_node`, `find_compatible`, `resolve_node`
  (paths and aliases), `chosen`, `chosen_bootargs`, `chosen_stdout_path`,
  `root_model`, `root_compatible`, `dtb_total_size`.
- Decode interrupts: `first_interrupt_desc` resolves `interrupts` /
  `interrupts-extended`, follows `interrupt-parent` phandles, and maps
  controller-specific cells to `InterruptInfo` (GIC SPI/PPI numbering,
  trigger flags; PLIC single-cell). `generic_pci_legacy_interrupt` walks
  `interrupt-map` / `interrupt-map-mask` for a (bus, device, function, pin)
  key. `pmu_irq_or` resolves the ARM CPU PMU IRQ once and caches it.
- Extract PCI host facts: `generic_pci_host_info` (CAM vs ECAM, config
  window, bus range) and `generic_pci_non_prefetchable_mem_range`
  (`ranges` parsing, preferring the non-prefetchable window).
- Extract memory layout: `read_memory_regions`,
  `read_reserved_memory_regions` (memreserve plus `reserved-memory`
  children), and `read_named_reserved_memory_regions`, each returning a
  fixed-capacity array plus a used count; `*_from_ptr` unsafe variants work
  before global initialization.
- Describe the power-off register: `syscon_poweroff` resolves the
  `syscon-poweroff` node's regmap phandle, offset, and value into a
  `SysconControl` physical address plus write value.
- Provide CPU node helpers: `is_cpu_node`, `is_enabled_cpu_node`,
  `cpu_node_reg`, `enabled_cpu_nodes`, and `dice_region` /
  `interrupt_controller` passthroughs.

## Non-Responsibilities

- No DTB parsing itself: structure parsing, node iteration, and property
  bytes belong to `rs_fdtree`; this crate only interprets them.
- No device instantiation or driver binding: callers turn nodes into
  devices; `kdriver` owns probing.
- No interrupt controller programming: decoded `InterruptInfo` is data for
  `kirq`-based platform init, not a controller driver.
- No MMIO mapping and no memory adoption: region arrays describe physical
  addresses; `memspace` and MM init own mapping and page allocation.
- No ACPI: on ACPI platforms the parallel crate is `acpi`.

## Scope

```text
drivers/platform/of/
├── src/
│   └── lib.rs      # global DTB, semantic accessors, PCI/interrupt/memory decoding
├── ut-tests/
│   └── of_unittest.rs  # unit tests (unittest harness)
└── Cargo.toml
```

## Architecture

```text
bootloader DTB pointer
      |  unsafe init_device_tree_ptr / *_from_ptr
      v
LazyInit<LinuxFdt<'static>>  (one global DTB, lifetime 'static)
      |
  rs_fdtree structural view (nodes, properties, phandles)
      |
  semantic decoding in `of`:
    interrupts (GIC/PLIC cells, interrupt-map) -> InterruptInfo
    PCI host (reg, ranges, bus-range, CAM/ECAM) -> PciHostInfo/PciRangeInfo
    memory (/memory, memreserve, reserved-memory) -> MemoryRegion[N]
    chosen / model / compatible / syscon-poweroff -> plain data
```

Re-exports `rs_fdtree::{Chosen, Dice, FdtError, FdtNode,
InterruptController, LinuxFdt, MemoryRegion, NodeProperty, RegIter}` so
callers need only this crate.

The global PCI accessor delegates to an explicit-tree parser so unit tests
exercise the production decoder with isolated DTBs. Both access paths share
one phandle lookup rule, including the `linux,phandle` fallback. PCI host
information, memory windows and INTx routing also share the same host lookup:
prefer `pci-host-ecam-generic`, then `pci-host-cam-generic`.

The PCI interrupt-map parent buffer holds up to 16 cells: a controller with
2 address cells and 3 interrupt cells consumes 5. This follows the Linux
`MAX_PHANDLE_ARGS` capacity policy. Larger parent specifiers produce a warning
and return `None`. The child specifier must have three address cells and one
interrupt cell, checked before key-size arithmetic or decoding into the 4-cell
buffers. Other child layouts return `None`. Tests cover unsupported child
interrupt counts, a 5-cell GIC parent and the 16/17-cell parent boundary.

## Execution Context

- Early boot. `init_device_tree_ptr` runs once on the boot hart before any
  accessor; the `# Safety` contract requires the DTB blob to remain valid
  and accessible for the whole program lifetime (the bootloader buffer must
  not be reused or unmapped).
- The `*_from_ptr` readers exist for the window before global init is
  possible and touch no shared state.
- No allocator use on accessor paths (fixed-size `[MemoryRegion; N]`
  returns); no threads or scheduler required.
- `pmu_irq_or` is documented for hot-path use: it reads an
  `AtomicUsize` cache (acquire/release) and only walks the tree once.

## Concurrency Model

- One-shot `LazyInit` global: first initializer wins; accessors before init
  return `None` rather than blocking or panicking.
- `PMU_IRQ_CACHE` is a relaxed-use single-word cache: `0` means unresolved,
  a resolved non-zero IRQ is stored with `Release` and read with `Acquire`;
  resolution is idempotent, so a redundant re-resolve is harmless.
- All accessors take `&'static` snapshots of an immutable tree; no locking.

## Error Model

- Initialization: `Result<(), FirmwareInitError>` with `MissingDeviceTreePtr`
  and `BadDeviceTree(FdtError)`.
- Everything else: `Option` — absent nodes, properties, phandles, or
  undecodable cells yield `None`; malformed sizes fall back to defaults
  where the spec allows (for example `bus-range` defaults to `[0, 0xff]`,
  `#address-cells` to 1 or 2).
- No panics on malformed firmware data; undecodable input degrades to
  `None` or `Unknown` variants (`InterruptTrigger::Unknown`,
  `InterruptControllerKind::Unknown`).

## Firmware Trust Boundary

Device-tree contents are external input:

- Structural validity is delegated to `rs_fdtree` validation at
  `LinuxFdt::from_ptr` time (`FdtError` surfaced as
  `FirmwareInitError::BadDeviceTree`).
- Cell-level decoding is length-checked everywhere: property byte slices
  must cover `N * 4` bytes, interrupt-map entries are bounds-checked per
  step, and `ranges` chunks are validated before indexing.
- Unknown encodings are surfaced as `Unknown` values instead of guesses.
- Residual risk: a valid DTB that lies about addresses or IRQ numbers is
  indistinguishable from truth here; platform policy decides whether a
  missing value is fatal.

## Unsafe Code

Four unsafe entry points, each with a `# Safety` section and a matching
`SAFETY:` comment at the single unsafe block:

- `init_device_tree_ptr(ptr)` — fabricates `LinuxFdt<'static>` from the
  boot pointer; the caller guarantees the blob stays readable forever.
- `dtb_total_size_from_ptr(ptr)` — same contract, lifetime limited to the
  call.
- `read_memory_regions_from_ptr(ptr)` /
  `read_reserved_memory_regions_from_ptr(ptr)` — same contract, no global
  state touched.

The `'static` lifetime is the deliberate soundness bargain: it is valid
only because the boot DTB lives in memory the kernel never reclaims.

## Design Decisions

- Thin semantic layer over `rs_fdtree` instead of a private parser: one
  structural implementation, one set of fuzz-resistant DTB checks, and this
  crate stays focused on kernel-specific decoding.
- Global `LazyInit` DTB plus `*_from_ptr` escape hatches: most code wants a
  plain `fdt()`; the earliest memory-discovery phase needs to read regions
  before a global can be initialized, without duplicating parsers.
- Fixed-capacity region arrays (`const N`) instead of `Vec`: memory
  discovery runs before the allocator exists.
- GIC cell semantics (SPI base 32, PPI base 16, trigger flag bits) encoded
  here so every driver gets identical IRQ numbering instead of each
  re-deriving it from raw cells.
- PMU IRQ cached in an atomic: the PMU IRQ is read on every profiling
  tick; a one-time tree walk with an acquire/release cache keeps that path
  branch-cheap and lock-free.
- `syscon-poweroff` support without ACPI: direct kernel boot on machines
  such as the LoongArch virt still needs a firmware-declared power-off
  register; the DT node is that declaration.
