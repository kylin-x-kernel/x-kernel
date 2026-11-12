# kalloc — Design Document

## Test Allocation Probe

Unittest builds expose `current_cpu_allocated_bytes_for_test`, a read-only view
of successful cumulative Rust heap bytes on one CPU. The caller keeps the same
IRQ/preemption guard across the before/after samples and measured operation.
Frees cannot hide intervening allocations. This probe adds no production
instrumentation and does not measure remote CPUs, failed attempts or live usage.

## Scope and Role

`kalloc` is the kernel-wide global memory allocation layer of x-kernel. It receives the free memory regions handed over during boot,
provides the Rust global heap, contiguous pages, DMA32 pages and RAII page objects to upper layers, and is responsible for routing these requests to
the byte allocator, the global page buddy and the corresponding per-CPU caches.

The main users include:

- the Rust `alloc` ecosystem, and kernel code that uses types such as `Box`, `Vec`, and `Arc`;
- page tables, user virtual memory, the page cache, and general contiguous-page users;
- subsystems such as `kdma` that need DMA32 physical addresses;
- Kernel components retaining single or contiguous allocations through `PageRef`;
- optional allocation tracking.

## Background

x-kernel runs in a `no_std` environment, and needs both to satisfy the Rust `GlobalAlloc` interface and to directly
manage boot memory and contiguous pages for different purposes. A single allocation strategy cannot simultaneously serve small objects, page granularity, special
alignment, DMA address restrictions and multi-CPU hot paths, so `kalloc` uses a two-level central allocator and places per-CPU caches at
two levels, pages and slab objects:

- `BuddyPageAllocator<4096>` is the final owner of all ordinary memory regions;
- the byte allocator selected by Kconfig manages heap regions obtained from the page allocator;
- the page-level PCP caches commonly used contiguous blocks of 1–4 pages;
- under the slab configuration, the per-CPU object cache caches objects of 8–2048 bytes.

The per-CPU slab cache is the first version of the current object-cache design. It reduces contention of a stable small-object working set on the central
byte allocator lock, but it does not replace the central allocator, and it provides no full reclaim, CPU
hotplug or NMI support.

All mutable access to the PCP and slab caches goes through the crate-private `AllocLocalOwner`. The owner holds
`NoPreemptIrqSave`, carrying the condition "pinned to the current CPU and local IRQ re-entry disabled" through one synchronous cache operation;
the PCP/slab modules no longer read the raw per-CPU base themselves, nor return cache references to callers. The owner
only establishes execution-context exclusivity; the specific object/page provenance remains the responsibility of the corresponding allocation API.

## Scope

This design covers:

```text
mm/kalloc/
├── Cargo.toml
├── src/
│   ├── lib.rs          # Global allocator, initialization, routing, statistics and GlobalAlloc
│   ├── page.rs         # Unified PageRef, allocation, zeroing and final release
│   ├── page/split.rs   # Cleanup iterator transferring independent split pages
│   ├── pcp.rs          # 1–4 page per-CPU page cache
│   ├── slab_cache.rs   # Per-CPU object cache under the slab configuration
│   └── tracking.rs     # Optional allocation tracking
├── ut-tests/
│   ├── lib.rs
│   ├── pcp.rs
│   ├── slab_cache.rs
│   └── tracking.rs
└── docs/
    ├── design.md
    └── security.md
```

The concrete underlying algorithms such as buddy, slab and TLSF belong to `mm/alloc-engine`; this document only describes how `kalloc` composes
them, and its synchronization and calling constraints.

Kernel unit tests are mounted in the corresponding `src/` modules through `unittest::path_mod_test!`,
and are compiled only under `cfg(unittest)`. The names and
visibility of the public modules `tests_kalloc` and `tests_tracking` are unchanged; the PCP and slab tests keep their own private `tests` modules.
The `slab_cache` tests require the `slab` feature to be enabled, and the `tracking` tests require the `tracking` feature to be enabled.

## Configuration

A build must select exactly one byte allocator through Kconfig:

| feature | `DefaultByteAllocator` | per-CPU slab object cache |
|---------|------------------------|---------------------------|
| `slab` | `SlabByteAllocator` | Enabled |
| `buddy` | `BuddyByteAllocator` | Not enabled |
| `tlsf` | `TlsfByteAllocator` | Not enabled |

Selecting none of them triggers `compile_error!`. The build configuration convention is to select only one; the current source does not
report a separate error for "multiple selected at once", and instead the branch order of `cfg_if!` picks one.

