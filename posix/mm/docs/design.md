# posix-mm — Design Document

## System commitment admission

`RLIMIT_AS` remains the per-process virtual footprint limit. Independently,
`memspace` enforces Linux's default overcommit heuristic for new private writable
commitments, writable protection upgrades and remap growth. A single new
commitment larger than managed RAM is denied even with unlimited `RLIMIT_AS`.
The heuristic does not cap aggregate commitments or eagerly allocate backing.
Before destructive `MAP_FIXED` replacement, the syscall preflights this admission
using the requested private/write/noreserve policy. VMA installation performs
the owned charge; mapping creation, fork, changes and destruction are accounted
inside `memspace`. `MAP_NORESERVE` is preserved on file/anonymous mappings and
fresh `MREMAP_DONTUNMAP` source mappings. Shared objects keep their own quotas.

## Positioning

`posix-mm` implements the POSIX/Linux-style memory-management syscall entry points, including
`mmap`, `mremap`, `brk`, `madvise`, `msync`, `mincore`, `memfd_create`, and the
`mlockall`/`munlockall` pair (validated no-ops: the kernel has no swap, so every
page is already resident). It sits between the userspace ABI and the kernel `mm`/`fs` subsystems, and is responsible only for
argument validation, typed request construction, object assembly, and error-code translation.

`posix-mm` does not own the VMA tree, page tables, the page cache, file objects, or anonymous objects.
This state is maintained by `mm/memspace`, `mm/filemap`, `pagecache`, `memfs`,
`kvfs`, and `mm/anon` respectively.

## Scope

- `src/brk.rs`
- `src/limits.rs`: address-space limit admission for mapping growth.
- `src/mmap.rs`
- `src/memfd.rs`
- `src/mincore.rs`

## Architecture

### syscall-to-MM request boundary

All raw syscall arguments are first frozen into a typed request inside `posix-mm`, and only then enter
`mm/memspace` or `mm/filemap`:

```text
sys_mmap(raw args)
  -> MmapRequest::from_raw()
  -> MmapRequest { flags, map_type, page_size, permissions, offset }
  -> MmapRequest::resolve_address_with_limit() (RLIMIT_AS and commitment admission)
  -> file-backed: filemap::mmap_*_file(); anonymous: construct VmArea + VmRuntimeRef
  -> MmSpace::map_runtime_vma()

sys_mprotect(raw args)
  -> MprotectRequest::from_raw()
  -> MmSpace::protect()

sys_munmap(raw args)
  -> MunmapRequest::from_raw()
  -> MmSpace::unmap()

sys_madvise(raw args)
  -> MadviseRequest::from_raw()
  -> MmSpace::madvise_dontneed()

sys_msync(raw args)
  -> MsyncRequest::from_raw()
  -> LiveAddressSpace::msync_range() -> MmUserHandle::msync_range()

sys_brk(raw addr)
  -> BrkRequest
  -> MmSpace::map() or MmSpace::unmap()
```

`mremap` move-style relocation has one extra contract boundary:

```text
sys_mremap(move)
  -> MmSpace::resolve_mremap_source()
  -> MmSpace::relocate_mapping()
```

The syscall layer must not retire the old source range with ordinary
`MmSpace::unmap()` after a successful move. Ordinary unmap semantics may detach
private backing ownership, while relocation only retires the old virtual role.

This boundary carries the Linux-visible ABI policy:

- `MmapProt` / `MappingPermissions` derive the current permission
  and maximum permission from `PROT_*`;
- `MmapFlags` / `MapType` derive shared/private, fixed policy,
  anonymous/file-backed, populate, and page-size policy from `MAP_*`;
- unknown `MAP_*` bits return `InvalidInput` at the syscall boundary;
- the ordinary `MAP_PRIVATE` / `MAP_SHARED` paths allow some Linux-known deferred
  flags to pass through the typed request as a compatible no-op policy;
