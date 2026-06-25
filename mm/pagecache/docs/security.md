# pagecache — 安全与可靠性分析
## 信任模型

`pagecache` 信任调用方已经完成对象级权限校验。它不负责文件访问控制，只负责在一个已授权对象内部管理 cached folios 与长度语义。

## 外部边界 / 攻击面

- `read_into(offset, len)`
- `read_into_or_create(offset, len)`
- `write_from(offset, data)`
- `resize(len)` / `set_len(len)`
- `sync(data_only)`
- `with_folio_or_create(index, ...)`
- `add_evict_listener(...)`

这些接口都可能被不可信用户输入间接驱动，因此必须稳健处理越界、溢出和缺页分配失败。

## unsafe 代码清单

1. `Folio::new_zeroed`
   - 对分配到的整页执行 `write_bytes`
   - 不变量：页分配器返回独占、可写的一页虚拟映射

2. `Folio::data`
   - 从 `VirtAddr` 构造 `&mut [u8]`
   - 不变量：folio 独占这页内存，且调用方持有 `&mut Folio`

## 内存安全不变量

- 每个 `Folio` 独占一页缓存页。
- `Folio::data()` 只能在持有 `&mut Folio` 时调用。
- `MappingInner.pages` 中每个页索引最多对应一个 folio 实例。
- `len` 描述的是对象可见字节数，不等于缓存页数量。
- evict listener lifetime 由 `EvictRegistration` guard 管理；guard drop 后
  listener 不得再被调用。

## 线程安全

- `Mapping` 通过 `Mutex<MappingInner>` 串行化页树与长度更新。
- 单个 `Folio` 通过自己的 `Mutex` 串行化页内容访问。
- evict listener 注册和注销在 `MappingInner` 锁下完成；通知时只调用当时仍
  注册的 listener。
- 没有 `unsafe impl Send/Sync`；线程安全完全依赖现有同步原语。

## 威胁分析

1. 越界读写
   - 通过页内偏移和 `min()` 裁剪访问长度

2. 长度扩展溢出
   - 通过 `checked_add` 拒绝溢出

3. 截断后尾页数据泄漏
   - `resize`/`set_len` 对新 EOF 后的尾部清零

4. 洞页读取返回未初始化数据
   - 洞页直接返回零填充

5. address-space writeback 短写
   - `AddressSpaceOperations::writepages()` 必须把短写转换为错误；否则 dirty
     folio 可能被错误清除

6. range writeback 错误清 dirty
   - `Mapping::writeback_range()` 只有在对应 folio writeback 成功后才能清除 dirty bit

7. evict listener 泄漏或悬挂回调
   - `add_evict_listener()` 返回 RAII guard，drop 自动 unregister，不向调用者暴露
     需要手动管理的 public id

## 故障模式与影响分析（FMEA）

| 故障 | 触发条件 | 当前处理 | 影响 |
|---|---|---|---|
| 页分配失败 | 内存不足 | 返回 `KError::NoMemory` | 上层可转成 `ENOMEM` 或 fault failure |
| 长度计算溢出 | `offset + len` 溢出 | 返回 `KError::InvalidInput` | 拒绝本次写入 |
| dirty folio 被释放 | address-space 未 writeback 或 in-memory object drop | 记录警告 | 提示调用方缺少 writeback/drop 前清理 |
| filesystem 短写 | file-backed dirty folio writeback | 返回错误并保留 dirty | 防止数据丢失 |
| range writeback 失败 | `AddressSpaceOperations::writepages()` 返回错误 | 返回错误并保留当前 folio dirty | 调用者可重试 `msync` |

## 故障管理

- 本 crate 不 panic 处理普通输入错误。
- 分配失败和输入错误通过 `KResult` 返回。
- dirty folio 在 drop 时只告警，不尝试隐藏损坏。

## 已知限制

1. 仅实现最小 in-memory/file-backed mapping 模型。
2. 没有 readahead / reclaim。
3. 页树使用 `BTreeMap`，未做热点优化。
4. 还没有 Linux `i_mmap` 那样的完整反向映射树，但 `resize` 现在已经能通过 registered-view notifier 触发最小版对象级 invalidate。
5. registered file-backed views now record explicit object start bytes, so unaligned private executable prefixes are still included in object-driven truncate/invalidate coverage.

## 审计清单

- 是否所有 `unsafe` 都有具体不变量说明。
- 是否所有长度扩展都经过 `checked_add`。
- 是否截断路径对尾页剩余字节清零。
- `resize` 返回的 `TruncatePlan` 是否准确覆盖被删除页和 surviving tail。
- `affected_views()` / `invalidate_work()` 是否只包含真正覆盖 shrink 区间的 view hits，而不是无差别扩大 invalidate 范围。
- `MappingViewRange.object_start/object_len` 是否准确覆盖了 private file prefix 的对齐页前缀，而不会因为只看 `vm_pgoff` 派生值漏掉仍属于 file-backed object 的字节。
- registered view notifier 是否只拆掉超 EOF 的 present PTE，而不会错误移除仍然有效的 VMA 元数据。
- evict listener 调用方是否保存 `EvictRegistration` guard，并依赖 drop 自动注销。
- 是否洞页读取始终返回零而非未初始化内存。
- `writeback_range()` 是否只写回与请求范围相交的 dirty folio，并在失败时保留 dirty。
