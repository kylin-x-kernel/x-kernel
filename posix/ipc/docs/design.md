# posix-ipc — 设计文档

## 定位

`posix-ipc` 实现 Linux/POSIX IPC syscall 适配层。它负责用户可见的 IPC
标识符、权限元数据、生命周期规则和 syscall ABI，不拥有页缓存、VMA tree
或页表。

当前共享内存路径中：

- SysV shm keys、shmid、attach count、`IPC_RMID` 状态由 `posix/ipc` 管理。
- SysV shm 段内容由 `memfs::shmem::create_kernel_file()` 创建的 shmem file 承载。
- 实际 shared mapping 由 `mm/filemap` 创建 `VmArea + VmRuntimeRef`。

## 范围

- `src/msg.rs`
- `src/shm.rs`

## SysV shm 架构

```text
sys_shmget()
  -> snapshots current Arc<Cred>
  -> ShmManager allocates shmid / key mapping
       -> ShmInner::new(cred)
            -> memfs::shmem::create_kernel_file("SYSV...")
       -> shmem object into opened VfsFile
       -> set file length to page-aligned segment size

sys_shmat()
  -> lookup ShmInner
  -> choose process virtual address
  -> filemap::mmap_shared_file(FileMmapRequest)
  -> MmSpace::map_runtime_vma()
  -> record pid -> shmid -> vaddr attach

sys_shmdt()
  -> lookup shmid by process vaddr
  -> MmSpace::unmap()
  -> decrement attach count
  -> remove segment if IPC_RMID and attach count is zero
```

`ShmInner` stores IPC metadata and an `Arc<kvfs::VfsFile>`. It does not store
physical pages or an anonymous shared object. The file is a private
tmpfs/shmem-style regular inode whose content is owned by inode-scoped
`pagecache::Mapping`.

`ShmInner::new()` receives the operation's credential snapshot explicitly. It uses
the snapshot for the backing file and initializes `shm_perm.uid/gid/cuid/cgid`
from effective IDs, matching Linux `ipc_addid()`. The credential itself is not
stored as duplicate `ShmInner` state; the opened `VfsFile` owns its `f_cred`.

## 执行上下文

- SysV IPC syscall runs in process context.
- `shmget` may allocate IPC metadata, shmem inode state and page-cache owner
  metadata.
- Unit-test and kernel-task callers must explicitly choose `initial_cred()` or
  another credential instead of implicitly reading a nonexistent user thread.
- `shmat` may allocate VMA metadata and file-backed runtime state.
- These syscalls must not run from interrupt context.

## 并发模型

- `SHM_MANAGER` protects global key/shmid and process attach maps.
- Each `ShmInner` has its own sleepable `Mutex`.
- Lock order is `SHM_MANAGER -> ShmInner` when both are needed.
- `shmat` releases `ShmInner` before mapping through `MmSpace` and filemap, then
  reacquires it to publish the attach record.
- `shmdt` unmaps the process range before removing the attach record.

## 设计决策

1. SysV shm content is file-backed shmem, not anonymous shared pages.
   原因：Linux ordinary SysV shm is implemented on top of shmem/tmpfs files, and
   X-Kernel should share one content owner model across SysV shm, memfd and
   `/dev/shm`.

2. IPC metadata remains in `posix/ipc`.
   原因：keys, ids, `shmid_ds`, attach count and `IPC_RMID` are syscall-visible
   IPC semantics, not KFS or MM responsibilities.

3. The shmem file length is page-aligned.
   原因：SysV mappings are page-granular; keeping the backing file length aligned
   prevents file-backed fault handling from reporting EOF inside the mapped
   segment tail.

## 当前兼容边界

Supported:

- `shmget` segment creation and keyed lookup;
- `shmat` shared mapping through filemap/pagecache;
- `shmdt` detach and address-space unmap;
- `shmctl(IPC_STAT/IPC_SET/IPC_RMID)` metadata operations;
- process-exit cleanup through `clear_proc_shm()`;
- `/dev/shm` is mounted as a tmpfs instance during VFS bootstrap.

Not supported:

- `SHM_HUGETLB`;
- full credential/capability checks;
- complete Linux namespace and `/proc/sysvipc` reporting;
- multi-attach of the same shmid by one process at different addresses.
