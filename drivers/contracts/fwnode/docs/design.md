# fwnode — Design

## Purpose

`fwnode` defines the backend-neutral contract for firmware descriptions.
Device Tree, ACPI objects and static board tables all describe the same facts
— nodes, properties, translated MMIO resources, interrupt specifiers and
phandle-like references — but in different representations. Consumers such as
interrupt-controller drivers should depend on those facts, not on a firmware
format. This crate is that seam.

It is deliberately small:

- no firmware parser and no dependency on `rs_fdtree` or `acpi`;
- no allocation, `no_std`, and no `unsafe`;
- no kernel policy: no IRQ mapping, MMIO mapping, driver registration or
  resource acquisition;
- no dynamic backend registry. A backend is an ordinary borrowed value chosen
  by the platform integration.

## Scope

```text
drivers/contracts/fwnode/
├── src/
│   └── lib.rs          # Contract types and object-safe backend trait
├── tests/
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
- `FwNode::next_dfs`, `children`, `descendants` and `firmware_ids` are
  convenience iterators built only from `parent`, `first_child`,
  `next_sibling` and `firmware_id`. They add no state and no allocation.
- `FwProperty` / `FwPropertyValue` carry a property in its firmware
  representation. Accessors (`as_u32`, `as_u64`, `as_str`, `as_bool`) interpret
  only unambiguous encodings and return `FwError::InvalidValue` otherwise.
- `FwParams` is a fixed-capacity array of 32-bit cells. The capacity matches
  Linux's `IRQ_DOMAIN_IRQ_SPEC_PARAMS` (16), covering current interrupt and
  reference bindings without a heap.
- `FwMmioResource` is one already-translated CPU-address resource.
- `FwIrqSpec` and `FwReference` pair a target `FwNode` with raw argument
  cells.

## Contract Boundary

The contract describes firmware facts, not Linux `irq_domain` behavior.
`FwIrqSpec` names the controller and preserves its binding cells in firmware
order; the controller driver owns the meaning of those cells. Likewise a
`FwReference` leaves the argument count to the referenced binding
(`#clock-cells`, `#reset-cells`, and so on), supplied by the caller.

Backends are responsible for their own parsing and validation:

- absence is `Ok(None)`;
- malformed content is an error, never silently `false` or empty;
- unknown handles are `FwError::InvalidNode`;
- an address encoding the backend cannot translate is
  `FwError::Unsupported`;
- a destination that cannot hold the value is `FwError::BufferTooSmall`.

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
