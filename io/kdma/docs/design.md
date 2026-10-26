# kdma — Design

## Purpose

`kdma` is the kernel's DMA memory layer: it allocates coherent memory that
devices can read and write, maps existing CPU buffers into temporary DMA
transactions through a pooled coherent bounce area, and defines the bus
address type and physical↔bus conversion shared by all DMA-capable
drivers. It exposes two flat unsafe entry pairs —
`allocate_dma_memory`/`deallocate_dma_memory` and
`map_dma_buffer`/`unmap_dma_buffer` and consumes the MM-owned `kiface_mm_api::DmaMappingIf` capability for mapping
attribute transitions.

## Responsibilities

- Coherent allocation: `allocate_dma_memory(layout)` carves page-granular
  regions from the global allocator (`kalloc`, `UsageKind::Dma`), applies
  `READ | WRITE | UNCACHED | SHARED` mapping flags through
  `kiface_mm_api::DmaMappingIf::set_mode`, and registers the range with the platform
  (`kplat::dma::prepare`). `deallocate_dma_memory` reverses all three
  steps.
- Temporary mappings: `map_dma_buffer(buffer, direction)` stages the
  buffer through the coherent bounce pool (copy-in unless the direction is
  `DeviceToDriver`), tracks the mapping in `active_mappings` keyed by bus
  address, and fences (`SeqCst`) around the copies. `unmap_dma_buffer`
  copies data back (unless `DriverToDevice`), removes the tracking entry,
  and recycles the bounce buffer.
- Address translation: `p2b` / `b2p` over the linear
  `kbuild_config::PHYS_BUS_OFFSET` mapping, and `DmaBusAddress` as the
  `u64` bus address wrapper handed to devices.
- Result reporting: `DmaInfo { cpu_addr, bus_addr }` pairs the CPU view
  with the device view of each region.
- Bounce pool: a lazily created 1024-page coherent region sub-allocated
  by a TLSF byte allocator (`bounce_pool`), sized once from the same
  coherent path.
- Mapping attributes: kiface-mm-api accepts only Coherent or Normal mode; memspace
  chooses the corresponding flags and owns page-table locking. DMA owns page
  allocation and platform sharing, not arbitrary page-table permissions.

## Non-Responsibilities

- No IOMMU management: bus addresses are direct physical+offset values;
  IOVA translation and IOMMU programming are out of scope.
- No cache maintenance beyond the chosen mapping attributes: coherence is
  achieved with `UNCACHED` mappings plus fences, not architecture cache
  flush calls.
- No streaming scatter-gather: `map_dma_buffer` handles one contiguous
  slice per mapping; descriptors chaining multiple buffers live in
  drivers.
- No device ownership policy: the crate cannot tell which device may
  access which bus address; that obligation stays with callers (see the
  `# Safety` contracts).

## Scope

```text
io/kdma/
├── src/
│   ├── lib.rs          # public unsafe API, DmaBusAddress, DmaInfo, p2b/b2p
│   ├── dma.rs          # DmaAllocator, coherent paths,
│   │                   # bounce mapping bookkeeping, dma-trace
│   └── bounce_pool.rs  # TLSF-backed coherent bounce pool
└── Cargo.toml
```

## Architecture

```text
drivers
  |  allocate_dma_memory(layout)          map_dma_buffer(buf, dir)
  v                                              |
SpinNoIrq<DmaAllocator>  <--- one global lock ---+
  |  alloc_coherent_pages                             |
  |    kalloc alloc_dma_pages (UsageKind::Dma)        v
  |    kiface_mm_api::DmaMappingIf::set_mode (UNCACHED|SHARED)   BouncePool (lazy, 1024 pages)
  |    kplat::dma::prepare / release                 TLSF byte allocator
  |    p2b -> DmaInfo{cpu_addr, bus_addr}            copy-in/out + SeqCst fences
  |                                           active_mappings: BTreeMap<bus, BounceMapping>
```

## Execution Context

