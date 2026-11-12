# kalloc — Security and Reliability Analysis

## Test Probe Boundary

The audit scope includes guest fixtures in `ut-tests/lib.rs`, `ut-tests/pcp.rs`,
`ut-tests/slab_cache.rs`, and `ut-tests/tracking.rs`, mounted in their original
modules under `cfg(unittest)`. The slab and tracking feature gates are unchanged;
test extraction does not change allocation ownership or unsafe invariants.

The unittest-only allocation probe borrows a `NoPreemptIrqSave` guard and reads
an existing per-CPU atomic counter through a published CPU area. It does not
change allocator metadata or expose mutable references. The guard prevents task
migration and ordinary local IRQ attribution during measurement; NMI attribution
and remote allocation are outside the contract. No production API is added.

## Overview

`kalloc` sits at the foundational layer of kernel memory ownership. It converts untyped virtual-address regions into
Rust heap objects, contiguous pages, and DMA32 pages, and maintains these resources through raw pointers, address arithmetic, low-level allocator
contracts, and per-CPU mutable state. Once the invariants here are broken, the impact is usually not
a local allocation failure but freelist corruption, double allocation of pages, out-of-bounds DMA, or kernel undefined behavior.

This document covers the entire crate: the central byte/page allocator,
page-level PCP, slab object caches, `PageRef`, usage accounting and tracking.
The per-CPU slab cache is one of these safety boundaries.

## Trust Model

```text
boot memory map / kernel callers / C components / DMA users
                         |
                         | region, Layout, pointer, page count, address
                         v
                    kalloc safe/unsafe API
                         |
          +--------------+---------------+
          |                              |
          v                              v
  alloc-engine contracts          per-CPU storage + IRQ model
          |                              |
          +--------------+---------------+
                         v
               allocator-owned kernel memory
```

`kalloc` trusts the following conditions:

1. The `[va, va + size)` passed in by the boot flow is mapped, writable, and exclusively owned, contains at least one complete 4 KiB page,
   and does not overlap with other regions; `va` is page-aligned, and `p2v` / `v2p` are valid for these addresses.
2. The deallocation caller uses the starting address, layout, or page count of the original allocation, frees it only once, and
   keeps the same `UsageKind`.
3. The addresses returned by `alloc-engine` satisfy the requested size, alignment, non-nullness, and unique ownership, and accept
   the deallocation that matches the allocation.
4. The per-CPU storage of the current CPU is installed; ordinary interrupt and preemption constraints are enforced as the API requires;
   NMIs do not enter the allocator.
5. The DMA caller hands the returned DMA range only to devices permitted to access that region, and the upper layer handles page attributes,
   share/unshare, and device synchronization.

`kalloc` does not verify whether an arbitrary address actually belongs to some allocator, and does not keep per-allocation metadata sufficient to
detect all wrong-layout frees, double frees, or cross-allocator frees in production.

## External Boundaries / Attack Surface

| Boundary | Input | Current handling | Residual risk |
|------|------|----------|----------|
| Boot memory | Free regions that `kruntime` filters from the firmware/boot memory description and passes in via `p2v` | The underlying buddy rejects input that is not page-aligned, is too small, exceeds the region-count limit, or overlaps an existing region; the first region establishes the page/byte allocator | Cannot verify whether the address is actually mapped, writable, belongs to free RAM, or conflicts with an owner outside the allocator; endpoint arithmetic still relies on a trusted range |
| Rust kernel callers | `Layout`, raw allocation pointer, page count, alignment, fixed address, `UsageKind` | Allocation returns `Result`; some size computations and summary usage subtraction use checked arithmetic | Some deallocation APIs that look safe still depend on the caller contract, and wrong arguments may cause UB |
| DMA / devices | Contiguous page count, alignment, virtual-to-physical address translation result | After allocation, checks that the entire physical range is below 4 GiB, and returns it if not | There is no DMA32 zone; a wrong `v2p`, range overflow, or device overreach can still cause erroneous DMA |
| Tracking/observability | enable/disable, generation range, visitor | Feature-gated, global lock, and recursion guard | Backtraces/addresses may leak the kernel layout; a visitor re-entering the tracking API may deadlock or panic |

