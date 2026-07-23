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
       -> pagecache::PageCache
            -> BTreeMap<PageIndex, Folio> resident folios
            -> BTreeSet<PageIndex> derived dirty-tag index
```

`kvfs::AddressSpace` 在 materialize 时提供闭包，由自己的
`AddressSpaceOperations::read_folio()` 塑造 folio。writeback 时也由
`AddressSpace` 传入当前 inode `i_size`；`PageCache` 不缓存第二份 EOF。

`Folio::dirty` 是 dirty 状态源。Rust `BTreeMap` 没有 Linux XArray 的 per-entry
mark，因此 `dirty_pages` 只作为 `PAGECACHE_TAG_DIRTY` 的派生索引，用于枚举
writeback 候选，不提供写回许可，也不引入第二份 folio 状态。

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

- growth：清零已缓存旧尾页中原 EOF 到新范围之间的字节，并将该 folio 标记 dirty、
  插入 dirty tag；
- shrink：清除并删除新 EOF 之后的整页；
- partial EOF：清零 surviving folio 的尾部并标记 dirty。

growth 和 shrink 都直接在 PageCache inner lock → folio lock 下修改 cached folio，
不经过 `with_folio`/`with_folio_or_create`。因此 `resize_cached_folios` 必须在同一
inner-lock 临界区内同步 dirty tag，不能依赖 folio access 的 reconciliation。

该方法不更新任何可见长度，也不产生 VM invalidation。Linux
`i_size_write -> unmap -> truncate page cache -> unmap` 的事务由
`kvfs::AddressSpace` 负责。

### Writeback

`writeback_until` 和 `write_cache_pages` 接收 owner 传入的
`visible_len`，据此裁剪最后一个 folio 的有效字节。成功后清 dirty；失败时恢复
dirty 并结束 under-writeback 状态。

writeback 从独立的 dirty folio index 枚举目标范围，不扫描全部 resident folio。
dirty tag 只产生候选 `(index, Arc<Folio>)`。真正开始写回前，writeback 在
PageCache inner lock → folio lock 的同一临界区内重新确认：

1. `inner.pages[index]` 仍与候选 `Arc` 相同；
2. 当前 folio 仍然 dirty；
3. 当前 folio 尚未 under writeback。

缺失、已替换或 clean 的候选不会写回，并会根据当前 resident folio 修正 dirty tag。
通过校验后，writeback 在同一临界区内清 dirty、设置 under-writeback 并删除 dirty
tag。I/O 期间发生 redirty 时，folio access 会重新插入 tag；I/O 失败也会恢复 dirty
并重新插入 tag。batch writeback 完成阶段会再次核对最终 dirty 状态；单页同步
writeback 在持有 folio lock 的 callback 成功后保持 clean，后续 writer 会在
clean → dirty 状态变化时重新插入 tag。

`with_folio` 和 `with_folio_or_create` 只在 closure 前后的最终 dirty 状态不同时执行
reconciliation。dirty → dirty 和 clean → clean 不改变 tag，避免只读访问 dirty folio
时额外获取 PageCache inner lock。

## 并发模型

- 一个 mutex 保护 resident folio tree 和 dirty index；
- 每个 folio 的 mutex 保护数据和 dirty/writeback 状态；
- 同时核对 resident tree、dirty index 与 folio 状态时，锁顺序固定为
  PageCache inner lock → folio lock；folio access closure 释放 folio lock 后才能重新取得
  PageCache inner lock；
- 本 crate 不拥有 inode data lock、mapped-view lock 或 MM lock；
- 不在 folio-tree mutex 下调用文件系统 I/O；
- `writeback_until` 为保持候选在单页同步 I/O 期间稳定，会持有 folio lock 调用
  backend callback；该 callback 不得重入同一个 `PageCache`；
- `write_cache_pages` 在锁内完成状态过渡和数据复制，释放 folio lock 后才调用 batch
  backend callback；
- 当前没有 Linux 完整的 wait-on-writeback/invalidate 协调。候选收集到开始写回之间
  的 stale-folio 窗口由本 crate 自身关闭；batch I/O 启动后的 resize/invalidate 仍须由
  owner 串行化。

## 设计决策

1. 保留独立算法 crate，是因为 folio 分配、dirty/writeback 和 sparse storage 是可复用
   算法；其 API 不暴露 file object 语义。
2. 不保存 length。唯一 EOF 是 `VfsInode::size()`。
3. 不保存 object-id 或 views。它们属于 `kvfs::AddressSpace`，对应 Linux
   `address_space::i_mmap`。
4. `PageCacheKind` 只决定 dirty folio 被意外 drop 时是否告警，不建立第二套对象类型。
5. resident folio 使用 `BTreeMap`，dirty tag 使用独立 `BTreeSet`。该集合只是补足
   `BTreeMap` 缺少 per-entry mark 的能力，writeback 成本因此与 dirty folio 数量相关；
   未来可用 Linux XArray dirty tag 风格实现替换具体索引，不改变 `AddressSpace` 契约。

## Drop / 资源释放

`AddressSpace` 释放唯一的 `Arc<PageCache>` 后，cached folios 随之释放。
`truncate_final()` 是 inode final teardown 的显式丢弃路径；普通 writeback 失败仍保留
dirty 状态。shrink 和 final teardown 会同时删除 resident folio 与对应 dirty index；
surviving truncated tail 被清零并重新加入 dirty index。
