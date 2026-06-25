# posix-mm — 设计文档

## 定位

`posix-mm` 实现 POSIX/Linux 风格内存管理 syscall 入口，包括
`mmap`、`mremap`、`brk`、`madvise`、`msync`、`mincore` 和
`memfd_create`。它位于用户态 ABI 与内核 `mm`/`fs` 子系统之间，只负责
参数校验、typed request 构造、对象装配和错误码转换。

`posix-mm` 不拥有 VMA tree、页表、page cache、文件对象或 anonymous object。
这些状态分别由 `mm/memspace`、`mm/filemap`、`pagecache`、`memfs`、
`kfs` 和 `mm/anon` 维护。

## 范围

- `src/brk.rs`
- `src/mmap.rs`
- `src/memfd.rs`
- `src/mincore.rs`

## 架构

### syscall-to-MM request boundary

所有 raw syscall 参数先在 `posix-mm` 内冻结为 typed request，再进入
`mm/memspace` 或 `mm/filemap`：

```text
sys_mmap(raw args)
  -> MmapRequest::from_raw()
  -> MmapRequest { flags, map_type, page_size, permissions, offset }
  -> MmSpace::mmap_resolve_addr()
  -> filemap::mmap_*_file() or MmSpace::map()

sys_mprotect(raw args)
  -> MprotectRequest::from_raw()
  -> MmSpace::protect()

sys_munmap(raw args)
  -> MunmapRequest::from_raw()
  -> MmSpace::unmap()

sys_madvise(raw args)
  -> MadviseRequest::dontneed_from_raw()
  -> MmSpace::madvise_dontneed()

sys_msync(raw args)
  -> MsyncRequest::from_raw()
  -> MmSpace::msync_range()

sys_brk(raw addr)
  -> BrkRequest
  -> MmSpace::map() or MmSpace::unmap()
```

`mremap` move-style relocation has one extra contract boundary:

```text
sys_mremap(move)
  -> MmSpace::resolve_mremap_source()
  -> MmSpace::map_relocated_snapshot()
  -> MmSpace::move_pages()
  -> MmSpace::drop_mapping_metadata()
```

The syscall layer must not retire the old source range with ordinary
`MmSpace::unmap()` after a successful move. Ordinary unmap semantics may detach
private backing ownership, while relocation only retires the old virtual role.

这个边界承担 Linux-visible ABI policy：

- `MmapProt` / `MappingPermissions` 从 `PROT_*` 推导 current permission
  和 maximum permission；
- `MmapFlags` / `MapType` 从 `MAP_*` 推导 shared/private、fixed policy、
  anonymous/file-backed、populate 和 page-size policy；
- unknown `MAP_*` bits 在 syscall 边界返回 `InvalidInput`；
- 普通 `MAP_PRIVATE` / `MAP_SHARED` 路径允许部分 Linux-known deferred
  flags 作为兼容 no-op policy 通过 typed request；
- `MAP_SHARED_VALIDATE` 对这些 deferred flags 返回
  `OperationNotSupported`；
- `MAP_FIXED` / `MAP_FIXED_NOREPLACE` 要求传入地址满足目标页大小对齐；
- `munmap(addr, 0)` 返回 `InvalidInput`；
- `MprotectRequest` 拒绝 `PROT_GROWSDOWN` / `PROT_GROWSUP`；
- `MsyncRequest` 拒绝 unknown flags 和 `MS_ASYNC | MS_SYNC` 冲突组合，并把
  raw `MS_*` bitmask 转成 `memspace::MsyncPolicy`。

### file-backed mapping handoff

`posix-mm` 不直接构造 file-backed `VmArea` internals。file-backed VMA
和 runtime 装配通过 `filemap::FileMmapRequest` 进入 `mm/filemap`：

```text
sys_mmap(file-backed)
  -> fd lookup
  -> FileMmapRequest
  -> filemap::{mmap_shared_file, mmap_private_file}
  -> VmArea + VmRuntimeRef
  -> MmSpace::map()
```

这样 syscall adapter 只表达 Linux ABI，file-backed first-fault policy、
EOF 处理、private file COW source 和 shared mapping 约束留在
`mm/filemap`。

### memfd_create

```text
sys_memfd_create
  -> validate flags
  -> bounded user name load
  -> memfs::shmem::create_memfd_file(allow_sealing)
       -> MemoryFs::new_with_name_and_flags("tmpfs", 0)
       -> private tmpfs mount
       -> regular file under private root
       -> regular-file inode
       -> inode-owned Mapping
       -> inode-scoped ShmemObjectState
  -> kfs::OpenOptions::open_loc()
  -> current process fd table install
```

`memfd_create` 创建 fd-only 的 tmpfs/shmem 风格匿名文件对象。对象内容由
regular-file inode 的 inode-owned `pagecache::Mapping` 提供。`posix-mm`
只处理 syscall ABI、名字校验和 fd 安装；private tmpfs file 创建与 shmem
policy state 由 `memfs::shmem` factory 拥有，KFS 只负责打开返回的 regular-file
location。名字只作为调试标签，不进入全局路径命名空间。

