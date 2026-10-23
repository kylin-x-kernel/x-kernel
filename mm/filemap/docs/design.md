# filemap — Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Position

`filemap` is the MM-side bridge for file-backed mappings.

It is the X-Kernel counterpart of the Linux file mmap bridge spread across
`file_operations::mmap()`, `generic_file_mmap()`, `filemap_fault()`, and the
VMA-side `vm_operations_struct` setup.

It is not a file content owner. File-backed content ownership belongs to the
inode address-space object: `VfsInode::i_mapping -> kvfs::AddressSpace`.
`filemap` receives an opened `kvfs::VfsFile` and borrows the Linux-style
`file->f_mapping` equivalent through `VfsFile::mapping()`.
It has no direct dependency on the `pagecache` crate and cannot retain the
underlying cache container as a second owner.

## Responsibilities

- Convert file-backed mmap requests into `(VmArea, VmRuntimeRef)`.
- Keep Linux `MAP_SHARED` and `MAP_PRIVATE` file mapping modes explicit.
- Call the VFS `File::mmap()` callback through an internal `MmapMapper`.
- Build file-backed VMA metadata, including file offset, inode id, and path.
- Implement shared file runtime fault handling.
- Implement shared file runtime `msync` dispatch into the inode-owned
  `kvfs::AddressSpace`.
- Enforce shmem/memfd shared writable mapping seal policy through KFS
  inode-scoped checks.
- Implement private file runtime first materialization, EOF checks, file prefix
  handling, and ELF `memsz > filesz` zero-fill semantics.
- Register file object views so `kvfs::AddressSpace` truncate/invalidate work
  can reach `MmSpace`.

## Non-Responsibilities

- It does not own page-cache folios.
- It does not own inode lifecycle.
- It does not define the object/view/invalidate language.
- It does not own private anonymous page state.
- It does not own the VMA tree or page tables.
- It does not parse syscall ABI flags.

Those responsibilities belong to:

- `kvfs`: inode `i_mapping`, `AddressSpace`, and filesystem address-space ops.
- `pagecache`: cache storage algorithms privately owned by `kvfs::AddressSpace`.
- `kvfs`: VFS/open-file facade used by current file descriptor paths.
- `vmobj`: object/view/invalidate language.
- `anon`: private/shared anonymous object ownership.
- `memspace`: VMA tree, page-table mutation, fault dispatch, and invalidate
  consumption.
- `posix/mm`: Linux syscall ABI parsing and errno policy.

## Architecture

```text
posix/mm or process/kexec
  -> filemap public constructor
      -> VFS File::mmap() callback bridge
      -> build FileSharedRuntime or FilePrivateRuntime
      -> build VmArea file metadata
  -> MmSpace::map_runtime_vma()

FileSharedRuntime
  -> SharedFileSourceAdapter
  -> Arc<kvfs::VfsFile>
  -> VfsFile::mapping()
  -> VfsInode::i_mapping / AddressSpace
  -> map shared folio into PTE

msync(MS_SYNC)
  -> MmSpace::msync_range()
  -> FileSharedRuntime::msync()
  -> VfsInode::i_mapping / AddressSpace::writepages_range()
  -> AddressSpaceOperations::writepages()
  -> AddressSpace-owned page-cache writeback

FilePrivateRuntime
  -> Arc<kvfs::VfsFile>
  -> VfsFile::mapping() for first materialization
  -> AnonPrivateObject for post-write private state and fork COW

kvfs::AddressSpace::truncate_setsize()
  -> publish inode::i_size
  -> MappingViewNotifier (first unmap)
  -> internal cached-folio truncate
  -> MappingViewNotifier (second unmap)
  -> MmSpaceInvalidate
  -> InvalidateHandle
  -> MmSpace drain and PTE zap
```

## Modules

- `mmap.rs`
  - Public file mmap bridge functions.
  - Internal VFS `MmapMapper` bridge.
  - Shared/private file mapping mode selection.
- `runtime.rs`
  - Common file-backed VMA metadata construction.
  - Source/view registration helpers.
  - `new_file_private_vma()` for executable/image private mappings.
- `shared.rs`
  - `FileSharedRuntime`, the `MAP_SHARED` file-backed `VmRuntimeOps`.
- `private.rs`
  - `FilePrivateRuntime`, the `MAP_PRIVATE` file-backed `VmRuntimeOps`.
  - First materialization from file source and private anonymous COW handoff.
- `invalidate.rs`
  - `MappingViewNotifier` bridge from object-side invalidation into
    `MmSpace`.

## Public API

The crate root intentionally exposes only:

- `FileMmapRequest`
- `mmap_shared_file()`
- `mmap_private_file()`
- `new_file_private_vma()`

Callers must not depend on runtime internals.

## Key Flows

### File-backed `mmap`

```text
sys_mmap
  -> posix/mm parses Linux flags
  -> filemap::mmap_shared_file() or mmap_private_file()
  -> File::mmap(MmapMapper)
  -> FileSharedRuntime or FilePrivateRuntime
  -> VmArea with FileMappingInfo
  -> MmSpace::map_runtime_vma()
```

### Shared file fault

1. `MmSpace` finds the `VmArea` and dispatches to its runtime.
2. `FileSharedRuntime` checks file permissions.
3. The runtime resolves the faulting page's file offset.
4. If the faulting page starts at or beyond current file length, the runtime
   returns object-level bad-address so `MmSpace` can report a bus-error class
   fault.
5. Otherwise the runtime materializes a folio through inode-owned
   `kvfs::AddressSpace` and maps the folio into the page table.
6. Shared writable mappings initially install the PTE without write permission.
   The first write fault enters `kvfs::VfsFile::page_mkwrite()`, which
   takes the address-space invalidate lock shared, locks the target folio,
   rechecks EOF, invokes the filesystem preparation callback, marks the folio
   dirty, and only then remaps the PTE writable.
