# X-Kernel Memory Management Current Architecture

## 目标

本文描述当前 X-Kernel 内存管理主平面的组件分层、职责边界和
Linux 语义对应关系。

判断标准是当前代码中的 crate、公开接口、数据结构和调用关系。

## 分层原则

当前 MM 主平面按职责分成七层：

1. `posix/mm`：Linux 用户态内存 ABI 与 errno policy。
2. `mm/memspace`：进程地址空间、VMA 集合、fault dispatch、VMA 变形。
3. `mm/filemap`：file-backed mmap adapter 与 file-backed runtime。
4. `mm/pagecache`：inode-owned file/shmem cached object。
5. `mm/anon`：anonymous object、private page state、fork/COW lineage。
6. `mm/vmobj`：object id、mapping view、object-side invalidate 语言。
7. `mm/page_table`：页表执行层。

核心约束：

- VMA 描述映射实例，不拥有页内容。
- file-backed 内容由 inode-owned `pagecache::Mapping` 拥有。
- anonymous private/shared 内容由 `mm/anon` 拥有。
- `filemap` 是 file-backed mmap adapter，不是 file object owner。
- `memspace` 是地址空间 owner，不是 pagecache 或 anon object owner。
- `vmobj` 提供 object/view/invalidate 的公共语言。
- `page_table` 只执行 PTE 操作，不定义高层 VM 语义。

## 当前组件图

```text
sys_mmap / sys_munmap / sys_mprotect / sys_brk
        |
        v
  +-----+------+
  |  posix/mm  |
  | ABI policy |
  +-----+------+
        |
        v
  +-----+------------------+
  |      mm/memspace       |
  | MmSpace / VmAreaSet /  |
  | VmArea / fault dispatch|
  +-----+------------------+
        |
        +---------------------------+
        |                           |
        v                           v
+-------+-------+            +------+------+
|   mm/filemap  |            | mm/page_table|
| file mmap     |            | PTE actions  |
| adapter       |            +-------------+
+-------+-------+
        |
        +---------------------------+
        |                           |
        v                           v
+-------+--------+           +------+------+
|  mm/pagecache  |           |   mm/anon   |
| file Mapping   |           | anon object |
+-------+--------+           +------+------+
        |                           |
        +-------------+-------------+
                      |
                      v
                 +----+-----+
                 | mm/vmobj |
                 | id/view/ |
                 | invalidate
                 +----------+
```

## Linux 对应关系

| X-Kernel 当前实体 | Linux 对应物 | 当前职责 |
| --- | --- | --- |
| `posix/mm` | `mm/mmap.c`, `mm/mprotect.c`, `mm/mremap.c`, `mm/memfd.c`, `mm/mincore.c` | syscall ABI、参数校验、errno/policy |
| `MmSpace` | `mm_struct` | 一个进程地址空间 owner |
| `VmArea` | `vm_area_struct` | 单个映射实例的范围、权限、backing metadata |
| `VmRuntimeOps` | `vm_operations_struct` | VMA 侧 fault/map/unmap/protect/fork/mremap 执行入口 |
| `pagecache::Mapping` | `struct address_space` | file/shmem cached object 与对象长度 |
| `vmobj::MappingView` | `i_mmap` / rmap view | object 到 VMA 的映射视图 |
| `anon::AnonPrivateObject` | `anon_vma` + private anon page owner | private anonymous state 与 COW lineage |
| `page_table::PageTable` | arch page table helpers | PTE install/unmap/protect |
| `filemap` | `file_operations::mmap`, `generic_file_mmap`, `filemap_fault` 的 X-Kernel 组合边界 | file-backed VMA/runtime adapter |

## 组件职责

### `posix/mm`

源码范围：

- `posix/mm/src/mmap.rs`
- `posix/mm/src/brk.rs`
- `posix/mm/src/memfd.rs`
- `posix/mm/src/mincore.rs`

职责：

