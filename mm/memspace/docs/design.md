# memspace — Design Document

Msync test runtimes own their sync callbacks through fallible `KBox` allocation.
Fixture mapping may run with MM serialization held, including inside a sync
callback replacing a mapping, so callback allocation uses NOWAIT.

## Post-populate action ownership

`PopulateHook` owns a fallibly allocated `KBox` callback. A concrete `Option<F>`
adapter replaces the previous boxed callable alias; producers use the fallible
constructors and consumers invoke the owned action through `call`. This
adapter accepts consuming-only `FnOnce(&mut MmSpace)` closures without moving
an erased unsized value. Calling consumes the hook; dropping an unused hook
only drops its captures. `FaultCompletion` accepts an already prepared owner
and transfers it at most once. Invocation is synchronous in the original fault
context, not deferred to a worker, and outer MM locks remain held.

Default construction uses KERNEL and requires sleepable, reclaim-compatible
task context. A backend holding an MM lock must not assume KERNEL is safe:
prepare before locking, or use NOWAIT while failure can still be handled.
NOIO/NOFS also permit sleeping and do not remove every MM reclaim dependency.
Explicit flags cover owner storage only, not capture construction or cleanup.
Required hooks must be prepared before irreversible changes; returning an
allocation error after committing an operation that needs its hook is invalid.

All current production backends return `None`, so this migration introduces
no allocation into existing fault paths. Future producers must propagate
preparation errors through the backend result; NoMemory remains classified as
`PageFaultOutcome::OutOfMemory`. No NOFAIL guarantee is implemented.

## Positioning

`memspace` is the address-space owner of X-Kernel. It maintains `MmSpace`, `VmAreaSet`,
`VmArea`, the page-table root, and VMA-side runtime dispatch.

It owns neither file-backed cached content nor anonymous page content.
Content objects are managed by `kvfs::AddressSpace` and `mm/anon` respectively; `memspace` only holds
VMA metadata, runtime execution references, and page-table state.

## Background

The current model explicitly distinguishes three kinds of responsibilities:

- `VmAreaSet`: the container of VMA metadata in an address space.
- `VmArea`: the range, permission, backing, file metadata, and
  inheritance metadata of a single mapping instance.
- `VmRuntimeRef` / `VmRuntimeOps`: the VMA-side map/unmap/protect/fault/fork/mremap
  execution entry points, with a role close to Linux `vm_operations_struct`.
- `MmUserHandle` / `MmPin`: address-space lifetime capabilities. `MmUserHandle`
  corresponds to Linux `mm_users` and indicates that a runtime or temporary path is still actively using the user mappings;
  `MmPin` reserves a Linux `mm_count`-style object pin, which only guarantees that the `MmSpace` object and its stable
  identity remain observable, and does not extend the lifetime of user mappings.
  `MmPin::clone` only increments reference counts; it neither allocates nor
  takes the MM lock or acquires an active user. Before unpublishing an exiting
  thread, kprocess retains a pin with matching residency and ASID metadata.
  The final pin must survive while scheduling can still use that root.

Linux correspondence:

- `MmSpace` corresponds to `mm_struct`.
- `VmArea` corresponds to `vm_area_struct`.
- `VmRuntimeOps` corresponds to the VMA-side `vm_operations_struct`.
- `VmBackingInfo` describes the file/anon/linear backing object a VMA points to.

## Scope

- `src/aspace.rs`
- `src/vma.rs`
- `src/fault.rs`
- `src/backend/*`
- `src/iomap.rs`

### Kernel unit test layout

Test sources live in `ut-tests/` at the crate root and are mounted into the original source modules through `unittest::path_mod_test!`,
compiled only under `cfg(unittest)`. Module names, private visibility, and
the way tests are executed remain unchanged, and `super` still refers to the original production module.

| Test file | Original module path | Test cases |
| --- | --- | --- |
| `ut-tests/lib.rs` | `tests_memspace` | 3 |
| `ut-tests/aspace.rs` | `aspace::tests` | 16 |
| `ut-tests/vma.rs` | `vma::tests` | 17 |
| `ut-tests/fault.rs` | `fault::tests` | 2 |
| `ut-tests/cpu_residency.rs` | `cpu_residency::tests_cpu_residency` | 3 |
| `ut-tests/backend_private.rs` | `backend::private::tests` | 6 |
| `ut-tests/backend_shared.rs` | `backend::shared::tests` | 1 |
| `ut-tests/aarch64_asid.rs` | `aarch64_asid::tests` | 6 |