Other optional capabilities:

- `tracking`: records the layout, backtrace and generation of Rust global heap allocations;
- `tee` / `dice`: no code in this crate is gated by them any more. The two features are still declared, because
  the feature graphs of `api/kfeat` (`dice`) and `tee_kernel` (`tee`) enable them.

## Architecture

```text
                       Rust alloc / kernel callers
                                      |
                                      v
                              GlobalAllocator
                                      |
              +-----------------------+-----------------------+
              |                                               |
              | byte / Rust heap                              | page APIs
              v                                               v
   slab per-CPU object cache                         per-CPU page set (PCP)
       (slab feature only)                              1 / 2 / 3 / 4 pages
              |                                               |
              | miss/full                                     | miss/full
              v                                               v
 SpinNoIrq<DefaultByteAllocator>                 SpinNoIrq<BuddyPageAllocator>
              |                                               ^
              | heap growth                                   |
              +-----------------------------------------------+

Other paths:
  large Rust allocation ---------------------------> page allocator
  DMA32 / alloc_pages_at --------------------------> page allocator directly
  PageRef -----------------------------------------> page APIs + metadata + Drop
  tracking ----------------------------------------> wraps Rust GlobalAlloc path
```

When `tracking` is enabled, the recursion flag is a per-CPU `AtomicBool`. An RAII guard disables preemption and IRQs before reading the flag,
and clears the flag of the same CPU on drop. The guard spans the backtrace, the tracking
lock and the wrapped allocation callback; this is the latency cost the diagnostic feature pays to guarantee recursion and migration semantics, and
it is not part of the hot path when tracking is normally off.

The main state of `GlobalAllocator` is:

| State | Protection | Meaning |
|------|----------|------|
| `balloc` | `SpinNoIrq` | The central byte allocator of the current configuration |
| `palloc` | `SpinNoIrq` | 4 KiB page buddy |
| `heap_ready` | `AtomicBool` | Whether the byte allocator has obtained its first heap region |
| `page_ready` | `AtomicBool` | Whether the page allocator has obtained at least one region |
| per-CPU usage counters | Two cumulative atomics per CPU, per `UsageKind` | caller-live diagnostic counters |

## Initialization State

The allocator has only one-way state transitions and does not support runtime teardown:

```text
Empty
  |
  | init_page_allocator(), or the first phase of global_init()
  v
PageReady
  |
  | bootstrap_heap_if_needed(), first byte allocation,
  | or the second phase of global_init()
  v
HeapReady

PageReady / HeapReady -- add_memory() --> extends palloc; from PageReady it also bootstraps the byte heap (-> HeapReady)
```

The actual boot path is executed by `kruntime` in the following order:

1. Initialize the per-CPU storage of the primary CPU;
2. Initialize the memory map and enumerate the regions marked `FREE`;
3. The first region initializes the page allocator through `global_init()`, and at least
   `MIN_HEAP_SIZE` (32 KiB) is taken from it as the first byte heap;
4. Subsequent regions are added to the page allocator through `global_add_memory()`;
5. Each AP installs its own per-CPU register before entering ordinary kernel code.

`init_page_allocator()` supports a path that publishes only the page allocator and establishes the byte heap lazily later.
`page_ready` and `heap_ready` publish state with Release/Acquire; the addition of memory regions and allocator
metadata mutation are still serialized by the corresponding locks. The initialization calls themselves rely on the boot flow executing serially.

## Core Algorithm Flow

### Rust heap / byte allocation

`GlobalAllocator::alloc(layout)` routes in the following order:

1. When `layout.size() > LARGE_ALLOC_THRESHOLD_BYTES` (currently 1 MiB), page
   allocation is performed directly, to avoid very large objects fragmenting the byte heap; usage is counted by the actual number of pages. Smaller mid-sized
   buffers stay in the byte heap, to reuse backing memory that has already been transferred from the page allocator and to reduce
   pressure on high-order contiguous blocks of the global buddy.
2. Under the slab configuration, if `SizeClass::from_layout()` accepts the layout, the slab object cache of the current CPU
   is accessed first.
3. Other small requests go to the `DefaultByteAllocator` protected by `balloc`.
4. When the central byte allocator cannot satisfy the request, a new region is obtained from `palloc`, added to the byte heap,
   and the request is retried.