- 解析 Linux `mmap`/`munmap`/`mprotect`/`mremap`/`brk`/`memfd_create` ABI。
- 执行用户参数校验、flag policy 和 errno 映射。
- 为 anonymous mapping 调用 `memspace`。
- 为 file-backed mapping 调用 `filemap` 生成 `(VmArea, VmRuntimeRef)`。

不拥有：

- VMA tree；
- page table；
- pagecache；
- anonymous private page state；
- file-backed runtime internals。

### `mm/memspace`

源码范围：

- `mm/memspace/src/aspace.rs`
- `mm/memspace/src/vma.rs`
- `mm/memspace/src/fault.rs`
- `mm/memspace/src/backend/*`
- `mm/memspace/src/iomap.rs`

核心类型：

- `MmSpace`
- `VmArea`
- `VmAreaSet`
- `VmRuntimeRef`
- `VmRuntimeOps`
- `VmBackingInfo`
- `FaultInput`
- `FaultOutcome`
- `InvalidateHandle`

职责：

- 维护地址空间和 VMA 集合。
- 处理 VMA insert/split/merge/unmap/protect/relocate。
- 统一执行 page fault 的 VMA 查找、权限裁决和 runtime dispatch。
- 消费 object-side invalidate request 并执行 PTE zap。
- 为 fork/mremap 调用 runtime clone/relocate contract。

不拥有：

- file cached content；
- anon private page slots；
- object id 分配规则；
- syscall ABI。

### `mm/filemap`

源码范围：

- `mm/filemap/src/mmap.rs`
- `mm/filemap/src/runtime.rs`
- `mm/filemap/src/shared.rs`
- `mm/filemap/src/private.rs`
- `mm/filemap/src/invalidate.rs`

公开 API：

- `FileMmapRequest`
- `mmap_shared_file()`
- `mmap_private_file()`
- `new_file_private_vma()`

职责：

- 把 file-backed mmap 请求转换成 `(VmArea, VmRuntimeRef)`。
- 调用 VFS `File::mmap()` callback。
- 构建 file-backed VMA metadata：file offset、inode id、path、backing kind。
- 实现 read-only `MAP_SHARED` file runtime。
- 实现 `MAP_PRIVATE` file runtime 的首次 materialization、EOF 检查、
  file prefix、ELF `memsz > filesz` zero-fill。
- 注册 file object view，让 `pagecache::Mapping` 的 resize/invalidate 能到达
  `MmSpace`。

不拥有：

- inode lifecycle；
- file cached folios；
- dirty/writeback；
- object/view/invalidate 公共语言；
- private anonymous page state；
- VMA tree 或 page table。

### `mm/pagecache`

源码范围：

- `mm/pagecache/src/lib.rs`

核心类型：

- `Mapping`
- `MappingIdentity`
- `MappingView`
- `MappingViewNotifier`
- `TruncatePlan`

职责：

- 作为 inode-owned cached object。
- 维护 file/shmem object identity。
- 提供 sparse read、write、resize 和 object-visible length。
- 为 truncate/resize 生成 object-side invalidation work。

不拥有：

- VMA tree；
- syscall ABI；
- page fault dispatch；
- anonymous COW state。

### `mm/anon`

源码范围：

- `mm/anon/src/lib.rs`
- `mm/anon/src/private.rs`
- `mm/anon/src/shared.rs`

核心类型：

- `AnonPrivateObject`
- `AnonSharedObject`
- `AnonObjectId`
- `AnonLineageId`

职责：

- 拥有 anonymous private/shared object identity。
- 管理 private page state。
- 为 fork/COW 提供 lineage 和 child object。
- 作为 file-private write fault 后的 private page owner。
- 注册 anon-side mapping view 并产生 object hit/invalidate 语言。

不拥有：

- file source content；
- syscall ABI；
- VMA tree；
- page-table root lifecycle。

### `mm/vmobj`

源码范围：

- `mm/vmobj/src/lib.rs`

核心类型：

- `VmObjectId`
- `MappingView`
- `MappingViewId`
- `MappingViewSpec`
- `ObjectInvalidateRequest`
- `ObjectInvalidateWork`

