# page_table — Security and Reliability Analysis

## Trust Model

```text
Callers (memspace, kexec, ...)
   │
   │ safe API: PageTable64::query/query_entry/modify,
   │           PageTableMut::map/unmap/remap/protect/replace_if_same/finish
   │
   v
┌──────────────────────────────────────────────────────────┐
│ page_table                                               │
│                                                          │
│ ┌── Unsafe boundary ───────────────────────────────────┐ │
│ │ table64.rs: alloc_table() — write_bytes              │ │
│ │ table64.rs: table_of() — from_raw_parts              │ │
│ │ table64.rs: table_of_mut() — from_raw_parts_mut      │ │
│ └──────────────────────────────────────────────────────┘ │
│                                                          │
│ ┌── Indirect safety obligations: PagingHandler ────────┐ │
│ │ H::alloc_frame() / H::dealloc_frame()                │ │
│ │ H::p2v() — physical-to-virtual address translation   │ │
│ └──────────────────────────────────────────────────────┘ │
│                                                          │
│ ┌── Indirect safety obligations: PagingMetaData ───────┐ │
│ │ M::flush_tlb() — TLB invalidation                    │ │
│ │ M::vaddr_is_valid() — virtual address validation     │ │
│ │ M::paddr_is_valid() — physical address validation    │ │
│ └──────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────┘
```

- **Safe API callers** trust the module to maintain PTE invariants and TLB
  consistency.
- **PagingHandler implementors** must ensure that `p2v()` returns a valid
  virtual address and `alloc_frame()` returns an aligned physical frame.
- **PagingMetaData implementors** must ensure that `flush_tlb()` invalidates
  the required translations and that `vaddr_is_valid()` / `paddr_is_valid()`
  reflect hardware address constraints. An implementor that clears
  `INVALID_TO_VALID_NEEDS_TLBI` asserts that its walkers cannot hold a
  translation for an invalid entry, and must supply a `fence_fresh_map()`
  that orders the entry write against every other translation table walker
  that may observe it.

## Unsafe Code Inventory

### 1. `alloc_table()` (`table64.rs`)

```rust
unsafe { core::ptr::write_bytes(ptr, 0, PAGE_SIZE_4K) };
```

**Invariant**: `ptr` is the virtual address obtained by applying `H::p2v()` to
the physical frame returned by `H::alloc_frame()`. The frame is allocated,
aligned to 4 KiB, and `PAGE_SIZE_4K` bytes long.

**Safety argument**: `H::alloc_frame()` must return a valid, exclusively
allocated physical frame. `H::p2v()` must provide a writable virtual mapping
that uniquely corresponds to that frame.

**Callers**: `PageTable64::try_new()` and
`PageTableMut::next_table_mut_or_create()` request a new table. The unsafe
write occurs only after frame allocation succeeds.

On AArch64 the clear is followed by `karch::dsb_ishst()` before `alloc_table()`
returns. A parent descriptor or TTBR can therefore expose only a completed
clear, even to hardware walkers that do not observe the software lock. A
finalization barrier after publishing the parent would leave a window in which
old frame contents could be consumed as valid descriptors.

This mitigation and its KVM evidence cover AArch64. The generic initialization
ordering obligation is documented on `PagingMetaData`; other backends retain
their existing protocols and require separate architecture review. This audit
neither certifies their publication ordering nor establishes a missing barrier.

### 2. `table_of()` (`table64.rs`)

```rust
unsafe { core::slice::from_raw_parts(ptr, ENTRY_COUNT) }
```

**Invariant**: `paddr` identifies a valid page table frame aligned to 4 KiB,
`H::p2v(paddr)` maps that frame, and its contents are `ENTRY_COUNT` valid PTEs.

**Safety argument**: `alloc_table()` allocates and zeroes the frame. For the
supported PTE types, its size is exactly `ENTRY_COUNT * size_of::<PTE>()`,
or 512 × 8 = 4096 bytes. The PTE types implement `Copy` and have no destructor
side effects. The frame must remain live throughout every access to the slice.