This crate does not directly parse user addresses, does not directly access MMIO/PIO, and does not directly handle file system, network, or IPC
payloads. User workloads can indirectly drive allocation pressure; when optional tracking information is exposed by upper-layer
interfaces such as `/dev/memtrack`, the upper layer must separately establish a permission boundary.

## Unsafe Code Inventory

### U-01: Rust `GlobalAlloc` and raw allocation pointers

Location: `unsafe impl GlobalAlloc for GlobalAllocator` and `large_alloc()` in `src/lib.rs`.

Main operations:

- Construct a `NonNull<u8>` from the non-zero integer address returned by the page allocator;
- Implement the raw pointer ABI of `GlobalAlloc::alloc` / `dealloc`;
- Hand allocation failure over to `handle_alloc_error()`.

Relied-on invariants: the underlying allocator returns an address that is valid, aligned, exclusive, and covers the complete layout; the pointer/layout
received by dealloc must match the original allocation. The internal `balloc` / `palloc` locks and the
per-CPU guard protect metadata against concurrency, but cannot repair a pointer forged by the caller.

### U-02: per-CPU slab intrusive list

Location: `ObjectCache::pop()`, `push_unchecked()`, and `try_push()` in `src/slab_cache.rs`.

Main operations: cast the starting address of a cached object to `Option<NonNull<u8>>`, and read or write the next link from the first word.

Relied-on invariants:

- The object is a live, writable, exclusive allocation of the corresponding canonical class layout;
- The caller has finished all accesses, and the object is not in any other cache/freelist;
- The object size/alignment is sufficient to hold the link; a compile-time assertion verifies that the link does not exceed the smallest 8-byte class;
- Local IRQs stay disabled during list mutation, and remote CPUs do not modify this CPU's list.

The crate-private `AllocLocalOwner` holds a `NoPreemptIrqSave` and serves as a required
parameter of all PCP/slab cache operations. The cache module constructs temporary references only within the owner's mutable borrow scope and does not return references to callers;
therefore raw per-CPU mutable access is concentrated in two owner-backed helpers. This protocol does not support NMIs entering the
allocator; NMI paths must use preallocated storage.

`GlobalAllocator::alloc_from_percpu_slab()` and `dealloc()` establish an IRQ guard and route by
`SizeClass`; `with_object_cache!` keeps the enum class consistent with `ObjectCache<N>`.

### U-03: slab refill/drain ownership

Location: `ObjectCache::drain()`, `PerCpuSlabCache::{try_free,drain}`
in `src/slab_cache.rs`, and the crate-private current-CPU cache API.

Main operations: transfer raw objects among the per-CPU cache, the caller, and the central `ByteAllocator`, and call the central deallocation with the
canonical layout.

Relied-on invariants: the central allocator lock is already held by the caller; all cached objects were originally allocated by the same central
allocator with the same class layout; the current-CPU mutable reference is not re-entered on the same CPU. Violating these conditions may put an object into the wrong freelist or create a mutable alias.

### U-04: Fresh exact zeroing and reference-range transfer

Locations: alloc_zeroed_pages_exact_ref and PageRef::from_raw in src/page.rs.
Zeroing writes only the checked selected range while new, exclusive and unpublished.
Exact surplus inventory is already returned and is never written by this clear.
The single-page helper uses the same path. No safe slice API is exposed.

Raw transfer includes selected PA and count. Restoration requires the original
pair and lifetime holds for every selected independent page or one encompassing
compound. Metadata validates a homogeneous range but cannot prove provenance,
unique consumption, initialization, access exclusivity or external retirement.
A shared PageRef cannot grant an exclusive content reference. Single-page PA
adapters may omit count only under their explicit length-one allocation contract.

### U-05: per-CPU usage shared references

Location: `with_current_usage_counters()` and `usage_snapshot()` in `src/lib.rs`.

Main operations: obtain a shared reference to the per-CPU usage slot through `kcpu_slot::area()` and `PERCPU_USAGE_COUNTERS.shared_at()`, and access its atomic counters from the allocation/free hot paths or the cross-CPU aggregation path.

