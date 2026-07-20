# pagecache — 设计文档
## 定位

`pagecache` 提供 inode-owned 的页缓存对象 `Mapping`，作为 X-Kernel 对齐 Linux `address_space` 的落地点。它负责管理“一个文件/共享内存对象拥有哪些缓存页”，而不是“一个进程如何映射这些页”。

它同时覆盖内存型对象和 file-backed 文件对象：`MappingKind::InMemory`
用于 memfs/tmpfs/shmem 风格对象，`MappingKind::FileBacked` 通过 `MappingOps`
从文件系统 materialize folio，并由 VFS address-space writeback 路径回写 dirty
folio。

Linux 对应关系：

- `include/linux/fs.h` 中的 `struct address_space`
- `mm/filemap.c` 中的通用 page cache 辅助逻辑
- `mm/shmem.c` 中的 inode-backed 共享内存页缓存

## 背景

内存对象的 ownership 边界按职责拆分：

- `memspace` 持有 VMA/fault 语义；
- `filemap` 负责 file-backed mmap adapter/runtime；
- `pagecache::Mapping` 拥有 inode-scoped cached content；
- `vmobj` 提供 object-side view/rmap 和 invalidation work 语言。

这个边界让 regular file mmap、tmpfs/shmem/memfd 都能共享同一套
inode-owned content-object 模型。

## 范围

- `src/lib.rs`
- `MappingKind::{InMemory, FileBacked}`
- source-backed `MappingOps`
- `filemap_add_folio()` 的非覆盖式 readahead completion
- final teardown through `Mapping::truncate_final()`
- RAII `EvictRegistration`
- 不包含 VMA/page-table/fault dispatch
- 不包含自适应 readahead policy、swap、reclaim 或完整文件系统 writeback policy

## 架构

```text
inode / memfs file
    -> Mapping
         -> BTreeMap<PageIndex, Folio>
         -> MappingOps

file open instance / mmap instance
    -> 引用 Mapping
```

这里的关键边界与 Linux 一致：`Mapping` 拥有 cached folios，open file 或 mmap 只持有引用，不持有内容本体。

## 调用约束 / 执行上下文

- 允许在普通进程上下文调用。
- 当前实现可能分配页，允许睡眠，不适用于中断上下文。
- 依赖内核页分配器已初始化。
- 当前不依赖 current process 或 VMA。

## 算法流程

### `read_into`

1. 读取当前对象长度。
2. 逐页定位 `PageIndex`。
3. 已缓存 folio 直接复制数据。
4. 未缓存 folio 视为洞，返回零填充。

### `read_into_or_create`

1. 读取当前对象长度。
2. 逐页定位 `PageIndex`。
3. 缺失 folio 通过 `MappingOps::instantiate_folio()` materialize。
4. 用于 file-backed `File::read_at()`，保证 regular file reads 与
   inode-owned `Mapping` 内容一致。

### `write_from`

1. 扩展对象长度。
2. 逐页确保 folio 存在。
3. 拷贝写入数据。
4. 标记 folio dirty。

### materialize / writeback iteration

1. `Mapping::new(kind, len, ops)` 可创建 source-backed object。
2. `MappingOps::instantiate_folio()` 是缺页 materialize hook。
3. dirty folio writeback 由 VFS `AddressSpaceOperations::writepages()` 发起。
4. `Mapping::writeback_range()` 只负责枚举与对象 byte range 相交的 dirty
   folio；调用方写回成功后才清 dirty。
5. 写回期间 folio 处于 under-writeback 状态；失败路径清除该状态但保留 dirty，
   让调用方可以重试。
6. writeback pass 可以带页数预算；预算耗尽后剩余 dirty folio 留给后续 pass。
7. `Mapping::truncate_final()` 属于对象生命周期结束路径，直接清理 cached
   folio，不进入 ordinary writeback。

### readahead completion

1. VFS `ReadaheadControl` 保存 mapping 与连续 folio window，对应 Linux
   `readahead_control` 的最小语义层。
2. 文件系统读取 backing bytes 后调用 `filemap_add_folio()` 发布 folio。
3. 若前台 read/write 已先插入同一 index，readahead completion 返回 `false`，不得覆盖
   现有 folio，尤其不得用陈旧磁盘内容覆盖 dirty 数据。
4. 当前实现允许竞态双方重复读取 backing storage；不额外维护 claim table 或 request ID。

### evict listener

1. `Mapping::add_evict_listener()` 返回 `EvictRegistration`。
2. 调用方持有该 guard 表示 listener 仍然注册。
3. `EvictRegistration::drop()` 自动 unregister；调用方不保存或传播裸整数 id。
4. `invalidate_from_page()`、`resize()` shrink 和 `truncate_final()` 释放
   folio 时通知仍然存活的 listener。

### `resize` / `set_len`