**Callers**: `walk_page_table!` in `ref` mode, `dealloc_tree()`, and `next_table()`.
Root access relies on ownership of the allocated root frame. Descending into a
subtable relies on the intermediate entry identifying a live table frame;
`next_table()` and `next_table_mut()` reject a zero physical address,
retain the huge-page error, and require `is_table()` before translating a child
address. This also protects recursive destruction from non-present huge leaves
whose architecture-specific `is_huge()` predicate returns false.

### 3. `table_of_mut()` (`table64.rs`)

```rust
unsafe { core::slice::from_raw_parts_mut(ptr, ENTRY_COUNT) }
```

**Invariant**: The requirements for `table_of()` also apply, and the caller
must have exclusive write access to the page table frame.

**Safety argument**: `PageTableMut` holds `&mut PageTable64`, providing exclusive
access through that page table object. The frame layout is the same as for
`table_of()`. Callers must also prevent conflicting accesses through any
borrowed subtrees shared by `copy_from()`.

**Callers**: `walk_page_table!` in `mut` mode, `walk_page_table_create!`,
`next_table_mut_or_create()`, and `copy_from()`, all through mutable access.

### 4. AArch64 User Page Table Destruction (`table64.rs`)

`Drop` reads the current ASID through the registered provider, executes
`dsb ishst` through the safe `karch::dsb_ishst()` primitive, and synchronously
invalidates translations with
`PagingMetaData::flush_tlb_process_asid(None, asid)` before returning table
frames to the allocator.

**Invariant**: The provider context remains valid throughout destruction and
the callback performs read-only access. The caller has already stopped hardware
use of the page table being destroyed. The barrier orders earlier PTE stores,
and invalidating the entire ASID covers non-leaf translation caches.
`MmSpace` declares `pgtbl` before `user_asid_context`, so field destruction drops
the page table first and preserves the callback context until it is no longer
needed.

## Memory Safety Invariants

1. **Valid PTE physical addresses**: physical addresses in present PTEs must
   identify valid backing frames that have not been released by another owner.
2. **Page table frame lifetime**: `PageTable64` owns `root_paddr` and all subtable
   frames it allocates recursively. The caller must stop hardware use before
   `Drop`. An AArch64 user table with a registered ASID provider synchronously
   invalidates that ASID's leaf and non-leaf translation caches before freeing
   its first table frame, then releases frames recursively. The provider context
   must outlive the entire destruction sequence.
3. **TLB consistency**: PTE changes require TLB invalidation to prevent CPUs from
   using stale mappings. `PageTableMut::finish()` and `Drop` finalize pending
   invalidations. The one exception is an invalid-to-valid transition on an
   architecture that reports `INVALID_TO_VALID_NEEDS_TLBI == false`: no stale
   translation can exist for a slot that was verified unused, so `map()`
   records a fence request instead of an invalidation, and `finish()` emits
   `fence_fresh_map()` once per boundary unless an invalidation already ran,
   because that sequence already provides the architecture's publication barrier.
   Every other transition (valid-to-valid replacement, permission change, unmap, teardown)
   still invalidates unconditionally.
   On AArch64, each `karch` invalidation helper completes preceding PTE stores
   with `dsb ishst` before issuing TLBI, then completes the invalidation with
   DSB and ISB. A post-TLBI DSB alone does not order the PTE store before a
   walker can refill the invalidated translation.
4. **Injective p2v mapping**: `H::p2v()` must map distinct physical addresses to
   distinct virtual addresses; otherwise references created by `table_of` /
   `table_of_mut` may alias.
5. **Frame alignment**: `H::alloc_frame()` must return physical addresses aligned
   to 4 KiB so that PTE encoding and page table frame layout remain valid.
6. **Address validity**: physical addresses passed to `map`/`remap` must be within
   `PA_MAX_ADDR`, and virtual addresses must have the architecture's canonical
   form. The same virtual address requirement applies to `query`/`protect`/`unmap`.
   Violations return `PtError::InvalidAddress`.
