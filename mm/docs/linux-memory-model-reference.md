# Linux-Aligned Memory Model Reference

## 目标

本文用 Linux MM 的分层模型解释当前 X-Kernel MM 组件边界。

本文只描述当前架构与 Linux MM 模型的对应关系。它回答：

1. 当前 X-Kernel 的每个 MM 组件对应 Linux 哪类职责？
2. 哪些 Linux 用户可见语义已经由当前组件承载？
3. 哪些 Linux 语义在当前代码中明确未实现或不完整？
4. 代码 review 时应如何判断职责是否放错层？

## Linux 分层模型

Linux 用户态虚拟内存核心可以拆成四层：

1. ABI / policy layer
2. address-space / VMA layer
3. backing-object / rmap layer
4. page-table / architecture execution layer

X-Kernel 当前也按这四层组织，只是用更细的 Rust crate 表达 ownership。

```text
Linux syscall ABI
  -> mm_struct / vm_area_struct
  -> address_space or anon_vma-backed object
  -> PTE install / TLB effects
```

X-Kernel 当前对应：

```text
posix/mm
  -> memspace::{MmSpace, VmArea}
  -> kvfs::AddressSpace / anon::* / vmobj::MappingView
  -> page_table::PageTable
```

`filemap` 位于 `posix/mm`、VFS file source、`memspace`、`pagecache` 和 `anon`
之间。它是 file-backed mmap adapter，不是单独的内容对象层。

## Layer 1: ABI / Policy

Linux 代码落点：

- `mm/mmap.c`
- `mm/mprotect.c`
- `mm/mremap.c`
- `mm/memfd.c`
- `mm/mincore.c`
- `mm/mlock.c`

X-Kernel 当前落点：

- `posix/mm/src/mmap.rs`
- `posix/mm/src/brk.rs`
- `posix/mm/src/memfd.rs`
- `posix/mm/src/mincore.rs`

当前职责：

- syscall 参数解析；
- Linux flag/prot/policy 判断；
- errno 映射；
- fixed/hint/anonymous/file-backed 分派；
- `brk` heap policy；
- `memfd` file object 创建入口。

边界：

- 不拥有 VMA tree。
- 不安装 PTE。
- 不持有 pagecache 或 anon page state。
- 不访问 file-backed runtime 内部实现。

## Layer 2: Address Space / VMA

Linux 代码落点：

- `include/linux/mm_types.h`
- `include/linux/mm.h`
- `mm/mmap.c`
- `mm/memory.c`

Linux 核心对象：

- `struct mm_struct`
- `struct vm_area_struct`
- `struct vm_operations_struct`

X-Kernel 当前落点：

- `mm/memspace/src/aspace.rs`
- `mm/memspace/src/vma.rs`
- `mm/memspace/src/fault.rs`
- `mm/memspace/src/backend/*`

X-Kernel 核心对象：

- `MmSpace`
- `VmArea`
- `VmAreaSet`
- `VmRuntimeOps`
- `VmRuntimeRef`
- `VmBackingInfo`
- `FaultInput`
- `FaultOutcome`

当前职责：

- 地址空间 owner；
- VMA 查找、插入、拆分、合并、删除；
- `mprotect` / `munmap` / `mremap` 类 VMA 变形；
- page fault 的 VMA 级分发；
- fork clone / runtime relocate；
- consume object-side invalidate 并执行 PTE zap。

边界：

- `VmArea` 可保存 file metadata、offset、backing kind，但不拥有 file content。
- `MmSpace` 可调用 runtime，但不成为 file/anon object owner。
- page fault 语义必须先通过 VMA 权限检查。

## Layer 3: Backing Object / Rmap

Linux 这一层分成 file-backed 和 anonymous 两支。

### File-backed branch

Linux 代码落点：

- `include/linux/fs.h`
- `mm/filemap.c`
- `mm/truncate.c`
- `mm/shmem.c`
- filesystem-specific file/inode code

Linux 核心对象：

- `struct file`
- `struct inode`
- `struct address_space`
- `file->f_mapping`
- `inode->i_mapping`
- `i_mmap`