Relied-on invariants: while the current-CPU area is resolved, `NoPreempt` pins the executing CPU; an unpublished area is skipped (early-boot accounting is dropped, and `usage_snapshot()` skips it) rather than assumed initialized; `usage_snapshot()` iterates CPU indexes below `kbuild_config::NR_CPUS`; areas are published monotonically and never unpublished; all concurrent access goes through atomic fields and shared references, and the runtime does not concurrently reset per-CPU areas.

## Memory Safety Invariants

The following conditions must always hold:

1. Every region added to the allocator has a page-aligned base, covers at least one mapped, writable, exclusively owned
   whole 4 KiB page, and its actually managed portion does not overlap existing regions; a tail that is not a whole page is not managed.
2. At any moment every allocation has exactly one owner: the caller, a per-CPU cache, or the central
   allocator freelist; it cannot be in two states at once.
3. Once a byte heap backing page is handed to `balloc`, it must not be allocated by `palloc`
   to any other purpose for its whole lifetime.
4. Page allocation/free use the same start address and page count; byte allocation/free use a layout that maps to
   the same underlying allocation class.
5. Every address in the PCP denotes a complete, page-aligned, contiguous block of the corresponding `NR_PAGES`; the buddy rounding slack of
   non-power-of-two blocks such as 3 pages has already been returned.
6. Every object in a slab cache belongs to the corresponding `SizeClass` and is allocated with the canonical layout; its first
   word serves as the next link only after the caller has relinquished ownership.
7. A current-CPU cache mutation must not run concurrently with a same-CPU interrupt/NMI or a remote walker.
8. Each PageRef owns a nonempty homogeneous selected range: one hold per
   independent page, or one compound hold covering the entire range. The count
   stored in the handle is its selected length, not the retained compound size.
   Content permissions and business byte lengths require separate contracts.
9. A usage slot is updated only through atomic shared access; the per-CPU area must not be
   re-initialized while accounting is running.
11. usage/tracking metadata is diagnostic information only and must not be used to replace allocator provenance or
    ownership judgments.

## Thread Safety

| Shared state | Synchronization condition | Cases not covered |
|----------|----------|----------------|
| `DefaultByteAllocator` | `balloc: SpinNoIrq<_>` | NMI reentrancy, incorrect raw pointer/layout |
| `BuddyPageAllocator` | `palloc: SpinNoIrq<_>` | NMI reentrancy, incorrect region/page contract |
| PCP | local IRQ off + current-CPU storage | NMI, CPU offline/remote inspection |
| slab object cache | local IRQ off + current-CPU storage | NMI, CPU offline/remote inspection |
| ready flags | Release store / Acquire load | High-level protocol errors from concurrent duplicate initialization |
| Usage of each kind | per-CPU cumulative atomics; free Release / snapshot Acquire | Cross-CPU/cross-kind linearizable snapshots, allocator metadata publication, immediate wrong-kind free detection |
| tracking map | `SpinNoIrq<GlobalState>` + per-CPU recursion guard | Recursive acquisition of tracking state by a visitor |

The tracking recursion guard uses a per-CPU `AtomicBool::swap` and acquires
`NoPreemptIrqSave` before reading the flag. RAII destruction guarantees that both the normal return and unwind paths clear the flag of the original CPU, preventing
an old plain bool from being left on the wrong CPU after migration or early exit. The cost is that IRQ/preemption stay disabled during the tracking callback, backtrace,
and tracking lock; this is a feature-gated diagnostic cost, and
tracking data must not be used as a normal runtime latency baseline.

The established nested lock order is tracking state (when enabled) → `balloc` → `palloc`. Byte heap growth is
`balloc -> palloc`; no other path may hold `palloc` and then acquire `balloc`. Usage accounting adds no
lock nesting. Ordinary IRQs are excluded through `SpinNoIrq` or an explicit `IrqSave`; NMI is not supported.

