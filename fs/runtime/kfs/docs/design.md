# kfs — 设计文档

## 定位

`kfs` 是 X-Kernel 的高层文件系统运行时。它把底层 `kvfs` 节点、
挂载上下文和文件描述符对象组合成内核其它子系统可直接使用的文件接口。

内存相关路径中，`kfs` 负责把 regular-file inode 接入
`VfsInode::i_mapping -> kvfs::AddressSpace -> pagecache::Mapping`。它不拥有
VMA tree、页表、file-backed mmap runtime、匿名页对象，也不再作为路径解析
公共入口。

## 范围

- `src/highlevel/file.rs`
- `src/highlevel/mapping.rs`
- `src/highlevel/fs.rs`

shmem/tmpfs anonymous file factory lives in `fs/filesystems/memfs/src/shmem.rs`.

## 架构

```text
kvfs Location / VfsInode
  -> i_mapping: kvfs::AddressSpace
       -> page_cache: pagecache::Mapping
       -> a_ops: AddressSpaceOperations
  -> kvfs::VfsFile
  -> kfs::File runtime wiring

memfs::shmem factory
  -> private memfs::MemoryFs("tmpfs")
  -> private root mount
  -> create regular-file Location
  -> Location regular-file inode
  -> ShmemObjectState attached to inode_data
  -> kfs::OpenOptions::open_loc()
  -> kfs::File

kfs::File
  -> write/append/set_len policy checks
  -> shared writable mmap/write-fault policy checks for filemap
```

Linux 的对象关系是 `struct inode::i_mapping` 指向 `struct address_space`。
X-Kernel 对应关系是 `VfsInode::i_mapping` 持有 `kvfs::AddressSpace`，
`AddressSpace` 内部持有唯一的 `pagecache::Mapping`。KFS 不再提供第二层 cache
句柄；普通 open-file 实例只持有 `Location`/`VfsFile`，需要页缓存时通过 inode
的 address-space 取得同一个 `pagecache::Mapping`。

`memfs::shmem` 创建 fd-only anonymous tmpfs-style regular file，并把 seal
policy state 挂到 inode data 上。KFS 只负责把该 `Location` open 成 `File`，
以及在 write/resize/mmap policy 边界调用 shmem helper。实际页内容仍由 inode
address-space 的 `pagecache::MappingKind::InMemory` 管理。

## 调用约束 / 执行上下文

- 高层文件 API 运行在普通内核任务或进程上下文。
- 文件创建、page cache instantiate、read/write/sync 路径可能分配内存并阻塞。
- 这些接口不适用于中断上下文。
- shmem state 通过 inode data 绑定到 VFS inode lifetime。

## 算法流程

### memfd-style anonymous file

```text
posix-mm sys_memfd_create
  -> memfs::shmem::create_memfd_file(allow_sealing)
  -> MemoryFs::new_with_name_and_flags("tmpfs", 0)
  -> private root mount
  -> open regular file under that root
  -> attach ShmemObjectState { kind: Memfd, seals }
  -> kfs::OpenOptions::open_loc()
  -> VfsInode::i_mapping
  -> AddressSpace page-cache on first cached I/O or mmap use
```

For memfd objects, `allow_sealing == false` initializes `F_SEAL_SEAL`.
`allow_sealing == true` initializes an empty seal set.

### shmem seal policy checks

```text
File::write_at / append
  -> shmem::check_write_allowed(location)
  -> page-cache or direct file write

File::set_len
  -> shmem::check_resize_allowed(location, old_len, new_len)
  -> VFS set_len
  -> AddressSpace page-cache Mapping::set_len()

filemap shared mmap / mprotect(PROT_WRITE)
  -> shmem::check_shared_writable_mapping_allowed(location)
  -> allow only when neither F_SEAL_WRITE nor F_SEAL_FUTURE_WRITE is present

filemap shared write fault
  -> shmem::check_shared_write_fault_allowed(location)
  -> allow only when F_SEAL_WRITE is absent
```