职责：

- 提供 object-neutral identity。
- 表达 object range 到 VMA range 的 view mapping。
- 承载 object-side invalidate work。
- 让 `pagecache`、`anon`、`memspace` 使用同一套 object/view 语言。

不拥有：

- 页内容；
- VMA tree；
- page table；
- ABI policy。

### `mm/page_table`

源码范围：

- `mm/page_table/src/*`

职责：

- 执行 PTE install/unmap/protect/remap。
- 提供 arch-aware page table mutation API。
- 处理 page size、mapping flags 和 TLB 相关执行约束。

不拥有：

- VMA policy；
- backing object；
- syscall ABI。

## 关键路径

### Anonymous `mmap`

```text
sys_mmap
  -> posix/mm parses flags and range
  -> MmSpace::map_anon_private or shared-anon path
  -> VmArea inserted into VmAreaSet
  -> page table populated lazily on fault
```

### File-backed `mmap`

```text
sys_mmap
  -> posix/mm builds FileMmapRequest
  -> filemap::mmap_shared_file or mmap_private_file
  -> VFS File::mmap callback
  -> FileSharedRuntime or FilePrivateRuntime
  -> MmSpace::map_runtime_vma
```

### Page fault

```text
trap
  -> MmSpace::handle_page_fault
  -> VmAreaSet lookup
  -> permission/backing checks
  -> VmRuntimeOps::handle_fault
  -> page_table PTE install or fault error
```

### File truncate / resize invalidation

```text
pagecache::Mapping::resize
  -> vmobj object hit / invalidate work
  -> filemap MmSpaceInvalidate adapter
  -> MmSpace invalidate queue
  -> MmSpace drains request and zaps present PTEs
```

### File-private COW

```text
FilePrivateRuntime first fault
  -> read initial bytes from pagecache::Mapping
  -> commit page into AnonPrivateObject

write/fork COW
  -> memspace private backend helpers
  -> AnonPrivateObject page state and lineage
```

## 当前支持边界

已实现或作为当前主路径存在：

- anonymous private mapping；
- anonymous shared object identity；
- file `MAP_PRIVATE` first materialization；
- file-private post-write state owned by `AnonPrivateObject`；
- read-only file `MAP_SHARED` fault path；
- file object view registration；
- object-driven resize/truncate invalidation request；
- VMA split/merge/protect/unmap metadata preservation；
- fork/COW helper path for private mappings。

当前明确不支持或不完整：

- writable regular-file `MAP_SHARED`；
- `msync` dirty shared file writeback；
- Linux `page_mkwrite` / write-notify equivalent；
- readahead/fault-around；
- hole-punch/collapse-range；
- reclaim/swap/memcg/NUMA/THP；
- direct QEMU unittest collection for `filemap` crate-local tests。

## 设计不变量

1. `VmAreaSet` 中的 VMA 不允许重叠。
2. VMA split/unmap/protect 必须保留 file metadata、object offset 和 backing kind。
3. Page fault 必须先完成 VMA 查找和权限检查，再安装 PTE。
4. File-backed shared/private 的 file source identity 来自 inode-owned
   `pagecache::Mapping`。
5. File-private 写后页属于 `AnonPrivateObject`，不属于 `filemap` runtime。
6. Object-side invalidate 必须通过 `vmobj` 语言表达，再由 `MmSpace` 执行 PTE
   zap。
7. `posix/mm` 不得构造 `VmArea` internals 或访问 runtime internals。
8. `filemap` 外部调用者只能使用 crate-root whitelist API。
9. `page_table` 不判断 Linux ABI policy。
10. Unsupported Linux semantics must fail explicitly, not silently degrade.

## 文档关系

- 本文描述当前 X-Kernel MM 主架构。
- `linux-memory-model-reference.md` 描述当前架构如何对齐 Linux MM 模型。
- crate-local `docs/design.md` 和 `docs/security.md` 描述具体 crate 的实现细节、
  execution context、安全边界和限制。