The first establishment of the byte heap uses at least 32 KiB. Subsequent growth requests take the current total heap size and this layout into account,
and choose a power-of-two size; when page allocation fails, the size is halved step by step until one page or the
smallest region that can hold the layout. Backing regions already obtained by the byte heap are not proactively returned to the page allocator.

Deallocation must carry the pointer and layout of the original allocation. Large objects over 1 MiB are returned to the page
allocator; slab-cache-eligible objects first go into the current CPU cache; other objects are returned to the central byte
allocator.

### Contiguous-page allocation

The path of `alloc_pages(num_pages, align_pow2, kind)` is:

- `num_pages` is 1–4 and `align_pow2 <= 4096`: access the current CPU's PCP first;
- PCP miss: acquire the `palloc` lock and refill in batches;
- Larger page counts or special alignment requirements: access the global page buddy directly;
- On success, update the diagnostic statistics according to `UsageKind`; `RustHeap` is handled separately by the upper-level heap path.

`dealloc_pages()` prefers a PCP push for blocks of 1–4 pages; when the cache is full it drains in batches; larger
blocks are returned directly to the buddy. A small block obtained with special alignment can also enter the PCP as an ordinary block of the same page count after it is freed,
because its address satisfies at least ordinary page alignment.

`alloc_pages_at()` requires a specified starting virtual address, so it bypasses the PCP and calls the buddy's address-specified interface directly.

### Page-level PCP

`pcp.rs` keeps four fixed-capacity arrays for each CPU:

| block size | Per-CPU capacity |
|------------|-------------|
| 1 page | 12 entries |
| 2 pages | 6 |
| 3 pages | 6 |
| 4 pages | 6 |

A cache hit only updates the current CPU's array and count. The refill target is `2/3` of the capacity:

1. First request from the buddy a contiguous range covering the whole batch, to reduce the number of lock acquisitions;
2. If the large request fails due to fragmentation, fall back to per-block requests, allowing a partial refill;
3. Rounding slack that the buddy produces for non-power-of-two page counts is returned immediately as valid buddy chunks;
4. When the cache is full, collect the block currently being freed and a batch of cache blocks, sort them by address, merge adjacent ranges,
   and then return them to the buddy through `split_to_chunks()`.

Pages in the PCP have already been freed from the caller's point of view, but the global buddy still sees them as allocated.

### Per-CPU slab object cache (slab configuration)

`slab_cache.rs` keeps an intrusive LIFO list for each CPU and each `SizeClass`. The supported classes
are 8, 16, 32, 64, 128, 256, 512, 1024 and 2048 bytes.

The first machine word of each free object stores the next link; the cache metadata is only the head and the object count.
All cache-eligible objects are obtained from the central allocator using the `(class_size, class_size)` canonical
layout, which guarantees that:

- the size and alignment of the original request are satisfied;
- refill and drain of the same class use a consistent layout;
- an object allocated on CPU A can be freed on CPU B and enter CPU B's cache of the same class;
- no per-object owner CPU or original layout metadata is needed.

An allocation hit pops directly; an empty cache refills in a batch within a single central-lock critical section. The number of objects per batch is
`min(2 pages of object payload, 32 objects)`, where one object is returned directly to the caller and the rest go into the cache.
The high watermark is set to `2 * batch - 1`; when the cache is full, the next free returns the current object and `batch - 1` objects from the cache
to the central allocator, and then keeps one batch. As a result, refill and drain
each call the central allocator at most 32 times.

| class bytes | batch objects | high watermark |
|-------------|---------------|----------------|
| 8–256 | 32 | 63 |
| 512 | 16 | 31 |
| 1024 | 8 | 15 |
| 2048 | 4 | 7 |

Objects in the cache are still regarded as allocated by the central `SlabByteAllocator`. This ownership choice avoids
modifying the central freelist on every fast-path free, but it means that the central `used_bytes()` includes objects that the caller has already
freed into the per-CPU cache.

### DMA32 and address-specified pages

`alloc_dma_pages()` bypasses the PCP, first obtains contiguous pages from the global buddy, and then uses `v2p` to check whether the whole physical
range is below 4 GiB. If the range does not satisfy the limit, it is returned immediately and `NoMemory` is returned. This interface is responsible only for memory
placement; page attribute switching, platform share/unshare and device lifetime are handled by upper layers such as `kdma`.

The current page allocator has no separate DMA32 zone, so it uses "allocate first, check afterwards" and cannot guarantee that it will find low free pages
when they exist.

### Page allocation consumers

