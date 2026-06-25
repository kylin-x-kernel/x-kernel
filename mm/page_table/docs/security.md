# page_table — 安全与可靠性分析

## 信任模型

```
调用者（memspace, kexec, ...）
   │
   │ safe API: PageTable64::query/query_entry/modify,
   │           PageTableMut::map/unmap/remap/protect/replace_if_same/finish
   │
   v
┌──────────────────────────────────────────────────┐
│  page_table                                      │
│                                                  │
│  ┌── unsafe 边界 ──────────────────────────────┐ │
│  │ table64.rs: alloc_table() — write_bytes     │ │
│  │ table64.rs: table_of() — from_raw_parts     │ │
│  │ table64.rs: table_of_mut() — from_raw_parts │ │
│  └─────────────────────────────────────────────┘ │
│                                                  │
│  ┌── 间接 unsafe（通过 PagingHandler）─────────┐ │
│  │ H::alloc_frame() / H::dealloc_frame()       │ │
│  │ H::p2v() — 物理地址到虚拟地址转换            │ │
│  └─────────────────────────────────────────────┘ │
│                                                  │
│  ┌── 间接 unsafe（通过 PagingMetaData）────────┐ │
│  │ M::flush_tlb() — TLB 刷新                   │ │
│  │ M::vaddr_is_valid() — 地址验证               │ │
│  │ M::paddr_is_valid() — 地址验证               │ │
│  └─────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────┘
```

- **safe API 调用者**：信任模块正确维护 PTE 不变量和 TLB 一致性。
- **PagingHandler 实现者**：需保证 `p2v()` 返回有效虚拟地址、`alloc_frame()` 返回对齐的物理帧。
- **PagingMetaData 实现者**：需保证 `flush_tlb()` 真正刷新 TLB，`vaddr_is_valid()` / `paddr_is_valid()` 正确反映硬件地址约束。

## unsafe 代码清单

### 1. `alloc_table()`（`table64.rs`）

```rust
unsafe { core::ptr::write_bytes(ptr, 0, PAGE_SIZE_4K) };
```

**不变量**：`ptr` 指向 `H::alloc_frame()` 返回的物理帧经 `H::p2v()` 转换后的虚拟地址，该帧已分配且对齐到 4K，长度为 `PAGE_SIZE_4K`。

**为何安全**：`H::alloc_frame()` 保证返回有效的物理帧，`H::p2v()` 保证转换后的虚拟地址可写且唯一映射到该物理帧。

**调用者**：`PageTable64::try_new()`、`PageTableMut::next_table_mut_or_create()` — 均在帧分配成功后调用。

### 2. `table_of()`（`table64.rs`）

```rust
unsafe { core::slice::from_raw_parts(ptr, ENTRY_COUNT) }
```

**不变量**：`paddr` 指向一个有效的 4K 对齐页表帧，`H::p2v(paddr)` 返回的虚拟地址映射该帧，帧内容为 `ENTRY_COUNT` 个 `PTE`。

**为何安全**：页表帧由 `alloc_table()` 分配并清零，大小恰好为 `ENTRY_COUNT * size_of::<PTE>()` = 512 × 8 = 4096 字节。PTE 类型均满足 `Copy`，无 drop 副作用。

**调用者**：`walk_page_table!` 宏（ref 模式）、`dealloc_tree()`、`next_table()` — 均在确认 PTE present 后调用。

### 3. `table_of_mut()`（`table64.rs`）

```rust
unsafe { core::slice::from_raw_parts_mut(ptr, ENTRY_COUNT) }
```

**不变量**：同 `table_of()`，但要求调用者拥有对页表帧的独占写权限（通过 `&mut self` 借用链保证）。

**为何安全**：`PageTableMut` 持有 `&mut PageTable64`，Rust 借用规则保证无其他引用。物理帧布局同 `table_of()`。

**调用者**：`walk_page_table!` 宏（mut 模式）、`walk_page_table_create!` 宏、`next_table_mut_or_create()`、`copy_from()` — 均通过 `&mut self` 调用。

## 内存安全不变量