X-Kernel 当前落点：

- `fs/kvfs`
- `fs/boot`
- `fs/filesystems/memfs`
- `mm/pagecache`
- `mm/filemap`
- `mm/vmobj`

当前对象关系：

```text
File
  -> inode location
  -> VfsInode { i_size, i_mapping }
       -> AddressSpace { host, a_ops, object_id, MappingView }
            -> private pagecache::PageCache folio storage
       -> object invalidate work

filemap
  -> builds file-backed VmArea + VmRuntimeRef
  -> registers MappingView
  -> owns VfsFile reference, not the underlying PageCache
```

当前语义：

- `kvfs::AddressSpace` 是 file/shmem cached content owner。
- `filemap::mmap_shared_file()` 创建 read-only shared file runtime。
- `filemap::mmap_private_file()` 创建 private file runtime。
- file-private initial bytes come from `kvfs::AddressSpace`。
- file truncate/resize invalidation starts from object side and reaches
  `MmSpace` through `vmobj` requests。

当前限制：

- writable regular-file `MAP_SHARED` is rejected。
- dirty/writeback and `msync` semantics are not complete。
- `page_mkwrite` equivalent is not implemented。

### Anonymous branch

Linux 代码落点：

- `include/linux/rmap.h`
- `mm/rmap.c`
- `mm/memory.c`
- `mm/huge_memory.c`

Linux 核心对象：

- `anon_vma`
- private anonymous pages
- anonymous reverse mapping
- COW lineage

X-Kernel 当前落点：

- `mm/anon/src/private.rs`
- `mm/anon/src/shared.rs`
- `mm/memspace/src/backend/private.rs`
- `mm/memspace/src/backend/shared.rs`

当前对象：

- `AnonPrivateObject`
- `AnonSharedObject`
- `AnonObjectId`
- `AnonLineageId`

当前语义：

- anonymous private/shared mappings have stable object identities。
- private anonymous pages are owned by `AnonPrivateObject`。
- fork/COW lineage is represented by anon object state。
- file-private write result pages use the same anon-private ownership model。

边界：

- `anon` 不读取 file source。
- `anon` 不拥有 VMA tree。
- `anon` 只通过 runtime/memspace path 影响 page table。

### Object-neutral language

Linux 里 file rmap 与 anon rmap 是不同实现，但概念上都需要 object 到 VMA 的
关系。

X-Kernel 当前用 `mm/vmobj` 提供公共语言：

- `VmObjectId`
- `MappingView`
- `MappingViewId`
- `MappingViewSpec`
- `ObjectInvalidateWork`
- `ObjectInvalidateRequest`

用途：

- `kvfs::AddressSpace` 用它表达 file object -> VMA view。
- `anon` 用它表达 anonymous object -> VMA view。
- `memspace` 用它消费 object-side invalidation。

## Layer 4: Page Table / Architecture Execution

Linux 代码落点：

- `include/linux/pgtable.h`
- `mm/pgtable-generic.c`
- `arch/*/mm/*`
- `mm/memory.c`

X-Kernel 当前落点：

- `mm/page_table/src/*`
- architecture paging support under `arch/`

职责：

- PTE install；
- PTE unmap；
- permission protect；
- page-size-aware mapping；
- architecture mapping flags；
- TLB effect hooks where available。

边界：

- 不解释 syscall flags。
- 不选择 backing object。
- 不保存 VMA metadata。

## File-Backed `mmap` 当前模型

### `MAP_SHARED`

当前路径：

```text
sys_mmap
  -> posix/mm validates flags
  -> filemap::mmap_shared_file
  -> File::mmap callback
  -> FileSharedRuntime
  -> MmSpace::map_runtime_vma
  -> fault reads kvfs::AddressSpace
```

当前语义：

- read-only shared file mappings are supported。
- file source identity comes from inode-owned `kvfs::AddressSpace`。
- fault at or beyond file length returns object-level bad-address semantics。
- object resize/truncate can invalidate mapped PTEs through registered views。

当前限制：