`F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` block ordinary write and append
operations. `F_SEAL_GROW` blocks file growth, and `F_SEAL_SHRINK` blocks file
shrink. Ordinary memfs/tmpfs files without `ShmemObjectState` bypass these
checks and keep regular file semantics.

For shared mappings, `F_SEAL_WRITE` blocks new writable `MAP_SHARED` mappings,
`mprotect(PROT_WRITE)` upgrades, and write faults. `F_SEAL_FUTURE_WRITE`
blocks new writable shared mappings and protection upgrades while allowing
existing writable shared mappings to keep faulting.

`ShmemObjectState` tracks active shared pages and active writable shared pages
registered by `mm/filemap`. Adding `F_SEAL_WRITE` is rejected while any writable
shared mapping count is non-zero, matching Linux memfd sealing semantics without
making `pagecache::Mapping` interpret seal policy.

### inode-owned mapping

```text
File::read/write/mmap/sync
  -> VfsFile location/inode/address-space view
  -> page-cache or direct regular-file path
  -> Location::address_space()
  -> AddressSpace::get_or_insert_page_cache()
  -> pagecache::Mapping
```

For in-memory filesystems such as `tmpfs` and `memfs`, the node advertises
always-cache semantics through VFS node flags. The inode address-space uses that
to create an `InMemory` page-cache mapping; address-space writeback is a no-op and
cached folios are the file content source. This avoids filesystem-name dispatch
and keeps the decision at the inode/address-space boundary.

## 并发模型

- `VfsInode` owns a stable `AddressSpace` from inode construction.
- `AddressSpace` serializes creation of the single page-cache `Mapping`; the
  mapping protects resident folios and RAII evict listeners with sleepable
  `Mutex` values.
- inode teardown final-invalidates an instantiated page-cache mapping; it does
  not call file-node `sync()` for address spaces that never owned cached pages.
- `File` serializes position-based append through its open-file position lock;
  offset writes operate directly against the inode-owned mapping.
- Shmem seals are stored under a sleepable `Mutex` because seal management and
  file operation enforcement run in blocking task context.

## 设计决策

1. `VfsInode::i_mapping` owns file/shmem cache identity.
   原因：Linux keeps file cache identity in `inode->i_mapping`; KFS must not
   create a second cache identity in an open-file facade.

2. `memfs::shmem` owns current shmem policy state and anonymous tmpfs factory.
   原因：Linux shmem/memfd policy belongs to the file/inode boundary, while
   cached data and object identity are generic page-cache responsibilities.

3. `pagecache::Mapping` remains seal-unaware.
   原因：regular file, tmpfs, memfd and SysV shm should share the same cached
   content abstraction; seal enforcement is file operation and mmap policy.

4. `memfd_create` enters through `memfs::shmem::create_memfd_file()`.
   原因：POSIX syscall code should not construct private tmpfs files directly.
   The filesystem-specific factory owns tmpfs/shmem inode construction; KFS
   only opens the returned regular-file location.

5. write/resize seal checks run in KFS file operations.
   原因：write, append and truncate-style resize are file operation semantics.
   Keeping the checks at this layer prevents `posix-mm`, `pagecache` or
   individual callers from duplicating shmem policy.

6. Shared writable mmap seal checks are exposed by KFS file objects and executed by
   `mm/filemap`.
   原因：the seal state is inode-scoped KFS policy, but writable shared mapping
   creation, protection upgrade and dirty-tracking write faults are MM runtime
   events.

7. SysV shm enters through `memfs::shmem::create_kernel_file()`.
   原因：SysV IPC owns keys, ids and attach lifetime, while segment contents
   should use the same tmpfs/shmem file object model as memfd and `/dev/shm`.

## Drop / 资源释放

Anonymous shmem files are regular VFS file objects on a private tmpfs-style
mount. When the fd, file, location and inode references disappear, the inode
data attachments, `ShmemObjectState`, `AddressSpace` and page-cache mapping are
released by normal reference counting.