Single-page helpers and alloc_pages_exact_ref return the same range PageRef.
An exact request owns precisely the requested number of contiguous independent
pages. alloc_compound_pages_ref is an explicit power-of-two allocation whose
member views retain the whole compound. There is no separate buffer-owner type
or safe byte-slice API. Initial zeroing is provided for single/exact allocation
before the first holder escapes.

A PageRef retains lifetime, not exclusive content access. Consumers perform their
own address mapping and prove actual pointer reads/writes safe. VMM single-page
PA storage deliberately omits the length-one part of the raw pair and restores
with length one. Display consumers keep their requested byte bounds in business
state. Existing multi-page VMM/display consumers select exact independent ranges;
they do not acquire compound semantics merely by requesting contiguous memory.

### Usage accounting

`UsageKind` distinguishes Rust heap, virtual memory, page cache, page tables,
DMA and general kernel page allocation accounting.

- `allocated_bytes` and `freed_bytes` are accumulated separately for each CPU and each kind, and the hot path only modifies the current
  CPU's atomic cache line;
- allocation and free can happen on different CPUs, and a snapshot takes the cumulative allocated amount over all CPUs minus the cumulative freed amount;
- small Rust heap allocations are counted by caller requested bytes;
- direct page-backed Rust heap allocations and other page uses are counted by page-aligned bytes;
- `usages()` first reads the cumulative freed amounts of all CPUs with acquire, then reads the cumulative allocated amounts, and computes the difference with checked
  arithmetic. A legitimate cross-CPU free does not cause local counter underflow; if the total freed amount exceeds the total allocated amount, it fails fast
  during aggregation;
- a snapshot is not guaranteed to be linearizable across CPUs or across kinds, and concurrent allocation/free may make the result temporarily too large or too small relative to
  the exact value at any single moment; the statistics do not participate in allocator ownership or reclaim decisions.

`used_bytes()` / `available_bytes()` present the view of the central byte allocator; `total_pages()` /
`used_pages()` / `available_pages()` present the view of the global buddy. `total_pages()` returns the total
number of allocatable pages under management (used plus free, excluding allocator metadata), for upper layers such as tmpfs to choose a default capacity;
it is not the current free amount, and reading it neither reserves memory nor changes growth, allocation or reclaim policy. Byte heap backing, PCP blocks and per-CPU
slab objects cause the expected differences between these central views and caller-live usage.

### Allocation tracking

The `tracking` feature is maintained outside the Rust `GlobalAlloc` path:

- a `BTreeMap` keyed by allocation address;
- the original `Layout`, the allocation backtrace and an increasing generation;
- a per-CPU `IN_GLOBAL_ALLOCATOR` recursion guard, which prevents the tracker's own allocations from entering
  tracking again;
- `enable_tracking()` / `disable_tracking()` and the generation-range visitor API.

Tracking is for diagnostics only and does not change allocator ownership. It introduces a global tracking lock, backtrace
collection and map operations, so the performance once it is enabled is not representative of the ordinary allocation path. Paths that call the
`GlobalAllocator` page/byte API directly do not go through this wrapper and are not automatically added to the tracking map.

### C ABI symbols

This crate no longer exports any C ABI symbols. Previously `ffi.rs` exported `malloc`,
`free`, `calloc`, `memcpy`, `memmove`, `memset`, `memcmp` and `__memcpy_chk` under `tee` / `dice`, for linking by the C
components of the time (mbedtls and others); the TEE crypto stack has been changed to a pure Rust implementation, and no other C code in the tree enters the kernel
image, so the whole module was removed.

After the removal, `memcpy` / `memmove` / `memset` / `memcmp` are provided by the `compiler_builtins` that rustc ships for the target; it exports these four names as weak symbols, which take effect naturally at link time. This point is crucial:
`core::ptr::write_bytes` and `copy_nonoverlapping` / `copy` are lowered by LLVM into calls to C symbols of the same names, so **whoever provides this strong symbol decides the implementation of page zeroing and page copying across the whole kernel**. The previous hand-written shim overrode the weak definition with a `#[no_mangle]` strong symbol, turning a byte-by-byte loop into the mem primitive of the whole kernel; now this is handed back to the upstream implementation,
and this crate no longer needs to maintain its correctness and performance.

## Calling constraints / execution context

- **Initialization prerequisite**: the page API must be used after `page_ready`; byte/Rust heap allocation
  additionally requires an existing page region so that the byte heap can be established or extended.
