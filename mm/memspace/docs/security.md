# memspace — Security and Reliability Analysis

## Post-populate hook safety

`PopulateHook` uses safe one-shot adaptation over `KBox<dyn FnMut(&mut MmSpace)>`;
it adds no raw-pointer ownership or unsafe code. Taking a hook empties the
completion slot, calling consumes its owner, and cancellation or construction
failure drops captures without invoking the callback. Capture destructors run
in the dropping caller's context, independently of allocation flags.

Raw fault dispatch keeps the outer MM lock while invoking post-populate hooks.
Those callbacks must not reacquire that lock or assume reclaim-safe execution.
The separate admission wait hook runs only after the active-MM resolver releases
MM serialization.
NOWAIT permits allocation without reclaim but can fail; callers must allocate
before committing any operation that cannot tolerate loss of its completion
action. Default KERNEL construction requires sleepable, reclaim-compatible
context and must not be used merely because the lock is a sleeping mutex.

Tests cover consuming-only captures, one-shot extraction, capture cleanup on
cancellation and invalid flags, synchronous dispatch, and NoMemory outcome
classification. They do not force global allocator exhaustion or exercise
reclaim, which the current backend does not implement. Existing production
backends construct no post-populate hooks.

## Trust Model

The analysis scope covers the production implementation in `src/` and the kernel test fixtures in `ut-tests/`.
Test files are mounted inside the original private modules via `unittest::path_mod_test!` and are compiled only under
`cfg(unittest)`; the ASID tests are still gated by the parent module's AArch64 architecture gate.
The test migration does not change production visibility, unsafe preconditions, or the lifetime of address-space resources.

`memspace` trusts:

- that the syscall, loader, and kernel VM callers have already validated the origin of their inputs;
- that `page_table` correctly performs PTE modification and TLB finalization;
- that the KVFS file address-space, anon, and vmobj crates correctly maintain their respective object lifetimes;
  `pagecache` maintains only its own private folio storage;
- that callers guarantee exclusive mutable access to the same `MmSpace` through an outer lock.

`memspace` is responsible for:

- address-space boundary checks;
- VMA non-overlap, ordering, and metadata consistency;
- synchronization of VMA permissions with page-table permissions;
- saving and reporting the fault class;
- re-validation of object-side invalidate requests and PTE zap.
- The `MmUserHandle` / `MmPin` lifetime capabilities, which distinguish active user mappings
  from object pins.

## External Boundaries / Attack Surface

- Address-space editing requests such as `mmap` / `munmap` / `mprotect` / `mremap` / `brk`.
- CPU page faults or user-runtime fault requests.
- `fork()` / clone address-space copy.
- File/anon object-side invalidation requests.
- Kernel linear mapping and I/O mapping helpers.

## Unsafe Code Inventory

`src/aspace.rs`

- `copy_nonoverlapping` in `read()` / `write()`
  - Relies on the physical address obtained from the page-table query being convertible to a valid kernel address through the direct map.
  - Relies on the caller guaranteeing that the target user address range has already passed the VMA/permission checks.

`src/vma.rs` and `src/fault.rs` introduce no `unsafe`; `src/backend/mod.rs` (`alloc_frame` zero-fills frames via `write_bytes`) and `src/backend/private.rs` (`copy_private_page` via `copy_nonoverlapping`) contain `unsafe` blocks.

## Memory Safety Invariants

- Every VMA range must lie entirely within the address-space bounds of `MmSpace`.
- `VmAreaSet::try_insert()` must reject overlapping VMAs.
- `VmAreaSet::unmap()` / `protect()` must keep VMAs non-overlapping and ordered after split/trim.
- `page_offset()` and `FileMappingInfo.offset` must be adjusted in step with VMA split/trim.
- `VmArea::max_flags()` must preserve the original maximum permission after split/protect/fork/mremap.
- `VmArea::backing()` and `VmRuntimeRef::backing_info()` must be consistent.
- When `mprotect` updates the VMA current permission, the present PTE flags must use
  the return value of `VmRuntimeOps::on_protect()`; the requested flags must not be written into the page table unconditionally.
- `msync_range()` may only let the shared file runtime perform a file-range datasync (including filesystem persistence); anonymous,
  linear and private file mappings must not write back to the file source.
- A `FilePrivate` backing must keep the file source object, the anon result object and the
  anon lineage together.