7. `mprotect(PROT_WRITE)` updates VMA permissions but keeps existing shared
   file PTEs write-protected, so write-fault dirty tracking is preserved.
8. For shmem/memfd files, `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` reject new
   writable shared mappings and `mprotect(PROT_WRITE)` upgrades. `F_SEAL_WRITE`
   also rejects later shared write faults.

### Private file fault

1. `FilePrivateRuntime` computes the file source offset and VMA-local page
   prefix.
2. If this is a valid file-backed portion, it copies initial bytes through
   `VfsFile::mapping()` from the inode address space.
3. If this is an executable/image zero-fill tail (`memsz > filesz`), it keeps
   the page faultable and zero-filled.
4. Private page state is prepared and committed through `AnonPrivateObject`.
5. Later write faults use the shared private-anon COW helpers, not filemap-owned
   frame tables.
6. Every unmapped refault rechecks current inode `i_size` before consulting a
   retained private-anon page. This prevents the second truncate unmap from
   being followed by reuse of stale private object state beyond the new EOF.

### Truncate / invalidate

1. The set-length path takes the address-space invalidate lock exclusively,
   then `kvfs::AddressSpace::truncate_setsize()` publishes inode `i_size`.
2. `AddressSpace` emits mapped-view invalidation, truncates its private cache
   storage, then emits a second invalidation for a private COW race.
3. The registered view notifier calls `MmSpaceInvalidate`.
4. `MmSpaceInvalidate` submits an `ObjectInvalidateRequest` through
   `InvalidateHandle`.
5. `MmSpace` drains the request and asks the matching runtime to unmap present
   PTEs while preserving VMA metadata.

### `msync`

1. `posix/mm` validates raw `MS_*` flags and calls `MmSpace::msync_range()`.
2. `MmSpace` walks VMAs and dispatches shared file overlaps to the runtime.
3. `FileSharedRuntime` converts the VMA overlap to a file object byte range.
4. `AddressSpaceOperations::writepages()` writes back intersecting dirty folios
   through the inode-owned page cache.
5. Private file mappings are not synced because post-write pages are owned by
   `AnonPrivateObject`.

## Current Compatibility Boundary

Supported now:

- read-only file `MAP_SHARED`;
- writable regular-file `MAP_SHARED` with write-fault dirty tracking;
- filesystem `page_mkwrite` preparation for shared write faults;
- memfd/shmem `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` enforcement for new
  shared writable mappings and `mprotect(PROT_WRITE)`;
- memfd/shmem `F_SEAL_WRITE` refusal while writable shared mappings are active,
  using filemap runtime registration against KFS inode-scoped shmem state;
- file `MAP_PRIVATE` first fault;
- file-private COW and fork isolation through `AnonPrivateObject`;
- EOF page-start bus-error behavior;
- final file page zero-tail behavior;
- object-driven truncate invalidation for registered views.

Not supported by the current filemap runtime:

- readahead/fault-around;
- hole-punch/collapse-range;
- DAX, THP, reclaim, swap, memcg, NUMA.

## 只读文件页的映射所有权

对齐的完整 4 KiB 私有文件页可直接安装只读缓存页 PTE。未私有化页面的
`PinnedFolio` 在安装 PTE 后转为 `MappedFolio`，按运行时对象偏移保存在
filemap 的文件页表中；匿名对象
仅保存实际复制产生的匿名帧，不再包装文件源或比较借用槽的物理地址。
首次只读映射和文件页 COW 发布均在 AddressSpace 失效读锁下复核 EOF，
并完成 PTE 与相应所有权的发布。COW 使用 `MaybeUninit<u8>` 目标，页锁内
完成整页复制，再通过 `commit_after` 原子发布匿名帧；旧文件 pin 在 PTE/TLB
清理完成后移除。

COW 直接复制当前 PTE 对应的保留页对象，不重新加载相同文件索引，因此缓存
条目替换、回收或驻留配额耗尽不会给已有映射引入额外的读盘或配额申请。
fork 先安装文件页 PTE，再由匿名页 helper 一次提交匿名子对象引用；任一步
失败均撤销本轮子 PTE，避免遗留匿名逻辑引用。mremap 经 memspace 的统一
事务先准备全部目标 PTE，再清除源 PTE；失败时先撤销本轮目标 PTE 并完成
TLB 处理，再撤销目标 VMA 与引用。源映射仍保持完整的 PTE 与页所有权，
不依赖后续 READ/WRITE 缺页补偿，也不产生同一偏移的文件/匿名双重所有者。
共享文件 runtime 的 shared/writable 区间登记在 metadata-only retirement
回调中撤销：失败目标与成功迁移的旧源均须撤销登记，防止泄漏后错误阻塞
`F_SEAL_WRITE`。此回调不执行普通 unmap，也不摘除匿名 backing。

truncate 的对象失效经 memspace 清除匹配 VMA 的 PTE、完成 TLB finalization，
随后调用 `on_invalidate_complete`；filemap 立即释放该区间 pin，不等待后续
缺页。MM 忙时失效请求排队，pin 持续保留到排队请求实际完成。munmap 和
MADV_DONTNEED 同样先清 PTE 并完成 TLB 刷新，再释放已完成页的 pin；部分失败
时未处理页仍保留 pin。分裂 VMA 共用原对象偏移
索引，局部清理不影响剩余区间；fork 使用独立索引并共享 folio 引用；mremap
保留对象偏移索引，将虚拟地址转换基址迁到新地址。

`mprotect(PROT_NONE)` 清除现有 PTE 后也会在 TLB finalization 完成时通知
filemap 释放对应 pin；恢复读权限后，缺页可重新安装只读缓存页。