There are 8 test modules and 54 test cases in total; `aarch64_asid` keeps the AArch64 architecture gate of its parent module,
and other architectures select the remaining 48 test cases. The tests have no extra feature gates or serial/user markers.
They are verified using the guest unittest flow with a configured kernel, not replaced by host Cargo tests.

## Architecture

```text
MmSpace
  -> MmUserLifetime
  -> PageTable
  -> MmCpuResidency
  -> VmAreaSet
       -> VmArea
            -> VmBackingInfo
            -> FileMappingInfo
            -> VmRuntimeRef
                 -> Linear
                 -> AnonShared
                 -> AnonPrivate
                 -> FileShared
                 -> FilePrivate

object owner
  -> vmobj::ObjectInvalidateRequest
  -> MmSpace invalidate sink
  -> VMA lookup and runtime unmap
```

## Calling Constraints / Execution Context

- `MmSpace` is protected by an outer `Mutex` or equivalent exclusive access.
- The map/unmap/protect/fault/fork/mremap paths may allocate memory and run in a sleepable context.
- Not applicable to interrupt context.
- Callers must complete validation of user input at the syscall, loader, or kernel VM boundary.

## Algorithm Flow

### VMA lookup

1. `MmSpace::find_vma()` locates the VMA in `VmAreaSet`.
2. `VmArea::backing()` returns `VmBackingInfo`.
3. `VmArea::page_offset()` denotes the Linux `vm_pgoff`-style backing page offset.
4. `VmArea::file_mapping()` returns the file-backed introspection metadata.
5. `VmArea::runtime()` is used only inside `memspace` to execute map/unmap/fault operations.

### Futex backing resolution

`MmSpace::resolve_futex_backing()` is the MM boundary for non-private futex keys:

- A private VMA returns `(mm_id, virtual_address)`;
- A shared anon/file VMA returns
  `(VmObjectId, backing_page_index, byte_offset_in_page)`;
- The shared offset is computed through `VmArea::backing_offset_for()`, so VMA split, trim,
  different virtual mappings, and nonzero file offsets all preserve the same object-relative identity.

This API only reads VMA metadata and does not trigger a page fault. Callers must still use kuaccess to perform the actual
user word access and fault-in.

### VMA insertion

1. The caller constructs a `VmArea` and the corresponding `VmRuntimeRef`, and `MmSpace::map_runtime_vma()` validates the range.
2. `install_runtime_mapping()` checks for VMA overlap before modifying the page table or registering the incoming object view,
   returns `AlreadyExists` on overlap, and preserves the content, permissions, and range of the already installed mapping.
3. The runtime performs an eager map or prepares the lazy fault path.
4. The file/anon object view is registered using the current `mm_id` and `InvalidateHandle`, and then the VMA is published.
5. The internal assertion in `VmAreaSet::insert()` protects the exclusive-modification invariant that has already passed the pre-check.

The `map()` of a lazy private runtime does not itself modify the page table, so it cannot rely on page-table collision to reject overlap.
The regression test also checks that such a request returns an error, that the original physical page/content/write permission are preserved, and that the extended suffix has no VMA or PTE.
This protection does not explain why the upper layer chose the overlapping address, nor does it treat arbitrary upper-layer mmap/brk errors as already resolved.

### VMA transformation

- `VmAreaSet::unmap()` supports exact removal, front/back trim, and middle split.
- `VmAreaSet::protect()` supports partial range split and updates only the current permission.
- Protection changes use each runtime's actual PTE flags. Empty PTE flags
  follow the zap/TLB/runtime-notification contract even when the VMA request
  itself is nonempty; a later error cannot retain tokens for cleared prefixes.
- `VmAreaSet::merge_adjacent_where()` merges only adjacent VMAs that are semantically equivalent and have the same runtime kind.
- The file-backed `FileMappingInfo.offset` and `page_offset` are updated in sync during split/trim.

### mremap relocation

`mremap` move-style relocation is not modeled as ordinary `unmap`.