- writable regular-file `MAP_SHARED` is rejected with
  `OperationNotSupported`。
- shared dirty/writeback and `msync` are not complete。

### `MAP_PRIVATE`

当前路径：

```text
sys_mmap
  -> posix/mm validates flags
  -> filemap::mmap_private_file
  -> File::mmap callback
  -> FilePrivateRuntime
  -> MmSpace::map_runtime_vma
  -> first fault reads kvfs::AddressSpace
  -> private page committed into AnonPrivateObject
```

当前语义：

- initial file contents come from `kvfs::AddressSpace`。
- final partial file page zero-tail behavior is handled in file-private runtime。
- ELF/image `memsz > filesz` zero-fill is represented by private runtime source
  bounds。
- post-write private pages are owned by `AnonPrivateObject`。
- fork/COW uses anon-private lineage rather than filemap-owned frame tables。

关键不变量：

- A file-private VMA has both file source identity and anon private identity。
- File object truncate does not make already-private COW pages become shared
  file pages again。

## Anonymous Mapping 当前模型

### Private anonymous

当前路径：

```text
sys_mmap MAP_PRIVATE|MAP_ANONYMOUS
  -> posix/mm
  -> MmSpace anonymous mapping path
  -> private anon runtime
  -> AnonPrivateObject
```

当前语义：

- heap、stack、BSS、anonymous private mapping share the private-anon ownership
  model。
- fork and write fault use COW-oriented private object state。
- `MADV_DONTNEED`-style page discard is represented at runtime/object boundary。

### Shared anonymous

当前语义：

- anonymous shared mappings use explicit shared object identity。
- shared object identity can participate in object/view language。

当前限制：

- full Linux shmem/tmpfs/memfd behavior is only partially represented by current
  `memfs` + `pagecache` path。

## Invalidation 当前模型

当前 object-driven invalidate path:

```text
VfsInode data lock
  -> kvfs::AddressSpace::truncate_setsize
  -> publish inode::i_size
  -> produces first object hit/work
  -> truncate private cached-folio storage
  -> produces second object hit/work
  -> MappingViewNotifier
  -> filemap::MmSpaceInvalidate
  -> memspace::InvalidateHandle
  -> MmSpace drains request
  -> runtime unmaps present PTEs
```

语义：

- object length/state change is initiated by object owner。
- VMA metadata remains unless syscall explicitly changes the VMA。
- stale present PTEs are removed by address-space-side invalidation。
- subsequent faults re-evaluate current object length/state。

当前限制：

- invalidate is currently queue/drain based through `MmSpace`。
- hole-punch/collapse-range policy is not implemented。

## Review Checklist

Use this checklist for current MM changes:

1. Does syscall ABI policy stay in `posix/mm`?
2. Does address-space shape stay in `mm/memspace`?
3. Does file content identity stay in `kvfs::AddressSpace`?
4. Does anonymous private state stay in `AnonPrivateObject`?
5. Does shared object/view/invalidate language use `vmobj`?
6. Does `filemap` remain an adapter rather than content owner?
7. Does page-table code avoid high-level Linux ABI policy?
8. Are unsupported Linux semantics rejected explicitly?
9. Are VMA split/merge/protect/unmap metadata invariants preserved?
10. Do tests cover both user-visible semantics and object ownership boundaries?

## Current Linux Compatibility Matrix

| Area | Current status |
| --- | --- |
| `mmap` anonymous private | Supported in current MM path |
| `mmap` file private | Supported for current file-backed runtime |
| `mmap` file shared read-only | Supported |
| writable file `MAP_SHARED` | Explicitly unsupported |
| `munmap` VMA split/remove | Supported by `memspace` VMA model |
| `mprotect` metadata-preserving protect | Supported by `memspace` VMA model |
| page fault VMA dispatch | Supported |
| file-private COW | Supported through `AnonPrivateObject` |
| file truncate invalidation | Supported for current resize/invalidate path |
| `msync` / shared dirty writeback | Incomplete |
| readahead/fault-around | Not implemented |
| reclaim/swap/memcg/NUMA/THP | Out of current core path |
