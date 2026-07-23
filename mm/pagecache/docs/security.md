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
- `Folio::dirty` 是状态源，dirty index 只承担 `PAGECACHE_TAG_DIRTY` 风格的候选枚举；
- 对外 folio 操作返回时，dirty 状态变化已反映到当前 resident folio 的 dirty tag；
  已从 resident tree 移除的 index 不得残留在 dirty index；
- dirty tag 命中不构成写回许可；开始 I/O 前必须在 PageCache inner lock → folio lock
  的同一临界区重新校验 mapping identity、dirty 和 writeback 状态；
- `PageCache` 中不存在可与 inode `i_size` 分歧的 length 字段；
- VM object-id、views 和 notifier 不进入本 crate。

## 线程安全

folio tree/dirty index 与 folio data 分层加锁。需要同时核对两层状态时，锁顺序固定为
PageCache inner lock → folio lock；folio access closure 释放 folio lock 后再执行 dirty
index reconciliation。文件系统 I/O 在 tree lock 外执行，避免把 sleepable backing
operation 放入缓存索引临界区。`writeback_until` 会在 folio lock 下调用 backend
callback，该 callback 不得重入同一个 `PageCache`；batch writeback 则在调用 backend
前释放 folio lock。

## 威胁分析

| 编号 | 威胁 | 控制 |
|------|------|------|
| T-01 | 页内 offset/length 越界 | checked arithmetic 和 page-size boundary 检查 |
| T-02 | truncate 后尾页泄露旧数据 | shrink 清零 surviving tail |
| T-03 | writeback 失败却清除 dirty | 失败路径恢复 dirty 并结束 writeback |
| T-04 | readahead 覆盖前台 dirty folio | insert-if-absent，已缓存 folio 获胜 |
| T-05 | 缓存层形成第二个 EOF/object owner | 不保存 length、object-id、views 或 notifier |
| T-06 | dirty index 漏项导致 fsync 漏写 | folio dirty 状态转换时 reconciliation，resize/writeback 显式同步索引 |
| T-07 | writeback 完成覆盖并发 redirty | batch 完成阶段重新读取最终状态；单页写回后的 clean → dirty writer 重新设置 tag |
| T-08 | truncate/replace 后写回已脱离 mapping 的旧 folio | dirty tag 只用于枚举；开始写回前在固定锁序下重新校验当前 `Arc` identity |
| T-09 | 文件增长后 cached tail 的清零结果未进入 writeback | growth 在 inner lock → folio lock 下同时设置 dirty bit 和 dirty tag |

## 故障模式与影响分析（FMEA）

| 故障模式 | 当前处理 | 影响 |
|----------|----------|------|
| folio 分配失败 | 返回 `NoMemory` | 当前 I/O/fault 失败 |
| range arithmetic 溢出 | 返回 `InvalidInput` | 拒绝本次操作 |
| backing write 失败 | 保留 dirty | 后续可重试 |
| dirty tag 指向旧实例或 clean folio | 固定锁序重新核对当前 resident folio 并修正 tag | 不调用 backend，不污染替换后的 folio |
| 文件增长后 cached tail 清零但未持久化 | growth 同步设置 dirty bit 和 dirty tag | 后续 writeback 会覆盖新可见范围 |
| final teardown 有 dirty folio | 明确清 dirty 后释放 | inode 已退出生命周期 |

## 已知限制

- resident folio tree 和 dirty index 分别使用 `BTreeMap`/`BTreeSet`；已消除 writeback
  对全部 resident folio 的扫描，但尚未实现 Linux XArray tag/reclaim；
- 尚未实现 Linux 完整的 wait-on-writeback/invalidate 协调；本 crate 保证候选在开始
  I/O 前仍属于当前 mapping，batch I/O 启动后的 resize/invalidate 仍依赖 owner 串行化；
- `writeback_until` 持 folio lock 调用 backend，callback 重入同一个 `PageCache` 会造成
  死锁；
- 没有自适应 readahead、swap、memcg、NUMA 或 THP；
- inode/MM 一致性由 `kvfs::AddressSpace` 的事务保证，不在本 crate 重复实现。

## 审计清单

- 是否重新加入了 length、object-id、view 或 invalidation state；
- owner 是否传入当前 inode `i_size` 执行 writeback；
- growth 是否在清零 cached tail 后设置 dirty bit 和 dirty tag；
- shrink 是否清零 partial tail 并删除 EOF 后整页及其 dirty tag；
- writeback 失败是否保留 dirty；
- dirty 状态变化返回前是否更新 dirty index，并发 redirty 后索引是否继续保留；
- dirty tag 候选是否在开始写回前重新校验当前 mapping identity 和 folio 状态；
- shrink 和 final teardown 是否同步删除对应 dirty index；
- PageCache/folio 双锁路径是否保持 PageCache inner → folio 顺序；
- `writeback_until` backend callback 是否避免重入同一个 `PageCache`；
- 是否在 tree lock 下调用了可能阻塞的文件系统 I/O。