1. 更新对象长度。
2. 扩展时只对已缓存尾页的增长区间清零。
3. 收缩时删除 EOF 之后的 folio。
4. 对新的尾页剩余部分清零并标 dirty。
5. `resize` 显式返回 `TruncatePlan`，描述被删除的 folio、被清零的 surviving tail，以及 object-side `ObjectInvalidateWork`。
6. registered mapping views may carry a reverse-mapping notifier; `resize` emits stable view-hit invalidation work and lets that notifier kick the work back into `MmSpace`.
7. `MappingView` / `ObjectViewHit` / `ObjectInvalidateWork` 现在来自 `mm/vmobj`，而不是由 `pagecache` 独占定义。
8. registered mapping views carry both `page_offset` and explicit `object_start`.
   原因：`vm_pgoff` 只能表达 VMA 起点所在的 backing page；像 ELF `PT_LOAD` 这类 unaligned private file prefix，还需要单独记录 object-side 起始 byte，才能让 truncate/invalidate view coverage 与 Linux 的 file-backed object 语义一致。

## 并发模型

- `Mapping` 内部用单个 `Mutex<MappingInner>` 保护页树和长度。
- 每个 `Folio` 再用单独的 `Mutex` 保护页内容。
- 每个 `Folio` 的 dirty / under-writeback 状态跟随 folio lock 更新。
- 当前并发模型优先保证 ownership 和 object invalidation 语义清晰；高并发 pagecache 优化不属于本 crate 的当前职责。

## 设计决策

1. 用 `MappingKind` 区分内存对象和 file-backed 文件对象。
   原因：tmpfs/shmem/memfd 与 regular file mmap 都需要 inode-owned content
   identity，但 folio materialize/writeback 来源不同。

2. 用 `MappingOps` 只承接 folio materialize。
   原因：`pagecache` 拥有 cached folio 和 dirty state，但具体从哪里读页、如何写回属于 VFS inode/address-space contract。
   `msync` 进入 `AddressSpaceOperations::writepages()`，文件系统实现再使用
   `Mapping::writeback_range()` 选择 dirty folio。

3. 用 `BTreeMap<PageIndex, Folio>` 表达页索引树。
   原因：当前重点是稳定 object ownership、truncate/invalidate 与 mmap 语义；Linux 式 xarray/radix tree 可作为性能实现替换，不改变上层契约。

4. 保留 `MappingIdentity`。
   原因：futex shared key、shmem 对象身份和 file-backed object identity 都需要绑定 mapping，而不是 VMA/backend 实例。

5. `resize` 返回显式 `TruncatePlan`。
6. registered views can notify file-backed VMAs to zap present PTEs past the new EOF while keeping VMA metadata intact.
   原因：`MmSpace` 反向映射、truncate/invalidate 和 `i_mmap` 风格对象级失效，都需要 content-object 自己产出的失效计划。
7. `TruncatePlan.affected_views()` 只保留真正覆盖 shrink 区间的 registered view hits。
   原因：对象级失效应尽量接近 Linux `i_mmap` 的“只回推受影响 VMA”语义，而不是粗粒度广播给所有映射该对象的 VMA。
8. `MappingView` 显式保存 `MappingViewRange { vma_start/vma_len/object_start/object_len }`；对象侧 view/rmap 语义统一从这个 range 读取，`page_offset()` 仅作为从 `object_start` 派生出的辅助接口。
   原因：private executable mappings 的首个 file-backed page 可能覆盖对齐后的页前缀；对象级 reverse-mapping 不能把这段有效 file bytes 漏掉。
9. `pagecache` 只保留 file-backed object owner 语义；view/rmap 语言和 object-side invalidation work 归 `mm/vmobj`。
10. evict listener registration uses RAII.
   原因：listener lifetime 是资源生命周期；Rust guard/drop 语义比手动保存
   integer id 更能避免泄漏、重复 unregister 和 dangling callback。
11. readahead 不保存独立 claim 状态。
   原因：Linux 在 page cache 中插入 locked folio 后发起 I/O；当前同步实现只需保持
   `filemap_add_folio()` 的非覆盖插入语义即可解决前台竞态，额外 claim tree/ID 会复制
   page-cache residency 状态。

## Drop / 资源释放

- `Folio` drop 时归还页缓存页。
- `Mapping` 释放时，所有 folio 跟随释放。
- final teardown 通过 `truncate_final()` 清空 cached folio；这是对象生命周期结束
  的数据丢弃路径，不要求 ordinary writeback。
- ordinary invalidation 仍要求调用方先完成 writeback，不能直接丢弃 dirty folio。
- writeback 失败后 folio 仍保持 dirty，并清除 under-writeback 状态。
- `InMemory` mapping 没有外部 writeback；其最终释放由 final teardown 路径清理。
- `FileBacked` mapping 通过 VFS `AddressSpaceOperations::writepages()` 把 dirty folio 交回文件系统实现。