7. **Conditional replacement preserves intervening changes**: `replace_if_same()`
   may replace a present leaf PTE only when it exactly matches `PteSnapshot`.
   If the PTE has changed or is no longer present, the operation must return
   `PteReplaceError::Changed` without overwriting the current mapping.
8. **Encapsulated snapshot bits**: only `page_table` interprets the raw PTE bits
   in `PteSnapshot` for comparison. External callers depend on the physical
   address, permissions, and page size, not architecture-specific PTE encoding.
9. **Explicit flush-before-free boundary**: before releasing a physical page
   removed from a PTE, callers must explicitly call `PageTableMut::finish()`.
   `Drop` is a fallback and must not serve as an implicit proof that old pages
   are released after invalidation.
10. **Non-leaf translation caches**: per-VA leaf invalidation alone does not
    retire cached table-walk descriptors before table frames are reused.
    AArch64 user page table destruction with a registered ASID provider uses
    `dsb ishst` and invalidation of the entire ASID to establish this reclamation
    boundary. The caller must still stop hardware use of the address space.

## Thread Safety

`unmap_sparse_region` requires a uniform leaf size and validates address/length
alignment, checked end arithmetic, canonical endpoints, and root-table coverage
before clearing entries. Missing subtrees and non-present leaf entries are
skipped without allocation or invalidation. Only architecture-classified table
pointers are descended into, so non-present huge leaves cannot make the walker
interpret backing data as a page table. A different present leaf size
returns `MappedToHugePage` instead of clearing outside the requested range.
On traversal error, a cleared prefix remains pending in the guard; callers must
not release backing frames as if the whole range succeeded. No intermediate
table frame is reclaimed by this API, so it introduces no table-walk-cache
reclamation boundary. All cleared leaves retain the normal flush-before-free
obligation. Tests cover sparse/dense boundaries, high addresses, three-level
tables, huge leaves, invalid ranges, and deferred flush state.

| Type | `Send` condition | `Sync` condition |
|------|------------------|------------------|
| `PageTable64<M, PTE, H>` | Automatically `Send` when `M, PTE, H: Send`. | Automatically `Sync` when `M, PTE, H: Sync`; there is no internal lock. |
| `PageTableMut<M, PTE, H>` | Requires `PageTable64: Send` and `M::VirtAddr: Send`. | Requires `PageTable64: Sync` and `M::VirtAddr: Sync`; mutation still requires exclusive access. |
| `PagingFlags` | Automatically `Send` for its scalar representation. | Automatically `Sync` for its scalar representation. |
| `PageSize` | Automatically `Send` for its fieldless enum representation. | Automatically `Sync` for its fieldless enum representation. |
| `PtError` | Automatically `Send` for its fieldless enum representation. | Automatically `Sync` for its fieldless enum representation. |
| PTE types such as `X64PageEntry` | Required by the `PageTableEntry: Send` bound. | Required by the `PageTableEntry: Sync` bound. |

These auto-traits do not serialize page table mutations or accesses through
shared physical subtrees. Callers must provide the necessary synchronization.

## Threat Analysis

Status describes the controls present in this revision:

- **Mitigated**: this crate implements the stated control for callers that obey
  its API and provider contracts.
- **Partial**: this crate implements a control, but the named caller obligation
  remains outside its enforcement.
- **Delegated**: the named external component owns the control; this crate
  cannot independently establish the required invariant.
- **Accepted limitation**: the current API deliberately leaves the stated
  behavior to its caller. This does not mean the resulting misuse is safe.

