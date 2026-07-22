# pagecache — 安全与可靠性分析

## 信任模型

`pagecache` 信任 owning `kvfs::AddressSpace` 提供已经校验并串行化的 index、范围和
visible `i_size`。它不执行访问控制，也不拥有文件长度或 VM 生命周期。

## 外部边界 / 攻击面

- folio index 和页内 offset；
- owner 提供的 old/new length；
- writeback range 和 visible length；
- materialize/writeback closure 的失败结果。

## unsafe 代码清单

1. `Folio::new_zeroed()`：对页分配器返回的独占可写页清零。
2. `Folio::data()`：在持有 `&mut Folio` 时构造独占的页大小 mutable slice。

## 内存安全不变量

- 每个 `Folio` 独占一页；
- 一个 page index 最多关联一个 cached folio；
- folio bytes、dirty 和 writeback 状态只在 folio lock 下访问；
- `PageCache` 中不存在可与 inode `i_size` 分歧的 length 字段；
- VM object-id、views 和 notifier 不进入本 crate。

## 线程安全

folio tree 与 folio data 分层加锁。文件系统 I/O 在 tree lock 外执行，避免把
sleepable backing operation 放入缓存索引临界区。

## 威胁分析

| 编号 | 威胁 | 控制 |
|------|------|------|
| T-01 | 页内 offset/length 越界 | checked arithmetic 和 page-size boundary 检查 |
| T-02 | truncate 后尾页泄露旧数据 | shrink 清零 surviving tail |
| T-03 | writeback 失败却清除 dirty | 失败路径恢复 dirty 并结束 writeback |
| T-04 | readahead 覆盖前台 dirty folio | insert-if-absent，已缓存 folio 获胜 |
| T-05 | 缓存层形成第二个 EOF/object owner | 不保存 length、object-id、views 或 notifier |

## 故障模式与影响分析（FMEA）

| 故障模式 | 当前处理 | 影响 |
|----------|----------|------|
| folio 分配失败 | 返回 `NoMemory` | 当前 I/O/fault 失败 |
| range arithmetic 溢出 | 返回 `InvalidInput` | 拒绝本次操作 |
| backing write 失败 | 保留 dirty | 后续可重试 |
| final teardown 有 dirty folio | 明确清 dirty 后释放 | inode 已退出生命周期 |

## 已知限制

- folio tree 使用 `BTreeMap`，尚未实现 Linux XArray/reclaim；
- 没有自适应 readahead、swap、memcg、NUMA 或 THP；
- inode/MM 一致性由 `kvfs::AddressSpace` 的事务保证，不在本 crate 重复实现。

## 审计清单

- 是否重新加入了 length、object-id、view 或 invalidation state；
- owner 是否传入当前 inode `i_size` 执行 writeback；
- shrink 是否清零 partial tail 并删除 EOF 后整页；
- writeback 失败是否保留 dirty；
- 是否在 tree lock 下调用了可能阻塞的文件系统 I/O。