`MFD_ALLOW_SEALING` 控制初始 seal state：未设置时对象带
`F_SEAL_SEAL`，设置时对象从空 seal set 开始。

## 调用约束 / 执行上下文

- 所有 syscall 运行在进程上下文。
- 允许睡眠和分配内存。
- 依赖当前进程的 fd table 与地址空间状态。
- 不适用于中断上下文。

## 关键语义

### mmap flag validation

`mmap` request parsing 不使用 `from_bits_truncate()`，避免未知 ABI bits
静默进入 MM internals。解析结果分三类：

1. unknown `MAP_*` bits：拒绝。
2. Linux-known deferred flags：普通 mapping 路径按兼容 no-op policy 接受。
3. deferred flags + `MAP_SHARED_VALIDATE`：返回不支持。

固定地址 mapping 不做隐式向下取整。`MAP_FIXED` 和
`MAP_FIXED_NOREPLACE` 必须传入已按目标页大小对齐的地址。

### madvise(MADV_DONTNEED)

`MADV_DONTNEED` 要求起始地址页对齐，并拒绝 `addr + len` 溢出。结束地址
向上扩到整页后交给 `MmSpace::madvise_dontneed()`。

当前支持的主线是 private anonymous world：

- pure `AnonymousPrivate`
- `FilePrivate` 的 anonymous result object

VMA-side runtime 把该 advice 转成 private-anon object 的
`invalidate_range(object_start, len)`，再经 `mm/vmobj` 和 `MmSpace`
完成 object-driven PTE zap。

### msync

`msync` 执行 ABI request validation 后，把非空范围交给
`MmSpace::msync_range()`：

- `addr` 必须 4K 对齐；
- `len == 0` 成功且不进入 MM；
- unknown flags 和 `MS_ASYNC | MS_SYNC` 冲突组合返回错误；
- `MS_ASYNC` 保持现代 Linux no-op 语义；
- `MS_SYNC` 通过 `memspace` 的 VMA walk 只同步 shared file mapping；
- `MS_INVALIDATE` 返回不支持，因为 locked-page / invalidate 语义尚未接入。

实际 dirty folio 写回由 shared file runtime 进入 VFS
`AddressSpaceOperations::writepages()` 完成。`posix-mm` 不直接访问 page
cache、filemap runtime 或 VMA 内部状态。

### mremap relocation

`posix-mm` owns Linux `MREMAP_*` ABI policy but does not own relocation
backing semantics.

- source validation goes through `MmSpace::resolve_mremap_source()`;
- destination install goes through `MmSpace::map_relocated_snapshot()`;
- present PTE transfer goes through `MmSpace::move_pages()`;
- old-source retirement after successful move goes through
  `MmSpace::drop_mapping_metadata()`, not ordinary `MmSpace::unmap()`.

This preserves the ownership split between:

- VMA metadata;
- present PTE residency;
- private/file-private backing object lineage.

## 设计决策

1. `posix-mm` 只拥有 Linux ABI 翻译，不拥有 MM 内部对象。
   原因：raw flags、用户指针和 errno policy 属于 syscall 边界；VMA、
   page table、file runtime 和 object lifetime 属于下游 MM/FS owner。

2. file-backed mmap 通过 `mm/filemap` 装配。
   原因：file-backed first fault、EOF、private file COW source 和 shared
   writable 约束是 filemap runtime policy，不应散落在 syscall adapter。

3. `memfd_create` 使用 `memfs::shmem` factory 创建 private tmpfs mount +
   regular-file inode。
   原因：该模型提供 fd-only 对象语义和 inode-owned page cache identity，
   同时复用现有 VFS file/open 生命周期，并让 inode-scoped sealing 语义有
   统一挂载点。

4. `memfd` 名字只作为调试标签。
   原因：Linux `memfd` 不是路径创建接口，用户名字不能污染全局 VFS
   namespace。包含 `/` 的名字按 Linux 语义拒绝。

5. `MmapRequest` 保存 typed policy。
   原因：下游 `MmSpace` / `filemap` 不应再读取 raw syscall flags。

6. `mremap` move-style source retirement must use metadata-only removal.
   原因：relocation changes virtual placement and present PTE residency, but it
   must not re-run ordinary private backing teardown on the old source role.

## 已知限制

- `memfd_create` 支持 `MFD_CLOEXEC` 和 `MFD_ALLOW_SEALING` 初始 seal
  policy；`F_ADD_SEALS` / `F_GET_SEALS` 由 `posix-fs` 处理，hugetlb 扩展未接入。
- `memfd` 名字展示不保证完全等同 Linux `/proc/self/fd`。
- `madvise` 支持 `MADV_DONTNEED`，其它 advice 返回 `InvalidInput`。
- `msync` 支持 shared file dirty folio writeback，但 `MS_INVALIDATE` 完整语义未实现。
- file-backed writable `MAP_SHARED` 由 `filemap` runtime 通过 write-fault
  dirty tracking 支持；memfd/shmem seals 在 `filemap` 边界执行。
- `MappingPermissions.maximum` 已与 current permission 分离；匿名映射和
  private file 映射保留可被 `mprotect()` 提升的 may-permission，shared file
  映射按文件打开权限收窄。