Allocator initialization is completed serially by the boot CPU. The ready atomics are used to publish the completed state; they do not turn multiple
concurrent `global_init()` / `add_memory()` calls into a safe region initialization protocol.

## Threat Analysis

| ID | Threat description | Impact level | Trigger condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | An invalid, overlapping or reserved memory region is added to the allocator | High | boot memory classification, `p2v` or a caller argument is wrong | `kruntime` passes only regions marked `FREE`; the underlying buddy checks page alignment, minimum size and internal region overlap. Mapping validity, external ownership and endpoint ranges are still guaranteed by the boot boundary |
| T-02 | Wrong layout, page count, double free or cross-allocator free corrupts a freelist | High | A kernel caller violates the allocation/deallocation pairing | API rustdoc and RAII `PageRef` constrain normal paths; aggregate usage underflow detects some accounting errors. Immediate, complete provenance validation is absent, so the residual risk remains high. |
| T-03 | A slab intrusive link is written out of bounds, inserted twice or modified concurrently | High | stale alias, wrong size class, double free, NMI reentry | The canonical layout, the compile-time link-size check, the local IRQ guard and the central lock protect the normal path; NMI and forged pointers are explicitly unsupported |
| T-04 | The PCP stores a wrong page address or page count, then allocates twice or merges wrongly | High | The caller frees with a wrong `num_pages`, or the CPU-local concurrency constraint is broken | The fixed 1–4 page cache types and `split_to_chunks` keep the internal bookkeeping; input provenance is still guaranteed by the caller |
| T-05 | A DMA address exceeds the device-addressable range or points to the wrong physical page | High | wrong `v2p`, address-range arithmetic anomaly, caller/device lifetime error | `alloc_dma_pages()` checks that the end address is below 4 GiB and returns the pages on failure; the upper-layer `kdma` manages page attributes and platform share/release and still depends on correct device authorization |
| T-06 | Lock recursion, lock order inversion or an NMI interruption causes a permanent spin | Medium | The allocator is used in an NMI, a new path forms `palloc -> balloc`, or a tracking visitor locks the tracking state again | `SpinNoIrq` excludes normal IRQs, the documentation fixes the lock order, and the tracker has an allocation recursion guard; NMI and tracking callback reentry remain residual limitations |
| T-07 | A per-CPU cache hides reusable memory, prompting the central allocator to grow early or OOM | Medium | CPU/class working sets are unbalanced, memory pressure, a CPU stops running | The PCP has a fixed entry capacity; the slab cache uses a batch of at most 32 objects and a high-water mark of less than two batches; there is no all-CPU reclaim, so the risk is only bounded, not eliminated |
| T-08 | Size, page count, usage or address arithmetic overflows | Medium | Extreme caller input, a wrong region or unpaired accounting | `pages_to_bytes`, the per-CPU cumulative values and the usage summary subtraction use checked arithmetic, and `Layout` validates part of the input; some address endpoint computations still depend on the trusted kernel range |
| T-09 | tracking leaks kernel addresses/backtraces or significantly changes timing | Medium | The upper-layer memtrack output is read without authorization, or tracking performance is treated as the normal path | tracking is a feature-gated diagnostic feature; access control is the responsibility of the exposed interface, and performance analysis must distinguish the tracking state |
| T-10 | Multiple byte allocator features are selected at once, so the actual implementation differs from the configured expectation | Low | A non-Kconfig build wrongly enables multiple features | A normal build selects exactly one through Kconfig; selecting none fails to compile, while selecting several currently depends on the `cfg_if!` branch order and must be guaranteed by a build audit |
| T-11 | Non-linearized usage/central stats are used for security or reclaim decisions | Low | A caller mistakes a diagnostic snapshot for precise ownership | The API positions them as statistics; the documentation distinguishes the caller-live, byte central and page central views and provides no strong consistency guarantee |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure mode | Failure cause | Local effect | System effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | The allocator is called before the page region is ready | Wrong boot order, or no usable `FREE` region | debug assertion, allocation failure or access to an uninitialized allocator | Early boot failure | 1 | `kruntime` initializes it before ordinary kernel services; the page-table path asserts through `is_page_allocator_ready()` |
| F-02 | The byte heap or page buddy cannot satisfy a request | Memory exhaustion, fragmentation or cache residency | The explicit API returns `AllocError` | The Rust `GlobalAlloc` path logs OOM and then enters `handle_alloc_error`, usually terminating the current kernel run | 2 | The byte heap retries with a smaller growth request; PCP/slab allow partial refill; the explicit API returns the error upward |
| F-03 | A large PCP batch request fails | buddy fragmentation | The target batch cannot be obtained at once | Performance degrades, but allocation does not necessarily fail | 4 | Automatically degrades to refilling block by block, and returns an error only if not even one block can be obtained |
| F-04 | The central allocator is exhausted midway through a slab refill | byte heap pressure or fragmentation | partial cache | The current allocation is preferentially returned from the partial refill; on complete failure the byte heap is extended | 3 | Keep the objects already obtained without rolling back; the outer layer retries after heap growth |
| F-05 | A DMA allocation lands above 4 GiB | The buddy has no low-memory directed strategy | This DMA allocation is rejected | Device initialization or I/O fails even though low memory may still exist | 3 | Immediately return the unqualified pages and return `NoMemory`; a DMA32 zone may be introduced later |
| F-06 | usage overflow/underflow | Lifetime cumulative overflow, wrong-kind/free pairing or a counter anomaly | Panic on a hot-path cumulative overflow or a summary underflow | May stop the kernel | 2 | checked arithmetic fail-fast; usage does not take part in allocator metadata mutation |
| F-07 | A full cache triggers a batch drain | Normal burst of frees | Up to 32 central deallocations hold the lock with IRQs off | IRQ latency or cross-CPU allocation latency spikes | 3 | The object count and high-water mark provide an explicit upper bound; workload measurement and tuning are still needed |
| F-08 | The CPU holding the lock stalls | panic, NMI, hardware problem or wrong reentry | Other CPUs spin on the central allocator lock | System-level allocation hang | 1 | The critical section does not sleep; normal IRQs are disabled; NMI allocation is forbidden, and new paths must obey the lock order |
| F-09 | A tracking visitor reenters an incompatible API | The visitor calls `current_generation()` etc., acquiring `STATE` again | Spin deadlock or a `None.unwrap()` panic | The diagnostic interface hangs, possibly affecting the system | 2 | Allocation recursion is skipped by a per-CPU flag; the visitor must be short and must not recurse into the tracking state |
| F-10 | Invalid PageRef raw restoration | Forged, duplicated or stale transfer | Consumes a hold the caller does not own | Premature free or memory corruption | 1 | Unsafe API requires the exact outstanding transfer from the same incarnation; safe lookup cannot grant ownership |