- All public functions take a `SpinNoIrq` lock: callers must not sleep
  while holding the API, and the functions are safe from any context
  where spin locks are legal (task context; not from NMI-level code).
- Coherent allocation participates in the global page allocator and page
  table updates, so it must run after `kalloc` and the address-space
  subsystem are initialized; first use of the bounce path lazily
  allocates the pool through the same path.
- `map_dma_buffer`/`unmap_dma_buffer` copy up to the buffer length on the
  calling thread; they are intended for driver submit/completion paths.
- The `dma-trace` feature adds allocation-site bookkeeping for
  debugging; it is off in production builds.

## Concurrency Model

- One global `SpinNoIrq<DmaAllocator>` serializes coherent allocation,
  bounce mapping, and the mapping table; critical sections are short
  (page allocation, memcpy, BTreeMap ops).
- `active_mappings` makes double-map of one bus address an error
  (`AllocError::InvalidInput`) and keeps `unmap` exact-match; unmapping an
  inactive address panics — that is a driver contract violation, not a
  runtime condition.
- Memory ordering: explicit `fence(SeqCst)` before device handoff and
  after device ownership returns, bracketing the bounce copies; UNCACHED
  attributes make the coherent window itself uncached for the device.

## Error Model

- `AllocResult` (from `alloc-engine`): `NoMemory` when pages, mapping
  flag updates, or platform prepare fail; `InvalidInput` for zero-length
  mappings or an already-active bus address.
- Panics mark contract violations: unmapping a non-active address,
  length mismatch between map and unmap, bounce recycling outside the
  pool, and missing pool during recycle.
- `deallocate_dma_memory` ignores (logs under `dma-trace`) mismatched
  frees instead of corrupting the allocator — the trace layer detects and
  skips duplicate or inconsistent records.

## External Boundary And Inputs

- Device-visible bus addresses are the trust boundary: the `# Safety`
  contracts require callers to expose an allocation's bus address only to
  devices allowed to DMA that buffer, and to stop device access before
  `unmap_dma_buffer`.
- The bounce pool stages all temporary mappings, so driver-owned buffers
  are never directly exposed to devices today; this bounds the blast
  radius of a mis-programmed device to the 1024-page coherent pool.
- Platform integration (`kplat::dma::prepare`/`release`) treats each
  page-granular coherent region as shareable; the `SHARED` flag keeps
  TEE/platform share-unshare hooks balanced per allocation.

## Unsafe Code

All unsafe surface is concentrated in four public functions and their
internal forwardings, each with `# Safety` sections and matching
`SAFETY:` comments:

- `allocate_dma_memory` / `deallocate_dma_memory` — exact-once free with
  identical layout; no forged or offset CPU addresses; bus addresses only
  to permitted devices.
- `map_dma_buffer` / `unmap_dma_buffer` — buffer stays live and
  exclusively owned between the calls; device access stops before
  unmap; exact-once unmap with the matching address and length.
- Internal unsafe blocks: `NonNull::new_unchecked` on freshly allocated
  non-null pages, and `copy_nonoverlapping` between distinct live regions
  of validated length.

## Design Decisions

- Bounce-staged temporary mappings behind a stable API: staging through
  pooled coherent memory keeps driver code simple and safe today; a
  direct-map fast path can be reintroduced later without touching
  drivers.
- Page-granular coherent allocations: page-table attribute updates and
  platform prepare/release hooks stay balanced per allocation, avoiding
  partial-page flag aliasing between coherent and cached uses.
- `kiface_mm_api::DmaMappingIf` via `kiface`: the address-space owner implements flag
  updates as a provider, breaking the historical
  `kdma -> mm -> ... -> kdma` dependency cycle while keeping the kernel
  build platform-agnostic.
- Tracking map by bus address in a `BTreeMap`: double-mapping and
  mismatched unmaps are the classic DMA driver bugs; detecting them at
  the boundary turns silent memory corruption into a loud error or panic.
- `dma-trace` feature: coherent allocations record their call site and
  detect duplicate or mismatched frees at runtime, which is the cheapest
  way to debug DMA leaks without a full MMU tracer.
