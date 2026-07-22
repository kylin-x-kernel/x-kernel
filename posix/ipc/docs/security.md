# posix-ipc — Security and Reliability

## Trust Boundaries

`posix-ipc` accepts user-controlled syscall arguments for message queues and
SysV shared memory. It must validate ids, command values, user pointers, sizes
and attach addresses before mutating global IPC state or process address
spaces.

SysV shm content is delegated to KFS shmem files and `mm/filemap`. `posix-ipc`
must not bypass those layers by owning page frames directly.

## Core Invariants

- `SHM_MANAGER` is the only owner of global key/shmid lookup state.
- `ShmInner` owns only IPC metadata plus an `Arc<kvfs::VfsFile>` backing object.
- Segment construction receives an explicit immutable credential snapshot;
  `shm_perm` creator/owner IDs use its effective UID/GID.
- SysV shm contents must flow through inode-owned `kvfs::AddressSpace`.
- `ShmInner.page_num` is derived from a page-aligned segment size.
- `shm_nattch` must match the number of process attach records in
  `ShmInner.va_range`.
- A segment marked `IPC_RMID` is removed from global lookup after the last
  detach.
- `shmat` must publish the attach record only after `MmSpace` successfully maps
  the VMA.

## Locking Rules

- Preferred lock order is `SHM_MANAGER -> ShmInner`.
- Do not hold `SHM_MANAGER` while mapping or unmapping a process address space.
- Do not hold `ShmInner` while calling into `filemap::mmap_shared_file()` or
  `MmSpace::map_runtime_vma()`.
- KFS shmem state, pagecache mappings and VMA state are protected by their own
  subsystem locks.

## Failure Handling

| Failure | Handling | Result |
|---|---|---|
| unknown shmid | return `InvalidInput` | syscall fails without side effects |
| zero-size segment | return `InvalidInput` | no IPC object is created |
| shmem file creation failure | propagate error | no shmid is inserted |
| address-space allocation failure | propagate error | no attach record is published |
| duplicate attach by same process | return `InvalidInput` | any just-created mapping is unmapped |
| detach of unknown address | return `InvalidInput` | no attach count change |
| kernel task constructs a test segment | caller supplies `initial_cred()` explicitly | no current-user-thread lookup or panic |

## Known Limitations

- Permission checks still use simplified credential handling.
- `SHM_HUGETLB` is not implemented.
- SysV shm does not expose full Linux namespace/procfs accounting.
- Adding richer attach semantics requires representing multiple attach ranges
  per `(pid, shmid)` instead of one range.

## Audit Checklist

- New SysV shm behavior should keep content ownership in KFS/pagecache/filemap.
- IPC metadata changes must update `shmid_ds` consistently.
- Segment constructors must not call `current_cred()` or store a duplicate credential field.
- New paths that hold `SHM_MANAGER` must not call into `MmSpace`.
- `IPC_RMID` cleanup must re-check attach count while protected by IPC locks.
