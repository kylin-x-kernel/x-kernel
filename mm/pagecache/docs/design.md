# pagecache — 设计文档

## 定位

`pagecache` 是 `kvfs::AddressSpace` 使用的 folio 存储与通用缓存算法库，只对应
Linux `address_space::i_pages` 一侧的实现细节。它不是 file object，也不是第二个
`struct address_space`。

`MappingPages` 不拥有 inode、可见文件长度、VM object identity、`i_mmap` views 或
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

实现按职责分为私有模块，crate root 只导出领域类型：

- `mapping_pages.rs`：容器、锁内 resident tree/dirty marks 与 dirty reconciliation；
- `folio.rs`：页分配、数据访问、dirty/writeback 状态和释放；
- `folio_batch.rs`：有界、有序的 folio 引用集合；
- `filemap.rs`：lookup、materialize 和 insert-if-absent；
- `readahead.rs`：连续 cached run 查询；
- `truncate.rs`：尺寸变化时的页移除、尾页清零和 final teardown；
- `writeback.rs`：dirty candidate、batch helper、开始/完成协议与结果类型；
- `tests/`：resident storage、truncate 和 writeback 测试。

## 架构

```text
VfsInode
  -> kvfs::AddressSpace { host, a_ops, object_id, mapped_views }
       -> i_pages: pagecache::MappingPages (直接持有)
            -> Mutex<MappingPagesState>
                 -> BTreeMap<PageIndex, Arc<Mutex<Folio>>> entries
                 -> BTreeSet<PageIndex> dirty_marks (派生索引)
                 -> BTreeSet<PageIndex> writeback_marks (派生索引)
```

`kvfs::AddressSpace` 在 materialize 时提供闭包，由自己的
`AddressSpaceOperations::read_folio()` 塑造 folio。writeback 时也由
`AddressSpace` 传入当前 inode `i_size`；`MappingPages` 不缓存第二份 EOF。

`Folio::dirty` 是 dirty 状态源。Rust `BTreeMap` 没有 Linux XArray 的 per-entry
mark，因此 `dirty_marks` 只作为 `PAGECACHE_TAG_DIRTY` 的派生索引，用于枚举
writeback 候选，不提供写回许可，也不引入第二份 folio 状态。
`writeback_marks` 同理作为 `PAGECACHE_TAG_WRITEBACK` 的派生索引，只用于有界等待枚举。

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

growth 和 shrink 都直接在 MappingPages state lock → folio lock 下修改 cached folio，
不经过 `with_folio`/`with_folio_or_create`。因此 `resize_cached_folios` 必须在同一
state-lock 临界区内同步 dirty tag，不能依赖 folio access 的 reconciliation。

该方法不更新任何可见长度，也不产生 VM invalidation。Linux
`i_size_write -> unmap -> truncate page cache -> unmap` 的事务由
`kvfs::AddressSpace` 负责。

### Writeback

写回进度只由 KVFS `WritebackControl` 拥有。MappingPages 独占借用其预算和 skip
字段原地更新，返回 `KResult<()>`，后续错误不会丢失先前进度；不另建 stats/pass
对象。byte callback 使用 `Result<usize, (usize, KError)>` 返回完成前缀，或完成前缀
与错误。完成所有已开始的 folio 后再传播错误，只有完整成功页消耗预算。

`writeback_until` 和 `write_cache_pages` 接收 owner 传入的
`visible_len`，据此裁剪最后一个 folio 的有效字节。成功后清 dirty；失败时恢复
仍属于当前 mapping 的 folio 为 dirty，并结束 under-writeback 状态。

writeback 从独立的 dirty folio index 枚举目标范围，不扫描全部 resident folio。
dirty tag 只产生候选 `(index, Arc<Folio>)`。真正开始写回前，writeback 在
MappingPages state lock → folio lock 的同一临界区内重新确认：

1. `state.entries[index]` 仍与候选 `Arc` 相同；
2. 当前 folio 仍然 dirty；
3. 当前 folio 尚未 under writeback。

缺失、已替换或 clean 的候选不会写回，并会根据当前 resident folio 修正 dirty tag。
通过校验后，writeback 在同一临界区内清 dirty、设置 under-writeback 并删除 dirty
tag。I/O 期间发生 redirty 时，folio access 会重新插入 tag；I/O 失败时，当前 resident
folio 会恢复 dirty 并重新插入 tag。完成阶段在 MappingPages state lock → folio lock 的同一临界区再次核对
mapping identity；只有仍是当前 resident folio 的失败写回才能重新标脏并修改 dirty tag，
已移除或替换的 folio 只结束 under-writeback。batch writeback 同时核对最终 dirty 状态；
单页同步 writeback 在持有 folio lock 的 callback 成功后保持 clean，后续 writer 会在
clean → dirty 状态变化时重新插入 tag。
开始时同步设置 writeback 位与 mark；完成时按 folio identity 清 mark，释放锁后
唤醒共享哈希等待队列。等待者持有 folio 引用并重查 writeback 位。

