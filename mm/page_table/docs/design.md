# page_table — Design

## Purpose

This module provides a unified page table implementation for x-kernel across
multiple architectures. It defines architecture-independent page table traits
(`PageTableEntry`, `PagingMetaData`, and `PagingHandler`) and implements the
generic 64-bit multi-level page table `PageTable64` for x86_64, AArch64, RISC-V,
and LoongArch64. Subsystems such as `mm/memspace` and `process/kexec` use this
module as the foundation for virtual memory management.

## Background

x-kernel manages virtual address spaces on CPU architectures with different
PTE formats, numbers of page table levels, and TLB invalidation mechanisms.
Independent implementations would duplicate mapping, unmapping, region
operations, and TLB management, making consistent behavior difficult to maintain.
The common framework encapsulates these differences in PTE and metadata traits
so that the core mapping logic is implemented once.

## Scope

Source files:

```text
page_table/
├── src/
│   ├── lib.rs
│   ├── defs.rs
│   ├── macros.rs
│   ├── table64.rs
│   └── arch/
│       ├── mod.rs
│       ├── x86_64.rs
│       ├── aarch64.rs
│       ├── riscv.rs
│       └── loongarch64.rs
└── Cargo.toml
```

## Boundaries and Delegated Responsibilities

`page_table` owns page table frames, PTE traversal and mutation, conditional
replacement, and the ordering of TLB finalization relative to frame release.
The following responsibilities belong to other layers:

| Responsibility outside this crate | Owner or interface |
|----------------------------------|--------------------|
| Physical frame allocation policy, allocator initialization, and physical-to-virtual mapping setup | `PagingHandler` supplies allocation, release, and address translation. The kernel adapter is [`khal::paging::PagingHandlerImpl`](../../../arch/khal/src/paging.rs), backed by `kalloc` and `khal::mem` / `kaddr_layout`. This crate requests table frames but does not implement an allocator or establish the direct map. |
| TLB instructions, remote notification delivery, and IPI acknowledgements | `PagingMetaData` supplies architecture-specific invalidation. The architecture implementations delegate instructions to `karch`; software shootdowns use `TlbFlushIf`, implemented in [`kipi::tlb`](../../../arch/kipi/src/tlb.rs). AArch64 uses hardware Inner Shareable broadcast. This crate chooses when and what to invalidate, not how to deliver IPIs. |
| VMA policy, page faults, COW resource preparation, and ownership of mapped data pages | [`memspace`](../../memspace/src/aspace.rs) and its backends own these decisions. `replace_if_same()` only commits a PTE change; callers release or abort the corresponding data-page resources. Dropping a page table releases its table frames, not the data pages named by leaf PTEs. |
| ASID assignment, CPU residency, and stopping hardware use before destruction | The address-space owner supplies the ASID or CPU-mask provider and keeps its context alive. On AArch64, [`memspace::aarch64_asid`](../../memspace/src/aarch64_asid.rs) connects the provider to address-space state. This crate neither allocates ASIDs nor schedules CPUs away from an address space. |
| Synchronizing callers and managing borrowed subtree lifetimes | Callers serialize mutations and ensure that a source used by `copy_from()` outlives its borrowers. This crate has no internal address-space lock or reference count for borrowed table frames. |

## Architecture

```text
                    ┌─────────────────────────────────┐
                    │ Callers (memspace, kexec, ...)  │
                    └───────────────┬─────────────────┘
                                    │
                  safe API: map / unmap / query / protect
                                    │
                    ┌───────────────v─────────────────┐
                    │ PageTable64<M, PTE, H>          │
                    │ PageTableMut<M, PTE, H>         │
                    │                                 │
                    │ ┌─ walk_page_table! ──────────┐ │
                    │ │ 3/4 levels and huge pages   │ │
                    │ └─────────────────────────────┘ │
                    │                                 │
                    │ ┌─ Deferred TLB flushes ──────┐ │
                    │ │ ToFlush / finish()          │ │
                    │ └─────────────────────────────┘ │
                    └───┬───────────────────┬─────────┘
                        │                   │
              ┌─────────v───────┐  ┌────────v────────────┐
              │ PTE (trait)     │  │ M: PagingMetaData   │
              │ + arch impl     │  │ H: PagingHandler    │
              └─────────────────┘  └─────────────────────┘
                 x86_64               flush_tlb()
                 aarch64              alloc/dealloc_frame()
                 riscv                p2v()
                 loongarch64
```

