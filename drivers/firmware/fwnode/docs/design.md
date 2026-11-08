# fwnode — Design

## Purpose

`fwnode` defines the backend-neutral contract for firmware descriptions.
Device Tree, ACPI objects and static board tables all describe the same facts
— nodes, properties, translated MMIO resources, interrupt specifiers and
phandle-like references — but in different representations. Kernel IRQ
infrastructure and consumers such as interrupt-controller drivers should
depend on those facts, not on a firmware format. This crate is that seam.

The crate lives under `util/` because it is pure shared vocabulary: it has no
kernel, architecture or driver-framework dependency. Both `kirq` and
controller drivers may depend on it without weakening the driver-subsystem
boundary, which forbids kernel subsystems from depending on driver
capability contracts such as `driver_os_api`.

It is deliberately small:

- no firmware parser and no dependency on `rs_fdtree` or `acpi`;
- no allocation, `no_std`, and no `unsafe`;
- no kernel policy: no IRQ mapping, MMIO mapping, driver registration or
  resource acquisition;
- no dynamic backend registry. A backend is an ordinary borrowed value chosen
  by the platform integration.

## Scope

```text
drivers/firmware/fwnode/
├── src/
│   └── lib.rs          # Contract types and object-safe backend trait
├── tests/
│   ├── dma.rs          # Backend DMA-fact defaults
│   └── property.rs     # Host tests for value interpretation and capacity
└── Cargo.toml
```

## Type Roles

- `FwNodeId(u32)` is an opaque backend-local identity. The encoding may be a
  device-tree structure offset or a backend table index; consumers must not
  interpret it. `raw()` exists for diagnostics and for handing the value back
  to the same backend.
- `FwNodeOps<'a>` is the object-safe trait a backend implements. It owns all
  access to the underlying firmware representation. Methods return borrowed
  data with the backend's lifetime.
- `FwNode<'a>` is a copyable handle: a `FwNodeId` plus `&dyn FwNodeOps<'a>`.
  Equality compares both the identity and the backend identity, so handles
  from different backends with numerically equal IDs do not compare equal.
- `FwSource` reports whether a backend describes a device tree, ACPI, or a
  static table. It is provenance metadata for diagnostics and resource
  classification, not a second identity system.
- `FwNode::next_dfs`, `children`, `descendants` and `firmware_ids` are
  convenience iterators built only from `parent`, `first_child`,
  `next_sibling` and `firmware_id`. They add no state and no allocation.
- `FwNode::is_interrupt_controller` asks the backend whether firmware declares
  the node as an interrupt controller. Consumers must not test a firmware
  property name themselves; the device-tree backend answers from its own
  controller declaration, and future backends use their native representation.
- `FwProperty` / `FwPropertyValue` carry a property in its firmware
  representation. Accessors (`as_u32`, `as_u64`, `as_str`, `as_bool`) interpret
  only unambiguous encodings and return `FwError::InvalidValue` otherwise.
- `FwParams` is a fixed-capacity array of 32-bit cells. The capacity matches
  Linux's `IRQ_DOMAIN_IRQ_SPEC_PARAMS` (16), covering current interrupt and
  reference bindings without a heap.
- `FwMmioResource` is one already-translated CPU-address resource.
- `FwDmaRange` is one local bus-to-parent DMA window. `FwDmaRanges` reports
  whether the node has no declaration, an empty identity declaration, or a
  count of explicit windows; `FwNode::dma_range(index)` reads one validated
  window. These are local facts only: no ancestor translation, device mask,
  coherent default, IOMMU routing or host policy is applied in this contract.
- `FwNode::dma_window()` asks the backend for the device's already-resolved
  window (`FwDmaWindow`: bus start, CPU start, size) after its native DMA
  topology rules have been applied. Absence — including no nonempty
  declaration on the DMA-parent path — is `Ok(None)`, never an error.
  `FwDmaError` distinguishes firmware read errors from multi-window or
  IOMMU/interconnect topology, untranslatable windows and invalid windows. The
  contract exposes the result; it does not define how a backend derives it.
- `FwNode::dma_effective_coherency()` returns the effective coherency
  declaration, including backend-native inheritance. `Ok(None)` still means
  "nothing declares coherency"; applying a platform default remains the
  consumer's decision.
- `FwIrqSpec` and `FwReference` pair a target `FwNode` with raw argument
  cells.

## Contract Boundary

The contract describes firmware facts, not Linux `irq_domain` behavior.
`FwIrqSpec` names the controller and preserves its binding cells in firmware
order; the controller driver owns the meaning of those cells. Likewise a
`FwReference` leaves the argument count to the referenced binding
(`#clock-cells`, `#reset-cells`, and so on), supplied by the caller.

Backends are responsible for their own parsing and validation:

- absence is `Ok(None)`, except DMA range state, which keeps `Absent`
  distinct from an empty `Identity` declaration;
- malformed content is an error, never silently `false` or empty;
- unknown handles are `FwError::InvalidNode`;
- an address encoding the backend cannot translate, or a DMA fact the
  backend does not implement, is `FwError::Unsupported`;
- a destination that cannot hold the value is `FwError::BufferTooSmall`.

DMA coherency follows the same rule: `Ok(None)` means the node declares
neither coherent nor noncoherent (after any backend-native inheritance),
while a backend without DMA facts returns `FwError::Unsupported`. The contract
supplies no default: a consumer may apply an explicit, scoped platform policy
to `Ok(None)` (for example an architecture's `ARCH_DMA_DEFAULT_COHERENT`), but
it must not treat absence as a generic default, and `Unsupported` or any other
error stays an error and is never converted into absence. Resolved windows use
`FwDmaError::Firmware(FwError::Unsupported)` for the same purpose.

This distinction lets consumers enumerate resources with `Ok(None)` while
still surfacing corruption.

## Lifetimes and Concurrency

A backend is shared immutably and must be `Sync`. Every `FwNode` and every
borrowed value it returns is valid only while the backend lives. Handles
contain no mutable state, so they can be copied freely; concurrently reading
the same backend is allowed by the trait bound. The contract provides no
interior mutability, caching or invalidation protocol — a backend may cache
internally, but observable results must remain consistent.

## Execution Context

The crate is context-free: it performs no I/O, allocates nothing and takes no
locks. It is usable before the allocator exists. Consumers that call
backend methods which allocate must respect that backend's own execution
constraints; the device-tree backend documents its lazy index separately.

## Error Model

`FwError` is a closed enum of five structural failures. It intentionally does
not carry backend diagnostics or firmware text: consumers decide policy, and
the backend logs its own details. No method panics for invalid firmware data
or for an unknown handle.