1. **PTE 物理地址有效性**：所有 present 的 PTE 中存储的物理地址必须指向已分配的帧，且该帧未被其他所有者释放。
2. **页表帧生命周期**：`PageTable64` 拥有 `root_paddr` 及其递归分配的所有子表帧。`Drop` 时递归释放，不遗漏不重复。
3. **TLB 一致性**：修改 PTE 后必须刷新 TLB，否则 CPU 可能使用过期的映射。`PageTableMut::finish()` 和 `Drop` 保证刷新。
4. **p2v 单射性**：`H::p2v()` 必须是物理地址到虚拟地址的单射映射，否则 `table_of` / `table_of_mut` 创建的引用可能别名。
5. **帧分配对齐**：`H::alloc_frame()` 必须返回 4K 对齐的物理地址，否则 PTE 编码和页表帧布局不正确。
6. **地址有效性**：`map`/`remap` 的物理地址必须在 `PA_MAX_ADDR` 范围内，虚拟地址必须满足架构的规范地址形式。`query`/`protect`/`unmap` 的虚拟地址同理。违反此约束返回 `PtError::InvalidAddress`。
7. **条件替换不覆盖变化状态**：`replace_if_same()` 只有在当前 leaf PTE 与 `PteSnapshot` 完全一致时才允许替换。若 PTE 已改变或不再 present，必须返回 `PteReplaceError::Changed`，不得覆盖当前映射。
8. **snapshot raw bits 封装**：`PteSnapshot` 的 raw PTE bits 只能由 `page_table` 内部用于比较。外部调用者只能依赖物理地址、权限和页大小，不能解释架构 PTE 编码。
9. **flush-before-free 显式边界**：释放从 PTE 中移除的物理页前，调用者必须显式调用 `PageTableMut::finish()`。`Drop` 是兜底刷新机制，不应用作释放旧页的隐式排序证明。

## 线程安全