- Private-anon page state must be held by `AnonPrivateObject`, and the runtime must not own a separate
  private frame table.
- Detach frame, unmap PTE, TLB finalization and frame release must keep their order.
- A sparse range unmap of a private mapping skips only nonexistent page-table subtrees and non-present
  entries; the actual leaf size must match the runtime. When a page-table walk fails, backing ownership is kept,
  the RAII rollback of the detached slots restores the object records; the already cleared PTE prefix can be
  re-established by later faults, and frames must not be released early on the failure path. The original address-space lock and
  the TLB guard still cover the process and do not release intermediate page tables.
- `mremap` move-style source retirement must not call ordinary runtime
  `unmap()` on the moved source range.
- `relocate_mapping()` prepares every destination PTE before removing source
  PTEs. On failure, it finalizes rollback TLB invalidation before dropping only
  destination metadata and runtime references; rollback cannot detach the
  anonymous slots still owned by the source.
- Mapping-policy registrations use a separate metadata-only retirement hook;
  rollback does not call runtime `unmap()` and therefore cannot release source
  anonymous slots or alter source PTE ownership.
- For a shared VMA, `resolve_futex_backing()` must return an object-relative offset;
  a VMA-relative address must not be used as a cross-process key.
- `MmUserHandle` is the only capability that can retain the lifetime of user mappings; `MmPin` and an ordinary
  `Arc<Mutex<MmSpace>>` observer can retain only object observability and cannot prevent the mappings from being cleared when the last user
  is released.
- `MmPin` can be cloned in a short critical section. Final release can destroy
  the page table and requires sleepable context outside spinlocks, after CPUs
  stop using that root. Cloning cannot resurrect active users of a torn-down MM.
  An exiting thread's pin does not authorize further access to user memory.

## Thread Safety

- `MmSpace` mutable operations require external exclusive access.
- `InvalidateSink` spinlock only protects queue mutation; it must not cover
  runtime unmap, page-table mutation, or sleepable lock acquisition.
- `PageTable::modify()` enforces one mutable page-table guard at a time.
- file/anon object locks must not be acquired while holding `InvalidateSink`'s
  spinlock.
- `MmUserHandle::msync_range()` retains the active MM user across sleepable
  persistence. Callers must not hold MM or provider locks. MM releases its mutex
  before the file runtime hook and looks up each subsequent VMA after relocking;
  only an owned runtime and checked file byte range cross that unlock.
- File-coordinate overflow must not select anonymous fallback coordinates;
  anonymous page-to-byte translation also rejects multiplication/addition overflow.
- Scheduling adds the CPU to the new `MmCpuResidency` before activating its
  root; old-MM CPU bits remain conservative rather than being cleared at each
  switch. Page-table flush paths must not reconstruct the target set.
- `MmUserHandle::clone_user_unless_zero()` and the release of the last user use the same atomic
  state transition; once the torn-down state is entered, `MmPin::try_upgrade_user()` must fail.
  Cloning can acquire an active user under a short spinlock, but the returned
  handle must be released outside it. Last-user release locks the MM and clears
  mappings; it requires sleepable context without the MM lock already held.

## Threat Analysis

1. VMA metadata drifts from the runtime backing.
   - Mitigation: `runtime_for_vma()` compares `runtime.backing_info()` with
     `vma.backing()` and refuses to continue when they differ.

2. A file-private source object is confused with an anon result object.
   - Mitigation: `VmBackingKind::FilePrivate` explicitly stores both object ids and the lineage.

3. An object-side invalidate wrongly zaps an unrelated VMA.
   - Mitigation: `apply_invalidate_request()` rechecks the object id, view kind, VMA
     overlap and object offset.

4. An invalidate request is lost after it fails.
   - Mitigation: a failed request is re-enqueued; the current drain round ends and the request is retried at the next boundary.

5. A COW write fault overwrites a competing PTE.
   - Mitigation: the COW path compares both the object slot and the `PteSnapshot`, and returns retry if either changed.

6. A stale TLB entry accesses a freed frame.
   - Mitigation: `PageTableMut::finish()` is explicitly completed before a detached frame is freed.

7. The current CPU is removed too early from the old mm's target set during a scheduler switch.
   - Mitigation: the switch path publishes the CPU to the new mm before activating its root; old-mm CPU bits are retained conservatively rather than cleared at each switch.