- `MAP_SHARED_VALIDATE` returns
  `OperationNotSupported` for these deferred flags;
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE` require the supplied address to be aligned to the target page size;
- `munmap(addr, 0)` returns `InvalidInput`;
- `MprotectRequest` rejects `PROT_GROWSDOWN` / `PROT_GROWSUP`;
- `MsyncRequest` rejects unknown flags and the conflicting `MS_ASYNC | MS_SYNC` combination, and converts the
  raw `MS_*` bitmask into `memspace::MsyncPolicy`;
- `mlockall` validates its `MCL_*` word before accepting the request: an empty word, a bit outside
  `MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT`, and the bare `MCL_ONFAULT` are `InvalidInput`, matching
  `SYSCALL_DEFINE1(mlockall)`. The call still does no work because the kernel has no swap, so every page is
  already resident; only the input contract is enforced.

### file-backed mapping handoff

`posix-mm` does not construct file-backed `VmArea` internals directly. The file-backed VMA
and runtime assembly enter `mm/filemap` through `filemap::FileMmapRequest`:

```text
sys_mmap(file-backed)
  -> fd lookup
  -> FileMmapRequest
  -> filemap::{mmap_shared_file, mmap_private_file}
  -> VmArea + VmRuntimeRef
  -> MmSpace::map_runtime_vma()
```

This way the syscall adapter expresses only the Linux ABI, while the file-backed first-fault policy,
EOF handling, the private file COW source, and shared-mapping constraints stay in
`mm/filemap`.

### memfd_create

The regression program `uapps/memfd-test` calls `memfd_create` directly, creating an
object with a label containing `/`, and checks `ftruncate`, shared-mapping reads and writes, and content isolation between objects with the same name after formatting.
Before the fix, the syscall returned `EINVAL` for a slash, and the program exited non-zero. The in-kernel formatter
assertion only verifies a single backing-name component; it does not by itself prove that the syscall lets the name through or establish the mount topology.

The user-supplied name is a debugging label and may contain `/`. When creating the internal file name on the private tmpfs, `/` is
replaced with `:`, which keeps a single path component and avoids interpreting the label as a directory traversal. Each memfd uses its own
private mount, so labels that become identical after the conversion still create independent objects. The 249-byte limit and the flags validation
are unchanged; the `/proc/self/fd` name display may still differ from Linux.

```text
sys_memfd_create
  -> validate flags
  -> bounded user name load
  -> memfs::shmem::create_memfd_file(allow_sealing)
       -> MemoryFs::new_with_name_and_flags("tmpfs", 0)
       -> private tmpfs mount
       -> regular file under private root
       -> regular-file inode
       -> inode-owned kvfs::AddressSpace
       -> inode-scoped ShmemObjectState
       -> opened VfsFile
  -> current process fd table install