- **Per-CPU prerequisite**: the PCP, the slab cache and the tracking recursion guard all require that the current CPU's
  per-CPU storage has been installed. The formal run paths of the primary CPU and the APs satisfy this ordering.
- **No current process required**: the core allocator depends only on the CPU execution context, not on the current user process.
- **Does not sleep**: synchronization uses spinlocks, IRQ guards and atomics, and never calls a blocking wait; lock contention and batch
  refill/drain can still lengthen non-preemptible or IRQ-off time.
- **Interrupt context**: a per-CPU hit can complete under ordinary IRQ constraints; a miss or a full cache may acquire the central lock
  and extend the heap. The interface has no task/softirq/hardirq allocation policy and does not guarantee short latency.
- **NMI**: allocation/free in an NMI is not supported. Disabling ordinary local IRQs cannot rule out NMI reentry.
- **Early boot**: `global_init()` can be used before the scheduler is set up, but the region passed in must already be in a usable
  kernel virtual mapping, and the primary CPU's per-CPU storage must already be initialized.

## Concurrency model

| Data or path | Synchronization | Notes |
|------------|------|------|
| Central byte allocator | `SpinNoIrq` | All allocator metadata mutation is serialized |
| Global page buddy | `SpinNoIrq` | Page slow path, DMA and address-specified requests are serialized |
| Current CPU PCP | local IRQ off + current-CPU access | Remote mutation is not allowed |
| Current CPU slab cache | local IRQ off + current-CPU access | A cache hit does not take the `balloc` lock |
| `page_ready` / `heap_ready` | Acquire/Release atomics | Publish the initialization state; the initialization flow still requires serialization |
| Each usage | current-CPU cumulative atomics; free Release / snapshot Acquire | Supports cross-CPU free, provides diagnostic values only |
| tracking state | `SpinNoIrq` + per-CPU recursion guard | `tracking` feature only |

When the byte heap grows, it is allowed to enter page allocation while holding `balloc`, forming the lock order `balloc -> palloc`. No other path may acquire `balloc` while holding `palloc`; bootstrap releases the page lock first,
and then initializes the byte allocator. Usage accounting introduces no additional allocator lock order.

## Design decisions

### Central ownership and the per-CPU fast path

The central allocator keeps sole region ownership and the complex merging logic, and the per-CPU caches only hold blocks/objects that have already been
taken from the central allocator. This keeps the fast path simple, at the cost that cache residency is not visible to the central allocator
as free, so a reclaim across all CPUs needs to be designed separately.

### Layering of the page cache and the object cache

The PCP caches page blocks to reduce `palloc` lock contention; the slab object cache caches objects of fixed classes to reduce
`balloc` lock contention. The two serve different granularities and do not replace each other. Byte heap growth may still go through the PCP or the global
page buddy.

### slab canonical layout

Using the class canonical layout instead of the caller's original layout increases internal fragmentation within a class and the alignment requirements,
but in exchange it provides cross-CPU free, no per-object metadata, and a deterministic central deallocation layout. This is the core ownership constraint of the first version of the slab
object cache.

### Diagnostic statistics do not participate in correctness

Usage, `used_bytes()` and `used_pages()` each reflect a different layer, and the system can still change concurrently while they are read.
They are used for observation rather than for allocation admission, reclaim or safety checks, which avoids widening the lock
scope for linearizable snapshots. Accumulating allocation/free separately also means that the usage underflow check moves from an
immediate check on the deallocation hot path to a check performed when `usages()` aggregates.

## Drop / Resource Release

- `GLOBAL_ALLOCATOR`, the central allocator, per-CPU caches, and tracking state have static lifetime
  and are not dropped before shutdown.
- Boot memory regions added to the page allocator are transferred to `kalloc` permanently; removing a region is not supported.
- Heap regions the byte allocator obtains from the page allocator are not returned as a whole at runtime.
- PCP and slab caches return memory in batches when they reach their respective watermarks, but there is no shutdown, CPU-offline, or global
  pressure flush.
- Raw page API callers explicitly pair deallocation; PageRef performs final
  allocation release through Drop after the last hold ends.

## Range PageRef and reference units

PageRef in src/page.rs stores a first resident descriptor and a selected page
count. A compile-time assertion requires two machine words. A single handle
owns either one independent reference per selected base page, or one reference
on a compound covering the entire selected view. Modes are identified by stable
page-meta state, not a duplicate counter or per-handle mode tag. Mixed independent/
compound units in one range are forbidden. Single pages are order-zero units.
phys_addr plus page_count always describes this handle's selected range;
retained_range describes the full backing kept alive by a compound view.

