# posix-mm — Security and Reliability Analysis

## Independent resource limits

Finite `RLIMIT_AS` and default system commitment admission are separate checks.
Unlimited `RLIMIT_AS` no longer permits a single private writable commitment
larger than managed RAM. The check uses total capacity, not volatile free pages,
and is performed before destructive fixed-address replacement. `MAP_NORESERVE`
bypasses default private commitment admission as on Linux, so physical OOM is
still possible and handled by the process runtime. A passed heuristic admission
is not a guarantee of future physical backing or complete OOM recovery.

## Trust Model

`posix-mm` trusts that:

- the architecture fault/syscall entry has already switched control flow into the kernel;
- the current `kprocess` process/thread state is available;
- the `kfs`, `memspace`, and `filemap` subsystems each maintain their own internal invariants.

It is itself responsible for:

- validating user-supplied arguments;
- translating raw syscall arguments into typed requests;
- correctly assembling anonymous file objects and fds;
- converting lower-level errors into syscall results.

## External Boundary / Attack Surface

- User-supplied arguments of `mmap` / `mremap` / `mprotect` / `munmap` /
  `madvise` / `msync` / `memfd_create`
- The memfd name and other syscall structures read through user pointers
- The current process fd table and address-space state

## unsafe Code Inventory

This crate contains no direct `unsafe` code blocks.

## Memory Safety Invariants

- Raw syscall flags must first be converted into a typed request and must not be passed directly into `MmSpace`
  or `filemap`.
