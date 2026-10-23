# filemap — Security and Reliability

## Trust Boundaries

`filemap` accepts inputs from:

- `posix/mm` syscall parsing;
- `process/kexec` image mapping setup;
- VFS `File::mmap()` callbacks;
- inode-owned `kvfs::AddressSpace`;
- `MmSpace` fault and clone/relocation paths.

It must not trust raw user input directly. Syscall argument validation belongs
to `posix/mm`.

## Core Invariants

- File content is owned by `kvfs::AddressSpace`, not by filemap runtimes.
- Private file post-write pages are owned by `AnonPrivateObject`.
- Runtime backing identity must match `VmArea.backing()`.
- File-backed object ids must come from the inode-owned
  `kvfs::AddressSpace::object_id()`.
- Object invalidation requests must be derived from registered mapping views,
  not from ad-hoc runtime-local object ids.
- Writable regular-file `MAP_SHARED` must use write-fault dirty tracking:
  initial shared file PTEs are installed without write permission, and the
  first write fault must hold the address-space invalidate lock shared and the
  target folio lock while completing `FileMappingOperations::page_mkwrite` preparation,
  marking the inode-owned folio dirty, and making the PTE writable. Truncate
  and cache invalidation take the invalidate lock exclusively.
- `mprotect()` must not upgrade existing shared file PTEs directly to writable;
  VMA permissions may become writable, but the PTE remains write-protected
  until a write fault marks the folio dirty.
- For shmem/memfd files, shared writable mapping policy must be checked through
  KFS inode-scoped seal state: `F_SEAL_WRITE` and `F_SEAL_FUTURE_WRITE` reject
  new writable shared mappings and `mprotect(PROT_WRITE)` upgrades, while
  `F_SEAL_WRITE` also rejects shared write faults.
- `FileSharedRuntime::msync` must sync only the inode-owned source mapping; it
  must not touch private anonymous COW state.

## Failure Handling

- Permission elevation above file flags returns permission errors.
- File-backed faults past EOF return bad-address to the fault layer, which maps
  the condition to the bus-error class.
- Private unmapped faults check the current inode size before reusing retained
  COW state, so truncate's second PTE invalidation cannot be refaulted past EOF.
- COW races return retry-class outcomes instead of corrupting object or PTE
  state.
- Object invalidation apply failures are requeued by `MmSpace` rather than
  silently dropped.
- Shmem seal policy failures are returned as permission errors before the VMA
  permission or PTE state is upgraded.

## Lifetime Rules

- `FileSharedRuntime` and `FilePrivateRuntime` live as VMA runtime references.
- Both file runtimes keep an `Arc<VfsFile>` and reach the source only through
  `VfsFile::mapping()`; neither runtime keeps a direct page-cache reference.
- `SharedFileSourceAdapter` keeps an `AddressSpaceViewGuard`; dropping the
  runtime unregisters the object view. The guard weakly references the owning
  `AddressSpace` and is not a second content owner.
- `FilePrivateRuntime` owns an `AnonPrivateObject` for private result pages and
  keeps the Linux-like `vm_file` lifetime through `Arc<VfsFile>`; file content
  remains owned by the inode address space.
- `MmSpace` owns VMA metadata and page-table mutation sequencing.

## Locking Rules

- Address-space shape and page-table operations are serialized by the `MmSpace`
  lock and page-table mutation guards.
- `VfsInode` owns the only visible file size and serializes write/truncate with
  its data lock. Shared write faults hold that same lock across EOF recheck,
  filesystem mapping preparation, dirtying, and PTE publication.
  `kvfs::AddressSpace` protects registered views and delegates only folio
  storage to its private cache component.
- Individual folios protect their bytes with per-folio locking.
- Private anon publication and fork/COW state use `AnonPrivateObject`
  prepare/commit contracts.

## Known Limitations

- `MS_INVALIDATE`, exact range fsync, and locked-page `EBUSY`
  semantics are not implemented.
- Linux-style refusal to add `F_SEAL_WRITE` while writable shared mappings are
  already active is implemented through KFS shmem writable-shared-page
  accounting and filemap runtime registration.
- Advanced Linux behavior such as DAX, THP, readahead, reclaim, swap, memcg,
  and NUMA is out of scope.

## 文件缓存引用与匿名页隔离

- 文件 pin 永不授予用户写权限，mprotect 升级也必须再次缺页并执行 COW。
- 页缓存分配寿命由 filemap 持有的 pin 保证；匿名帧释放器不接收文件页。
- 失效完成回调仅在对应 PTE 删除及 TLB 完成后运行。失败或排队请求保留
  尚未完成区间的 pin；已完成前缀可立即回收，无需等待未来缺页。
- 局部失效按运行时对象偏移仅摘除对应引用；fork 的子映射拥有独立索引，
  父映射清理不会撤销子映射的寿命保证。
- COW 的未初始化目标仅暴露为 MaybeUninit 切片；整页初始化在发布前完成，
  PTE compare/replace 失败不改变原映射，也不发布匿名槽。
- COW 只使用该映射保留的页对象，并在失效读锁内复核 EOF；
  不重新取同索引缓存页替代旧映射的身份。
- `file_pages` 中的每个 token 与一个当前映射的只读 PTE 成对存在。生产清 PTE
  的路径（失效、`mprotect(PROT_NONE)`、`munmap`、`MADV_DONTNEED`）都在 TLB
  完成后由 runtime 自身释放或调用 `on_invalidate_complete` 删除对应 token；
  稀疏 unmap 的前缀失败只保留仍存在的 PTE。因此只读缺页发现同偏移 token 时返回
  `AlreadyExists` 是内部所有权契约检测，而不是用户可达的恢复路径。
- 保护变更按 runtime 实际返回的 PTE flags 判断是否清 PTE；例如裸 WRITE
  请求经 COW 去掉 WRITE 后变为空，此时须先完成 TLB 清理，再释放 token。
  若后续 VMA 的保护变更失败，已清除前缀也必须已经通知 runtime 释放 token。
- mremap 准备失败后源 PTE 与文件/匿名页所有权均保持原样；目标 PTE 先撤销并
  完成 TLB 处理，再释放目标 VMA/runtime。目标回滚不得执行会摘除共享匿名槽的
  ordinary unmap，因此后续直接 READ、WRITE、mprotect 或 fork 不依赖缺页补偿。
- 共享文件的目标准备失败或旧源 metadata 退休时，单独注销 shared/writable
  登记；不能让已经消失的映射持续阻塞 shmem `F_SEAL_WRITE`。
- fork 的文件页安装失败不提交匿名子引用；匿名 materialization 失败时，
  其 helper 回滚匿名 PTE 和引用，外层撤销已经安装的文件 PTE 后完成 TLB 刷新。
