# kdma — Design

The bounce-pool guest fixture retains its original Vec backing without resizing
or relocating it after registration. The pool and its allocations are retired
before that buffer is released; no boxed-slice conversion is needed.

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
  buffer through the coherent bounce pool (copy-in for every direction,
  including `DeviceToDriver`), tracks the mapping in `active_mappings` keyed
  by bus address, and fences (`SeqCst`) around the copies.
  `unmap_dma_buffer` copies data back (unless `DriverToDevice`), removes the
  tracking entry, and recycles the bounce buffer. `cancel_dma_mapping`
  releases a mapping the device never received without any copy-back, for
  rollback after a provider-side address check fails.
- Address translation: `p2b` / `b2p` over the linear
  `kbuild_config::PHYS_BUS_OFFSET` mapping, and `DmaBusAddress` as the
  `u64` bus address wrapper handed to devices.
- Result reporting: `DmaInfo { cpu_addr, bus_addr }` pairs the CPU view
  with the device view of each region.
- Bounce pool: a lazily created 1024-page coherent region sub-allocated
  by a TLSF byte allocator (`bounce_pool`), sized once from the same
  coherent path. The TLSF control block is const-initialized inside the
  global `DmaAllocator`; first use registers the region in place instead
  of constructing and moving the multi-KiB allocator on the caller's stack.
  The optional region records this second phase under the existing allocator
  lock; a value-returning `Once<BouncePool>` initializer would still construct
  the large control block at runtime before publishing it.
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
allocate_dma_memory(layout)
  -> allocate DMA pages and set UNCACHED|SHARED mapping
  -> confirm detached TLB invalidation
  -> lock allocator, prepare platform DMA, publish DmaInfo

map_dma_buffer(buf, dir)
  -> ensure bounce pool, then lock allocator
  -> TLSF allocation, copy-in/out, active_mappings bookkeeping
```

## Execution Context

- All public functions take a `SpinNoIrq` lock: callers must not sleep
  while holding the API, and the functions are safe from any context
  where spin locks are legal (task context; not from NMI-level code).
- A coherent mode change detaches its remote TLB invalidation. Allocation confirms it before
  taking the allocator lock for platform preparation and publication. A CPU
  waiting for that lock with interrupts disabled cannot acknowledge the
  shootdown. A completion that cannot confirm the invalidation is logged; the
  mode change is not silently assumed to have taken effect.
- Restoring Normal mode on release also detaches its invalidation. The pages
  rejoining the general allocator are freed inside that completion, after the
  flush, so a CPU that still holds the page's uncached translation cannot
  observe a cached alias of the reused page. The public wrapper runs the
  completion after unlocking, so CPUs spinning on the allocator lock with
  interrupts disabled can still answer the shootdown. A failed platform release,
  mapping restore, or final shootdown leaves these pages allocated.
- Coherent allocation participates in the global page allocator and page
  table updates, so it must run after `kalloc` and the address-space
  subsystem are initialized; first use of the bounce path lazily
  allocates the pool through the same path. Allocation confirms the coherent
  mapping before platform preparation or publication. Concurrent first users
  install one confirmed backing and release any unused candidate.
- `map_dma_buffer`/`unmap_dma_buffer` copy up to the buffer length on the
  calling thread; they are intended for driver submit/completion paths.
- Completion softirqs can submit pending requests on the interrupted idle
  task's 16 KiB stack. Both first-use pool registration and later mappings
  must avoid stack temporaries containing `TlsfByteAllocator`; the control
  block stays in the static allocator throughout its lifetime.
- The `dma-trace` feature adds allocation-site bookkeeping for
  debugging; it is off in production builds.

## Concurrency Model

- One global `SpinNoIrq<DmaAllocator>` serializes coherent allocation,
  bounce mapping, and the mapping table; critical sections are short
  (page allocation, memcpy, BTreeMap ops). Detached mapping invalidation
  runs after the lock is released and is not part of the critical section.
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
  initialized pool, and initializing an already registered pool.
- With `dma-trace` enabled, `deallocate_dma_memory` logs and skips mismatched
  frees instead of corrupting the allocator (without the feature no check is made) — the trace layer detects and
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

All unsafe surface is concentrated in five public functions and their
internal forwardings, each with `# Safety` sections and matching
`SAFETY:` comments:

- `allocate_dma_memory` / `deallocate_dma_memory` — exact-once free with
  identical layout; no forged or offset CPU addresses; bus addresses only
  to permitted devices.
- `map_dma_buffer` / `unmap_dma_buffer` — buffer stays live and
  exclusively owned between the calls; device access stops before
  unmap; exact-once unmap with the matching address and length.
- `cancel_dma_mapping` — the mapping is still unpublished (the device
  never received the address); exact-once cancellation with the matching
  address and length.
- Internal unsafe blocks: `NonNull::new_unchecked` on freshly allocated
  non-null pages, and `copy_nonoverlapping` between distinct live regions
  of validated length.

## Design Decisions

- Bounce-staged temporary mappings behind a stable API: staging through
  pooled coherent memory keeps driver code simple and safe today; a
  direct-map fast path can be reintroduced later without touching
  drivers. Staging is direction-independent so a mapping released without
  device access (queue-setup rollback) returns the original bytes for RX
  as well as TX; unpublished failure rollback uses cancellation instead of
  unmap so it never writes stale pool contents back.
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