```

`memfd_create` creates an fd-only, tmpfs/shmem-style anonymous file object. The object content is provided by the
inode-owned `kvfs::AddressSpace` of the regular-file inode. `posix-mm`
handles only the syscall ABI, name validation, and fd installation; creation of the private tmpfs file and the shmem
policy state are owned by the `memfs::shmem` factory, and the shmem object itself converts into an
opened `VfsFile`. The name is only a debugging label and does not enter the global path namespace.

`MFD_ALLOW_SEALING` controls the initial seal state: when it is not set, the object carries
`F_SEAL_SEAL`; when it is set, the object starts from an empty seal set.

## Calling Constraints / Execution Context

- All syscalls run in process context.
- Sleeping and memory allocation are allowed.
- They depend on the current process's fd table and address-space state.
- They are not applicable in interrupt context.

## Key Semantics

### mmap flag validation

`mmap` request parsing does not use `from_bits_truncate()`, to prevent unknown ABI bits
from silently entering the MM internals. The parse result falls into three categories:

1. unknown `MAP_*` bits: rejected.
2. Linux-known deferred flags: accepted on the ordinary mapping path as a compatible no-op policy.
3. deferred flags + `MAP_SHARED_VALIDATE`: return not supported.

Fixed-address mappings do not implicitly round down. `MAP_FIXED` and
`MAP_FIXED_NOREPLACE` must be given an address already aligned to the target page size.

### madvise(MADV_DONTNEED)

`madvise` requires the start address to be page-aligned and rejects overflow of `addr + len` (including overflow after
rounding up to a page boundary). For `MADV_DONTNEED`, the end address is extended up to a whole page and then handed to
`MmSpace::madvise_dontneed()`; the four performance hints
(`MADV_NORMAL`/`MADV_RANDOM`/`MADV_SEQUENTIAL`/`MADV_WILLNEED`) produce no
per-VMA action, but like `MADV_DONTNEED` they require the range to be fully mapped (the Linux
`madvise_walk_vmas` rule: if there is a hole, the whole call returns `ENOMEM`).

The currently supported mainline is the private anonymous world:

- pure `AnonymousPrivate`
- the anonymous result object of `FilePrivate`

The VMA-side runtime turns this advice into `invalidate_range(object_start, len)` on the private-anon object, and then
completes the object-driven PTE zap through `mm/vmobj` and `MmSpace`.

### msync

After ABI request validation, `msync` hands the non-empty range to the
existing `LiveAddressSpace::msync_range()` capability, keeping an active MM user:

- `addr` must be 4K-aligned;
- `len == 0` succeeds and does not enter MM;
- unknown flags and the conflicting `MS_ASYNC | MS_SYNC` combination return an error;
- `MS_ASYNC` keeps the modern Linux no-op semantics;
- `MS_SYNC` synchronizes only shared file mappings, through the VMA walk in `memspace`;
- `MS_INVALIDATE` returns not supported, because locked-page / invalidate semantics are not yet wired in.

Under the lock, MM converts the current VMA overlap into an overflow-checked half-open file byte range, keeps the current
runtime reference and releases the MM mutex, then calls the existing file's
`VfsFile::fsync_range(range, true)`. The filesystem completes the data writes, the waiting and the datasync
metadata/journal/device persistence it requires. `MsyncPolicy` stores only the MS_* writeback mode and
invalidate flags, and keeps no separate always-true datasync field. `posix-mm` does not directly access
the page cache, the filemap runtime or VMA internal state. After synchronization MM re-takes the lock and looks up the next VMA again,
and never holds an old mapping snapshot of the whole requested range; concurrent unmap/mmap is not blocked by file persistence.

### mremap relocation

`posix-mm` owns Linux `MREMAP_*` ABI policy but does not own relocation
backing semantics.

- source validation goes through `MmSpace::resolve_mremap_source()`;
- destination install, PTE transfer and source retirement share the
  `MmSpace::relocate_mapping()` transaction;
- destination PTE preparation failure rolls back the destination and leaves
  source PTEs and backing unchanged;
- successful source retirement drops metadata without ordinary
  `MmSpace::unmap()`, which would destroy moved private backing.

This preserves the ownership split between:

- VMA metadata;
- present PTE residency;
- private/file-private backing object lineage.

## Design Decisions

1. `posix-mm` owns only the Linux ABI translation, not the MM internal objects.
   Reason: raw flags, user pointers and the errno policy belong to the syscall boundary; VMA,
   page table, file runtime and object lifetime belong to the downstream MM/FS owners.

2. file-backed mmap is assembled through `mm/filemap`.
   Reason: the file-backed first fault, EOF, the private file COW source and the shared
   writable constraints are filemap runtime policy and should not be scattered across the syscall adapter.

3. `memfd_create` uses the `memfs::shmem` factory to create a private tmpfs mount +
   regular-file inode.
   Reason: this model provides fd-only object semantics and an inode-owned page cache identity,
   while reusing the existing VFS file/open lifecycle and giving the inode-scoped sealing semantics
   a single attachment point.

4. The `memfd` name is used only as a debugging label.
   Reason: Linux `memfd` is not a path-creation interface, and a user-supplied name must not pollute the global VFS
   namespace. Linux debugging labels may contain `/`; the formatter replaces `/` with `:`,
   then adds the `memfd:` prefix, converting it into a single private backing component.

5. `MmapRequest` stores the typed policy.
   Reason: the downstream `MmSpace` / `filemap` should no longer read raw syscall flags.

6. `mremap` move-style source retirement must use metadata-only removal.
   Reason: relocation changes virtual placement and present PTE residency, but it
   must not re-run ordinary private backing teardown on the old source role.

## Known Limitations

- `memfd_create` supports `MFD_CLOEXEC` and the `MFD_ALLOW_SEALING` initial seal
  policy; `F_ADD_SEALS` / `F_GET_SEALS` are handled by `posix-fs`, and the hugetlb extension is not wired in.
- The `memfd` name display is not guaranteed to be exactly identical to Linux `/proc/self/fd`.
- `madvise` supports `MADV_DONTNEED` and four performance hints
  (`MADV_NORMAL`/`MADV_RANDOM`/`MADV_SEQUENTIAL`/`MADV_WILLNEED`, treated as
  no-ops but still requiring the range to be fully mapped, with an unmapped range returning `ENOMEM`); only an unknown advice returns
  `InvalidInput`.
- `msync` supports shared file dirty folio writeback, but the full semantics of `MS_INVALIDATE` are not implemented.
- file-backed writable `MAP_SHARED` is supported by the `filemap` runtime through write-fault
  dirty tracking; memfd/shmem seals are enforced at the `filemap` boundary.
- `MappingPermissions.maximum` is now separated from the current permission; anonymous mappings and
  private file mappings keep a may-permission that `mprotect()` can raise, while shared file
  mappings are narrowed according to the file's open permissions.

When `brk` fails to fill in a pre-mapped range or to shrink a range, it returns the original break, and publishes the new heap top only after the mapping operation
succeeds, consistent with the existing ABI for a failed extension beyond the initial range.

## Kernel unit-test layout

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/lib.rs`: mounted by `src/lib.rs` as `tests`.
- `ut-tests/memfd.rs`: mounted by `src/memfd.rs` as `tests`.
- `ut-tests/mmap.rs`: mounted by `src/mmap.rs` as `tests`.