| 类型 | `Send` 条件 | `Sync` 条件 |
|------|-------------|-------------|
| `PageTable64<M, PTE, H>` | 当 `M, PTE, H: Send` 时自动 `Send` | 非 `Sync`（内部无锁，`root_paddr` 可变） |
| `PageTableMut<M, PTE, H>` | 当 `PageTable64` 为 `Send` 时 `Send` | 非 `Sync`（持有 `&mut`） |
| `PagingFlags` | 自动 `Send`（`Copy`） | 自动 `Sync`（`Copy`） |
| `PageSize` | 自动 `Send`（`Copy`） | 自动 `Sync`（`Copy`） |
| `PtError` | 自动 `Send`（`Copy`） | 自动 `Sync`（`Copy`） |
| `X64PageEntry` 等 PTE | 自动 `Send`（`Copy` + `Send` 约束） | 自动 `Sync`（`Copy` + `Sync` 约束） |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | `PagingHandler::p2v()` 返回错误虚拟地址，导致 `table_of` 读写非法内存 | 高 | `p2v()` 实现有 bug 或物理地址未映射 | `PagingHandler` 实现者必须保证 `p2v()` 的正确性；内核启动阶段验证直接映射 |
| T-02 | `PagingHandler::alloc_frame()` 返回已使用的帧，导致页表帧别名 | 高 | 帧分配器 bug（双重分配） | 帧分配器需保证唯一性；`alloc_table()` 清零帧内容可部分检测（若原帧非零） |
| T-03 | 修改 PTE 后未刷新 TLB，CPU 使用过期映射 | 高 | 绕过 `PageTableMut` 直接修改内存 | 所有修改通过 `PageTableMut` 进行，`Drop` 自动刷新；禁止直接操作 PTE 内存 |
| T-04 | `PageTable64` 在多线程间共享且无外部同步，导致数据竞争 | 高 | `PageTable64` 非 `Sync`，但通过 `unsafe` 强制共享 | 类型系统阻止 `&PageTable64` 跨线程；调用者需使用 `Mutex` 等外部同步 |
| T-05 | `copy_from` 后源页表被 drop，目标页表中的借入条目指向已释放帧 | 高 | `feature = "copy-from"` 时源页表先于目标页表 drop | `borrowed_entries` bitmap 标记借入条目，`Drop` 时跳过释放；调用者必须保证源页表生命周期覆盖目标 |
| T-06 | `map_region` 部分映射失败，已映射的页面未回滚 | 中 | `map_region` 中间某页返回 `AlreadyMapped` 或 `NoMemory` | 调用者需自行处理部分映射状态；建议先 `unmap_region` 再 `map_region` |
| T-07 | SEV C-bit 位置配置错误，导致加密内存明文访问 | 高 | `kbuild_config::SEV_CBIT_POS` 与实际硬件不一致 | C-bit 位置由构建配置决定，需与硬件匹配；`EncodedPtePhys` 封装了所有 C-bit 操作 |
| T-08 | 无效虚拟地址传入 map/query 等操作，导致越权映射或非法内存访问 | 高 | 调用者传入非规范形式地址或超出地址空间范围的地址 | `map`/`remap`/`query`/`protect`/`unmap` 入口处检查 `vaddr_is_valid()` 和 `paddr_is_valid()`，违反则返回 `InvalidAddress` |
| T-09 | COW 提交期间 PTE 被其他路径修改后仍被覆盖 | 高 | fault 路径复制页面后使用无条件 `remap()` 提交 | 使用 `query_entry()` + `replace_if_same()`；Changed 时不写 PTE，由调用者 abort prepared page 并 retry |
| T-10 | PTE 已清除但旧物理页在 TLB flush 前被释放并复用 | 高 | unmap/COW replace 后立即释放对象页，依赖 `PageTableMut::drop` 稍后刷新 | 释放前显式调用 `PageTableMut::finish()`，以 `TlbFlushReceipt` 作为资源释放排序边界 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `alloc_table()` 帧分配失败 | 物理内存耗尽 | 无法创建新页表或映射 | 新进程/地址空间创建失败 | 2 | 返回 `PtError::NoMemory`，调用者可回收内存后重试 |
| F-02 | `unmap` 对未映射地址调用 | 调用者逻辑错误 | 返回 `NotMapped` | 无影响 | 4 | 返回错误码，调用者应检查 |
| F-03 | `map` 对已映射地址调用 | 调用者重复映射 | 返回 `AlreadyMapped` | 无影响 | 4 | 返回错误码，防止静默覆盖 |
| F-04 | `dealloc_tree` 递归深度过大 | 页表层级深且条目多 | 栈溢出 | 系统崩溃 | 1 | 4 级页表最大递归深度为 4，每级 512 条目，实际递归深度可控 |
| F-05 | TLB shootdown IPI 丢失 | SMP 模式下远端 CPU 未响应 | 远端 CPU 使用过期映射 | 内存一致性违反 | 1 | IPI 机制需保证可靠投递；AArch64 使用硬件 TLBI 无此问题 |
| F-06 | `protect_region` 跳过未映射页 | 区域中部分页未映射 | 权限修改不完整 | 安全策略未完全生效 | 2 | 未映射页静默跳过（按 4K 推进），调用者应确保区域完整映射 |
| F-07 | `remap` 修改正在使用的页表项 | 并发访问同一地址空间 | TLB 不一致 | 数据损坏 | 1 | 调用者需通过外部同步保证独占访问 |
| F-08 | 无效地址传入 map/query | 调用者未校验地址范围 | 返回 `InvalidAddress` | 操作被拒绝 | 4 | 入口处检查 `vaddr_is_valid`/`paddr_is_valid`，返回错误码 |
| F-09 | `unmap` 对非 present 条目调用 `clear()` | 逻辑错误：先 clear 再检查 present | 非法修改 PTE 保留位，破坏架构特定状态 | 硬件行为未定义 | 2 | 修复为先检查 `is_present()` 再 `clear()`；非 present 条目直接返回 `NotMapped` |
| F-10 | `map_region` 中途 panic | `phys_getter` 闭包 panic | 部分映射已生效，TLB 未刷新 | TLB 不一致 | 2 | `PageTableMut::Drop` 保证刷新已记录的地址；调用者应确保 `phys_getter` 不 panic |
| F-11 | 中断上下文中调用 map/unmap | 调用者在中断处理程序中操作页表 | 可能死锁（若持锁）或 TLB 刷新不完整 | 系统挂起 | 1 | 调用者需确保不在中断上下文中使用 `PageTableMut`；文档标注调用约束 |
| F-12 | `replace_if_same` 返回 Changed 后调用者泄漏已准备页面 | 调用者未执行 abort/cleanup | 单页或对象引用泄漏 | 长期内存泄漏 | 2 | `page_table` 只保证不覆盖当前 PTE；COW/anon/file 对象层必须在 Changed 分支释放 prepared resource |
| F-13 | 调用者忽略显式 flush finalization 并提前释放 unmapped frame | 资源释放顺序错误 | stale TLB 可能继续访问已复用 frame | 隔离破坏或数据损坏 | 1 | 释放路径必须在 `finish()` 返回后执行；审计所有 `dealloc_frame` 与 object-page release 调用点 |

