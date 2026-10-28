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

所有页内存 unsafe 操作集中在私有 `src/folio.rs`，字段不向其他模块开放。

1. `Folio::new_zeroed(index)`：对页分配器返回的独占可写页清零。
2. `Folio::data()`：仅允许未发布页的独占 owner 借用；已发布页只由
   `FolioGuard::data()` 在稳定页对象自身的 mutex 下访问。
3. `PinnedFolio::copy_to_uninit()`：锁住 pin 持有的同一稳定页对象后复制完整初始化页。
   缓存摘除或替换不改变其分配或锁，允许继续复制原页；目标范围由独占切片限定。

## 内存安全不变量

- 同步写回错误返回前保留已完成页的预算更新，并结束所有已开始的 folio writeback；
- 提交式写回转移完成责任，callback 返回不清 writeback；owner 必须限制在途批次，
  在所有 I/O 结束后于任务上下文完成或丢弃 completion，不得在 IRQ/BH 中获取 cache mutex；
- 每个 `Folio` 独占一页；
- `FolioBatch` 只持有有界、按遍历顺序排列的引用；引用存活不等于 mapping identity 仍有效；
- batch clear/drop 不结束 writeback；任何已开始的写回必须先显式完成，包括失败后缀；
- 遍历候选和待完成批次各最多 31 页，连续写回数据最多 31 页；临时内存不随
  整个 dirty range 或被截断的页数增长；提交式回写的总在途内存由调用方另行限额；
- 一个 page index 最多关联一个 cached folio；发布前校验 tree key 与 folio immutable index 一致；
- folio bytes、dirty 和 writeback 状态只在 folio lock 下访问；
- pinned-folio copy-to-uninit 在稳定页对象自身的锁内读取完整初始化页；
  成功时完整写入 MaybeUninit 目标后才返回；
- 移除先在 folio lock 下永久标记失效，再删除索引；普通和批次访问在同一锁下校验，
  失效候选不执行 callback，普通路径重新查询，成功 callback 不重放；
- 批次读取 callback 只读取数据及更新预读标记，不修改 dirty 或驻留状态；
  允许索引查询，但不得等待其他 folio，预读 I/O 必须在释放当前 folio 锁后执行；
- owner 必须使 backing read 与发布排除 truncate，并在复制时限制当前 EOF；
- `Folio::dirty` 是状态源，dirty index 只承担 `PAGECACHE_TAG_DIRTY` 风格的候选枚举；
- `Folio::writeback` 是状态源，writeback index 只承担等待枚举；
- 对外 folio 操作返回时，dirty 状态变化已反映到当前 resident folio 的 dirty tag；
  已从 resident tree 移除的 index 不得残留在 dirty index；
- dirty tag 命中不构成写回许可；开始 I/O 前必须在 folio lock → MappingPages state lock
  的同一临界区重新校验 mapping identity、dirty 和 writeback 状态；
- writeback 完成时必须按相同锁序重新校验 mapping identity；失败只重新标脏当前
  resident folio，已脱离 mapping 的 folio 只结束 writeback；
- 清 writeback 位及 mark 后释放两层锁再唤醒；等待者重查位；
- `MappingPages` 中不存在可与 inode `i_size` 分歧的 length 字段；
- VM object-id、views 和 notifier 不进入本 crate。

## 线程安全

folio tree/dirty index 与 folio data 分层加锁。需要同时核对两层状态时，锁顺序固定为
folio lock → MappingPages state lock；索引锁下只允许非阻塞 try_lock，失败必须释放
索引锁再等待。folio access closure 持页锁完成 dirty index reconciliation。writeback 和缓冲缺页读取的文件系统 I/O 在 tree lock 外执行。
缓冲读 owner 用 invalidate shared lock 协调 EOF、页保留和初始化，避免迟到读入恢复
被截断的数据。读入前 folio 已进入 resident tree，I/O 期间持页锁而不持 tree lock；
只有完整初始化的页可标记 uptodate，失败页不暴露部分数据。等待页锁后重新查找 resident，
不使用等待期间已脱离缓存的引用。读入 callback 不得重入映射或获取截断所需锁。
`writeback_until` 会在 folio lock 下调用 backend
callback，该 callback 不得重入同一个 `MappingPages`；batch writeback 则在调用 backend
前释放 folio lock，但回调不得等待或截断自身提交批次的 folio，否则完成路径无法执行。

## 威胁分析