- Unknown flags must explicitly return an error and must not be silently dropped through `from_bits_truncate()`.
- Linux-known deferred `mmap` flags must be handled within the `posix-mm` boundary:
  an ordinary mapping may pass under a compatible no-op policy, while `MAP_SHARED_VALIDATE` must return
  not-supported.
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE` addresses must be aligned to the target page size.
- successful `mremap` move must not retire the source with ordinary
  `MmSpace::unmap()`.
- `munmap(addr, 0)` must fail and must not pass through to `MmSpace` as a successful no-op.
- `PROT_GROWSDOWN` / `PROT_GROWSUP` must be rejected at the syscall boundary.
- The conflicting combination `MS_ASYNC | MS_SYNC` must be rejected; unknown `msync` flags must also be rejected.
- `MS_INVALIDATE` must return unsupported until locked VMA and invalidate semantics exist.
- `posix-mm` must not directly access pagecache or filemap runtime internals; `msync`
  must enter MM through the captured `LiveAddressSpace::msync_range()`, keeping an active user;
  the syscall does not hold the MM mutex while performing file persistence.
- `mremap` move must preserve the ownership split between VMA metadata,
  present PTE residency, and backing object pages.
- The file object returned by `memfd_create` must hold a valid `Location`.
- The anonymous file `Location` must be bound to a regular-file inode.
- The contents of an anonymous file inode must be provided by an inode-owned `kvfs::AddressSpace`.
- `memfd_create` must create the anonymous file through the `memfs::shmem` factory; the syscall layer must not
  duplicate private tmpfs file creation, reopen an anonymous location, or bypass the inode-scoped
  shmem policy state.
- `MFD_ALLOW_SEALING` must only affect the initial seal set: when unset, the initial set is
  `F_SEAL_SEAL`; when set, the initial seal set is empty.
- Reading the `memfd` name must have a Linux-compatible upper bound and must not scan user memory without bound.
- Anonymous file objects should not depend on global path uniqueness.

## Thread Safety

- fd installation is synchronized through the process resource table.
- The `memfs::shmem` state, the VFS inode, and the `AddressSpace` are each protected by their own locks.
- `posix-mm` does not additionally cache raw pointers or unsynchronized shared state across syscalls.

## Threat Analysis

| Threat | Mitigation |
|------|------|
| A user pollutes the global path space through the `memfd` name | The `memfs::shmem` factory creates the anonymous file on a private mount |
| A user constructs a name containing `/` to affect VFS path semantics | A `/` in the Linux debug label is accepted and replaced with `:` only in the internal file name on the private mount; the user label is never parsed as a path |
| A user provides an unterminated or overlong memfd name | `load_string_with_max_len()` limits it to 249 bytes excluding NUL |
| A user enters an undefined path through unknown `memfd_create` flags | `MemfdFlags::from_bits()` rejects unknown bits |
| Raw `mmap` flags silently leak into MM internals | `MmapRequest` explicitly validates unknown/deferred flags |
| An unaligned fixed mmap overwrites the wrong address | The fixed policy rejects unaligned addresses at the request parsing stage |
| `munmap(addr, 0)` is treated as a successful no-op | `MunmapRequest` rejects zero length at the syscall boundary |
| `MADV_DONTNEED` damages a file source object | Currently only pure private anon and file-private anon result objects are handled |
| `msync` writes private COW pages back to the original file | `MmSpace::msync_range()` lets only the shared file runtime perform writeback |
| `MS_INVALIDATE` is faked as success | Currently returns `OperationNotSupported` |
| An `mremap` move failure leaves a half-completed source mapping, or the old source teardown destroys private backing that the destination still needs | All move branches uniformly use the `relocate_mapping` transaction; on preparation failure the source state is preserved and the destination is revoked, and on success only the source metadata is retired |

## Failure Modes and Effects Analysis (FMEA)

| Failure | Condition | Handling | Impact |
|---|---|---|---|
| User name read fails | Invalid user pointer, illegal UTF-8, missing NUL, too long | Return an error | The syscall fails |
| anonymous file creation fails | Out of memory or VFS allocation failure | Return an error | The syscall fails |
| fd installation fails | The fd table is full | Return an error | The file object is released immediately |
| `mmap` flag unsupported | unknown `MAP_*`, `MAP_SHARED_VALIDATE` carrying a deferred flag, or file-backed hugetlb | Return an error | mmap fails and the address space is not modified |
| fixed mmap address not aligned | The `MAP_FIXED*` address does not satisfy the target page size | Return an error | mmap fails and the address space is not modified |
| `munmap` range illegal | Length is 0 or the downstream address space rejects it | Return an error | The address space is not modified |
| `mprotect` grow flag unsupported | `PROT_GROWSDOWN` or `PROT_GROWSUP` | Return an error | protect fails and the PTE/VMA are not modified |
| `MADV_DONTNEED` range illegal | The address is unmapped, not page-aligned, or `addr + len` overflows | Return an error | advice fails and the object is not modified |
| `msync` flag unsupported | unknown flags or the `MS_ASYNC | MS_SYNC` combination | Return an error | sync fails and the object is not modified |
| shared file sync fails | Data, metadata, journal or device persistence of the file-range datasync fails | Propagate the filesystem/file error cursor result, and unfinished data keeps its retry state | The syscall fails and the caller may retry |

## Fault Management

- Argument and user-memory errors are returned through `KResult`.
- Ordinary syscall failures are not handled through panic.
- If fd installation fails after the anonymous file is created, the object is reclaimed naturally by the `Arc` lifetime.

## Known Limitations

- `memfd_create` supports the `MFD_ALLOW_SEALING` initial seal policy;
  `F_ADD_SEALS` / `F_GET_SEALS` are handled by `posix-fs`, and shared writable mmap
  seal enforcement is handled by `mm/filemap`; the hugetlb extension is not wired in.
- The `memfd` name is for debugging only and is not guaranteed to match the Linux `/proc/self/fd` display exactly.
- `madvise` supports `MADV_DONTNEED` and four performance hints (no-ops, but they still require the range to be fully
  mapped and page-aligned, and an unmapped range returns `ENOMEM`); `InvalidInput` is returned only when the advice value is unknown,
  the address is unaligned, or the range overflows.
- The maximum permission and current permission of `mmap` are separate; the may-permission of a shared file
  mapping is bounded by the file open permissions.
- `MS_SYNC` now enters the shared file range datasync; the filesystem may conservatively sync more ranges.
  The `MS_INVALIDATE` and locked-page `EBUSY` semantics are not implemented.
- Dirty-PTE collection and write re-protection after writeback belong to Issue G; it cannot be claimed that this already covers
  consecutive mmap writes on the same writable PTE that no longer trigger a fault.
- File-backed writable `MAP_SHARED` is supported by the `filemap` runtime through write-fault
  dirty tracking.

## Audit Checklist

- Does `uapps/memfd-test` check, through real syscalls, slash-label creation, mapped read/write,
  and content isolation between objects with the same formatted name; formatter string assertions cannot replace this check, nor can they
  alone prove the private mount topology.

- When a new syscall flag is added, are the typed request validation and the documentation updated together?
- Does the syscall first construct a typed request and only then call `MmSpace` / `filemap`?
- Does any `from_bits_truncate()` still silently swallow unknown ABI flags?
- Does fixed-address mapping still reject unaligned addresses?
- Does `memfd_create` still not depend on `/tmp` or any other global path?
- Does the shmem factory always return a regular-file inode on the private mount?
- Does the `memfd` regular-file inode always reuse the inode-owned `AddressSpace`?
- Does `MADV_DONTNEED` hit only private-anon objects and complete the PTE zap through the object-driven
  invalidate main path?

`brk` does not ignore errors from re-establishing/shrinking the mapping; on failure it does not publish the requested new heap top,
so that the user never observes an incomplete address-space change.

Shared file mapping creation and permission changes run under MM serialization.
Write faults may return a retry outcome to leave that critical section before a
lifecycle transition needs to revoke the mapping's PTEs.

`mremap` inherits the existing VMA permissions and runs under MM serialization.
File expansion and migration update registrations through the runtime open
callback rather than new-map admission. Fixed replacement removes the target,
trims a shrinking source, then resolves the remaining source and prepares its
move. These removals remain effective if a later preparation fails. The syscall
has no mapping-admission retry around this destructive sequence.

## RLIMIT_AS

Address-space growth is admitted using a process-resource limit snapshot and the
VMA footprint while holding the MM lock. Concurrent mapping operations cannot
spend the same remaining budget. Fixed mmap accounts for holes and existing
destination mappings before removing them. Growing mremap checks its delta
before fixed replacement; fixed `MREMAP_DONTUNMAP` removes the destination
before checking the retained source's charge, so rejection can leave that
destination unmapped. A denied brk leaves the break unchanged. Unlimited limits
retain the normal fast path; finite-limit arithmetic is checked.
This limit governs virtual address-space admission, not physical memory
availability, and is not checked again when a page fault allocates backing.

## Locked-memory requests

`mlockall(2)` is accepted as a no-op because the kernel has no swap, so every
page is already resident and cannot be evicted. Being a no-op must not make the
entry point a permissive one: the `flags` word is validated against
`MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT` before the call reports success. An
empty word, an unknown bit, and the bare `MCL_ONFAULT` return `InvalidInput`,
exactly as `SYSCALL_DEFINE1(mlockall)` does. Accepting them would tell a caller
that it had locked memory it never asked the kernel to lock, and a hardening
path that treats success as proof of residency would draw the wrong conclusion.
`munlockall()` likewise reports success after doing nothing, which is the
correct answer for memory that was never pinned.