## 故障管理

- **错误码**：`PtError` 枚举覆盖 6 种故障场景（`NoMemory`、`NotAligned`、`NotMapped`、`AlreadyMapped`、`MappedToHugePage`、`InvalidAddress`），`feature = "kerrno"` 时可转换为 `KError`。条件替换额外使用 `PteReplaceError` 区分页表错误和 PTE 已变化的 retry-class 状态。
- **TLB finalization**：`PageTableMut::finish()` 返回 `TlbFlushReceipt`，表示调用点已经完成一次 flush boundary。它不是错误码，也不承诺具体架构 flush 方式。
- **Panic 策略**：本模块无 panic 路径。`walk_page_table!` 中的 `unreachable!` 仅在 `LEVELS` 不为 3 或 4 时触发，而所有架构实现均满足此约束。
- **故障恢复**：所有错误通过 `PtResult` 返回，调用者可决定重试或降级。

## 隐私分析

本模块不直接处理用户数据，但管理虚拟地址映射，直接影响用户进程的内存隔离。
需确保：
- 不同地址空间的页表相互独立
- 用户态页表不包含内核映射（或通过 KPTI 隔离）
- SEV C-bit 正确设置，防止加密内存泄露
- 无效地址无法通过 map 建立越权映射

## 已知限制

1. `PageTable64` 非 `Sync`，多线程共享需外部加锁，当前无内置锁支持。
2. `map_region` 部分失败时不回滚已映射页面，调用者需自行处理。
3. `copy_from` 功能要求源页表生命周期覆盖目标页表，无编译期检查。
4. `walk_page_table!` 宏仅支持 3 级和 4 级页表，5 级页表（如 x86_64 LA57）需扩展。
5. `dealloc_tree` 使用递归释放，极端情况下可能导致栈溢出（实际 4 级深度可控）。
6. `vaddr_is_valid` / `paddr_is_valid` 的正确性依赖 `PagingMetaData` 实现者，无编译期保证。

## 审计清单

修改本模块时需验证：

- [ ] 每个 `unsafe` 块均有 `SAFETY:` 注释
- [ ] 新增 PTE 修改路径后 TLB 刷新未被遗漏
- [ ] 新增条件替换路径后，Changed 分支不会覆盖当前 PTE
- [ ] 新增释放旧物理页路径后，释放发生在显式 `PageTableMut::finish()` 之后
- [ ] `PagingHandler` 实现满足 `p2v` 单射性和帧分配对齐要求
- [ ] `Drop` 实现正确处理所有状态（含 `copy-from` 借入条目）
- [ ] 新增 `PagingFlags` 变体后所有架构的 `From` 转换已更新
- [ ] `walk_page_table!` 宏变更后 3 级和 4 级路径均正确
- [ ] SMP TLB shootdown 路径在 `feature = "smp"` 下已测试
- [ ] `map`/`remap` 入口的 `paddr_is_valid` / `vaddr_is_valid` 检查未被绕过