## Fault Management

- `GlobalAllocator::{alloc,alloc_pages,alloc_dma_pages,alloc_pages_at,add_memory}` use
  `AllocResult` to report errors such as `NoMemory` and `InvalidInput`.
- Rust `GlobalAlloc::alloc` cannot silently pass an ordinary failure to most Rust allocations; it records the
  layout and central heap state and then calls `handle_alloc_error()`.
- `global_init()`, `init_page_allocator()`, and some internal invariants use assert/expect; boot or
  allocator metadata errors are handled fail-fast, and the kernel does not attempt to keep running after such errors.
- PCP and slab refill accept partial progress, to avoid discarding blocks/objects already obtained
  just because the batch target was not reached.
- When the DMA32 check fails, the unqualified allocation is first returned to the buddy, and then an error is returned.
- Per-CPU usage cumulative counters use checked addition; a total freed amount exceeding the total allocated amount fails
  fast when summarized in `usages()`, but this is not an immediate or complete pointer provenance check.
- The spinlock has no recovery protocol for allocator corruption; once an unsafe invariant is broken, it cannot be assumed that
  subsequent error returns still keep the system safe.

### Capacity Query Boundary

`total_pages()` reads the managed capacity only under the global page-allocator lock. It does not take the byte-heap lock,
and it neither publishes nor releases pages. Using it for upper-layer filesystem quota does not constitute a physical-memory reservation; actual allocations
can still fail, and the capacity value must not be treated as remaining memory or as an admission guarantee.