8. Relocation source retirement misuses ordinary unmap.
   - Mitigation: a successful move uses metadata-only retirement after destination
     install and PTE transfer.

9. An mm object observer blocks exit cleanup or reactivates a torn-down address space.
   - Mitigation: only `MmUserHandle` increments the active user count; `MmPin` only pins the object, and
     upgrade and the last release are mutually exclusive through a single atomic state.

10. Exit cleanup copies a large VMA set under memory pressure.
    - Mitigation: walk the stable set directly before and after TLB completion;
      this outer walk needs no VMA-count-sized allocation. Backend teardown may
      still allocate and has its own failure constraints.

## Failure Mode and Effects Analysis (FMEA)

| Failure | Condition | Handling | Impact |
|---|---|---|---|
| VMA overlap | The insertion path does not validate | `install_runtime_mapping` pre-checks with `VmAreaSet::overlaps()` and returns `AlreadyExists` before modifying the page table or registering the object view, and `VmAreaSet::try_insert()` rejects it once more | Avoids address-space ambiguity; overlaps that a user-space syscall sequence can trigger degrade to a recoverable error instead of panicking the whole machine |
| Runtime/backing mismatch | The runtime is inconsistent with the VMA metadata | Return `BadAddress` and log a warning | The fault/map operation fails |
| Invalidate apply failure | Runtime unmap or the page-table path fails | The request is re-enqueued and the current drain stops | Clearing of the stale PTE is delayed |
| COW conflict | The object slot or PTE changed | Return a retry-class outcome | Fault retry |
| OOM | Frame or page-table allocation fails | Return `OutOfMemory` / `NoMemory` | The syscall or fault fails |
| File backend fault | File page out of range, I/O failure or quota exhausted | `BusError` outcome | The upper layer delivers SIGBUS; ENOSPC for an ordinary write is unchanged |
| `mremap` source teardown destroys moved private backing | Relocation is mistaken for an ordinary unmap | Metadata-only retirement | Avoids the moved mapping pointing at freed object pages |
| Stale mm user acquire | The last user release races with a new user acquire | Single atomic-state CAS; upgrade/acquire fails after teardown | Avoids reusing the cleared mappings after exit cleanup |

## Fault Management

- Address-space edit failures are returned through `KResult`.
- The fault path returns `FaultOutcome`, which must not be collapsed into a bool at the `memspace` boundary.
- Retry-class faults include ordinary retry and COW conflict retry.
- `BusError` denotes a backing-object-level error, including mapped-file EOF/range faults, I/O
  failure and `StorageFull`. Quota exhaustion does not indicate an address permission error and should not be classified as SIGSEGV; allocation failure
  `NoMemory` is still classified separately as `OutOfMemory`.
- An invalidate drain failure affects only the current request; the request stays in the queue to wait for the next boundary.

## Known Limitations

- `AddrSpace` still exists as a compatibility alias of `MmSpace`.
- `BackendOps` is still used as the shared execution helper for anonymous/linear runtimes.
- Writable regular-file `MAP_SHARED`, reclaim/swap/memcg/NUMA/THP are not implemented by the current
  `memspace` paths.
- `MS_INVALIDATE`, locked-page `EBUSY`, and running the slow
  provider sync after releasing the `MmSpace` lock are not implemented.
- The invalidate sink uses a queue/drain model and does not provide the strong synchronization semantics of Linux's synchronous rmap tree.

## Audit Checklist

- [ ] Does the new runtime's `backing_info()` match the installed `VmArea.backing()`?
- [ ] Do VMA split/trim/protect preserve the continuity of `page_offset` and the file offset?
- [ ] Does the fault path perform the VMA permission check before calling the runtime?
- [ ] Does the file-private path preserve both the file object and the anon object identity?
- [ ] Does private-anon frame release happen after TLB finalization?
- [ ] Do invalidate requests enter `MmSpace` through the `vmobj` object/view language?
- [ ] Is only queue manipulation done inside the `InvalidateSink` spinlock?
- [ ] Do unsupported Linux semantics explicitly return an error rather than silently degrading?
- [ ] Does a newly added address-space observer hold only an `MmPin` or a plain `Arc`, without misusing
  `MmUserHandle` to extend the lifetime of user mappings?

## Invalidation Completion Notification Order