| ID | Threat | Impact | Trigger | Mitigation and verification | Status |
|----|--------|--------|---------|-----------------------------|--------|
| T-01 | `PagingHandler::p2v()` returns an incorrect virtual address, causing invalid memory access through table slices. | High | A faulty conversion or a missing direct-map mapping. | [`PagingHandlerImpl::p2v`](../../../arch/khal/src/paging.rs) delegates through `khal::mem::p2v` to [`kaddr_layout::p2v`](../../kaddr_layout/src/lib.rs), which uses checked addition of `PAGE_OFFSET`; `v2p` rejects addresses outside the linear-map and kernel-image windows. The provider verification conditions below check the remaining mapping obligation. No page-table-local check proves that the returned virtual page is mapped. | Delegated to `khal`, `kaddr_layout`, and the direct-map owner. |
| T-02 | `PagingHandler::alloc_frame()` returns a live frame again, causing page table frames to alias. | High | Duplicate allocation by the frame allocator. | [`PagingHandlerImpl::alloc_frame`](../../../arch/khal/src/paging.rs) asserts `is_page_allocator_ready()` and calls `alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageTable)`. [`GlobalAllocator::alloc_pages`](../../kalloc/src/lib.rs) protects PCP access with `IrqSave` and global allocation with `palloc.lock()`. Verify live-frame uniqueness and alignment as specified below. `alloc_table()` zeroes frames but does not detect duplicates. | Delegated to `kalloc` / `alloc-engine`; uniqueness is not rechecked here. |
| T-03 | A PTE changes without TLB invalidation, leaving stale mappings in use. | High | Direct memory writes bypass `PageTableMut`. | `map`, `unmap`, `remap`, `protect`, and `replace_if_same` record invalidations; `PageTableMut::drop` calls `finish()`. `finish_reports_explicit_flush_boundary` checks pending-state finalization. `map` skips the invalidation only for a slot it verified unused and only when the architecture reports `INVALID_TO_VALID_NEEDS_TLBI == false`; `fresh_map_fences_once_without_invalidating` and `unmap_still_invalidates_on_a_fence_only_architecture` pin that boundary, and `fresh_map_invalidates_when_architecture_requires_it` pins the conservative default. Direct PTE memory writes remain outside this API contract. | Mitigated for the listed mutation APIs. |
| T-04 | Unsynchronized mutations or aliases of a shared page table cause a data race. | High | Callers bypass exclusive access or mutate shared physical subtrees without coordination. | `PageTable64::modify(&mut self)` requires exclusive Rust access to one table object. The caller must also lock accesses through distinct objects sharing `copy_from()` subtrees; the borrow checker cannot detect those physical aliases. | Partial; shared-subtree synchronization belongs to callers. |
| T-05 | Dropping a `copy_from` source leaves borrowed entries pointing to freed frames. | High | The source is dropped before its destination. | `copy_from()` marks `borrowed_entries`, and `PageTable64::drop` skips those subtrees. This prevents double release by the borrower but does not keep the source alive. The source-outlives-destination contract must be checked at each call site. | Partial; source lifetime is caller-owned and not type-checked. |
| T-06 | `map_region` leaves partial mappings after failure. | Medium | An intermediate mapping returns `AlreadyMapped` or `NoMemory`. | `map_region()` returns the first `map()` error without rollback. Reproduce by pre-mapping a later page, mapping a region spanning it, then querying the successful prefix. Callers requiring atomic behavior must clean up that prefix without removing pre-existing mappings. | Accepted limitation; rollback and recovery belong to the caller. |
| T-07 | An incorrect SEV C-bit position applies the wrong encryption attribute. | High | `kbuild_config::SEV_CBIT_POS` differs from the hardware setting. | `EncodedPtePhys` centralizes C-bit encoding and decoding in `arch/x86_64.rs`; validate the build's `SEV_CBIT_POS` against the deployment hardware. The wrapper does not discover or validate that hardware setting. | Delegated to build and platform configuration. |
| T-08 | Invalid addresses reach mapping or query operations. | High | A noncanonical virtual address or an unsupported physical address. | Entry points call `vaddr_is_valid()` and, for physical mapping inputs, `paddr_is_valid()`, returning `InvalidAddress` on failure. `test_paging_metadata_bounds` checks physical bounds. These checks establish address format/range, not mapping ownership or the validity of `H::p2v()`. | Mitigated for address range errors; metadata correctness remains a provider contract. |
| T-09 | A COW commit overwrites an intervening PTE update. | High | The fault handler uses unconditional `remap()` after preparing a page. | `replace_if_same()` compares the snapshot and returns `Changed` without writing on mismatch. `replace_if_same_reports_changed_without_overwrite` verifies the competing mapping survives. The caller must abort its prepared resource on that result. | Partial; PTE preservation is implemented, resource abort is caller-owned. |
| T-10 | An old data page is reused before invalidating its former translation. | High | An unmap or COW replacement frees a page before a deferred flush. | `PageTableMut::finish()` returns `TlbFlushReceipt`; `finish_reports_explicit_flush_boundary` checks finalization. Audit data-page release sites for a preceding explicit `finish()` after the PTE change. The receipt does not prevent a caller from releasing early. | Partial; release ordering is enforced by callers. |
| T-11 | A cached intermediate descriptor reaches a reused page table frame. | High | An AArch64 user page table is destroyed before retiring its ASID's walk-cache entries. | With a registered provider, `PageTable64::drop` executes `dsb ishst` and `flush_tlb_process_asid(None, asid)` before the first `dealloc_frame()`. `user_drop_invalidates_latest_asid_before_freeing_frames` verifies full-ASID scope, the current ASID, and flush-before-free ordering. | Partial; hardware quiescence and provider lifetime remain address-space-owner obligations. |