| Component | Responsibility |
|-----------|----------------|
| `defs.rs` | Core traits and types: `PagingFlags`, `PageTableEntry`, `PagingMetaData`, `PagingHandler`, `PageSize`, `PtError`, `PteSnapshot`, `PteReplaceError`, and `TlbFlushReceipt`. |
| `table64.rs` | Generic 64-bit multi-level page tables: `PageTable64` for read-only queries and `PageTableMut` for mutations, conditional replacement, and batched TLB invalidation. |
| `macros.rs` | Traversal macros `walk_page_table!` / `walk_page_table_create!` and PTE helpers `impl_pte_debug!` / `impl_pte_common_ops!`. |
| `arch/x86_64.rs` | x86_64 PTEs with four levels and SEV C-bit encryption; `X64PagingMetaData`. |
| `arch/aarch64.rs` | AArch64 PTEs with four levels and `Arm64Attr` attributes; `A64PagingMetaData`. |
| `arch/riscv.rs` | RISC-V PTEs for three-level Sv39 and four-level Sv48; `Sv39MetaData` / `Sv48MetaData`. |
| `arch/loongarch64.rs` | LoongArch64 PTEs with four levels and `LaFlags` attributes; `LA64MetaData`. |

## State Machines

### PageTableMut TLB Flush State

```text
  None ──flush(vaddr)──> Addresses ──flush(beyond threshold)──> Full
    │                       │                                  │
    │                       └────finish()──> None <──finish()───┘
    └────────────────────────────finish()──> None
```

`PageTableMut` carries an independent `fresh_map` flag for architectures whose
`INVALID_TO_VALID_NEEDS_TLBI` is `false`. `map()` sets it instead of recording
an address, and `finish()` clears it after emitting `fence_fresh_map()` once,
skipped when the same boundary already invalidated something.