```text
source snapshot
  -> relocate_mapping()
       -> prepare destination VMA/runtime and all destination PTEs
       -> remove source PTEs and finalize TLBs
       -> retire source metadata
```

The three ownership layers are explicit:

- `VmArea` metadata: this virtual range has a mapping role;
- present PTE residency: this virtual page currently points at a frame;
- backing object ownership: anonymous state is owned by anon objects; the
  file-backed source and its object identity are owned by the inode
  `kvfs::AddressSpace`.

Relocation may change the first two layers without destroying the third.
The address-space lock covers the complete transaction. PTE preparation
failure removes only the newly installed destination PTEs and finalizes TLBs
before dropping the destination metadata/runtime. All source PTEs and backing
references remain intact. In particular, rollback must not call ordinary
runtime `unmap`, because the destination shares the source anonymous object.
Source-retirement runtimes are resolved before publication so the commit path
has no fallible metadata lookup. Shrinking callers trim a discarded tail first.
Runtime registrations that are external to page-table ownership use the
metadata-only `on_mapping_retired()` callback on rollback and commit; it never
detaches anonymous slots or replaces the PTE/TLB cleanup contract.
Anonymous shared relocation supports the existing fixed page-array extent;
growth is rejected before destination publication rather than exposing a VMA
that extends beyond the backing page array.

### Page fault

```text
FaultInput
  -> MmSpace::handle_fault_input()
  -> VMA lookup
  -> permission check
  -> FaultContext with VMA-derived offsets
  -> VmRuntimeRef::handle_fault()
  -> PageFaultOutcome
```

`FaultOutcome` preserves the Linux user-visible fault classes:

- resolved;
- retry / COW retry;
- unmapped;
- access denied;
- file-range `BusError`;
- out of memory;
- no progress / failed.

### Object-driven invalidate

```text
kvfs::AddressSpace / anon object
  -> vmobj::ObjectInvalidateRequest
  -> InvalidateHandle
  -> MmSpace invalidate sink
  -> drain_pending_invalidations()
  -> apply_invalidate_request()
  -> runtime.unmap()
```

After receiving an object-side invalidate, `MmSpace` re-checks it against the current VMA metadata:

- whether the object id matches;
- whether the shared/private view kind matches;
- whether the object offset is still consistent with the VMA offset;
- whether the request range still covers the current VMA overlap.

After a match, only present PTEs are zapped and the VMA metadata is kept. The next fault re-decides based on the current object state.

If applying a single request fails, `MmSpace` puts it back at the tail of the queue and ends the current drain round, to avoid retrying the same boundary indefinitely.

### Private anonymous discard / COW

Private-anon and private-file teardown share `unmap_private_object_range`.
After detaching the object slots, it calls the page-table sparse range API,
which skips absent subtrees instead of probing every virtual page from the
root. It retains the runtime's uniform page size, address-space lock, explicit
TLB finish, and final object/frame release. The shared helper also serves
private discard and address-space cleanup; VMA deletion, object-page indexing,
shared backends, and intermediate page-table reclamation are unchanged.

- Post-write private pages of `AnonymousPrivate` and `FilePrivate` are held by
  `AnonPrivateObject`.
- `MADV_DONTNEED` converts the VMA range into an object range through the runtime; the
  private-anon helper then detaches the object page slots, unmaps present PTEs, and releases the frames after TLB
  finalization completes.
- A COW write fault uses an object slot comparison and the PTE snapshot comparison of `PageTableMut::replace_if_same()`.
  A change on either side returns a retry-class outcome.

### `msync`

`MmUserHandle::msync_range()` retains the active MM user while
`MmSpace::msync_range()` owns Linux-style VMA traversal. Under the MM mutex it
finds the current VMA and translates the overlap into a checked half-open file
byte range. It retains only that file runtime, releases the mutex for datasync,
then reacquires it and looks up the next VMA anew. Concurrent unmap or replacement
cannot invalidate the retained file or make later sync use a stale VMA snapshot.

Holes yield `NoMemory` after later mappings are processed unless a filesystem
error takes precedence. Plain `MS_ASYNC` returns at the first hole, as Linux does.
The validated `MsyncPolicy` carries only writeback mode and invalidate intent.
MM dispatches only `MS_SYNC` on shared file mappings; anonymous, linear, private
file and non-sync requests do not enter a persistence hook. Private COW pages
cannot be written back to the source. MM owns neither persistence state nor a
separate data/metadata policy; the runtime calls the existing file's datasync.

