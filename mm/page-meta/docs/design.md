# page-meta — Design Document

## Scope and Positioning

Establishes a stable metadata identity for physical base pages. The boot caller holds a list of physical ranges; `init()` normalizes it in place
and deducts the metadata backing region, then returns a PageMetaTable. After the caller publishes the unified lookup, the remaining
non-empty ranges in the list are handed to the allocator. Physical ranges reuse memaddr::PhysAddrRange; no separate inventory type is wrapped around it.

PageMeta records the permanent reservation of this service's backing storage,
an optional allocation-head reference count, head/tail geometry, and the
allocation round's UsageKind.
The class preserves original allocation accounting, not actual page type, current
users, content permissions or reclaimability. Metadata does not track allocator
inventory. Lookup grants no ownership, content access or release authority.
Current input covers only RAM marked FREE by HAL.

## Architecture and Responsibilities

- `src/lib.rs`: errors, public initialization and identity queries, and global publication.
- `src/page.rs`: PageMeta, head counter, tail offset, allocation order and reference-state
  transitions. It defines no RAII holder and does not return inventory.
- `src/page/group.rs`: independent-range admission/clone/validation and compound
  group admission/resolution, retirement and exclusive split.
- `src/free_ranges.rs`: organizes the caller's free physical ranges and carves the backing memory out of them.
- `src/backend/mod.rs`: selects the private storage and PFN index implementation, and exports init and Table.
- `src/backend/segments.rs`: the current compact segment index, array storage, lookup and initialization writes.
- `ut-tests/{free_ranges,segments,page,group}.rs`: handoff, bidirectional page identity,
  reference-state transitions and boundary cases.
- `core/kruntime/ut-tests/page_metadata.rs`: the existing allocator handoff and global lookup.

```text
kruntime holds the physical range list
  → page_meta::init(&mut ranges, map_storage)
       → shared function normalizes the ranges
       → backend builds the coverage index, requests the backing region, initializes
       → shared function deducts the backing region, keeps the remaining physical ranges
  → publish(table)
  → iterate the non-empty remaining ranges, p2v, then hand to the existing allocator
```

The initial state and permanent flag of PageMeta are defined by shared methods. The backend computes the storage requirement, organizes the
index and returns stable borrows; it does not decide the global publication policy and has no knowledge of the allocator's bitmap or free lists.
The caller's mapping callback receives only the complete backing region actually requested by the backend, and does not require all descriptor page contents to have an existing
kernel mapping. The descriptor of a metadata page and the physical page content it describes are different objects.

## Initialization and Inventory Rules

1. The shared normalize step validates start/end order and address arithmetic, aligns inward to whole 4 KiB pages, ignores empty ranges,
   sorts by PA, rejects physical overlaps, and merges adjacent physical ranges. Unused entries are set to empty, and the slice length is unchanged.
2. The backend directly iterates the ranges already normalized by the shared layer and builds the segment table and descriptor count before any inventory deduction;
   it does not re-sort, re-merge or re-validate the input ranges. The reserved pages themselves must also have a PageMeta; the remaining balance must not be mistaken for coverage.
3. The shared with_reserved_storage selects storage in whole pages from the tail of the largest available range, and the backend maps and initializes it.
   Inventory is deducted only if that initialization succeeds; if mapping or initialization fails, that request is not deducted.
4. covered_pages() derives coverage from the final segment. lookup(PFN) and
   pfn_of(metadata) provide bidirectional identity conversion. Reverse lookup
   checks descriptor membership and uses a binary segment search; it needs no
   page-content mapping. Table stores no coverage count, and PageMetaTable stores
   no metadata-storage count.
5. kruntime publishes serially, then initializes the allocator with the first non-empty range in ascending PA order and hands over the subsequent non-empty ranges in turn,
   preserving buddy's low-address-first allocation order and preventing DMA32 requests from first hitting high-address RAM.
   Empty entries must be skipped; the allocator deducts its own internal metadata itself. The metadata overhead in the boot log is computed transiently as
   the number of covered pages minus the number of pages remaining before handoff, and is not persistent state of the table.

On error, no metadata table is published. The input slice may already have been normalized when an error occurs; if a future backend reserves multiple times, earlier
successful deductions remain in effect, and no rollback across requests is promised. If allocator initialization fails after the metadata table is published, boot stops under the
fatal-boot policy, and the whole transaction is not retried.

## Backend Boundaries and Limits

The public API does not expose the segment count, array indices, backing contiguity or index capacity. The current private backend first
computes the segment table, counts and storage size in local variables, then reserves and initializes the contiguous backing array, and finally
constructs a valid Table in one step. Table stores only the segment table, the number of valid segments and the descriptor pointer. The entry_count needed during construction is used only to
compute the array byte size and initialize the slots, and is not kept after construction.
The capacity is currently 64 merged physical segments; this is a limit of that
implementation and is unrelated to the length of the caller's list or the allocator's limits. The backend is selected through compile-time modules.

A future backend may change the index, the storage chunking and the number of requests. Schemes that need extra virtual-address/page-table capabilities
still require the boot integration to provide the corresponding resources; the current interface does not promise to establish mappings itself.

## Concurrency, Context and Lifetime

The primary core initializes serially before the heap, other allocator users and secondary-core startup; the heap is not used internally.
The mapping callback must also be valid in that context. After initialization and permanent marking complete, klazy::Once is responsible for publishing
visibility; shared lookup does not allocate, wait, take locks or write descriptors.