`with_folio` 和 `with_folio_or_create` 只在 closure 前后的最终 dirty 状态不同时执行
reconciliation。dirty → dirty 和 clean → clean 不改变 tag，避免只读访问 dirty folio
时额外获取 MappingPages state lock。

## 并发模型

- 一个 mutex 保护 resident folio tree 和 dirty index；
- 每个 folio 的 mutex 保护数据和 dirty/writeback 状态；
- 同时核对 resident tree、dirty index 与 folio 状态时，锁顺序固定为
  MappingPages state lock → folio lock；folio access closure 释放 folio lock 后才能重新取得
  MappingPages state lock；
- 本 crate 不拥有 inode data lock、mapped-view lock 或 MM lock；
- writeback 的文件系统 I/O 在 folio-tree mutex 外执行；
- 现有 `with_folio_or_create` 在 tree mutex 下调用 materialize closure，owner 的
  `read_folio` 可能执行 backing I/O；该 closure 不得重入同一容器。这是保留的
  同步读入行为，目录与所有权重构不改变其锁范围；
- `writeback_until` 为保持候选在单页同步 I/O 期间稳定，会持有 folio lock 调用
  backend callback；该 callback 不得重入同一个 `MappingPages`；
- `write_cache_pages` 在锁内完成状态过渡和数据复制，释放 folio lock 后才调用 batch
  backend callback；
- `wait_on_writeback_range` 不持锁睡眠；truncate 先移除非忙页，再等待写回并复核 identity 后移除余页。
  KVFS 仍用 invalidate shared lock 覆盖完整 writeback operation，本阶段不缩小其锁范围。

## 设计决策

Linux 对照源码为用户指定的 `/root/codes/linux`，版本 7.2，提交
`818bebeb63dd6bf5f4e07e145f6cdbace520a34c`。`include/linux/fs.h` 的
`address_space::i_pages` 是内嵌 XArray；`mm/filemap.c`、`mm/truncate.c` 和
`mm/page-writeback.c` 围绕 mapping 操作该索引。这里以 `MappingPages` 的方法表达
低层算法，不再建立独立的 PageCache owner。

`MappingPagesState` 是 crate 内部的锁保护数据，不对外导出。它不是新增语义对象：
Rust `BTreeMap` 没有 per-entry marks，需要把 entries 与派生 dirty marks 放在同一
mutex 下才能保持索引一致性。Folio 的数据与 flags 仍封装在 `folio.rs`。

1. 保留独立算法 crate，是因为 folio 分配、dirty/writeback 和 sparse storage 是可复用
   算法；其 API 不暴露 file object 语义。
2. 不保存 length。唯一 EOF 是 `VfsInode::size()`。
3. 不保存 object-id 或 views。它们属于 `kvfs::AddressSpace`，对应 Linux
   `address_space::i_mmap`。
4. resident folio 使用 `BTreeMap`，dirty/writeback tag 使用独立 `BTreeSet`。该集合只是补足
   `BTreeMap` 缺少 per-entry mark 的能力，枚举成本因此与 tagged folio 数量相关；
   未来可用 Linux XArray dirty tag 风格实现替换具体索引，不改变 `AddressSpace` 契约。

Folio 的 immutable `index` 对应 Linux `folio::index`，detach 后仍保留，供批处理和
identity 校验使用；FolioBatch 只存引用，entries key 在插入时必须匹配该 index。
归属仍由 owning AddressSpace 的 `i_pages[index]` Arc identity 表达，不增加反向 owner。

## Drop / 资源释放

`AddressSpace` 直接拥有 `MappingPages`，不单独分配或引用计数缓存容器。
AddressSpace 销毁时容器随字段释放；entries 持有的 folio 引用随之释放，已有
folio 引用仍通过 `Arc` 保活页数据。
`truncate_final()` 是 inode final teardown 的显式丢弃路径；普通 writeback 失败仍保留
dirty 状态。shrink 和 final teardown 会同时删除 resident folio 与对应 dirty index；
surviving truncated tail 等待写回后清零并重新加入 dirty index；增长时保留尾页，在 folio 锁内清零并标脏，无需等待持数据副本的批量写回。

MappingPages 的 Drop 不等待 I/O：最终所有权排除仍借用容器的写回操作；在任务上下文中
清 dirty 后，由字段析构释放 entries/marks，外部持有的 folio 也完成清理。
需要等待写回的清理通过显式 `truncate_final()` 完成。