| 编号 | 威胁 | 控制 |
|------|------|------|
| T-01 | 页内 offset/length 越界 | checked arithmetic 和 page-size boundary 检查 |
| T-02 | truncate 后尾页泄露旧数据 | shrink 清零 surviving tail |
| T-03 | writeback 失败却清除当前 folio 的 dirty | 失败路径在 identity 校验后恢复 dirty 并结束 writeback |
| T-04 | readahead 覆盖前台 dirty folio | insert-if-absent，已缓存 folio 获胜 |
| T-05 | 缓存层形成第二个 EOF/object owner | 不保存 length、object-id、views 或 notifier |
| T-06 | dirty index 漏项导致 fsync 漏写 | folio dirty 状态转换时 reconciliation，resize/writeback 显式同步索引 |
| T-07 | writeback 完成覆盖并发 redirty | batch 完成阶段重新读取最终状态；单页写回后的 clean → dirty writer 重新设置 tag |
| T-08 | truncate/replace 后写回已脱离 mapping 的旧 folio | 开始与完成写回时均在固定锁序下校验当前 `Arc` identity；失败不重新标脏旧 folio |
| T-10 | 大映射写回或 truncate 整段收集引用耗尽临时内存 | 固定容量 FolioBatch；dirty 游标前进；删除后复用 batch |
| T-09 | 文件增长后 cached tail 的清零结果未进入 writeback | growth 在 folio lock → state lock 下同时设置 dirty bit 和 dirty tag |

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
- truncate 等待写回时释放 mapping/folio 锁，重新加锁后复核 identity 与 writeback；
  owner 仍须串行化尺寸变化并禁止在截断范围内重新填页；
- `writeback_until` 持 folio lock 调用 backend，callback 重入同一个 `MappingPages` 会造成
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
- MappingPages/folio 双锁路径是否保持 folio → MappingPages state 顺序，索引锁下是否只允许非阻塞 try_lock；
- `writeback_until` backend callback 是否避免重入同一个 `MappingPages`；
- writeback 是否在 tree lock 外调用 backend，materialize closure 是否避免重入；
- `MappingPagesState` 是否保持 crate 内部可见，容器是否始终由 AddressSpace 直接持有。

预读标记的设置与消费要求独占 folio 访问；标记只影响性能策略，不能替代 uptodate
或 resident 身份检查。缓存历史查询返回瞬时提示，不延长驻留期或授权发布。

## Stable folio lifetime

- `Arc<Folio>` unifies physical ownership, immutable identity and the mutex that
  protects every safe content access. Public callbacks receive only a
  `FolioGuard`; its fields are private and it cannot yield or replace the owner.
- Pins refer only to this object. A replaced index cannot redirect their lock
  or accidentally retain the replacement page. The physical address is derived
  from this object's stable allocation.
- `PinnedFolio::map_readonly` consumes the pin and returns a must-use
  `MappedFolio` token. The unsafe publication contract requires EOF validation
  under the invalidate guard and retention of the returned token through PTE
  removal and completed TLB invalidation. Clone tokens share the same object.
- WRITE, DEVICE, UNCACHED and SHARED attributes are rejected before publication.
  Existing raw physical-address consumers are migrated in follow-up consumers;
  this provider change does not authorize writable user aliases.
- Copying takes a fixed single-page array, so an incorrect length cannot reach a
  kernel panic. A retained detached page remains copyable, serialized with that object's
  writers, and initializes the full output. It neither proves current EOF nor
  promises an immutable snapshot. Buffered resident traversal instead rejects
  invalidated entries because it promises current cache data.
- Ordinary shared ownership retains pages without a separate pin counter or registry.

## Cache residency lifecycle

The filesystem owns capacity policy and supplies one `CacheResidency` provider
when constructing an inode mapping. Only a winning publication calls
`try_add_folio()` under folio and index mutexes; its charge is attached to the
stable folio. Detachment, truncate and mapping destruction cannot release quota
while pins, tokens or batches retain that physical page. The last reference
frees the allocation before returning its single charge, with neither mutex
held. Failed admissions and losing allocations own no charge; failed reads keep
the resident reservation charged. A charge retains its provider past mapping
destruction, so delayed release cannot use a destroyed accounting object.
Providers must not reenter mappings, acquire folio locks, or perform I/O.
For memfs, the provider is its filesystem-owned quota, so lock order is cache
folio -> index -> quota limit; remount and statfs never acquire cache locks.
Speculative physical storage remains outside the admission policy until insertion.


Resident batch traversal reports absent identities only from its bounded index
snapshot. An existing non-uptodate or invalidated candidate is resolved again
before exposing data; initialization errors are preserved. The owner holds
invalidation shared through the batch, including EOF checks and user copies.
Only one folio is locked at a time; callbacks cannot reenter this mapping or
wait for another folio. An initialized hit takes no additional index lock.

完成对象通过内部 Arc 保留同一索引及 folio identity，最后一个完成引用释放前不销毁
索引。该保活不授予物理块映射寿命：文件系统仍须阻止 truncate/eviction 过早释放块。
完成只接受原始快照长度内的前缀；非法前缀重脏整批，成功完成不能抹掉并发重新写脏。
