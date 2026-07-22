# pagecache — 设计文档

## 定位

`pagecache` 是 `kvfs::AddressSpace` 使用的 folio 存储与通用缓存算法库，只对应
Linux `address_space::i_pages` 一侧的实现细节。它不是 file object，也不是第二个
`struct address_space`。

`PageCache` 不拥有 inode、可见文件长度、VM object identity、`i_mmap` views 或
mmap invalidation 生命周期。当前只有 `kvfs` 直接依赖本 crate；MM 和 open-file
runtime 不能直接引用它。

## 范围

- sparse `PageIndex -> Folio` 存储；
- folio materialization 的 insert-if-absent；
- dirty/writeback 枚举；
- owner 提供 old/new `i_size` 后的尾页清零和 folio 删除；
- final inode teardown 时释放 cached folios。

不包含：

- inode/`i_size` ownership；
- `AddressSpaceOperations`；
- VM object-id、mapped-view registration 或 PTE invalidation；
- VMA、fault dispatch、权限和 syscall ABI。

## 架构

```text
VfsInode
  -> kvfs::AddressSpace { host, a_ops, object_id, mapped_views }
       -> pagecache::PageCache { cached_folios }
```

`kvfs::AddressSpace` 在 materialize 时提供闭包，由自己的
`AddressSpaceOperations::read_folio()` 塑造 folio。writeback 时也由
`AddressSpace` 传入当前 inode `i_size`；`PageCache` 不缓存第二份 EOF。

## 调用约束 / 执行上下文

- 运行于普通 task context，可能分配内存并获取 sleepable mutex；
- 不可在中断上下文调用；
- 不依赖 current process；
- owner 必须在调用 resize/writeback 算法时提供已串行化的 inode 状态。

## 算法流程

### Materialize

1. 查询 folio tree。
2. 命中时复用现有 folio。
3. 缺失时调用 owner 提供的 materialize closure。
4. 在同一 index 已被并发插入时保留先到达的 folio。

### Resize cached folios

`resize_cached_folios(old_len, new_len)` 只消费 owner 提供的尺寸：

- growth：清零已缓存旧尾页中原 EOF 到新范围之间的字节；
- shrink：清除并删除新 EOF 之后的整页；
- partial EOF：清零 surviving folio 的尾部并标记 dirty。

该方法不更新任何可见长度，也不产生 VM invalidation。Linux
`i_size_write -> unmap -> truncate page cache -> unmap` 的事务由
`kvfs::AddressSpace` 负责。

### Writeback

`writeback_until` 和 `write_cache_pages` 接收 owner 传入的
`visible_len`，据此裁剪最后一个 folio 的有效字节。成功后清 dirty；失败时恢复
dirty 并结束 under-writeback 状态。

## 并发模型

- 一个 mutex 保护 folio tree；
- 每个 folio 的 mutex 保护数据和 dirty/writeback 状态；
- 本 crate 不拥有 inode data lock、mapped-view lock 或 MM lock；
- 不在 folio-tree mutex 下调用文件系统 I/O。

## 设计决策

1. 保留独立算法 crate，是因为 folio 分配、dirty/writeback 和 sparse storage 是可复用
   算法；其 API 不暴露 file object 语义。
2. 不保存 length。唯一 EOF 是 `VfsInode::size()`。
3. 不保存 object-id 或 views。它们属于 `kvfs::AddressSpace`，对应 Linux
   `address_space::i_mmap`。
4. `PageCacheKind` 只决定 dirty folio 被意外 drop 时是否告警，不建立第二套对象类型。

## Drop / 资源释放

`AddressSpace` 释放唯一的 `Arc<PageCache>` 后，cached folios 随之释放。
`truncate_final()` 是 inode final teardown 的显式丢弃路径；普通 writeback 失败仍保留
dirty 状态。