No in-tree architecture currently opts in: AArch64 kept the default `true`
after PR !852 correlated the flush-free fresh map with KVM-killed vCPUs on
aarch64 daily runs ("kvm run failed Function not implemented", Stage-2 aborts
on IPA outside RAM, builds #78-#85). The mechanism remains available pending
a proven root cause.

| From | To | Trigger |
|------|----|---------|
| `None` | `Addresses` | First call to `flush(vaddr)`. |
| `Addresses` | `Addresses` | Another address is recorded within `FLUSH_THRESHOLD` (16). |
| `Addresses` | `Full` | Another address would exceed the threshold. |
| `Addresses` | `None` | `finish()` or `Drop`. |
| `Full` | `None` | `finish()` or `Drop`. |

### PTE Lifecycle

```text
  EMPTY ──new_page()/new_table()──> PRESENT ──clear()──> EMPTY
                                      │
                              set_paddr()/set_flags()
                                      │
                                      v
                                 PRESENT (updated)
```

## Algorithms

### Page Table Traversal (`walk_page_table!`)

For a four-level page table:

1. Obtain the P4 table from `root_paddr`.
2. Index P4 with `p4_idx(vaddr)` to obtain P4E.
3. Follow P4E to P3, or return `NotMapped` if no next-level table exists.
4. Index P3 with `p3_idx(vaddr)` and return P3E if it maps a 1 GiB huge page.
5. Otherwise, follow P3E to P2.
6. Index P2 with `p2_idx(vaddr)` and return P2E if it maps a 2 MiB huge page.
7. Otherwise, follow P2E to P1.
8. Index P1 with `p1_idx(vaddr)` and return P1E for a 4 KiB page.

Three-level page tables (Sv39) skip P4 and start at P3.

### Mapping Creation (`walk_page_table_create!`)

1. Walk the page table levels. If an intermediate entry is unused, call
   `alloc_table()` to allocate a new page table frame.
2. Return a mutable PTE reference at the requested level.
3. The caller writes the PTE contents.

### Region Mapping (`map_region`)

1. Check that `vaddr` and `size` are aligned to 4 KiB.
2. When huge pages are allowed, prefer the largest usable page size
   (1 GiB, then 2 MiB, then 4 KiB). Both virtual and physical addresses must
   satisfy its alignment, and the remaining region must be large enough.
3. Call `map()` for each page, advancing the virtual address and reducing the
   remaining size.
4. Return an error if any mapping fails. Earlier mappings are not rolled back.

### Sparse Region Unmapping (`unmap_sparse_region`)

The caller supplies a uniform leaf page size and an aligned virtual range.
The operation validates the range before mutation, then walks allocated tables
once per covered subtree. A missing intermediate entry skips its entire clipped
coverage interval; an allocated leaf table is scanned in place. Directories
are identified by architecture-specific `is_table()`: normally a present
non-huge entry, but on LoongArch a nonzero physical address without attributes.
Non-present leaves are left unchanged even when `is_huge()` returns false.
The walk neither allocates nor frees table frames.
Its structure follows Linux v7.0 (`028ef9c96e96`) `zap_*_range`, adapted to this
crate's three/four-level tables and uniform backing page-size contract.

Every cleared leaf records an invalidation through the existing `flush` state.
A present leaf of a different size is an error, including a partially covered
huge page: the API does not split huge mappings. A mismatch can leave a cleared
prefix, so callers must preserve backing ownership on error and allow pending
invalidations to finish. This does not change the strict, hole-rejecting
`unmap_region` API. Unit tests count visited entries only in test builds to
verify subtree skipping without timing thresholds.

### Batched TLB Invalidation

1. `PageTableMut` maintains a `ToFlush` value.
2. Each successful PTE change made by
   `unmap/unmap_sparse_region/remap/protect/replace_if_same` records its address with `flush(vaddr)`.
   `map` records the address only when the architecture reports
   `INVALID_TO_VALID_NEEDS_TLBI`; it always installs into a slot it verified
   unused, and an architecture that cannot cache a translation for an invalid
   entry has nothing to invalidate there.
3. Up to 16 addresses are retained for individual invalidation.
   Exceeding this threshold changes the state to `Full`.
4. `finish()` invalidates each recorded address for `Addresses`, or performs
   a full invalidation for the target scope for `Full`. When nothing was
   pending and at least one fresh mapping was installed, it emits
   `M::fence_fresh_map()` instead, so the entry writes reach other translation
   table walkers without the cost of an invalidation broadcast. It then
   returns a `TlbFlushReceipt`.
5. `Drop` calls `finish()` automatically to finalize pending invalidations.

Because `TlbFlushReceipt` reports only pending invalidations, a boundary that
installed fresh mappings on a fence-only architecture returns a receipt with
`had_pending() == false`. Callers that release a physical frame after removing
it from a PTE still rely on an invalidation having run, and that path is
unaffected: removal always records an address, so its receipt reports pending
work.

`TlbFlushReceipt` marks an explicit ordering boundary after page table changes.
It hides architecture-specific details and indicates that flush finalization
has completed at that call site.
Before releasing a physical frame or object-owned page removed from a PTE,
higher layers must call `finish()` and perform the release afterward.
Relying only on `PageTableMut::drop` prevents forgotten flushes but does not
express the required release-after-flush ordering in the resource lifecycle.

### PTE Snapshots and Conditional Replacement

`PageTable64::query_entry(vaddr)` returns a `PteSnapshot` describing the present
leaf PTE observed by one page table walk. It contains:

- The base physical address of the leaf PTE.
- Decoded `PagingFlags`.
- The leaf page size.
- Raw PTE bits, interpreted only within `page_table` for conditional comparison.

`PageTableMut::replace_if_same(vaddr, expected, paddr, flags)` is the commit
primitive for transactional paths such as COW:

1. The caller observes the current PTE with `query_entry()`.
2. The caller prepares replacement resources, such as a new COW page, outside
   the page table mutation.
3. The caller invokes `replace_if_same()` to commit.
4. If the current leaf PTE exactly matches `expected`, the operation replaces
   the physical page and permissions and records a TLB invalidation.
5. If the PTE has changed or is no longer present, it returns
   `PteReplaceError::Changed` without overwriting the current mapping.
6. Invalid virtual or physical addresses and other page table walk errors
   return `PteReplaceError::PageTable`.

This interface defines only the page table compare-and-replace contract.
It does not choose resource release policies for COW, anonymous pages, or file
pages. On `Changed`, the caller must discard prepared but uncommitted resources
and decide whether to retry according to the fault semantics.

## Concurrency Model

- **`PageTable64` has no internal lock**: callers must externally synchronize
  mutations to a shared page table. Its `Send` and `Sync` auto-traits depend on
  its type parameters; they do not provide locking for page table updates.
- **`PageTableMut` borrows `&mut PageTable64`**: Rust borrowing rules require
  exclusive mutable access through that page table object.
- **SMP TLB invalidation**: with `feature = "smp"`, `flush_tlb_all_cpus()` uses
  the `TlbFlushIf` interface, implemented through `kiface`, to send shootdown
  IPIs to remote CPUs. AArch64 uses hardware Inner Shareable TLBI instructions
  without software IPIs.
- **`PagingMetaData` requires `Send + Sync`** so metadata can cross thread
  boundaries.
- **`PageTableEntry` requires `Send + Sync`** so PTE values can cross thread
  boundaries.

## Design Decisions

### Trait Parameters Instead of Const Generics

`PageTable64<M, PTE, H>` delegates architecture details, frame allocation, and
TLB invalidation through three trait parameters:

- `M: PagingMetaData`: page table levels, address widths, and TLB invalidation.
- `PTE: PageTableEntry`: PTE encoding and decoding.
- `H: PagingHandler`: frame allocation and physical address translation.

This keeps architecture-specific PTE formats out of the shared mapping
algorithm and allows that algorithm to be implemented once.

### Deferred TLB Invalidation in PageTableMut

Flushing after every map or unmap is expensive during batch operations,
particularly when SMP shootdowns are needed.
`PageTableMut` records invalidation requests and handles them in `finish()`
or `Drop`:

- Small batches invalidate addresses individually.
- Large batches use a full invalidation to avoid repeated per-address overhead.
- The threshold of 16 balances targeted invalidation against batching overhead.

Paths that release old physical pages should explicitly call `finish()` and
release resources after receiving `TlbFlushReceipt`.
`Drop` remains a fallback for mapping changes that omit explicit finalization.

### Conditional Replacement Instead of Reusing remap

`remap()` overwrites a present entry's physical address and permissions
unconditionally. A COW write fault needs a conditional commit because another
path may change the PTE while the fault handler copies the page.
An unconditional overwrite could lose an intervening update or revert an
already resolved fault to an older state.

`replace_if_same()` connects PTE observation and replacement through an explicit
compare-and-replace contract. The page table layer checks whether the PTE still
matches the snapshot; the memory object layer owns resource preparation,
commit, and abort. A typical COW commit sequence is:

```text
query_entry()
  -> prepare replacement page outside page-table mutation
  -> replace_if_same()
       Ok(_) => commit object state
       Changed => abort prepared page and retry fault
       PageTable(error) => abort and propagate error
```

### Macros for Page Table Traversal

`walk_page_table!` and `walk_page_table_create!` support both immutable access
for queries and mutable access for mapping and unmapping, as well as three-
and four-level layouts.
Macros generate the appropriate access pattern at compile time and share the
traversal logic. Rust generics do not conveniently express different call
patterns for `&T` and `&mut T` in this implementation.

### TlbFlushIf Through kiface

The memory management subsystem depends on `page_table`, while the IPI
subsystem depends on memory management. A direct dependency on the IPI
subsystem would create a cycle.
`kiface` binds exactly one implementation at link time while requiring only the
interface definition at compile time, breaking that cycle.

### EncodedPtePhys for x86_64 PTEs

AMD SEV requires an encryption C-bit in the physical address encoded in a PTE.
Its position is selected by `kbuild_config::SEV_CBIT_POS`.
`EncodedPtePhys` encapsulates setting and clearing that bit, keeping SEV details
out of the `PageTableEntry` implementation.

### Both Sv39 and Sv48 for RISC-V

Sv39 uses three page table levels and 39-bit virtual addresses; Sv48 uses four
levels and 48-bit virtual addresses.
Both modes share `Rv64PageEntry` and differ in the level count and address
widths supplied by `PagingMetaData`.
The `Sv39MetaData` / `Sv48MetaData` type parameters let the generic `PageTable64`
traversal adapt to either layout.

## Drop and Resource Release

- **`PageTable64::drop`** recursively frees owned page table frames.
  - The caller must first stop hardware use of the page table.
  - For an AArch64 user page table with a registered ASID provider, destruction
    reads the associated ASID before freeing the first table frame, executes
    `karch::dsb_ishst()`, and synchronously completes invalidation of the entire ASID,
    including non-leaf translation caches. Per-VA leaf invalidation cannot
    replace this reclamation boundary.
  - The provider context must remain alive until page table destruction
    completes. `MmSpace` declares `pgtbl` before `user_asid_context` to preserve
    this lifetime through field destruction order.
  - With `feature = "copy-from"`, entries marked in `borrowed_entries` are
    skipped so that subtrees shared from the source page table are not freed.
  - Child tables are freed recursively before their parent table frame.
    Both read-only and mutable descent require `is_table()`, so destruction
    leaves non-present huge data mappings untouched rather than treating their
    backing addresses as child tables.
- **`PageTableMut::finish`** finalizes pending TLB invalidations and returns a
  `TlbFlushReceipt`, providing an explicit flush-before-free ordering point.
- **`PageTableMut::drop`** calls `finish()` automatically as a fallback when
  explicit finalization is omitted.