For T-01, provider integration validation must check that each returned table
frame has a full writable 4 KiB mapping within the live linear-map RAM region,
that `v2p(p2v(pa)) == pa`, and that distinct live frames have distinct virtual
addresses. Round-trip arithmetic alone cannot establish that a PTE exists.
`vaddr_is_valid()` / `paddr_is_valid()` validate mapping inputs; neither checks
the address returned by `H::p2v()` or establishes the direct map.

For T-02, an allocator validation must hold multiple frames live simultaneously,
check `pa % PAGE_SIZE_4K == 0` and pairwise-distinct physical addresses, then
release each frame exactly once and exercise reuse. Existing
[`alloc-engine` tests](../../alloc-engine/src/buddy_alloc.rs), including
`test_alloc_one_page` and `test_alloc_exhaust_then_free`, check alignment,
exhaustion, and returned capacity; they are not an end-to-end proof of the
production handler's uniqueness under concurrent PCP allocation. These are
external validation obligations, not new runtime checks in `page_table`.

### AArch64 Teardown Regression Coverage

The `table64.rs::tests::aarch64_drop` tests run the actual AArch64 destructor
with a recording `PagingMetaData` implementation and frame-release handler:

- `user_drop_invalidates_latest_asid_before_freeing_frames` maps a page, completes
  its leaf invalidation, changes the provider's ASID, and then drops the table.
  It checks exactly one full-ASID invalidation using the new ASID, no per-VA or
  unscoped invalidation during teardown, and no frame release before that call.
- `user_drop_without_provider_skips_asid_flush` checks the unregistered path.
- `kernel_drop_skips_user_asid_flush` checks the kernel-table exclusion even
  when a provider is installed. Both exclusion cases still release all table
  frames.

### Fresh-Map Invalidation Policy Coverage

The `table64.rs::tests` fresh-map cases run a recording `PagingMetaData` whose
`INVALID_TO_VALID_NEEDS_TLBI` is `false`, modeling the batching contract used by
AArch64's current fence-only policy. The recording metadata substitutes for the
real architecture implementation:

- `fresh_map_fences_once_without_invalidating` installs two pages into unused
  slots in one boundary and checks that no invalidation was issued, that the
  receipt reports nothing pending, and that the two maps share a single fence.
- `unmap_still_invalidates_on_a_fence_only_architecture` checks that removing
  the mapping still invalidates and does not add a redundant fence.
- `fresh_map_invalidates_when_architecture_requires_it` checks that a metadata
  implementation keeping the default still reports a pending invalidation for
  the same fresh map.

`aarch64_fresh_map_uses_publication_fence` only asserts the AArch64 policy
flag. It does not detect removal of the `alloc_table()` or `fence_fresh_map()`
DSB. Recording a hook invocation would not prove the instruction's presence or
its ordering against a hardware walker either.

