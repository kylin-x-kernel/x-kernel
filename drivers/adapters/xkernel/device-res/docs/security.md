# device-res-xkernel — Security And Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs`
(`XKernelResourceProvider`), `src/mmio.rs`, `src/dma.rs`, `src/memory.rs`,
`src/irq.rs`, and `src/time.rs`. No modules are
excluded; the crate adds no state of its own and forwards to `memspace`
/ `kirq` / `kdma` / `kalloc`, whose own security documents cover those subsystems.


## Trust Model

```text
device-res provider contract
        │ validated resource descriptors and handler objects
        ▼
device-res-xkernel
        │
        ├── memspace MMIO mapping
        ├── kirq IRQ registration / MSI-X
        └── kdma DMA allocation / mapping
```

`device-res-xkernel` trusts:

- `memspace::iomap_device` to reject invalid MMIO physical ranges;
- `kirq` to validate IRQ descriptors, manage the shared action
  lifecycle, in-flight synchronization, and teardown;
- `kdma` to return paired coherent/streaming DMA allocation metadata;
- `khal::time::monotonic_time()` to return a monotonic, never-backwards
  time;
- the driver remove path to have stopped device DMA and interrupt sources
  before devres cleanup runs.

The crate exposes no driver-facing `devm_*` helpers and never installs a
global provider itself; resource acquisition must go through the
`device_res` provider contract. `kdriver::resource` holds the static
`XKernelResourceProvider` instance and passes it explicitly to
`device_res::devm_*_with_provider()` and to internal adaptation paths
such as VirtIO PCI/MSI-X that need direct provider capabilities.

## Unsafe Boundaries

### DMA allocation

`alloc_coherent()` validates the `DmaSpec` through
`Layout::from_size_align()` before calling `kdma::allocate_dma_memory()`.

Invariants:

- the layout is non-zero with a legal alignment;
- the returned buffer is owned exclusively by a `device_res::DmaCoherent`
  or a devres cleanup;
- release rebuilds the layout from the same `DmaSpec`.

### DMA free

`free_coherent()` rebuilds `kdma::DmaInfo` from the `DmaAllocation` and
calls `kdma::deallocate_dma_memory()`.

Invariants:

- the `DmaAllocation` came from this provider's `alloc_coherent()`;
- each allocation is freed exactly once;
- the driver has stopped any device DMA that could touch the buffer.

### Streaming DMA map/unmap

`map_streaming()` and `unmap_streaming()` call the `kdma` streaming API
according to `DmaDirection`.

Invariants:

- the caller's buffer remains valid for the mapping lifetime;
- `unmap_streaming()` consumes the same provider-returned `DmaMapping`;
- the direction matches the original mapping.

## Concurrency And Lifecycle

- The provider itself has no mutable shared state; `kdriver::resource`
  holds the static provider reference and passes it explicitly at
  acquisition time.
- IRQ action-list synchronization is protected by `kirq`.
- `TimeOp::monotonic_time()` keeps no crate state and forwards to the
  X-Kernel time source.
- Devres cleanup runs in `DeviceObject` LIFO order; releasing an IRQ
  waits through `kirq` for stale hardirq snapshots to drain.

## Known Limitations

- `device_res` has reserved the threaded IRQ provider contract, but the
  current X-Kernel provider, built on the mainline `kirq`, supports only
  shared hardirq requests; threaded requests still return
  `ResError::Unsupported`.
- MSI-X is implemented on the x86_64 backend only; other architectures
  return `ResError::Unsupported`.

## Ordinary CPU allocation

The unsafe `CpuMemoryOp` implementation relies on the ordinary allocator's
unique allocation, `Layout` alignment and internal synchronization. Its
nonempty requests use the inherent fallible allocation entry, and its release
requires the original live pointer/layout with no remaining borrows. The safe
`CpuBuffer` owner enforces the pairing. This path does not create DMA resources
or change mapping flags; drivers must separately perform any DMA/cache work.
Tests exercise this host path at small, bounded-staging and larger-alignment
sizes. Exhaustion is injected at the capability boundary rather than forcing
the kernel out of memory.