`MS_INVALIDATE` returns not supported in the current model because locked VMA
and invalidate semantics are not represented yet.

### Protection Changes

`MmSpace::protect_mapping_range()` separates VMA permission state from present
PTE flags. The requested flags are recorded in `VmAreaSet::protect()`, while
`VmRuntimeOps::on_protect()` returns the effective PTE flags to install for
already-present mappings.

This allows runtimes such as shared file mappings to keep writable VMAs backed
by write-protected PTEs until the first write fault performs runtime-specific
dirty tracking.

### Fork

1. Create a new child `MmSpace`.
2. Copy the metadata of each `VmArea`.
3. Shared runtimes share the object identity.
4. Private runtimes form COW through the `AnonPrivateObject` lineage and page-table write protection.
5. The child object page state is committed only after TLB finalization completes for the parent/child page-table modifications.

## Concurrency model

- `MmSpace` is serialized by an outer lock.
- The user-count state transitions of `MmUserHandle` do not depend on the `MmSpace` main lock; minting a handle from a bare
  `Arc<Mutex<MmSpace>>` briefly reads the mm-owned lifetime, and when the last
  user is released, it acquires the `MmSpace` main lock and performs `clear()`.
- `MmUserHandle::clone_user_unless_zero()` and the last user release are mutually exclusive through the same atomic
  state transition; once the torn-down state is entered, a later `MmPin` can no longer be upgraded to an active user.
  Cloning only increments reference counts and can run under a short spinlock
  without allocation or MM locking. Release the returned handle outside that
  critical section, in sleepable task context without holding the MM lock.
- `MmSpace` owns `MmCpuResidency`, which records CPUs that may retain its TLB
  state. Scheduling adds the CPU to the new MM before switching and does not
  immediately clear it from the old MM. Conservative retention can target extra
  CPUs for shootdown but must not omit CPUs that still need invalidation.
- `InvalidateSink` uses `SpinNoIrq` to protect short queue operations, and does not run runtime
  unmap or acquire the `MmSpace` main lock while holding that lock.
- `PageTable::modify()` provides an exclusive page-table modification guard.
- `VmAreaSet` does not expose independent mutable shared access.
- The internal concurrency of file/anon objects is maintained by the corresponding object crates themselves.

## Design decisions

1. `VmArea` stores Linux-style VMA metadata.
   Reason: split/protect/unmap/fork/mremap all need stable metadata, rather than inferring it from runtime
   private state.

2. `VmRuntimeRef` only represents a VMA-side execution reference.
   Reason: a runtime can perform faults and PTE operations, but must not become the owner of the file cache or anon pages.

3. `VmBackingInfo` explicitly distinguishes `Linear`, `AnonymousShared`, `AnonymousPrivate`,
   `FileShared`, and `FilePrivate`.
   Reason: a file-private mapping must retain the file source object, the anon result
   object, and the anon lineage at the same time.

4. `FaultInput` is separate from `FaultContext`.
   Reason: the trap/user runtime input contains only the fault facts reported by the CPU; VMA-derived
   object offsets must be attached after the VMA lookup.

5. Object-driven invalidate uses a `vmobj` request.
   Reason: both file-backed and anonymous objects need the same object -> VMA view language.

6. `msync` uses a runtime hook rather than having the syscall look up the pagecache in reverse.
   Reason: `MmSpace` owns VMA traversal, while `filemap` owns file-backed runtime
   source state. This keeps syscall ABI, VMA dispatch, and content-object
   writeback in separate owner crates.

7. relocation source retirement is separate from ordinary runtime unmap.
   Reason: ordinary private/file-private unmap may detach backing object pages;
   successful relocation only retires the old virtual role.

## Drop / Resource Release

- `MmSpace::clear()` cleans up the present PTEs of all VMAs through runtime unmap.
- `PageTableMut::finish()` is the TLB finalization boundary before detached frames are released.
- After `VmAreaSet` is cleared, the VMA metadata and the runtime references are released together.
- File/anon object view guards are released together with the runtime/object references, and the views are unregistered.

