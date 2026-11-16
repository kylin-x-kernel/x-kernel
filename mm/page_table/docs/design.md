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
│   ├── table64/
│   │   └── retirement.rs
│   └── arch/
│       ├── mod.rs
│       ├── x86_64.rs
│       ├── aarch64.rs
│       ├── riscv.rs
│       └── loongarch64.rs
├── ut-tests/
│   ├── defs.rs
│   ├── table64.rs
│   ├── owned_mappings.rs
│   └── pte_ownership.rs
└── Cargo.toml
```

Guest tests are mounted from their original source modules with
`unittest::path_mod_test!` under `cfg(unittest)`: `ut-tests/defs.rs` retains
`defs::tests_page_table_defs`, and `ut-tests/table64.rs` retains `table64::tests`,
including its nested `aarch64_drop` module. `ut-tests/owned_mappings.rs` is mounted
as `table64::tests_owned` and checks automatic failure retention, fallible
synchronous permission revocation, exact transfer conservation, aliases and
live-leaf destruction. The five cases in `ut-tests/pte_ownership.rs` mount as
crate-root `tests_pte_ownership` and exercise the selected architecture's private
category encoding. Architecture and `smp` gates and serial execution flags
remain attached to the corresponding tests and helpers.

## Boundaries and Delegated Responsibilities

`page_table` owns page table frames, PTE traversal and mutation, conditional
replacement, and the ordering of TLB finalization relative to frame release.
The following responsibilities belong to other layers:

| Responsibility outside this crate | Owner or interface |
|----------------------------------|--------------------|
| Physical frame allocation policy, allocator initialization, and physical-to-virtual mapping setup | `PagingHandler` supplies allocation, release, and address translation. The kernel adapter is [`khal::paging::PagingHandlerImpl`](../../../arch/khal/src/paging.rs), backed by `kalloc` and `khal::mem` / `kaddr_layout`. This crate requests table frames but does not implement an allocator or establish the direct map. |
| TLB instructions, remote notification delivery, and IPI acknowledgements | `PagingMetaData` supplies architecture-specific invalidation. The architecture implementations delegate instructions to `karch`; software shootdowns use `TlbFlushIf`, implemented in [`kipi::tlb`](../../../arch/kipi/src/tlb.rs). AArch64 uses hardware Inner Shareable broadcast. This crate chooses when and what to invalidate, not how to deliver IPIs. |
| VMA policy, page faults, COW preparation and object ownership | [`memspace`](../../memspace/src/aspace.rs) and its backends retain policy and object references. Reference-consuming mappings transfer an opaque `H::MappingReference` into the leaf PTE; this crate owns only that exact-range mapping hold and its retirement. File-cache leaves use independent managed holds. Remaining raw leaf data stays caller-owned; other backing migrations remain separate. |
| ASID assignment, CPU residency, and stopping hardware use before destruction | The address-space owner supplies the ASID or CPU-mask provider and keeps its context alive. On AArch64, [`memspace::aarch64_asid`](../../memspace/src/aarch64_asid.rs) connects the provider to address-space state. This crate neither allocates ASIDs nor schedules CPUs away from an address space. |
| Synchronizing callers and managing borrowed subtree lifetimes | Callers serialize mutations and ensure that a source used by `copy_from()` outlives its borrowers. Borrowers cannot mutate shared subtrees; source changes must invalidate every borrower. This crate has no internal address-space lock or reference count for borrowed table frames. |

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
| `defs.rs` | Core traits and types: `PagingFlags`, `PageTableEntry`, `PagingMetaData`, `PagingHandler`, `PageSize`, `PtError`, `PteSnapshot`, `PteReplaceError`, `TlbFlushReceipt`, `TlbFlushError`, and `TlbFlushResult`. |
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

  Addresses / Full ──confirmed invalidation────> None
  Addresses / Full ──unconfirmed invalidation──> Full
```