The metadata table is movable, and descriptor addresses do not change. The metadata table does not return the original RAM on Drop. The caller keeps the backing region and mappings until the metadata table and all
borrows are destroyed; after global publication they must remain valid permanently. Local tests first destroy the metadata table, allocator
and borrows, then reclaim the fixture. Lookups after publication are independent of the current task or any particular allocator instance.

kalloc raw release relies on the original allocation ownership contract rather
than checking every page's references or permanent flag. Callers must not return
reference-owned or permanent pages as raw allocations. This service neither
synchronizes buddy bitmap reservations nor detects raw-protocol double frees.
A permanent flag of false does not mean the page can be allocated or released.


## Allocation references and head/tail geometry

Each descriptor identifies one physical 4 KiB page. An independent page has
order zero and a zero head offset. An explicitly compound allocation has one head carrying
its order, original UsageKind and reference count. Each tail stores its distance
in base pages to that head. Tail counters do not represent independent holds.
The offset is a u32 page distance, not a truncated physical address or descriptor
pointer; PFN resolution remains behind the table API. The head order and all
member offsets are stable while any allocation hold exists.

`PageMetaTable::claim_reference_group` validates the entire geometry, coverage,
permanent status and unclaimed state before changing anything. It enters
Initializing at the head, initializes order/class and tail offsets, and publishes
the first head reference with Release after all links are established. Ordinary
errors leave descriptors unchanged. The allocator supplies exclusive unpublished
backing; metadata lookup by itself cannot prove that exclusivity.

`reference_allocation` resolves any selected member to its head PFN and order.
It is unsafe because the caller must retain an existing hold throughout the
read; independent atomic field reads are not a coherent ownership snapshot.
Clone and release operate on the resolved head's single counter. The count limit
excludes Initializing, Splitting and Retiring. Overflow returns ReferenceOverflow.
A subpage hold keeps the entire unsplit allocation alive.

The unique last release acquires earlier holders' releases and enters Retiring.
The holder first snapshots allocation geometry. `finish_reference_retirement`
clears every tail link while backing remains outside allocator inventory, resets
the head order/count, and returns the original class. kalloc then returns the
complete saved range without writing old metadata again. Admission and final
retirement are O(backing pages); clone and member resolution do not visit the
whole range. Raw allocator release does not gain a descriptor scan.

## Independent reference ranges

claim_reference_range validates every descriptor before claiming one independent
reference per page. Each admitted page keeps order zero, offset zero and its
original class. The caller owns all resulting holds; metadata does not store a
range-owner length because several handles may overlap with different bounds.
reference_range validates a transferred range: all pages must be independent or
all selected pages must fit in the first member's single compound. It requires
pre-existing lifetime protection rather than acquiring ownership from snapshots.

clone_reference_range consumes no original holds. Independent mode increments
each page, rolling back every successful prefix increment if a later counter
fails. Originals make rollback retirement impossible. Compound mode increments
only its head. All shape reads occur while caller-held references exclude geometry
changes. Independent range final release is performed per page by the allocator
holder, using existing order-zero retirement; there is no new range counter.

## Exclusive splitting

`split_reference` consumes one whole-allocation ownership obligation only on
success. It resolves geometry, then atomically freezes a head count of exactly
one into Splitting. Other holds, including raw transfers, yield NotExclusive
with no change. Acquire joins prior holders' releases. There is no fallible
operation after freezing: every tail becomes an independent order-zero page
with count one and the original class, and the head is published last as another
order-zero page. No content is moved and no allocator/accounting action occurs.

The caller must end content, mapping and hardware use before the unsafe split.
Reference exclusivity cannot prove hardware retirement. Success transfers one
hold per page to the caller, which must consume the old owner without releasing
it and retain/release every resulting page. kalloc's SplitPages transfer iterator
implements that cleanup obligation without a heap array or second reference
protocol. Splitting is O(backing pages) and does not support transferring arbitrary
shared/mapped references to children.

## Layout and holder boundary

`PageMeta` keeps an AtomicUsize counter, AtomicU32 relative head offset,
AtomicU8 class, AtomicU8 order and the independent immutable permanent bool.
A compile-time assertion requires 16 bytes on 64-bit targets; the public API does
not promise field offsets. Boot storage is still reserved from the actual type
size. The offset range is validated before admission, and direct field encoding
is not exposed as a public tag or allocator policy.

kalloc::PageRef stores the first descriptor and selected page count. It uses
one independent hold per selected page or one compound hold. Its range length
is handle-specific; the selected PFN and compound head PFN can differ. RAII ownership, raw transfers,
usage totals and returning inventory belong to kalloc. page-meta owns only
identity, geometry and reference transitions; it has no reclaim callback, direct
mapping dependency or allocator instance pointer. Dependencies remain
kalloc -> page-meta. global_table borrows the immutable published directory and
grants no ownership.

ReferenceState is a best-effort diagnostic with Tail and Splitting states,
not a coherent role/count snapshot. Unreferenced does not mean free: heap,
PCP, bitmap and raw users can own storage outside the reference protocol.
is_permanent remains a separate bool set before publication; it excludes this
service's metadata backing for the kernel lifetime and has no unreserve API.
UsageKind retains diagnostic allocation accounting, not actual page type or
reclaim policy. Clone/split do not reclassify or repeat the allocation charge.

The local group tests cover tail resolution, complete retirement and reuse,
failed exclusivity, successful split through a tail, class preservation and
coverage/permanent/geometry rejection before any claimed prefix. Range tests
force a later reference overflow and verify complete prefix rollback, plus exact
range coverage failure with unchanged descriptors. Existing
counter-overflow and reservation tests continue to cover the low-level protocol.