The runtime completion callback must come after PTE removal and TLB finish, and is
serialized by the same exclusive MM lock; it may only release the runtime references of the notified range and must not additionally clear other mappings. A failed invalidation
must not trigger the callback, so PTEs that may still be visible are always protected by a backing reference.

## DMA Mapping Capability

memspace directly implements kiface-mm-api DmaMappingIf. The caller selects Coherent
or Normal mode, not arbitrary flags. The provider updates the kernel page table
under the existing kernel layout lock and returns `Some(DmaMappingCompletion)`
when the remote invalidation was detached. The caller completes it after
releasing its own locks, because a CPU contending for the IRQ-disabled lock must
be able to acknowledge the shootdown IPI. When deferral is not possible (a user
page table, a fresh-map fence, or a pending managed retirement) the provider
returns `None` and the page-table guard's drop completes the flush in place as
before.
Coherent is READ|WRITE|UNCACHED|SHARED; Normal is READ|WRITE. The caller owns the
allocation and quiesces device access before restoration. Allocation, platform
sharing, rollback and device-cache synchronization are outside this capability.

A busy filesystem write-admission boundary is a retryable file fault, not a
permanent memory-access failure. The fault must unwind the MM lock before retry;
otherwise a filesystem transition revoking that same MM can deadlock. Completed
synchronous object invalidation includes PTE removal and TLB invalidation; an
already-destroyed MM has no remaining mappings to revoke.

Admission Retry is not materialization progress and must not exhaust a user
copy's no-progress bound. The active-MM resolver retains lifetime through waits,
but carries no borrowed VMA/PTE references across MM unlock. A provider returning
Retry must support the lock-free-of-MM wait hook; unsupported waits fail rather
than spin indefinitely. Every retry repeats current VMA and permission checks.
Waiting may block in task context and must not run under an MM or non-sleepable
lock, or inside an exception handler.

Relocation derives permissions from its source snapshot and accepts an explicit
target-replacement policy. Fixed replacement removes the target and discarded
source tail before preparing the retained source. File runtimes open inherited
registrations without new-map admission; failures retire destination registrations
while preserving retained source PTEs. File expansion also uses open and updates
the existing object-side view range before the VMA extension is published.

The unlocked `msync` interval includes final runtime/file-reference destruction.
A concurrent unmap and close can leave that temporary reference as the last
owner, so reacquiring MM first could block all mappings during file release.

## Managed file mapping relocation

move_pages grows its rollback ledger only for present source leaves and reserves
each slot before publishing the corresponding destination. Sparse holes require
no ledger allocation. Allocation failure rolls back the prepared destination
prefix while preserving every source. map_existing clones a managed source's actual transferred hold under exclusive
table access, or preserves a raw source's external ownership protocol. Failed
destination preparation removes only the prepared prefix; source PTEs and their
runtime owners remain intact. After all destinations exist, source PTE removal
retires its holds while destination holds independently retain the same pages.
Folio/content-token release has no TLB-completion dependency. Existing finish
advances retirement; global synchronous return guarantees remain a separate
TODO, with no new retry, pending-state or recovery engine introduced here.

## IO Mapping Invalidation Ordering

Kernel IO-window updates (`iomap_device` / `iounmap`) defer the remote TLB
invalidation until after `kernel_layout()` and `DEVICE_REGISTRY` are released.
A CPU that needs either IRQ-disabled lock spins with interrupts disabled and
cannot acknowledge a shootdown, so completing the invalidation under those
locks would stall the initiator until its bounded give-up. Ranges containing a
managed runtime fall back to the in-place path, whose completion runs under the
lock by design.

A deferred completion can still fail: both callers log the error and keep the
success result, because the PTE update and the VMA/registry change are already
published and cannot be rolled back into a coherent state. A failure therefore
leaves a stale remote translation for a device window after `iounmap`, or hides
the new mapping from a remote CPU after `iomap_device`.
`DeferredKernelTlbFlush::complete()` first retries a scoped or partial shootdown
as a full-TLB invalidation, so reaching this state requires the IPI path itself
to be broken; the in-place path previously dropped the same failure inside
`PageTableMut::Drop`. The blast radius is bounded by the driver contract that
device access is quiesced before `iounmap`, and the logged error is the only
observable signal. This residual risk is accepted: the callers have no failure
channel that can undo a published mapping.