`PageTableMut` carries an independent `fresh_map` flag for architectures whose
`INVALID_TO_VALID_NEEDS_TLBI` is `false`. Fresh installs through `map()`,
`map_sized()` or `map_raw()` set it instead of recording
an address, and `finish()` clears it after emitting `fence_fresh_map()` once,
skipped only when the same boundary confirmed its pending invalidation.

AArch64 opts in after completing new table initialization before parent
publication. PR !917 (`a50f7be4`) previously reverted PR !852's fence-only
policy after aarch64 daily KVM vCPU failures ("kvm run failed Function not
implemented", reported Stage-2 aborts on IPA outside RAM, builds #78-#85).
The recovery retains the final fresh-map fence and all required old-translation
invalidations; its protocol basis and controlled KVM validation are recorded
under [AArch64 Fresh-Map Recovery Evidence](#aarch64-fresh-map-recovery-evidence).

| From | To | Trigger |
|------|----|---------|
| `None` | `Addresses` | First call to `flush(vaddr)`. |
| `Addresses` | `Addresses` | Another address is recorded within `FLUSH_THRESHOLD` (16). |
| `Addresses` | `Full` | Another address would exceed the threshold. |
| `Addresses` | `None` | Confirmed `finish()` or `Drop` completion. |
| `Full` | `None` | Confirmed `finish()` or `Drop` completion. |
| `Addresses` / `Full` | `None` | Detached by `defer_kernel_invalidation` into `DeferredKernelTlbFlush`. |
| `Addresses` / `Full` | `Full` | Unconfirmed shootdown; retain data holds for retry. |

`PageTableMut::defer_kernel_invalidation` moves `Addresses`/`Full` into a
`DeferredKernelTlbFlush` for a kernel table so the caller can release the lock
that protects the table before the remote shootdown. This matters when the
shootdown waits for acknowledgements: a CPU that needs the same lock must not
spin with interrupts disabled, or it can never answer the IPI. The method
refuses user tables (their flushes are residency-scoped), fresh-map fencing,
and pending managed retirements, which must complete in place. `complete()`
returns the backend result and retries a failed scoped or partial shootdown once
as a full-TLB invalidation; callers must not treat the mapping change as
complete on error. Dropping the handle without completing it still invalidates,
but at the drop site and without a result.

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
   On AArch64, `alloc_table()` completes the frame's zeroing with `dsb ishst`
   before returning. Only then may `next_table_mut_or_create()` publish the
   parent descriptor. Hardware walkers do not take the software address-space
   lock; a barrier deferred to `finish()` cannot prevent them from reaching
   recycled frame contents in the meantime. Root frames use the same boundary
   before their address can be installed in TTBR.
2. Return a mutable PTE reference at the requested level.
3. The caller writes the PTE contents.

Initialization-before-publication is a separate architecture obligation from
fresh-map finalization; see the `PagingMetaData` contract. The explicit DSB in
`alloc_table()` is deliberately limited to AArch64, whose protocol and original
KVM regression were checked for this fix. Other backends are unchanged and were
not covered by that publication-ordering audit. Their walker, root-register and
remote synchronization rules must be checked independently; neither the absence
of this DSB nor the default `INVALID_TO_VALID_NEEDS_TLBI == true` establishes
whether an additional barrier is required there. This scope does not assert
that publication ordering is an AArch64-only concern.

### Raw Region Mapping (`map_region_raw`)

1. Check that `vaddr` and `size` are aligned to 4 KiB.
2. When huge pages are allowed, prefer the largest usable page size
   (1 GiB, then 2 MiB, then 4 KiB). Both virtual and physical addresses must
   satisfy its alignment, and the remaining region must be large enough.
3. Call `map_raw()` for each page, advancing the virtual address and reducing the
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
   `unmap`, `unmap_sparse_region`, `remap_raw`, `protect`, `replace_if_same`
   or `replace_if_same_raw` records the required leaf invalidation.
   Fresh installs through `map`, `map_sized` or `map_raw` record invalidation
   only when the architecture reports
   `INVALID_TO_VALID_NEEDS_TLBI`; they install only into slots verified unused,
   and an architecture that cannot cache a translation for an invalid
   entry has nothing to invalidate there.
3. Up to 16 distinct base-page addresses are retained for individual invalidation.
   Exceeding this threshold or changing a huge leaf selects `Full`.
4. `finish()` invalidates each recorded address for `Addresses`, or performs
   a full invalidation for the target scope for `Full`. When nothing was
   pending and at least one fresh mapping was installed, it emits
   `M::fence_fresh_map()` instead, so the entry writes reach other translation
   table walkers without the cost of an invalidation broadcast. It then
   returns a `TlbFlushReceipt`.
5. Software shootdown hooks return a typed completion result internally. Only
   confirmed invalidation clears the summary and releases gathered data holds.
   An error keeps the holds and a full-invalidation retry pending. `Drop` retries
   once; if completion remains unconfirmed, it leaves the holds pinned. Local
   hardware flush hooks remain synchronous `()` operations.
6. `finish()` advances this automatic retirement and returns diagnostic batching
   information; managed object owners need no completion check. `try_finish()`
   returns `Result<TlbFlushReceipt, TlbFlushError>` when a caller needs synchronous
   revocation. Its success establishes completion; its error must be handled.

AArch64 invalidation helpers in `karch` issue `dsb ishst` **before** TLBI to
publish the PTE changes, followed by a completion DSB and local ISB. A DSB after
TLBI alone does not prevent a walker from refilling the old translation before
the preceding PTE store is visible. Table initialization and invalidation are
separate ordering obligations: the former precedes parent publication, while
the latter precedes the invalidation instruction. Linux v6.18 follows this
separation in [`pmd_install()`](https://github.com/torvalds/linux/blob/v6.18/mm/memory.c#L425)
and [`__flush_tlb_page_nosync()`](https://github.com/torvalds/linux/blob/v6.18/arch/arm64/include/asm/tlbflush.h#L285).

AArch64 fresh mappings use this fence-only path after new table frames have
been initialized before publication. The finalization boundary emits `dsb ishst`
when no invalidation is pending. This does not cancel an earlier unmap in the
same batch: its queued invalidation is still completed, including the
pre-TLBI publication barrier.

`TlbFlushReceipt::had_pending()` describes work pending at that call, not proof
of completion. A fresh-map fence-only boundary has no invalidation pending.
Automatic callers may drop their own object references: the guard independently
keeps mapping references until confirmed retirement. Raw release and permission
revocation that must take effect before return use `try_finish()` and may proceed
only on success. `NotReady`, `Deferred`, `TimedOut` and `UnsupportedScope` explain
why that synchronous boundary was not reached. Unsupported SMP metadata defaults
must not turn local-only invalidation into a successful remote completion.
A failed hook may not issue a publication barrier, so fresh entries receive their
own fence on that path before the guard retains the full retry.

Automatic retention applies only to holds transferred through `map`,
`map_sized` or `replace_if_same`. A caller retaining a `PageRef` while
installing its physical address through `map_raw` has not transferred a mapping
hold. Existing PA-only callers are mechanically renamed to `map_raw`,
`map_region_raw`, `remap_raw` and `replace_if_same_raw`; this changes no
backing ownership. Production backing allocation, fields and destructors in
anon/pagecache/filemap/memspace are explicitly deferred. Existing raw release
paths that use `finish()` can still release backing after an unconfirmed
software shootdown. They require a separate migration to transferred mapping
holds, or to `try_finish()` with backing retained on error. Propagating an error
while dropping local owners does not establish a safe release boundary.

### PTE-owned data mappings and retirement

`PagingHandler::MappingReference` is an opaque, stable physical-range lifetime
hold; `page_table` has no dependency on `kalloc` or `page_meta`. The kernel HAL
uses `PageRef`, while raw-only providers use `Infallible` and must never restore
a reference from a raw leaf.

The primary entry points consume references:

| Entry point | Ownership transaction |
|-------------|-----------------------|
| `map(va, reference, flags)` | Consume one selected 4 KiB hold for a new base-page leaf. |
| `map_sized(va, reference, size, flags)` | Consume one selected hold exactly matching an aligned supported 4 KiB, 2 MiB or 1 GiB leaf. |
| `replace_if_same(va, snapshot, reference, flags)` | Conditionally replace one leaf with a new exact hold, preserving its size and returning the old snapshot on success. |
| `map_existing(source, destination)` | Duplicate a live leaf, acquiring an independent exact hold for a managed source; raw backing stays externally owned. |
| `unsafe replace_managed_with_external_if_same(va, snapshot, pa, flags)` | Conditionally publish an externally retained raw target and retire the old managed hold without consuming the caller target. |
| `map_raw`, `map_region_raw`, `remap_raw`, `replace_if_same_raw` | Use caller-owned physical backing; raw installation transfers no mapping hold. |

Exact and compound allocations both work because the provider reports the
handle's selected physical base and byte length, rather than the larger
allocation retained by a compound view. One leaf owns one selected hold. No
huge-page split/merge or allocator ownership migration is added. A caller
needing later fault rematerialization keeps an independent object hold.

`PageTableEntry::is_managed()` / `set_managed()` expose an ownership category
encoded in an architecture-private software bit. This category is separate from
public `PagingFlags` and the physical address. Each architecture owns its bit
selection and audit. Flag/address updates that retain the leaf preserve the
category; clearing removes it. Only a reference-consuming commit creates a
managed category; raw installs create none. Copying a PTE or `PteSnapshot`
copies descriptor bits, without acquiring or transferring another lifetime hold.

`PagingHandler::mapping_reference_into_raw(reference) -> (PA, bytes)` transfers
the selected hold without changing its lifetime counts. The leaf's normalized
physical base, size and managed category represent that outstanding transfer.
`unsafe mapping_reference_from_raw(PA, bytes)` reverses exactly one unconsumed
transfer without acquiring another reference. It cannot manufacture ownership
from an arbitrary valid PA, a raw mapping, or a copied descriptor. No permanent
per-leaf owner index or `BTreeMap` remains, so admission has no owner-index heap
allocation or associated fatal node-OOM path.

`unsafe mapping_reference_try_clone_from_raw(PA, bytes)` acquires a new hold
while exclusive table access retains a real managed source transfer. Raw-only
handlers reject it by default. HAL borrows the existing transfer through a
`ManuallyDrop<PageRef>` view; cloning never consumes that source. A valid PA or
metadata lookup alone does not establish this capability. `map_existing` requires
aligned source/destination leaf bases, validates destination coverage/mutability,
and preserves size, permissions and managed category through a distinct hold.
Clone or table-allocation failure leaves the source unchanged; a caller rolling
back a destination prefix unmaps only leaves it actually prepared. Raw sources
remain raw and require the caller to retain their external backing for both
addresses. No TLB retry or source-restore engine is introduced.

Admission validates exact range length, supported leaf geometry, VA/PA alignment
and coverage, subtree mutability and the available target slot before consuming
the hold into raw form. Intermediate table-frame allocation also precedes that
transfer. After transfer there is no fallible step before publishing the managed
PTE. A failed admission drops only the unpublished input hold and leaves any
existing mapping and transfer intact. Empty permissions install no new leaf and
drop that input after validation.

`replace_if_same` validates the new hold and compares the complete current leaf
with the snapshot before committing. A stale comparison leaves the old PTE and
its transfer untouched and drops the unpublished input. Success captures the
old leaf, including category, normalized PA and actual size, before replacing
or clearing it. Empty replacement permissions clear the old leaf and drop the
new unpublished input. Both forms queue required invalidation, then recover the
old managed transfer from the captured descriptor exactly once into
`retired_mapping_refs`. No new old-page hold is acquired by recovery. The return
value remains the old descriptive snapshot, not an ownership handle.

`remap_raw` and `replace_if_same_raw` may change permissions on a managed leaf
only while retaining its normalized PA; changing backing requires a new hold
through `replace_if_same`. Same-PA updates and nonempty `protect` preserve the
category. Permission clearing, `unmap`, strict region unmapping and sparse
unmapping capture the old leaf before clearing and use the same recovery and
retirement path. A huge leaf found through an interior address supplies its own
base PA and full size; no owner lookup is needed.

```text
mapping hold
  -> into_raw(PA, bytes), publish managed PTE
  -> capture old leaf, clear/replace PTE, record invalidation
  -> from_raw(old PA, leaf bytes), move recovered hold into retirement storage
  -> finish requested local/broadcast invalidation
       complete   -> drop old mapping hold
       incomplete -> retain hold, retry a full invalidation
```

The invalidation summary and retirement capacity remain independent. Retirement
keeps 16 holds inline, then fallibly reserves spill storage up to a total of
256. Full storage or spill OOM attempts an intermediate batch completion while
retaining the just-recovered hold through that attempt. Small retirements need
no spill allocation; large unmaps do not invalidate once per retired hold. An
explicit `finish()` receipt excludes already completed intermediate batches.

Only confirmed invalidation releases retired holds. An unconfirmed attempt
retains holds and a `Full` retry. If spill failure prevents storing a hold and
completion fails, that hold stays pinned; guard Drop also pins any holds whose
completion remains unconfirmed. This prevents reuse under stale translations at
the cost of unreclaimed pages. There is no asynchronous quarantine reaper.
Retained lifetime does not prove permission revocation; synchronous operations
still require successful `try_finish()`.

`copy_from()` borrows source table storage and its PTE-owned transfers without
duplicating holds. Borrowers cannot mutate shared subtrees; the source must
outlive them and invalidate every borrower on source changes. Before destroying
or overwriting any destination subtree, `copy_from()` prescans the entire
selected destination root-entry range and rejects live managed leaves in owned
subtrees. Those leaves must first be explicitly unmapped and their retirements
completed. A conflict in a later root entry cannot leave an earlier subtree
already destroyed. Existing borrowed roots do not own another transfer.

Table destruction requires an inactive address space, no future hardware use,
completed required retirement and no remaining borrowers. After the existing
AArch64 final user-ASID completion gate succeeds, destruction walks every owned
base/huge managed leaf once, recovers its exact transfer, and drops it before
freeing its containing table frame. Borrowed root entries are skipped. A failed
final ASID completion preserves table frames and all unrecovered transfers.

Execution contexts inherit existing table allocation and synchronous shootdown
constraints and require the provider's reference protocol to be initialized.
Admission adds no permanent per-leaf heap allocation; retirement alone may need
fallible bounded spill allocation. Reference transfer, recovery and Drop must
not sleep in the caller's mutation context. No internal lock or global owner
lookup is introduced.

### Ownership regression coverage

The existing `ut-tests/owned_mappings.rs` fixtures use `map`, `map_sized` and
`replace_if_same` for consumed references, and `map_raw`, `remap_raw` and
`replace_if_same_raw` for PA-only operations. Six additional cases check the
PTE-owned transfer protocol directly:

| Case in `table64::tests_owned` | Checked boundary |
|---------------------------------|------------------|
| `owned_leaf_transfers_without_acquiring_an_extra_reference` | Admission and old-leaf recovery conserve the single mapping hold; only completed retirement drops it. |
| `owned_aliases_retire_their_own_transfers_independently` | Two huge-leaf aliases carry independent exact transfers; retiring one leaves the other live. |
| `raw_alias_does_not_acquire_or_release_a_managed_transfer` | A raw alias remains unmanaged and cannot consume the neighboring managed leaf's hold. |
| `inactive_tree_destruction_recovers_live_base_and_huge_leaf_transfers` | Inactive-tree Drop recovers each live 4 KiB, 2 MiB and 1 GiB transfer once. |
| `empty_replacement_releases_unpublished_input_but_retains_the_old_mapping_on_failure` | Empty replacement releases only the never-published input before completion; a failed flush keeps the old mapping hold. |
| `three_level_tree_destruction_recovers_a_root_huge_leaf` | A three-level root can itself contain a live 1 GiB owned leaf; teardown recovers its exact range. |

The five `tests_pte_ownership` cases are
`raw_entries_start_unmanaged`, `managed_leaf_updates_preserve_ownership`,
`ownership_toggle_preserves_leaf_encoding`,
`ownership_does_not_change_table_decoding` and
`clear_discards_ownership_category`. They check the selected architecture's
category behavior across leaf updates, public flags, address/leaf/table decoding
and clear; they do not replace the architecture's hardware encoding audit.

HAL's `ut-tests/paging_page_refs.rs` retains real PageRef inventory/reuse and
exact/compound coverage. Its new
`inactive_table_destruction_releases_its_live_data_hold` checks release of a
still-installed data hold when a never-activated tree is destroyed. None of
these fixtures activates its table in hardware.

### AArch64 Fresh-Map Recovery Evidence

The architectural premise for `INVALID_TO_VALID_NEEDS_TLBI == false` is
independent of the historical KVM failure. Arm-maintained
[TF-A v2.13.0, TLB maintenance operations](https://trustedfirmware-a.readthedocs.io/en/v2.13.0/components/xlat-tables-lib-v2-design.html#tlb-maintenance-operations)
explains that adding a dynamic region over invalid descriptors needs no TLBI:
TLBs cannot architecturally retain invalid translation table entries. It cites
Armv8-A rev C.a D4.10.1. This does not remove an old mapping's queued invalidation
or the publication and context-synchronization obligations of its use path.

The missing new-table ordering is fixed in `alloc_table()`: clearing the frame
must complete before a parent descriptor or TTBR exposes it to a hardware
walker, which does not acquire the software address-space lock. The final
`fence_fresh_map()` runs after parent publication and cannot close that window.
The pre-TLBI `dsb ishst` addresses a separate obligation: publish preceding PTE
stores before retiring old translations when `finish()` has pending work.

The original server was used for the controls and recovery runs: host
4.19.90-89.11.v2401.ky10.aarch64, QEMU 10.0.11, KVM `-cpu host`, four vCPUs,
1 GiB RAM. The narrow case repeats the original Open POSIX
`sigaction_22-26.run-test` through dynamic exec, up to 5000 times per run.
The saved command, patches, build-info, configuration and raw guest/Harness
logs are linked below; these are executed results rather than policy assertions.

| Fixed version and scope | Observed result | Evidence |
| --- | --- | --- |
| `cea097df`, original fence-only policy, no new-table publication barrier | Last progress 750, then KVM `Function not implemented`, Harness rc=1 | [Single-factor control and raw logs](https://gitee.com/wxyy1/x-kernel/blob/68898d2d142727c78118a55a1e7c6e4fa2dafcf2/docs/investigations/pr917-publication-acceptance-20261013.md) |
| Same baseline, configuration and workload; only executable change is `dsb ishst` after the new table clear, before parent publication | Three runs of 5000 completed, all Harness rc=0; still no fresh-map TLBI | Same control record; its dirty bundle corresponds to the saved source patch |
| Recovery `ef5cdad5` | 15000 exec, 75/75 page_table+memspace tests with coverage, original daily/Open POSIX 1376/1376 passed; original 417 exclusions retained | [Recovery report and raw artifacts](https://gitee.com/wxyy1/x-kernel/blob/e6a165d5d8a1110b8693b4e85c5673b98177b5e7/docs/investigations/pr917-publication-20261013.md) |
| Rebase `af1d4a4b`, identical recovery patches over the ext4-only main change | Build, Clippy, fmt and one 5000-exec run passed | [Rebase report and raw artifacts](https://gitee.com/wxyy1/x-kernel/blob/2d68e2dd001cee3096a7af28250da2e61c7d32fe/docs/investigations/pr917-rebase-f9954af8-20261013.md) |

Each result belongs to its listed version. The daily result covers the original
`open-posix` case, not the complete daily suite. The single-factor comparison
supports recovery under that fixed baseline and workload; it does not uniquely
reconstruct the host fault IPA or cached translation, or imply that every other
synchronization scheme without this barrier must fail. Unit tests validate
policy and batching, not architectural DSB effects. Future changes must preserve
the actual initialization-before-publication and finalization barriers; real
walker regression coverage requires AArch64 KVM execution and instruction review.

### PTE Snapshots and Conditional Replacement

`PageTable64::query_entry(vaddr)` returns a `PteSnapshot` describing the present
leaf PTE observed by one page table walk. It contains:

- The base physical address of the leaf PTE.
- Decoded `PagingFlags`.
- The leaf page size.
- Raw PTE bits, including the private ownership category, interpreted only
  within `page_table` for conditional comparison. A snapshot owns no reference.

`PageTableMut::replace_if_same(vaddr, expected, reference, flags)` is the
reference-consuming commit primitive for transactional paths such as COW:

1. The caller observes the current PTE with `query_entry()`.
2. The caller prepares a new exact-range mapping hold outside the page table
   mutation, retaining any independent object ownership it still needs.
3. The operation validates that hold and compares the current leaf, including
   its size and raw category bits, with `expected`.
4. On a match, it captures the old leaf, commits the replacement or empty clear,
   records invalidation, and recovers any old managed transfer into retirement.
   It returns the previous snapshot, which transfers no ownership.
5. If the PTE changed or is no longer present, it returns
   `PteReplaceError::Changed` without changing that PTE or its transfer; the
   unpublished input hold is dropped. Invalid addresses, geometry and walk
   failures return `PteReplaceError::PageTable`.

`replace_if_same_raw(vaddr, expected, paddr, flags)` retains the PA-only
compare-and-replace contract for externally owned backing. It cannot change a
managed leaf's normalized PA, and same-PA updates preserve the category. A
successful clear still retires an old managed transfer. Callers crossing to an
externally owned target must use the explicit mixed transaction below rather
than weakening this raw operation's ownership contract.

`unsafe replace_managed_with_external_if_same` is the explicit bridge from a
managed file source to an externally owned raw anonymous target. It requires an
exact unchanged managed snapshot, identical aligned leaf geometry, valid complete
address ranges and nonempty READ/WRITE/EXECUTE access. Every ordinary failure is
prepublication. A stale comparison changes neither the source nor the caller's
target. On success a fresh raw descriptor replaces the managed leaf, then its old
hold enters normal retirement; the caller commits and retains its raw target's
object ownership. Automatic drain failure can retain the old hold but cannot
turn successful publication into an error that would authorize freeing the raw
target as an unused candidate. Existing `replace_if_same_raw` keeps its original
restriction. Completion remains the existing `finish`/`try_finish` contract.

The page table layer defines conditional PTE commit and mapping-hold retirement.
Higher layers retain object-state commit/abort and retry policy for COW,
anonymous pages and file pages. On `Changed`, the reference-consuming API drops
its consumed unpublished input; callers still clean up any other prepared
resources. The raw API owns none of those resources.

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
- `H: PagingHandler`: frame allocation, physical address translation, and
  reversible exact-range mapping-reference transfer/recovery.

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

Automatic managed retirement owns the mapping holds through pending invalidation;
upper layers release only their own references. Paths requiring synchronous
permission revocation or releasing raw backing use `try_finish()` and handle an
error. `Drop` remains an automatic retirement fallback, not a success report for
synchronous revocation.

### Conditional Replacement Instead of Reusing remap_raw

For raw leaves, `remap_raw()` overwrites a present entry's physical address and
permissions. Managed leaves require a replacement hold through `replace_if_same`
to change physical backing. A COW write fault needs a conditional commit because
another path may change the PTE while the fault handler copies the page.
An unconditional overwrite could lose an intervening update or revert an
already resolved fault to an older state.

`replace_if_same()` connects PTE observation and replacement through an explicit
compare-and-replace contract. The page table layer checks whether the PTE still
matches the snapshot and retires the old transfer; the memory object layer owns
resource preparation and object-state commit/abort. A reference-consuming COW
commit sequence is:

```text
query_entry()
  -> prepare exact replacement mapping hold outside page-table mutation
  -> replace_if_same(reference)
       Ok(old_snapshot) => commit object state; old managed hold is retired
       Changed => unpublished input drops; abort other state and retry fault
       PageTable(error) => unpublished input drops; abort and propagate error
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

- **`PageTable64::drop`** recursively frees owned page table frames and ends
  remaining PTE-owned data holds under the inactive-table retirement contract.
  - The caller must first stop hardware use of the page table.
  - For an AArch64 user page table with a registered ASID provider, destruction
    reads the associated ASID before freeing the first table frame, executes
    `karch::dsb_ishst()`, and synchronously completes invalidation of the entire ASID,
    including non-leaf translation caches. Per-VA leaf invalidation cannot
    replace this reclamation boundary. If the final ASID hook reports failure,
    table-frame transfers and all unrecovered PTE-owned transfers remain pinned
    instead of being returned to inventory.
  - The provider context must remain alive until page table destruction
    completes. `MmSpace` declares `pgtbl` before `user_asid_context` to preserve
    this lifetime through field destruction order.
  - With `feature = "copy-from"`, entries marked in `borrowed_entries` are
    skipped so that subtrees shared from the source page table are not freed.
  - After the hardware-retirement boundary, each owned managed base or huge
    leaf is visited once. Its private category authorizes exact-range recovery
    of the outstanding transfer before its containing descriptor frame is freed.
    Raw leaves never invoke reference recovery. Child tables are freed
    recursively before their parent. Both read-only and mutable descent require
    `is_table()`, so non-present data entries are never treated as child tables.
- **`PageTableMut::finish`** advances automatic retirement and reports only
  whether work was pending; failed completion retains mapping holds internally.
- **`PageTableMut::try_finish`** provides a synchronous success/error boundary
  for revocation and raw release, without changing managed ownership on failure.
- **`PageTableMut::drop`** advances retirement through the same private boundary;
  if it still cannot confirm completion, the remaining data holds stay pinned.


## Storage ownership supplied by the handler

The HAL handler now transfers one `kalloc::PageRef` for each table-storage
allocation through the existing physical-address interface. Destruction consumes
that transfer once; borrowed root entries acquire no additional storage hold.
Generic `page_table` remains independent of kalloc and page-meta.

`copy_from()` immediately destroys a replaced target-owned subtree only after
prescanning the complete destination root range for managed leaves in owned
subtrees. A managed conflict rejects the operation before any target subtree is
destroyed or overwritten; pending managed retirements must also be complete.
Its caller must end hardware use and required translation retirement before
copying. The current production path populates a fresh user tree from permanent
kernel mappings. Borrowed descriptors duplicate neither table-storage holds nor
data-mapping transfers; reference counts do not perform TLB invalidation.


### Live-leaf duplication and external replacement coverage

`table64::tests_owned` checks managed alias ownership, clone/occupied/alignment
rejection, table-allocation failure after a prepared prefix, raw-leaf duplication,
managed-to-raw match/mismatch validation, and old-hold retention on unconfirmed
completion. HAL `paging_page_refs` exercises independent aliases and external
replacement with real PageRef inventory accounting. These roots are never active;
the tests establish transfer/release contracts, not successful hardware shootdown.

### RISC-V writable leaf permissions

The shared PagingFlags-to-RvFlags conversion normalizes WRITE to hardware R|W.
Sv39 and Sv48 reserve W=1,R=0, so both new_page and set_flags must use this
normalization, including first write-only file faults and later mprotect updates.
This changes the hardware leaf only; VMA permission policy remains with memspace.
The managed software bit is preserved independently during protection changes.
