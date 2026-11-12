# page_table — Security and Reliability Analysis

## Trust Model

The analysis includes production sources in `src/` and guest fixtures in
`ut-tests/`. File-backed test modules retain their original module paths and
`cfg(unittest)` boundary; extraction does not widen production unsafe access.

```text
Callers (memspace, kexec, ...)
   │
   │ safe API: PageTable64::query/query_entry/modify,
   │           PageTableMut::map/map_sized/replace_if_same (consume holds)
   │           map_raw/map_region_raw/remap_raw/replace_if_same_raw
   │           unmap/protect/finish/try_finish
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
│ │ mapping_reference_into_raw/from_raw — exact hold    │ │
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
  consistency. `map`, `map_sized` and `replace_if_same` consume lifetime holds.
  The explicit raw APIs retain caller-owned backing obligations; a safe Rust
  signature does not register or prove that backing lifetime.
- **PagingHandler implementors** must ensure that `p2v()` returns a valid
  virtual address and `alloc_frame()` returns an aligned physical frame. The
  mapping provider must reversibly transfer one exact selected `(PA, bytes)`
  hold through `mapping_reference_into_raw` / `mapping_reference_from_raw`,
  without acquiring or releasing a count during either conversion.
- **PageTableEntry implementors** keep the managed category in a private
  architecture bit, outside public `PagingFlags` and the normalized PA. Retained
  leaves preserve it through flag/address updates; raw installs never create it.
  PTE copies and snapshots confer no independent ownership.
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
`next_table_mut()`, `next_table_mut_or_create()`, `unmap_sparse_region()`, and `copy_from()`, all through mutable access.

### 4. AArch64 User Page Table Destruction (`table64.rs`)

`Drop` reads the current ASID through the registered provider, executes
`dsb ishst` through the safe `karch::dsb_ishst()` primitive, and synchronously
invalidates translations with
`PagingMetaData::flush_tlb_process_asid(None, asid)` before returning table
frames to the allocator. If that final hook returns an error, Drop pins the
raw frame transfers and remaining managed data holds; it cannot reclaim them
based on an unsuccessful completion attempt.

**Invariant**: The provider context remains valid throughout destruction and
the callback performs read-only access. The caller has already stopped hardware
use of the page table being destroyed. The barrier orders earlier PTE stores,
and invalidating the entire ASID covers non-leaf translation caches.
`MmSpace` declares `pgtbl` before `user_asid_context`, so field destruction drops
the page table first and preserves the callback context until it is no longer
needed.

### 5. Recovering PTE-owned mapping transfers (`table64.rs`)

`PageTableMut::retire_mapping_reference()` invokes
`unsafe H::mapping_reference_from_raw(old_entry.paddr(), leaf_bytes)` only for a
captured old managed leaf after clear/replacement and invalidation recording.
`PageTable64::dealloc_entry()` uses the same provider boundary for each owned
base/huge leaf during inactive-tree destruction, before freeing its containing
descriptor frame. Borrowed roots are skipped; failed final AArch64 ASID
completion returns before recovering those outstanding transfers.

**Invariant**: the normalized PA and exact leaf byte length identify one
unconsumed transfer created by `mapping_reference_into_raw`. The mutation or
destruction path exclusively owns that transfer and recovers it once. Neither a
valid address, a copied PTE, nor a raw leaf authorizes recovery.

**Safety argument**: reference-consuming admission validates the selected range
and finishes all fallible work before transferring and publishing the category.
Changes capture category/PA/size before overwriting the descriptor. Recovery
creates no extra hold; mutation moves the recovered hold into bounded retirement
storage, and only confirmed invalidation releases it. Destruction instead
requires hardware quiescence and required retirement before walking live leaves.

The HAL implementation in `arch/khal/src/paging.rs` converts byte length to
base-page count and calls `PageRef::from_raw` for that exact transfer. This is
a provider trust boundary, not a general PA-to-owner lookup. Raw-only
`Infallible` providers must never be asked to recover a reference.

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
   using stale mappings. `PageTableMut::finish()` and `Drop` attempt pending
   invalidations; unconfirmed completion retains managed holds rather than
   proving permission revocation. Synchronous callers require `try_finish()`
   to succeed. The one exception is an invalid-to-valid transition on an
   architecture that reports `INVALID_TO_VALID_NEEDS_TLBI == false`: no stale
   translation can exist for a slot that was verified unused, so fresh installs
   through `map()`, `map_sized()` or `map_raw()` record a fence request instead
   of an invalidation, and `finish()` emits
   `fence_fresh_map()` once per boundary unless invalidation was confirmed,
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
6. **Address validity**: physical addresses selected by references or supplied
   through raw mapping/replacement APIs must be within `PA_MAX_ADDR`, and VAs
   must have the architecture's canonical form. Managed admission validates both
   endpoints and checked range arithmetic as well as exact size and alignment.
   `query`/`protect`/`unmap` retain their VA checks. Invalid addresses return
   `PtError::InvalidAddress`; invalid managed size/alignment returns `NotAligned`.
7. **Conditional replacement preserves intervening changes**: `replace_if_same()`
   and `replace_if_same_raw()` may replace a present leaf PTE only when it exactly
   matches `PteSnapshot`, including size and private category bits. A changed or
   absent PTE returns `PteReplaceError::Changed` without altering its transfer.
   The reference-consuming form drops only its unpublished new input on failure
   and returns the old descriptive snapshot on success.
8. **Encapsulated snapshot bits**: only `page_table` interprets the raw PTE bits
   in `PteSnapshot` for comparison. External callers depend on the physical
   address, permissions, and page size, not architecture-specific PTE encoding.
9. **Release after confirmed invalidation**: the guard automatically retains
   mapping holds transferred through `map`, `map_sized` or `replace_if_same`
   until invalidation completes.
   Before releasing raw backing removed from a PTE, callers must obtain success
   from `PageTableMut::try_finish()`; on error they must retain that backing.
   Neither `finish()` returning nor guard destruction proves raw backing can be
   released or permission revocation has completed.
10. **Non-leaf translation caches**: per-VA leaf invalidation alone does not
    retire cached table-walk descriptors before table frames are reused.
    AArch64 user page table destruction with a registered ASID provider uses
    `dsb ishst` and invalidation of the entire ASID to establish this reclamation
    boundary. The caller must still stop hardware use of the address space.
11. **One transfer per owned managed leaf**: the private category represents one
    exact selected `(PA, bytes)` transfer. PTE or snapshot copies do not create
    holds. Raw leaves never invoke `mapping_reference_from_raw`; retained managed
    leaves preserve category, and raw replacements cannot change their normalized
    PA. Clearing/replacing captures old metadata before recovering it once.
12. **Borrowed and destroyed trees**: `copy_from()` prescans the complete target
    root range for managed leaves in owned subtrees before any destruction or
    overwrite, and requires prior managed retirements complete. Borrowing copies
    no ownership. Inactive-tree destruction recovers each owned base/huge transfer
    once before table-frame release, skips borrowed roots, and preserves all
    unrecovered transfers if the final AArch64 ASID completion fails.

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
| T-02 | `PagingHandler::alloc_frame()` returns a live frame again, causing page table frames to alias. | High | Duplicate allocation by the frame allocator. | [`PagingHandlerImpl::alloc_frame`](../../../arch/khal/src/paging.rs) asserts `is_page_allocator_ready()` and calls `kalloc::alloc_page_ref(UsageKind::PageTable)`. [`GlobalAllocator::alloc_pages`](../../kalloc/src/lib.rs) protects PCP access with `IrqSave` and global allocation with `palloc.lock()`. Verify live-frame uniqueness and alignment as specified below. `alloc_table()` zeroes frames but does not detect duplicates. | Delegated to `kalloc` / `alloc-engine`; uniqueness is not rechecked here. |
| T-03 | A PTE changes without TLB invalidation, leaving stale mappings in use. | High | Direct memory writes bypass `PageTableMut`. | `map`, `map_sized`, `map_raw`, `unmap`, `remap_raw`, `protect`, `replace_if_same` and `replace_if_same_raw` record required invalidations; `PageTableMut::drop` attempts pending invalidation and pins managed holds on failure. `try_finish()` reports completion to synchronous callers. `finish_reports_explicit_flush_boundary` checks pending batching state, not remote completion. Fresh mapping skips invalidation only for a slot verified unused and only when the architecture reports `INVALID_TO_VALID_NEEDS_TLBI == false`; `fresh_map_fences_once_without_invalidating` and `unmap_still_invalidates_on_a_fence_only_architecture` pin that boundary, and `fresh_map_invalidates_when_architecture_requires_it` pins the conservative default. Direct PTE memory writes remain outside this API contract. | Invalidation is recorded; immediate revocation still requires successful synchronous completion. |
| T-04 | Unsynchronized mutations or aliases of a shared page table cause a data race. | High | Callers bypass exclusive access or mutate shared physical subtrees without coordination. | `PageTable64::modify(&mut self)` requires exclusive Rust access to one table object. The caller must also lock accesses through distinct objects sharing `copy_from()` subtrees; the borrow checker cannot detect those physical aliases. | Partial; shared-subtree synchronization belongs to callers. |
| T-05 | Dropping a `copy_from` source leaves borrowed entries pointing to freed frames. | High | The source is dropped before its destination. | `copy_from()` marks `borrowed_entries`, and `PageTable64::drop` skips those subtrees. This prevents double release by the borrower but does not keep the source alive. The source-outlives-destination contract must be checked at each call site. | Partial; source lifetime is caller-owned and not type-checked. |
| T-06 | `map_region_raw` leaves partial mappings after failure. | Medium | An intermediate mapping returns `AlreadyMapped` or `NoMemory`. | `map_region_raw()` returns the first `map_raw()` error without rollback. Reproduce by pre-mapping a later page, mapping a region spanning it, then querying the successful prefix. Callers requiring atomic behavior must clean up that prefix without removing pre-existing mappings. | Accepted limitation; rollback and recovery belong to the caller. |
| T-07 | An incorrect SEV C-bit position applies the wrong encryption attribute. | High | `kbuild_config::SEV_CBIT_POS` differs from the hardware setting. | `EncodedPtePhys` centralizes C-bit encoding and decoding in `arch/x86_64.rs`; validate the build's `SEV_CBIT_POS` against the deployment hardware. The wrapper does not discover or validate that hardware setting. | Delegated to build and platform configuration. |
| T-08 | Invalid addresses reach mapping or query operations. | High | A noncanonical virtual address or an unsupported physical address. | Entry points call `vaddr_is_valid()` and, for physical mapping inputs, `paddr_is_valid()`, returning `InvalidAddress` on failure. `test_paging_metadata_bounds` checks physical bounds. These checks establish address format/range, not mapping ownership or the validity of `H::p2v()`. | Mitigated for address range errors; metadata correctness remains a provider contract. |
| T-09 | A COW commit overwrites an intervening PTE update. | High | The fault handler uses unconditional `remap_raw()` after preparing a page. | `replace_if_same(reference)` and `replace_if_same_raw(paddr)` compare the snapshot and return `Changed` without writing on mismatch. The existing raw compare regression (`replace_if_same_reports_changed_without_overwrite`) preserves its competing-mapping check under the raw API name. Reference-consuming failure drops only the unpublished input hold; callers abort any remaining prepared object state. | Partial; PTE preservation and consumed-input cleanup are implemented, object-state abort remains caller-owned. |
| T-10 | An old data page is reused before invalidating its former translation. | High | An unmap or COW replacement frees a page before a deferred flush. | Managed mappings internally retain removed holds until confirmed invalidation; incomplete requests retain or pin them. Raw release and synchronous revocation require successful `try_finish()` and backing retention on error; existing raw callers using `finish()` do not meet that boundary. | Enforced for managed holds; raw release ordering remains caller-owned. |
| T-11 | A cached intermediate descriptor reaches a reused page table frame. | High | An AArch64 user page table is destroyed before retiring its ASID's walk-cache entries. | With a registered provider, `PageTable64::drop` executes `dsb ishst` and `flush_tlb_process_asid(None, asid)` before the first `dealloc_frame()`. `user_drop_invalidates_latest_asid_before_freeing_frames` verifies full-ASID scope, the current ASID, and flush-before-free ordering. | Partial; hardware quiescence and provider lifetime remain address-space-owner obligations. |
| T-12 | A forged or duplicated managed category causes invalid or double reference recovery. | High | A provider encodes category in public flags/address bits, a raw install sets it, or a copied PTE is treated as another owner. | Only reference-consuming commit creates category after exact-range validation; retained leaf updates preserve it, raw PA changes on managed leaves are rejected, and retirement/destruction recover each outstanding transfer once. Architecture bit selection and the HAL conversion contract require separate audit; ordinary PTE copies are non-owning. | Partial; transaction rules are local, architecture/provider correctness remains trusted. |

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

The `table64::tests::aarch64_drop` tests in `ut-tests/table64.rs` run the actual AArch64 destructor
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

The `table64::tests` fresh-map cases in `ut-tests/table64.rs` run a recording `PagingMetaData` whose
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
| F-03 | `map`, `map_sized` or `map_raw` targets an already mapped address. | Duplicate mapping request. | Returns `AlreadyMapped`. | No existing mapping is overwritten. | 4 | Return an error instead of silently overwriting the entry. |
| F-04 | Excessive recursion in `dealloc_tree`. | Unsupported or excessive page table depth. | Stack overflow. | Kernel crash. | 1 | Supported page tables have at most four levels. Each level has 512 entries, but sibling entries do not increase recursion depth. |
| F-05 | A TLB shootdown IPI is lost. | A remote CPU does not respond in SMP mode. | The remote CPU retains stale mappings. | Memory consistency is violated. | 1 | The IPI implementation must deliver shootdowns reliably; AArch64 uses hardware TLBI broadcast without software IPIs. |
| F-06 | `protect_region` skips unmapped pages. | Holes in the requested region. | Permissions change only on mapped pages. | A caller's assumption of complete region coverage may be invalid. | 2 | Unmapped pages are silently skipped in 4 KiB steps; callers requiring full coverage must ensure the region is mapped. |
| F-07 | `remap_raw` changes an entry while it is in use. | Uncoordinated access to the same address space. | Stale translations may remain in use before finalization. | Data corruption. | 1 | Callers must synchronize updates and complete the required TLB invalidation before reusing old resources. |
| F-08 | An invalid address is passed to mapping or query operations. | The caller omits address range validation. | Returns `InvalidAddress`. | The operation is rejected. | 4 | Validate with `vaddr_is_valid`/`paddr_is_valid` at operation entry and return an error. |
| F-09 | `unmap` clears a non-present entry. | Clearing before checking presence. | Architecture-specific state or reserved bits may be modified incorrectly. | Undefined hardware behavior. | 2 | Check `is_present()` before `clear()`; return `NotMapped` for a non-present entry. |
| F-10 | `map_region_raw` panics partway through. | The `phys_getter` closure panics. | Some mappings are installed with invalidations still pending. | TLB consistency depends on cleanup running. | 2 | `PageTableMut::drop` attempts recorded invalidations and pins managed holds on failure if destruction runs; raw backing remains the caller's responsibility. Callers must keep `phys_getter` free of panics and cannot rely on unwinding in aborting configurations. |
| F-11 | Mapping or unmapping runs in interrupt context. | An interrupt handler invokes page table operations. | Locks or shootdown completion may deadlock. | System hang. | 1 | Callers must respect allocator, lock, and TLB completion context requirements; do not use `PageTableMut` in interrupt context without establishing that these requirements are met. |
| F-12 | Prepared object resources leak after conditional replacement returns `Changed`. | The caller omits cleanup outside the consumed mapping hold, or used the raw API. | A prepared resource or object reference leaks. | Persistent memory leakage. | 2 | `replace_if_same(reference)` automatically drops its unpublished input and preserves the old transfer; object owners still abort other prepared state. `replace_if_same_raw` owns no backing and leaves all prepared-resource cleanup to its caller. |
| F-13 | An unmapped frame is freed before explicit flush finalization. | Incorrect resource release order. | A stale TLB entry may still access the reused frame. | Isolation failure or data corruption. | 1 | Use owned mapping holds, or release raw backing only after `try_finish()` succeeds and retain it on error; audit `dealloc_frame` and object-page release call sites. |
| F-14 | Retirement spill reservation fails or the bounded batch fills. | Heap pressure or 256 retained holds. | The batch must drain while the recovered old hold remains live. | Extra invalidation or pinned pages if completion fails. | 2 | Keep 16 holds inline and at most 256 total; attempt intermediate completion while retaining the incoming hold. Release only on confirmation, otherwise retain or pin with a full retry. There is no permanent owner-index allocation/OOM path. |
| F-15 | A later destination root entry contains an owned managed leaf during `copy_from`. | The caller failed to unmap and retire all destination managed leaves. | Copying is rejected before any subtree is destroyed or overwritten. | Caller precondition failure; existing mappings and transfers remain intact. | 2 | Prescan the entire selected root range before destructive work and require managed retirements complete. Hardware quiescence and source lifetime remain caller obligations. |

## Failure Handling

- **Error codes**: `PtError` covers six conditions: `NoMemory`, `NotAligned`,
  `NotMapped`, `AlreadyMapped`, `MappedToHugePage`, and `InvalidAddress`.
  With `feature = "kerrno"`, these convert to `KError`.
  Conditional replacement uses `PteReplaceError` to distinguish page table
  errors from an intervening PTE change that may require a retry.
- **TLB finalization**: automatic `finish()` / Drop retain managed holds on
  completion failure. `TlbFlushReceipt::had_pending()` is only batching metadata.
  `try_finish()` reports synchronous completion as success or `TlbFlushError`;
  raw release and immediate permission revocation must handle an error. A
  receipt from automatic progress does not authorize either operation.
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
- Invalid addresses cannot create unintended mappings through managed or raw APIs.

## Known Limitations

1. `PageTable64` has no internal lock. Callers must synchronize mutations to
   shared page tables independently of their `Send`/`Sync` auto-traits.
2. `map_region_raw` does not roll back earlier mappings after a partial failure;
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
- [ ] Owned mapping holds survive unconfirmed invalidation without caller completion checks.
- [ ] Raw backing is released only after `PageTableMut::try_finish()` succeeds and remains held on error.
- [ ] Immediate permission revocation is reported complete only after confirmed invalidation.
- [ ] `PagingHandler` implementations provide injective `p2v` mappings and aligned frame allocation.
- [ ] `Drop` handles all ownership states, including entries borrowed through `copy-from`.
- [ ] New `PagingFlags` variants are reflected in every architecture's `From` conversions.
- [ ] Changes to `walk_page_table!` preserve both three-level and four-level traversal.
- [ ] SMP TLB shootdown paths are tested with `feature = "smp"`.
- [ ] Managed and raw mapping/replacement APIs preserve PA/VA validity checks.
- [ ] Managed category stays private, survives retained-leaf updates, and is never created by raw installs.
- [ ] Exact-range into_raw/from_raw transfers conserve counts; retired-leaf recovery follows capture/clear and never consumes a raw or borrowed transfer.
- [ ] `copy_from` checks the whole destination root range before any overwrite, with managed retirements complete.
- [ ] Inactive-tree Drop recovers each owned base/huge transfer once before frame release; failed final ASID completion preserves unrecovered transfers.

### AArch64 fresh-map publication

Fresh invalid-to-valid mappings rely on table initialization completing before
parent publication and the final `dsb ishst` before the mapping is used. Clearing
an old mapping still queues invalidation even if the same virtual address is
remapped in that batch. A fresh-map fence does not replace that invalidation or
a caller's obligation to finish before reclaiming backing pages.


## Handler storage references and retirement

HAL allocation transfers a single lifetime hold to each table PA, and handler
release consumes it exactly once. The caller must end hardware use and required
translation retirement before destruction, including `copy_from()` when it
replaces a target-owned subtree. Borrowed entries must not consume the source's
transfer; source lifetime and alias synchronization remain caller obligations.
The table-storage counter neither validates the active hardware root nor
supplies a data-page hold. Managed data leaves use the separate exact-range
transfer/recovery contract documented below and in unsafe inventory item 5.


## PTE-owned data-reference boundary

The generic handler supplies a lifetime capability with a stable physical base
and selected byte length. It must retain every selected byte through Drop,
without granting content access or hardware-retirement authority. The HAL uses
the existing `PageRef` protocol and adds no reference counter. The primary APIs
`map(reference)`, `map_sized(reference, size)` and
`replace_if_same(reference)` consume one exact selected hold per leaf, including
compound views. No split/merge or public reference-cloning API is added.

`mapping_reference_into_raw(reference) -> (PA, bytes)` transfers that hold into
the managed leaf with no count change. The private ownership category plus the
leaf's normalized PA and size identify the outstanding transfer. The unsafe
`mapping_reference_from_raw(PA, bytes)` inverse consumes it exactly once,
without acquiring another hold. A PA metadata lookup, raw mapping, snapshot or
PTE copy is insufficient proof. Architecture-private category selection must
preserve hardware/software address and attribute fields; it is excluded from
ordinary `PagingFlags`. This common contract does not establish the outcome of
a per-architecture bit audit.

Admission validates exact size, alignment, checked address coverage, subtree
mutability and the target slot, and allocates any table frames before raw
transfer. No fallible step follows transfer before PTE publication. A failed
admission or stale compare drops only the unpublished input and leaves the
existing PTE and transfer untouched. Empty initial permissions install no leaf;
an empty matched replacement captures and clears the old leaf, retires its old
managed transfer, and drops the new unpublished input. Successful conditional
replacement still returns the old non-owning snapshot.

There is no permanent per-leaf owner index or `BTreeMap` node allocation, and
therefore no owner-index heap-OOM failure. Table-frame allocation can still
return `NoMemory`, and bounded retirement spill remains fallible.

Managed leaf changes preserve these invariants:

- Flag/address updates that retain a leaf preserve its category. `remap_raw`
  and `replace_if_same_raw` reject a changed normalized PA on a managed leaf;
  same-PA updates and nonempty `protect` preserve its transfer.
- Clear/replacement captures the old category, PA and leaf size before changing
  the PTE, records required invalidation, then recovers the old managed transfer
  exactly once into `retired_mapping_refs`. Raw leaves never invoke recovery.
  This includes huge leaves removed through an interior VA and a successfully
  cleared prefix of a partial unmap. No owner-index search or new reference
  acquisition is involved.
- `retired_mapping_refs` retains 16 holds inline and at most 256 with bounded
  spill. Full storage or spill OOM is an explicit intermediate drain boundary.
  A just-recovered hold stays live locally during that completion attempt; only
  confirmation permits Drop. Repeated updates to one VA can share invalidation
  while retiring many distinct holds.
- Internal software shootdowns retain typed `NotReady`, `Deferred`,
  `TimedOut` and `UnsupportedScope` errors. Automatic finalization keeps holds
  and a `Full` retry on unconfirmed completion, without requiring object owners
  to inspect a completion flag. Before AP interrupt services are ready, a
  nonempty remote scope is unconfirmed; SMP metadata defaults cannot report
  remote completion after local-only work. Synchronous revocation still needs
  successful `try_finish()`; retaining a page does not revoke permissions.
- If storage cannot retain a recovered hold and the drain is unconfirmed, that
  hold stays pinned. Guard Drop pins any remaining holds when its retry remains
  unconfirmed. This bounded-storage fail-closed policy can exhaust RAM under
  repeated failures; it is not successful reclaim and has no asynchronous reaper.
- `copy_from` borrows source PTE-owned transfers without duplicating them.
  Borrower mutation is rejected; the source outlives every borrower and must
  invalidate their shared translations. The full destination root-entry range
  is checked for managed leaves in owned subtrees before any target destruction
  or overwrite; those leaves must first be unmapped and fully retired.
- Table Drop requires ended hardware use, no future reactivation, completed
  required retirement and ended borrower access. It walks each owned base/huge
  managed leaf once, recovering and dropping its transfer before descriptor-frame
  release. Borrowed roots are skipped. A failed final AArch64 user-ASID completion
  preserves all table frames and unrecovered transfers.

Existing PA-only callers are mechanically moved to `map_raw`, `map_region_raw`,
`remap_raw` and `replace_if_same_raw`. Production backing allocation, fields
and destructors in anon/pagecache/filemap/memspace remain deferred. These raw
mappings have no guard-owned data hold: keeping a `PageRef` elsewhere does not
register a mapping transfer. Existing raw release paths using `finish()` can
still free backing after an unconfirmed software shootdown. They remain unsafe
under that condition until mapping ownership is transferred or raw backing is
retained on `try_finish()` failure. MMIO and permanent direct-map policy also
remain externally owned.

The ownership regression fixtures use `map`, `map_sized` and
`replace_if_same` as the reference-consuming test handles, and the explicit raw
names for PA-only fixtures. Coverage obligations include transfer/count
conservation, failed admission and stale-compare cleanup, private category
persistence across architecture PTE updates, sparse/huge/interior removal,
permission clears, bounded drains and spill OOM, failed completion and full
retry, borrowed-destination preflight, live-leaf destruction and aliases with
independent holds. The six added transfer, alias, destruction and empty-replacement cases and
five architecture-category cases are named in the
[ownership regression coverage](design.md#ownership-regression-coverage). HAL
tests retain real `PageRef` inventory/reuse and exact/compound backing checks,
including `inactive_table_destruction_releases_its_live_data_hold`. Test tables
are never activated in hardware; these fixtures do not prove real shootdown or
category-bit hardware correctness.