The AArch64 policy test is selected by the kernel's compilation target, not
by the CI build host; a cross-compiled AArch64 guest still includes it. This
selection does not make its assertion a hardware-ordering test.

The recording tests use serial execution and scoped probe resets to isolate
process-wide counters. Their page tables are never installed in hardware.
They validate bookkeeping and release order, not microarchitectural cache
eviction or cross-CPU visibility. Invalid-entry caching is an architecture
property. Barrier presence and placement, including the real
`karch::flush_tlb_asid()` sequence, require architecture code review.
The original AArch64 KVM single-factor exec control and recovery daily/Open
POSIX runs provide separate hardware-facing regression evidence, with versioned
logs and limits in
[design.md](design.md#aarch64-fresh-map-recovery-evidence). Host unit tests and
counter hooks cannot substitute for those runs.

Run the tests on AArch64 through the project configuration/build flow:

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make unittest UNITTEST_CRATE=page_table
```

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|----|--------------|-------|--------------|---------------|----------|------------|
| F-01 | `alloc_table()` cannot allocate a frame. | Physical memory exhaustion. | A new page table or mapping cannot be created. | Process or address space creation fails. | 2 | Return `PtError::NoMemory`; callers may reclaim memory and retry. |
| F-02 | `unmap` targets an unmapped address. | Caller logic error. | Returns `NotMapped`. | No mapping change. | 4 | Return an error for the caller to handle. |
| F-03 | `map` targets an already mapped address. | Duplicate mapping request. | Returns `AlreadyMapped`. | No existing mapping is overwritten. | 4 | Return an error instead of silently overwriting the entry. |
| F-04 | Excessive recursion in `dealloc_tree`. | Unsupported or excessive page table depth. | Stack overflow. | Kernel crash. | 1 | Supported page tables have at most four levels. Each level has 512 entries, but sibling entries do not increase recursion depth. |
| F-05 | A TLB shootdown IPI is lost. | A remote CPU does not respond in SMP mode. | The remote CPU retains stale mappings. | Memory consistency is violated. | 1 | The IPI implementation must deliver shootdowns reliably; AArch64 uses hardware TLBI broadcast without software IPIs. |
| F-06 | `protect_region` skips unmapped pages. | Holes in the requested region. | Permissions change only on mapped pages. | A caller's assumption of complete region coverage may be invalid. | 2 | Unmapped pages are silently skipped in 4 KiB steps; callers requiring full coverage must ensure the region is mapped. |
| F-07 | `remap` changes an entry while it is in use. | Uncoordinated access to the same address space. | Stale translations may remain in use before finalization. | Data corruption. | 1 | Callers must synchronize updates and complete the required TLB invalidation before reusing old resources. |
| F-08 | An invalid address is passed to mapping or query operations. | The caller omits address range validation. | Returns `InvalidAddress`. | The operation is rejected. | 4 | Validate with `vaddr_is_valid`/`paddr_is_valid` at operation entry and return an error. |
| F-09 | `unmap` clears a non-present entry. | Clearing before checking presence. | Architecture-specific state or reserved bits may be modified incorrectly. | Undefined hardware behavior. | 2 | Check `is_present()` before `clear()`; return `NotMapped` for a non-present entry. |
| F-10 | `map_region` panics partway through. | The `phys_getter` closure panics. | Some mappings are installed with invalidations still pending. | TLB consistency depends on cleanup running. | 2 | `PageTableMut::Drop` finalizes recorded invalidations if destruction runs; callers must keep `phys_getter` free of panics and cannot rely on unwinding in aborting configurations. |
| F-11 | Mapping or unmapping runs in interrupt context. | An interrupt handler invokes page table operations. | Locks or shootdown completion may deadlock. | System hang. | 1 | Callers must respect allocator, lock, and TLB completion context requirements; do not use `PageTableMut` in interrupt context without establishing that these requirements are met. |
| F-12 | Prepared pages leak after `replace_if_same` returns `Changed`. | The caller omits abort or cleanup. | A page or object reference leaks. | Persistent memory leakage. | 2 | `page_table` only preserves the current PTE; the COW, anonymous-page, or file-page owner must release prepared resources on `Changed`. |
| F-13 | An unmapped frame is freed before explicit flush finalization. | Incorrect resource release order. | A stale TLB entry may still access the reused frame. | Isolation failure or data corruption. | 1 | Release only after `finish()` returns; audit `dealloc_frame` and object-page release call sites. |

## Failure Handling

- **Error codes**: `PtError` covers six conditions: `NoMemory`, `NotAligned`,
  `NotMapped`, `AlreadyMapped`, `MappedToHugePage`, and `InvalidAddress`.
  With `feature = "kerrno"`, these convert to `KError`.
  Conditional replacement uses `PteReplaceError` to distinguish page table
  errors from an intervening PTE change that may require a retry.
- **TLB finalization**: `PageTableMut::finish()` returns `TlbFlushReceipt` to mark
  a completed flush boundary. The receipt is not an error code and does not
  specify the architecture's invalidation mechanism.
- **Panic policy**: ordinary operation failures use typed errors.
  Traversal macros and `copy_from()` contain `unreachable!()` branches for
  unsupported `LEVELS` values; all provided architectures use three or four
  levels. Caller callbacks and violated preconditions must not be assumed to
  be free of panics.
- **Recovery**: page table operation errors are returned through `PtResult`,
  while conditional replacement uses `Result<_, PteReplaceError>`.
  Callers choose whether to retry or fall back.

## Privacy Analysis

The module does not process user data directly, but virtual address mappings
determine memory isolation between user processes. Higher layers must ensure:

- Page tables for distinct address spaces preserve the intended isolation.
- Kernel mappings are inaccessible to user mode, with page table isolation
  such as KPTI where required.
- SEV C-bits are configured correctly to protect encrypted memory.
- Invalid addresses cannot create unintended mappings through `map`.

## Known Limitations

1. `PageTable64` has no internal lock. Callers must synchronize mutations to
   shared page tables independently of their `Send`/`Sync` auto-traits.
2. `map_region` does not roll back earlier mappings after a partial failure;
   callers must handle cleanup.
3. `copy_from` requires the source page table to outlive the destination.
   The compiler does not enforce this lifetime relationship.
4. `walk_page_table!` supports only three and four levels. Five-level layouts,
   such as x86_64 LA57, require extensions.
5. `dealloc_tree` frees tables recursively. Stack usage depends on the number
   of levels, which is bounded by four for the supported layouts.
6. Correctness of `vaddr_is_valid` / `paddr_is_valid` depends on the
   `PagingMetaData` implementation and is not checked at compile time.

## Audit Checklist

When modifying this module, verify that:

- [ ] Every `unsafe` block has a `SAFETY:` comment.
- [ ] New PTE mutation paths record the required TLB invalidations.
- [ ] Conditional replacement does not overwrite the current PTE on `Changed`.
- [ ] Old physical pages are released after an explicit `PageTableMut::finish()`.
- [ ] `PagingHandler` implementations provide injective `p2v` mappings and aligned frame allocation.
- [ ] `Drop` handles all ownership states, including entries borrowed through `copy-from`.
- [ ] New `PagingFlags` variants are reflected in every architecture's `From` conversions.
- [ ] Changes to `walk_page_table!` preserve both three-level and four-level traversal.
- [ ] SMP TLB shootdown paths are tested with `feature = "smp"`.
- [ ] `map`/`remap` cannot bypass `paddr_is_valid` / `vaddr_is_valid` checks.

### AArch64 fresh-map publication

Fresh invalid-to-valid mappings rely on table initialization completing before
parent publication and the final `dsb ishst` before the mapping is used. Clearing
an old mapping still queues invalidation even if the same virtual address is
remapped in that batch. A fresh-map fence does not replace that invalidation or
a caller's obligation to finish before reclaiming backing pages.