Shared file mapping creation and permission changes run under MM serialization.
Write faults may return a retry outcome to leave that critical section before a
lifecycle transition needs to revoke the mapping's PTEs.

`mremap` inherits the existing VMA permissions and runs under MM serialization.
File expansion and migration update registrations through the runtime open
callback rather than new-map admission. Fixed replacement removes the target,
trims a shrinking source, then resolves the remaining source and prepares its
move. These removals remain effective if a later preparation fails. The syscall
has no mapping-admission retry around this destructive sequence.

## Address-space resource admission

`mmap`, growing `brk`, and growing or `MREMAP_DONTUNMAP` remaps snapshot the
current process's `RLIMIT_AS` and check the projected VMA footprint under the
same MM lock as the mapping mutation. `MAP_FIXED` credits mapped destination
bytes before replacing them. A growing `mremap` checks its growth before fixed
destination removal; fixed `MREMAP_DONTUNMAP` removes the destination before
checking the retained source's additional charge, as in Linux `mremap_to`.
Shrinking mappings remains available after a limit is lowered.
Denied `mmap` and `mremap` return `ENOMEM`; denied `brk` returns the unchanged
break, matching the Linux syscall ABI.

The existing VMA tree supplies accounting for finite limits. Unlimited limits
bypass the accounting walk. The syscall layer adds no counters or policy fields
to the MM object and does not enforce an address-space limit at page-fault time.