The length belongs to the handle: two exact owners can have the same first PA
and different lengths. Per-page counts cannot recover those distinct release
obligations. into_raw therefore transfers (selected PA, selected page count),
and from_raw restores exactly that pair without acquiring references. No bare
lookup can mint a hold. HAL/VMM single-page adapters preserve their existing PA
interfaces under the fixed length-one contract; the generic page-table library
is not changed.

## Exact and compound allocation

alloc_pages_exact_ref validates count, alignment and mapping, obtains complete
power-of-two raw backing through existing PCP/buddy routing, then admits only
the requested prefix through claim_reference_range. All prefix descriptors are
validated before mutation. Remaining tail inventory is returned page by page
before publication, avoiding a rounded arbitrary-tail free. Non-RustHeap raw
charges are reversed for surplus; RustHeap typed ranges add only the retained
byte charge. Failures return the complete raw allocation and undo its raw charge.
No compound tail relationships are written for independent ranges.

alloc_compound_pages_ref explicitly requires a power-of-two count and uses group
admission. Head metadata carries one shared count/order/class; tail offsets resolve
that head. Any member view retains the whole backing. This is not Linux's ordinary
noncompound high-order contract, whose allocation-owner and page-reference release
obligations differ. All references here use independent or compound page units.

alloc_page_ref and alloc_zeroed_page_ref are the one-page exact wrappers.
alloc_zeroed_pages_exact_ref clears the entire selected independent range while
new, exclusive and unpublished. Requested exact pages are fully initialized;
returned spare inventory is outside the clear operation. No heap array/control
block is allocated by these page helpers.

## Clone, Drop and explicit transfer

try_clone_ref clones the selected lifetime protection. For independent ranges it
increments each page; if any increment fails, page-meta releases the successful
prefix while originals remain held. For compounds it increments the head once.
try_clone_range uses view-relative offset/count bounds; try_clone_page selects
one page relative to the current view. Exact subviews keep only their selected
pages; compound subviews keep the complete compound. Clone never repeats usage
accounting. Independent clone is O(selected pages); compound clone does not walk
the selected range.

Drop releases the first unit once for a compound, or each selected independent
page separately. Last release enters Retiring; metadata membership is cleared
before reusable storage is returned. Compound final return uses complete head
and order; independent final return uses one page and its original class. A
freed earlier page may be reused while later pages remain held. The base PFN is
saved before the first return, and no released descriptor is consulted afterward.
Raw ordinary/DMA release remains unchanged, with no added descriptor scan.

unsafe try_split consumes a range. Exact ranges already have independent holds,
so it transfers them into SplitPages without count changes or exclusivity checks.
A compound, including a subview, requires the sole head hold and quiescent whole
backing. Success splits the whole compound and transfers all children, not just
the selected view. Failure returns the same original owner and bounds. The caller
must end compound contents/mappings/devices and required retirement; reference
exclusivity alone cannot prove those conditions.

SplitPages in src/page/split.rs is a transfer guard. next materializes a single-
page PageRef without another count, and Drop releases every untaken hold. Existing
exact sharing is preserved; compound split copies the original class without a
new allocation charge. No physical merge or arbitrary shared compound splitting
is implemented.

Allocation/release needs initialized global metadata, direct mapping, inventory
and per-CPU storage. Operations do not sleep and support ordinary/IRQ context,
not NMI. Exact claim/clone/drop and compound claim/retirement/split are linear
in their relevant page count. IRQ callers must bound work. Allocator lock order
is unchanged; counts never establish content-access or hardware-retirement rights.

Guest sources cover exact non-power-of-two counts and surplus accounting,
independent same-start/different-length views, partial survival and shared transfer,
compound tail raw restoration, exclusive split failure/success, early iterator
cleanup, zeroing, invalid geometry and single-page table adapters. Local metadata
tests additionally force a mid-range clone overflow and verify prefix rollback,
and reject holes before any independent range admission changes descriptors.

## Owned normal-memory zeroing

`zero::zero_owned_memory` initializes allocator-owned normal RAM used by the
global `alloc_zeroed` path and exact zeroed page allocation. On AArch64 it may
use `DC ZVA` when the requested range contains a complete aligned cache block;
the unaligned prefix and tail use bounded byte stores. The helper never widens
stores beyond the caller's allocation.