File-backed page faults classify backend capacity exhaustion (`StorageFull`)
as `BusError`, alongside backend data and I/O failures. User runtime therefore
delivers `SIGBUS`; this conversion does not change write syscall errno mapping.

## Invalidation Completion Notification for Mapping References

`VmRuntimeOps::on_invalidate_complete(range)` is the generic runtime lifetime boundary: after an object
invalidation request has been matched by backing identity/offset, or after invalidate_present has been called,
memspace zaps the PTEs in batch under a single page-table guard and completes one TLB finalization, then
notifies the runtime page by page for the pages that were processed successfully; on failure, the unprocessed part keeps its references and request for retry. The notification does not delete
the VMA, and does not take on the concrete ownership of file pins or anonymous frames.

`PROT_NONE` protection also removes present PTEs. After the page-table guard
finishes TLB invalidation, `MmSpace` invokes the same completion hook so backing
runtimes can release references that only kept those translations valid.

Last-user address-space teardown walks the stable VMA set to unmap each range,
finishes TLB invalidation, then walks it again to complete runtime invalidation
before clearing the VMA metadata. It does not copy the VMA set into temporary
arrays, so this outer walk does not require memory proportional to VMA count.

Private anonymous offsets have one source: anonymous VMAs derive bytes from
`page_offset`, while file-private runtimes derive them from their retained
anonymous origin and the virtual displacement. Object invalidation asks that
runtime for the same coordinate used by faults, fork and relocation; splitting,
prefix unmapping and protection do not maintain a second VMA offset field.


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

## IO Mapping

`iomap_device` registers a device region and maps it in the kernel I/O window;
`iounmap` drops the last reference and removes the mapping. Both use the
deferred variants (`MmSpace::map_linear_deferred` / `unmap_linear_deferred`):
the page-table update happens under `kernel_layout()` and `DEVICE_REGISTRY`,
both locks are released, and only then is the remote TLB invalidation completed.
A CPU that contends for either IRQ-disabled lock therefore stays able to
acknowledge the shootdown. Ranges that contain a managed runtime fall back to
the in-place path, whose invalidation completion must run under the lock.

## MMIO Error Conversion

IoMapError converts into KError at callers' kernel error boundaries: InvalidRange
maps to InvalidInput, MappingFailed to Io, and NoMemory remains NoMemory. The
conversion does not release mappings or roll back partially initialized hardware.

`InvalidateHandle::try_apply` never waits for the MM mutex. It reports success
after matching PTE removal and TLB invalidation, including the vacuous case of a
destroyed MM. Handles created for view registration before an owned `Arc<MmSpace>`
exists are deferred: they carry no address-space pointer, so `try_apply` reports
incomplete instead of claiming the vacuous destroyed-MM success, and `submit`
leaves the request for the owning address space to drain. File-backed write-fault
admission contention is classified as `PageFaultOutcome::Retry` so the fault
returns from the MM critical section before retrying a conflicting filesystem
lifecycle or inode policy transition.

`MmUserHandle::resolve_page_fault` is the common sleepable fault entry. On
admission Retry it retains the existing backing runtime, releases the MM mutex,
calls `VmRuntimeOps::wait_for_fault`, then looks up the VMA and checks permissions
again. Raw locked fault handling remains available to exception paths. File
runtimes wait through their existing filesystem/inode admission resources; no
parallel wait table or persistent retry state is introduced. COW conflicts and
permanent faults retain their existing classification.

Relocation derives permissions from its source snapshot and accepts an explicit
target-replacement policy. Fixed replacement removes the target and discarded
source tail before preparing the retained source. File runtimes open inherited
registrations without new-map admission; failures retire destination registrations
while preserving retained source PTEs. File expansion also uses open and updates
the existing object-side view range before the VMA extension is published.

File byte-coordinate overflow returns `None` from VMA translation and cannot
fall back to anonymous coordinates. Anonymous backing offsets use checked page
multiplication and byte addition; addresses below the VMA start are rejected.

The current runtime is explicitly released before MM is reacquired for the next
VMA. This is part of the persistence boundary: dropping the last file reference
may run a filesystem release callback and wait for writeback, just as Linux
`fput()` after `vfs_fsync_range()` must run outside mmap serialization.

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
