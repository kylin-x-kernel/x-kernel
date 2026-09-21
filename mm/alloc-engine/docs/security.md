# alloc-engine: buddy safety and reliability

## Trust boundary

This allocator is an internal kernel mechanism. Callers must provide exclusively
owned, writable, page-aligned memory with a stable mapping, and keep it alive
until allocations and allocator bookkeeping are retired. The legacy integer
address APIs do not establish these ownership or mapping facts themselves.
Higher layers also ensure that stale translations or pinned users cannot access
pages returned for reuse. These external obligations remain unchanged.

## Unsafe operations and invariants

`add_region` checks alignment, minimum page size, address-range overflow and
region overlap before initializing a reserved bitmap prefix. Bitmap word access
uses indices derived from usable pages; storage is sized for all registered
pages and is never returned by allocation.

`push_free` initializes a `FreeNode` in an exclusively owned free chunk and links
it before setting its head bit. `is_free` rejects an unset bit before reading a
candidate header. `remove_free` only receives nodes from the matching list or a
successful bitmap-and-order check. List removal and bit clearing complete before
storage is returned or merged. `UnsafeRef` owns no storage and has no `Drop`, so
list operations can never free or move a chunk; page alignment and the minimum
page-size check satisfy the node layout requirements.

`Region::drop` calls `fast_clear` on every list instead of letting
`LinkedList::drop` walk them. A clearing drop stores an unlink marker into each
node, and those nodes live in caller memory that may already be reused or
re-registered by another allocator instance; the marker would then sit in a
block that a live bitmap still certifies, and a later removal would follow it as
a node pointer. Retiring a region therefore touches only the allocator's own
list headers. This crate also calls no allocating API of `intrusive-collections`
(its workspace `alloc` feature is unused here), keeping the allocator free of
recursive heap allocation.

All these transitions require `&mut` allocator access. Kernel callers serialize
shared instances using their existing allocator locks. The bitmap is not an
independent atomic cache and must never be updated outside this protocol.

## Threats and risk status

| ID | Risk | Concrete control | Status |
| --- | --- | --- | --- |
| A-01 | Live page bytes are interpreted as links | `Region::is_free` checks the out-of-band head bitmap before accessing `FreeNode`; live-payload permutation tests exercise this boundary | Mitigated within the caller ownership contract |
| A-02 | Bitmap pages are allocated or registered again | Allocation starts after the reserved prefix; overlap checks start at `region_start`; reserved-prefix test checks both paths | Mitigated |
| A-03 | A node is removed from the wrong order/list | Order is read only after bitmap validation; all removal paths use `remove_free` or `pop_free`; coalescing and fixed-address tests verify recoverable capacity | Mitigated |
| A-04 | Coalescing time grows with fragmented free-list length | At most one indexed membership check and direct unlink per order; no free-list traversal in `deallocate_pages` | Mitigated |
| A-05 | Double free, wrong allocation size, premature reuse or invalid backing pointer | Caller ownership, matching allocation/free sizes and higher-layer reuse completion are required; debug assertions detect some internal violations | External dependency; not fully detected by this API |
| A-07 | Retiring a region writes unlink markers into memory another allocator already owns | `Region::drop` calls `fast_clear`, which drops list headers only and performs no node stores | Mitigated |
| A-06 | Concurrent list/bitmap updates diverge | Exclusive allocator access and `kalloc` locking serialize the full operation | External synchronization required |

## Failure and resource handling

| Failure mode | Cause and effect | Handling |
| --- | --- | --- |
| Registration rejected | Misalignment, invalid range or overlap prevents adding capacity | `add_region` returns `InvalidInput` or `MemoryOverlap` before initializing metadata |
| Allocation exhausted | No suitable block remains; a higher-layer request cannot proceed | Allocation returns `NoMemory`; the caller selects retry or OOM policy |
| Ownership or bookkeeping corrupted | Double free or an external overwrite can corrupt lists and allow overlapping allocations | Debug assertions and permutation/forged-header tests detect regressions; there is no recovery from arbitrary kernel memory corruption |

Invalid registration and overlaps return `AllocError` before writing new bitmap
storage. Allocation exhaustion returns `NoMemory`; no partial allocation is
published. Metadata reservation reduces allocatable capacity and may alter the
largest available block. Freeing does not scrub old payload: initialization and
confidentiality before reuse remain the caller's responsibility.

Backing memory is not freed by `reset` or Drop. Test fixtures explicitly retire
the nested allocator before releasing backing allocations with their original
layouts. Audits must check every new split/removal path for synchronized head-bit,
list and free-page-count updates, and every metadata access for region bounds.

## Privacy and external interfaces

Freed chunks may contain prior kernel or user payload. The allocator writes only
its free headers and private bitmap; it neither logs payload nor guarantees
zero-filled allocations. Callers must initialize pages before exposing them to
another protection domain. There is no direct user-pointer, device, DMA, firmware
parser or syscall interface here; trusted callers translate those inputs into
owned, mapped memory and validated allocation requests.