## Privacy Analysis

The ordinary allocator treats the payload as opaque bytes and does not actively read the caller's live data. However:

- Ordinary allocation does not guarantee zeroing; the contents of a previous owner may remain, and a caller handling sensitive data must
  use alloc_zeroed_page_ref/alloc_zeroed_pages_exact_ref before publishing contents;
- The slab cache writes the next link into the first word of an already freed object, and the rest of the content is not erased automatically;
- PCP cached pages do not erase the payload either;
- tracking stores allocation addresses and backtraces. If an upper layer exposes them to user space, they may leak the kernel
  address layout, call relationships and workload information, so access must be restricted and address redaction considered.

This crate no longer defines any C ABI symbol. `memcpy`/`memmove`/`memset`/`memcmp` are provided as weak symbols by the `compiler_builtins` that rustc ships for the target,
and their input contract is the joint responsibility of that upstream implementation and LLVM lowering;
they are no longer part of this crate's trust boundary. Note that any strong symbol that redefines these four names
with `#[no_mangle]` silently replaces the kernel-wide page-zeroing and page-copy implementations, so adding such a symbol must be reviewed
as an architecture-level performance change.

## Known Limitations

1. **No unified reclaim**: byte heap backing is not returned to the page allocator as a whole; the PCP and slab caches
   have no all-CPU freeze/flush/shrink interface. Resources in a cache are returned in batches only when they reach the local high-water mark.
2. **No CPU hotplug lifecycle**: its PCP or slab cache is not drained before a runtime offline of a CPU.
3. **NMI is not supported**: the local IRQ guard and `SpinNoIrq` cannot exclude NMI reentry.
4. **No allocation context policy**: task, softirq and hardirq are not distinguished, and there are no GFP-like
   nonblocking/reclaim flags; the slow path may hold the central lock, loop in batches and extend the heap.
5. **No NUMA, memory cgroup or pressure-aware watermarks**: all central memory shares one page buddy
   and one byte allocator.
6. **DMA32 uses a check after allocation**: there is no low-memory zone or directed search, so a request may still fail
   while usable low pages remain.
7. **Deallocation provenance is not verified in production**: a wrong pointer, layout, page count,
   `UsageKind` or a double free depends mainly on the caller contract; the constraints of some public safe methods cannot be enforced by the
   type system.
8. **Statistics are not a strongly consistent ownership view**: the staged reads of per-CPU caches, byte heap backing, page alignment and
   per-CPU cumulative values make the meanings of `usages()`, `used_bytes()` and `used_pages()`
   differ; `usages()` does not guarantee linearization across CPUs/kinds, and concurrent changes may make the result temporarily too large or too small.
9. **tracking has a global lock and callback constraints**: it changes allocation latency; `allocations_in()` requires
   tracking to be enabled, and the visitor runs under the lock and must not recursively acquire the tracking state. Allocation/free records near the
   enable/disable boundary do not form a transactional snapshot, and a free during the disabled period does not clear existing records.
10. **Memory is not zeroed by default**: Unless the caller uses
    `alloc_zeroed_page_ref` / `alloc_zeroed_pages_exact_ref` or explicitly clears
    the allocation, reused memory can retain a previous owner's data.
11. **Slab object cache watermarks have not been tuned by measurement**: a batch is bounded by both 2 pages of payload and 32 objects,
    and the high-water mark is less than two batches; adjust it with the hit rate, lock-stat, memory retention and
    IRQ latency of the X-Kernel workload.

## Audit Checklist

When modifying `kalloc`, confirm the following:

- [ ] Does a new memory region entry validate mapping, page alignment, exclusivity, overlap and address endpoint overflow?
- [ ] Does allocation/deallocation keep the pointer, layout, page count and `UsageKind`
      paired on all branches?
- [ ] Is byte heap backing transferred ownership only once between `balloc` and `palloc`?
- [ ] Does a new page fast path handle alignment, non-power-of-two slack and buddy chunks correctly?
- [ ] Does every current-CPU PCP/slab mutable access cover the complete local IRQ-off/preemption-safe
      interval? Could it be accessed from an NMI or a remote CPU?
- [ ] Does a slab cache object always use the canonical layout of its class, with the first word rewritten only after the caller
      relinquishes it?
- [ ] Does a new lock path obey the tracking → `balloc` → `palloc` order and not sleep under a lock?
- [ ] Does every unsafe block have a traceable `SAFETY:` justification, and does the safe wrapper really establish its preconditions?
- [ ] Do page holders preserve exact transfer ownership, and do content users
      separately establish mapping, aliasing, initialization and byte bounds?
- [ ] Is the DMA endpoint computation checked, does the whole physical range satisfy the device limits, and is a failure rolled back completely?
- [ ] Is a strong symbol defining `memcpy`/`memmove`/`memset`/`memcmp` with `#[no_mangle]` newly added?
      Such a definition silently replaces the mem primitives of the whole kernel (including the page zeroing and COW copy on every page fault), and must be reviewed as an architecture-level
      performance change, with comparison data against the `compiler_builtins` implementation.
- [ ] Is a tracking visitor called while holding a lock, and is there a recursive state lock or a sensitive address leak?
- [ ] Do new statistics clearly state the unit and level, and are they not used as allocator correctness/reclaim conditions?
- [ ] Do new cache or allocator states synchronously update initialization, CPU hotplug, reclaim and shutdown policy?

## Range-holder ownership and cleanup

PageRef is two machine words: first descriptor and selected page count. Range
mode resides in immutable-while-held page-meta geometry. No safe constructor
accepts arbitrary descriptors or mixes independent pages with compound members.
Subrange bounds are relative to the handle and preserve that shape. State/count
snapshots are diagnostics, not authority to construct a holder.

Exact admission validates the full selected prefix before claiming one count
per page. Inventory is obtained as a complete power-of-two block; unused pages
have no typed holds and are returned with exact one-page operations before
publication. On ordinary failure all raw backing and its charge are restored.
Zeroing clears only selected live pages after surplus is returned. Compound
admission is separately requested and validates its full group before publication.

Clone of an independent range retains the originals while incrementing every
member. A failed increment releases exactly the acquired prefix, which cannot
retire because its originals remain held. Compound clones increment one head.
Drop snapshots the first unit: it releases that one compound hold, or creates
one temporary single-page owner for each independent hold. Earlier returns may
be reused, so no freed page is consulted to determine later range bounds.
Last retirement clears all relevant group links before returning inventory and
never writes old metadata after publication.

Exact raw restoration requires the full transferred length; the first descriptor
cannot distinguish simultaneous holders with different lengths. Compound raw
views also preserve selected bounds. PA-only single-page adapters explicitly
restore length one. Wrong lengths, duplicated/stale transfers or forged ownership
can corrupt counters and remain unsafe caller violations despite validation.

try_split of an independent range only transfers existing holds and permits
other independent holders. Compound splitting freezes exactly one head hold and
requires external quiescence. A failed split returns the original selected view;
a successful split yields the whole compound, including pages outside a subview.
The old owner is consumed without decrementation and SplitPages owns every child
until transfer or Drop. Class/accounting is preserved. mem::forget and lost raw
transfers can still leak ownership.

Metadata remains resident. Raw ordinary/DMA free still has no descriptor scan,
so its original provenance/bounds/class obligations remain caller-enforced.
Independent claim/clone/drop and compound relation setup/cleanup/split visit their
relevant ranges; IRQ callers must bound work. Lifetime protection does not establish
initialization, exclusive contents, mapping permissions, COW or hardware retirement.

## Owned normal-memory zeroing

`DC ZVA` is used only for exclusive, unpublished, normal cacheable RAM owned by
the allocator. MMIO, device memory, shared/DMA-visible buffers and arbitrary C
buffers are outside the contract. The block size comes from `DCZID_EL0`; if the
register forbids `DC ZVA` or no complete aligned block fits, the helper falls
back to `write_bytes` over exactly the requested range.
