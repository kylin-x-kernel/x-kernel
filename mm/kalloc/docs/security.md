# kalloc — 安全与可靠性分析

## 概述

`kalloc` 位于内核内存 ownership 的基础层。它把未经类型化的虚拟地址 region 转换为
Rust heap object、连续页和 DMA32 页，并通过 raw pointer、地址算术、底层 allocator
contract 和 per-CPU mutable state 维护这些资源。这里的不变量一旦破坏，影响通常不是
局部 allocation 失败，而是 freelist 损坏、页重复分配、越界 DMA 或内核未定义行为。

本文件分析整个 crate，包括中央 byte/page allocator、页级 PCP、slab object cache、
`GlobalPage`、usage、tracking 和可选 C ABI；per-CPU slab cache 只是其中一个安全边界。

## 信任模型

```text
boot memory map / kernel callers / C components / DMA users
                         |
                         | region, Layout, pointer, page count, address
                         v
                    kalloc safe/unsafe API
                         |
          +--------------+---------------+
          |                              |
          v                              v
  alloc-engine contracts          per-CPU storage + IRQ model
          |                              |
          +--------------+---------------+
                         v
               allocator-owned kernel memory
```

`kalloc` 信任以下条件：

1. 启动流程传入的 `[va, va + size)` 已映射、可写、独占，包含至少一个完整 4 KiB 页，
   且不同 region 不重叠；`va` 按页对齐，`p2v` / `v2p` 对这些地址有效。
2. deallocation caller 使用原 allocation 的起始地址、layout 或页数，只释放一次，并
   保持相同的 `UsageKind`。
3. `alloc-engine` 返回的地址满足请求的大小、对齐、非空性和唯一 ownership，并接受
   与 allocation 匹配的 deallocation。
4. 当前 CPU 的 per-CPU storage 已安装；普通中断和 preemption 约束按 API 要求执行；
   NMI 不进入 allocator。
5. C ABI caller 是受信任的内核组件，向 unsafe symbols 传入合法 pointer、长度和
   allocation provenance。
6. DMA caller 只把返回的 DMA range 交给获准访问该区域的设备，并由上层完成页属性、
   share/unshare 和设备同步。

`kalloc` 不验证任意地址是否真正属于某个 allocator，也不保存足以在 production 中
检测所有 wrong-layout free、double free 或跨 allocator free 的逐 allocation metadata。

## 外部边界 / 攻击面

| 边界 | 输入 | 当前处理 | 剩余风险 |
|------|------|----------|----------|
| 启动内存 | `kruntime` 从 firmware/boot memory 描述中筛选的 free region，经 `p2v` 传入 | 底层 buddy 拒绝未页对齐、过小、超出 region 数上限或与既有 region 重叠的输入；首个 region 建立 page/byte allocator | 无法验证地址是否真实映射、可写、属于 free RAM 或与 allocator 外部 owner 冲突；端点算术仍依赖可信范围 |
| Rust 内核 caller | `Layout`、raw allocation pointer、页数、alignment、固定地址、`UsageKind` | allocation 返回 `Result`；部分大小计算和汇总 usage subtraction 使用 checked arithmetic | safe 外观的部分 deallocation API 仍依赖 caller contract，错误参数可能导致 UB |
| DMA / 设备 | 连续页数量、alignment、虚实地址转换结果 | allocation 后检查整个物理区间低于 4 GiB，不满足则归还 | 没有 DMA32 zone；错误 `v2p`、区间溢出或设备越权仍可导致错误 DMA |
| TEE/DICE C ABI | C pointer、signed length、allocation/free 配对 | null、部分长度和 `Layout` 构造检查；symbols 标为 unsafe | 无法验证 raw region 真实长度、重叠和 provenance，恶意或损坏的 C caller 可造成任意内存破坏 |
| Tracking/观测 | enable/disable、generation range、visitor | feature-gated、全局锁和递归 guard | backtrace/地址可能泄露内核布局；visitor 重入 tracking API 可能死锁或 panic |

本 crate 不直接解析用户地址，不直接访问 MMIO/PIO，也不直接处理文件系统、网络或 IPC
payload。用户 workload 可以间接驱动 allocation 压力；可选 tracking 信息由上层
`/dev/memtrack` 等接口暴露时，上层必须另行建立权限边界。

## unsafe 代码清单

### U-01：Rust `GlobalAlloc` 与 raw allocation pointer

位置：`src/lib.rs` 中 `unsafe impl GlobalAlloc for GlobalAllocator`、`large_alloc()`。

主要操作：

- 把 page allocator 返回的非零整数地址构造成 `NonNull<u8>`；
- 实现 `GlobalAlloc::alloc` / `dealloc` 的 raw pointer ABI；
- 将 allocation failure 转交 `handle_alloc_error()`。

依赖不变量：底层 allocator 返回有效、对齐、独占且覆盖完整 layout 的地址；dealloc
收到的 pointer/layout 必须与原 allocation 匹配。内部 `balloc` / `palloc` 锁和
per-CPU guard 保护 metadata 并发，但不能修复 caller 伪造的 pointer。

### U-02：per-CPU slab intrusive list

位置：`src/slab_cache.rs` 中 `ObjectCache::pop()`、`push_unchecked()`、`try_push()`。

主要操作：把 cached object 的起始地址 cast 为 `Option<NonNull<u8>>`，从首 word
读取或写入 next link。

依赖不变量：

- object 是对应 canonical class layout 的 live、可写、独占 allocation；
- caller 已结束所有访问，且 object 不在其他 cache/freelist；
- object size/alignment 足以容纳 link；编译期断言验证 link 不超过最小 8 字节 class；
- list mutation 期间 local IRQ 保持关闭，远程 CPU 不修改本 CPU list。

`GlobalAllocator::alloc_from_percpu_slab()` 和 `dealloc()` 建立 IRQ guard 并按
`SizeClass` 路由；`with_object_cache!` 保持 enum class 与 `ObjectCache<N>` 一致。

### U-03：slab refill/drain ownership

位置：`src/slab_cache.rs` 中 `ObjectCache::drain()`、`PerCpuSlabCache::{try_free,drain}`
和 crate-private current-CPU cache API。

主要操作：在 per-CPU cache、caller 和中央 `ByteAllocator` 之间转移 raw object，并以
canonical layout 调用中央 deallocation。

依赖不变量：中央 allocator lock 已由 caller 持有；所有 cached object 最初由同一中央
allocator 使用相同 class layout 分配；current-CPU mutable reference 不发生同 CPU
重入。违反这些条件可能把对象放进错误 freelist 或制造 mutable alias。

### U-04：`GlobalPage` raw ownership 和 slice 构造

位置：`src/page.rs` 中 `GlobalPage::from_raw()`、`fill()`、`as_slice()`、
`as_slice_mut()`。

主要操作：从 raw VA/page count 重建 RAII owner，使用 `write_bytes`，并从 raw pointer
构造 slice。

依赖不变量：`GlobalPage` 唯一拥有从 `start_va` 开始的 `num_pages * 4096` 字节 live
allocation；该范围保持映射，且 `&mut self` 时不存在 aliasing access。safe constructors
由 page allocator 建立这些条件；unsafe `from_raw()` 的 caller 必须自行建立。

### U-05：C allocation header

位置：`src/ffi.rs` 中 `malloc()`、`calloc()` 和 `free()`。

主要操作：在 allocation 首 word 写入 user size，向 caller 返回 header 后的地址；free
时执行 pointer subtraction、读取 header、用 unchecked layout 和 `NonNull` 重建原始
allocation。

依赖不变量：pointer 必须来自本模块且只 free 一次；header 未被越界写破坏；size 算术和
alignment 与 allocation 时一致。null free 被接受，其他 provenance 不做运行时验证。

### U-06：C memory operations

位置：`src/ffi.rs` 中 `__memcpy_chk()`、`memcpy()`、`memmove()`、`memset()` 和
`memcmp()`。

主要操作：对 caller 提供的 raw region 逐字节 read/write 或执行 non-overlapping copy。

依赖不变量：pointer 对指定长度有效且满足读写权限；`memcpy` 输入不重叠；`memmove`
允许重叠但长度必须真实。null 和 `__memcpy_chk` 的 destination-size 检查只是部分防护，
不能证明 raw region 的有效性。

### U-07：per-CPU usage shared references

位置：`src/lib.rs` 中 `with_current_usage_counters()` 和 `usage_snapshot()`。

主要操作：通过 `current_ref_raw()` / `remote_ref_raw()` 取得 per-CPU usage slot 的共享
引用，并从 allocation/free 热路径或跨 CPU 汇总路径访问其中的 atomic counters。

依赖不变量：主 CPU 在 allocator 启用前已经初始化全部 per-CPU area；current-CPU
引用解析期间由 `NoPreempt` 固定执行 CPU；remote index 小于 `percpu_area_num()`；所有
并发访问只通过 atomic 字段和共享引用完成，且 runtime 不并发重置 per-CPU area。

## 内存安全不变量

以下条件必须始终成立：

1. 每个加入 allocator 的 region 都有页对齐 base，覆盖至少一个已映射、可写、独占的
   完整 4 KiB 页，且其实际纳入部分与既有 region 不重叠；非整页尾部不被管理。
2. 每个 allocation 在任意时刻只有一个 owner：caller、某个 per-CPU cache，或中央
   allocator freelist；不能同时处于两个状态。
3. byte heap backing page 一旦交给 `balloc`，在其整个生命周期内不得再由 `palloc`
   分配给其他用途。
4. page allocation/free 使用相同起始地址和页数；byte allocation/free 使用能映射到
   同一底层分配类别的 layout。
5. PCP 中每个地址都表示对应 `NR_PAGES` 的完整、页对齐、连续 block；3 页等非
   power-of-two block 的 buddy rounding slack 已经归还。
6. slab cache 中每个 object 都属于对应 `SizeClass`，使用 canonical layout 分配，其首
   word 只在 caller 已经 relinquish ownership 后作为 next link。
7. current-CPU cache mutation 不得与同 CPU interrupt/NMI 或 remote walker 并发。
8. `GlobalPage` 始终唯一拥有其记录的范围，且 `Drop` 只执行一次。
9. C allocation 的 hidden header 不被 caller 覆盖，返回 pointer 只能由配对的 C
   `free` 处理。
10. usage slot 只通过 atomic shared access 更新；per-CPU area 不得在 accounting 运行时
    被重新初始化。
11. usage/tracking metadata 只是诊断信息，不得用来替代 allocator provenance 或
    ownership 判断。

## 线程安全

| 共享状态 | 同步条件 | 不能覆盖的情况 |
|----------|----------|----------------|
| `DefaultByteAllocator` | `balloc: SpinNoIrq<_>` | NMI 重入、错误 raw pointer/layout |
| `BuddyPageAllocator` | `palloc: SpinNoIrq<_>` | NMI 重入、错误 region/page contract |
| PCP | local IRQ off + current-CPU storage | NMI、CPU offline/remote inspection |
| slab object cache | local IRQ off + current-CPU storage | NMI、CPU offline/remote inspection |
| ready flags | Release store / Acquire load | 并发重复初始化的高层协议错误 |
| 各类 usage | per-CPU cumulative atomics；free Release / snapshot Acquire | 跨 CPU/跨种类线性化快照、allocator metadata publication、即时 wrong-kind free 检测 |
| tracking map | `SpinNoIrq<GlobalState>` + per-CPU recursion guard | visitor 对 tracking state 的递归获取 |

既定嵌套锁顺序是 tracking state（启用时）→ `balloc` → `palloc`。byte heap 扩容是
`balloc -> palloc`；其他路径不得持有 `palloc` 再获取 `balloc`。usage accounting 不增加
锁嵌套。普通 IRQ 通过 `SpinNoIrq` 或显式 `IrqSave` 排除；NMI 不在支持范围。

allocator 初始化由 boot CPU 串行完成。ready atomics 用于发布已完成状态，不把多个
并发 `global_init()` / `add_memory()` 调用变成安全的 region 初始化协议。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 无效、重叠或保留内存 region 被加入 allocator | 高 | boot memory 分类、`p2v` 或 caller 参数错误 | `kruntime` 只传递标记为 `FREE` 的 region；底层 buddy 检查页对齐、最小大小和内部 region 重叠。映射有效性、外部 ownership 和端点范围仍由启动边界保证 |
| T-02 | wrong-layout、wrong-count、double free 或跨 allocator free 损坏 freelist | 高 | kernel caller 违反 allocation/deallocation 配对 | API rustdoc 和 RAII `GlobalPage` 限制正常路径，usage 汇总 underflow 可发现部分总量错误；没有即时或完整 provenance 检测，残余风险仍为高 |
| T-03 | slab intrusive link 被越界写、重复插入或并发修改 | 高 | stale alias、错误 size class、double free、NMI 重入 | canonical layout、compile-time link-size check、local IRQ guard 和中央锁保护正常路径；NMI 和伪造 pointer 明确不支持 |
| T-04 | PCP 保存错误页地址或页数，随后重复分配/错误合并 | 高 | caller 用错误 `num_pages` free，或 CPU-local 并发约束被破坏 | 固定的 1–4 页 cache 类型和 `split_to_chunks` 保持内部 bookkeeping；输入 provenance 仍由 caller 保证 |
| T-05 | C ABI raw pointer/length 导致越界读写或 header 破坏 | 高 | TEE/DICE C caller 传入无效、负值转换后的长度、重叠 memcpy 或错误 free pointer | unsafe ABI contract、null/部分 bounds 检查和配对 header；无法验证真实对象边界，接口只允许受信任组件调用 |
| T-06 | DMA 地址超出设备可寻址范围或指向错误物理页 | 高 | `v2p` 错误、地址区间算术异常、caller/device 生命周期错误 | `alloc_dma_pages()` 检查结束地址低于 4 GiB 并在失败时归还；上层 `kdma` 管理页属性和平台 share/release，仍依赖正确设备授权 |
| T-07 | 锁递归、锁顺序反转或 NMI 打断导致永久自旋 | 中 | allocator 在 NMI 中使用，新增路径形成 `palloc -> balloc`，或 tracking visitor 再锁 tracking state | `SpinNoIrq` 排除普通 IRQ、文档固定锁序、tracker 有 allocation recursion guard；NMI 与 tracking callback 重入仍是剩余限制 |
| T-08 | per-CPU cache 隐藏可复用内存，促使中央 allocator 提前扩容或 OOM | 中 | CPU/class 工作集不均衡、内存压力、CPU 停止运行 | PCP 固定 entry 容量；slab cache 使用最多 32 对象的 batch 和不足两个 batch 的高水位；没有全 CPU reclaim，风险只做到有界而未消除 |
| T-09 | 大小、页数、usage 或地址算术溢出 | 中 | 极端 caller 输入、错误 region 或 accounting 不配对 | `pages_to_bytes`、per-CPU 累计量和 usage 汇总 subtraction 使用 checked arithmetic，`Layout` 验证部分输入；部分地址端点计算仍依赖可信内核范围 |
| T-10 | tracking 泄露内核地址/backtrace 或显著改变时序 | 中 | 未授权读取上层 memtrack 输出，或把 tracking 性能当作正常路径 | tracking 是 feature-gated 诊断功能；访问控制由暴露接口负责，性能分析必须区分 tracking 状态 |
| T-11 | 同时选择多个 byte allocator feature，实际实现与配置认知不一致 | 低 | 非 Kconfig 构建错误启用多个 feature | 正常构建由 Kconfig 选择一种；未选择会编译失败，同时选择目前依赖 `cfg_if!` 分支顺序，需由构建审计保证 |
| T-12 | 把非线性化 usage/central stats 用于安全或 reclaim 决策 | 低 | caller 把诊断快照误当精确 ownership | API 将其定位为统计；文档区分 caller-live、byte central 和 page central 视图，未提供强一致保证 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | allocator 在 page region 就绪前被调用 | 启动顺序错误或无可用 `FREE` region | debug assertion、allocation failure 或访问未初始化 allocator | 早期启动失败 | 1 | `kruntime` 在普通内核服务前初始化；页表路径通过 `is_page_allocator_ready()` 断言 |
| F-02 | byte heap 或 page buddy 无法满足请求 | 内存耗尽、碎片化或 cache residency | 显式 API 返回 `AllocError` | Rust `GlobalAlloc` 路径记录 OOM 后进入 `handle_alloc_error`，通常终止当前内核运行 | 2 | byte heap 减小扩容请求重试；PCP/slab 允许 partial refill；显式 API 向上返回错误 |
| F-03 | PCP batch 大块申请失败 | buddy 碎片化 | 无法一次取得目标 batch | 性能下降，不一定 allocation 失败 | 4 | 自动退化为逐 block refill，只有一个 block 都拿不到才返回错误 |
| F-04 | slab refill 中央 allocator 中途耗尽 | byte heap 压力或碎片化 | partial cache | 当前 allocation 优先从 partial refill 返回；完全失败时扩展 byte heap | 3 | 保留已取得对象，不回滚；外层执行 heap growth 后重试 |
| F-05 | DMA allocation 落在 4 GiB 以上 | buddy 没有低端定向策略 | 本次 DMA allocation 被拒绝 | 设备初始化或 I/O 失败，即使低端内存可能仍存在 | 3 | 立即归还不合格页并返回 `NoMemory`；后续可引入 DMA32 zone |
| F-06 | usage overflow/underflow | lifetime 累计量溢出、wrong-kind/free 配对或计数异常 | 热路径累计溢出或汇总下溢时 panic | 可能导致内核停止 | 2 | checked arithmetic fail-fast；usage 不参与 allocator metadata mutation |
| F-07 | cache full 触发 batch drain | 正常突发 free | 最多 32 次中央 deallocation 持有锁并保持 IRQ-off | IRQ latency 或跨 CPU allocation latency 尖峰 | 3 | 对象数和水位提供明确上界；仍需 workload 实测和调参 |
| F-08 | 持锁 CPU 停滞 | panic、NMI、硬件问题或错误重入 | 其他 CPU 在中央 allocator 锁上自旋 | 系统级 allocation hang | 1 | 临界区不睡眠；普通 IRQ 被关闭；NMI allocation 禁止，新增路径必须遵守锁序 |
| F-09 | tracking visitor 重入不兼容 API | visitor 调用 `current_generation()` 等再次获取 `STATE` | 自旋死锁或 `None.unwrap()` panic | 诊断接口挂起，可能影响系统 | 2 | allocation recursion 由 per-CPU flag 跳过；visitor 必须短小且不得递归进入 tracking state |
| F-10 | `GlobalPage::from_raw()` ownership 错误 | 同一页已有 owner 或页数不匹配 | slice/fill/Drop 操作错误范围 | double free 或内存破坏 | 1 | API 为 unsafe，优先使用 safe constructors；caller 必须证明唯一 ownership |

## 故障管理

- `GlobalAllocator::{alloc,alloc_pages,alloc_dma_pages,alloc_pages_at,add_memory}` 使用
  `AllocResult` 报告 `NoMemory`、`InvalidInput` 等错误。
- Rust `GlobalAlloc::alloc` 不能把普通失败静默传给大多数 Rust allocation；它记录
  layout 和中央 heap 状态后调用 `handle_alloc_error()`。
- `global_init()`、`init_page_allocator()` 和部分内部不变量使用 assert/expect，启动或
  allocator metadata 错误按 fail-fast 处理；内核不尝试在此类错误后继续运行。
- PCP 和 slab refill 接受 partial progress，避免因为未达到 batch 目标而丢弃已经取得的
  block/object。
- DMA32 检查失败会先把不合格 allocation 归还 buddy，再返回错误。
- per-CPU usage 累计量使用 checked addition；总释放量超过总分配量在 `usages()` 汇总时
  fail-fast，但这不是即时或完整的 pointer provenance 检查。
- 自旋锁没有面向 allocator corruption 的恢复协议；一旦 unsafe 不变量破坏，不能认为
  后续错误返回仍可保持系统安全。

## 隐私分析

普通 allocator 把 payload 视为不透明字节，不主动读取 caller 的 live 数据。但是：

- 普通 allocation 不保证清零；之前 owner 的内容可能残留，处理敏感数据的 caller 必须
  使用 `GlobalPage::alloc_zero()`、C `calloc()` 或显式清零；
- slab cache 会在已经 free 的 object 首 word 写入 next link，其余内容不会自动擦除；
- PCP 缓存页也不会擦除 payload；
- tracking 保存 allocation 地址和 backtrace。上层若将其暴露给用户态，可能泄露内核
  地址布局、调用关系和工作负载信息，必须限制访问并考虑地址脱敏。

`memcpy`/`memmove`/`memset` 等 C symbols 会按受信 caller 请求处理 payload，但不解析
其语义，也不提供跨安全域的数据清理保证。

## 已知限制

1. **无统一 reclaim**：byte heap backing 不整体归还 page allocator；PCP 和 slab cache
   没有全 CPU freeze/flush/shrink 接口。cache 中资源只有达到本地高水位时才批量归还。
2. **无 CPU hotplug lifecycle**：runtime offline CPU 前不会 drain 其 PCP 或 slab cache。
3. **不支持 NMI**：local IRQ guard 和 `SpinNoIrq` 不能排除 NMI 重入。
4. **无 allocation context policy**：不区分 task、softirq、hardirq，也没有 GFP-like
   nonblocking/reclaim flags；slow path 可能持中央锁、批量循环并扩展 heap。
5. **无 NUMA、memory cgroup 或 pressure-aware 水位**：所有中央内存共用一个 page buddy
   和一个 byte allocator。
6. **DMA32 采用 allocation 后检查**：没有低端 zone 或定向搜索，可能出现可用低端页
   尚存但请求仍失败的情况。
7. **deallocation provenance 不做 production 验证**：错误 pointer、layout、页数、
   `UsageKind` 或 double free 主要依赖 caller contract；部分公开 safe 方法的约束无法由
   类型系统强制。
8. **统计不是强一致 ownership 视图**：per-CPU cache、byte heap backing、页对齐和
   per-CPU 累计量的分阶段读取会使 `usages()`、`used_bytes()`、`used_pages()` 的含义
   不同；`usages()` 不保证跨 CPU/跨种类线性化，并发变化可能令结果暂时偏大或偏小。
9. **tracking 有全局锁和回调约束**：会改变 allocation latency；`allocations_in()` 要求
   tracking 已启用，visitor 在锁内运行且不得递归获取 tracking state。启停边界附近的
   allocation/free 记录不构成事务快照，停用期间的 free 不会清除既有记录。
10. **C ABI 只做有限输入检查**：raw pointer 真实长度、provenance 和 signed length 语义
    依赖受信 C caller。
11. **默认不清零内存**：除 `alloc_zero()`、`calloc()` 或 caller 显式清零外，重用内存
    可能保留前一 owner 的数据。
12. **slab object cache 水位尚未实测调优**：batch 同时受 2 页 payload 和 32 对象限制，
    高水位不足两个 batch；需结合 X-Kernel workload 的 hit rate、lock-stat、内存保留和
    IRQ latency 调整。

## 审计清单

修改 `kalloc` 时应确认：

- [ ] 新的 memory region 入口是否验证映射、页对齐、独占性、重叠和地址端点溢出？
- [ ] allocation/deallocation 是否在所有分支保持 pointer、layout、页数和 `UsageKind`
      配对？
- [ ] byte heap backing 是否只在 `balloc` 与 `palloc` 之间转移一次 ownership？
- [ ] 新增 page fast path 是否正确处理 alignment、非 power-of-two slack 和 buddy chunk？
- [ ] 每个 current-CPU PCP/slab mutable access 是否覆盖完整 local IRQ-off/preemption-safe
      区间？是否可能从 NMI 或 remote CPU 访问？
- [ ] slab cache object 是否始终使用对应 class 的 canonical layout，且首 word 只在 caller
      relinquish 后改写？
- [ ] 新增锁路径是否遵守 tracking → `balloc` → `palloc` 顺序，且不在锁内睡眠？
- [ ] 所有 unsafe block 是否有可追溯 `SAFETY:` 说明，safe wrapper 是否真正建立其前提？
- [ ] `GlobalPage` 新增构造或转换是否保持唯一 ownership、有效映射和精确 Drop 页数？
- [ ] DMA 端点计算是否 checked，整段物理范围是否满足设备限制，失败是否完整回滚？
- [ ] C ABI 新增 symbol 是否明确 pointer、length、overlap、header 和 free 配对 contract？
- [ ] tracking visitor 是否在持锁期间调用，是否存在递归 state lock 或敏感地址泄露？
- [ ] 新增统计是否明确计量单位和层次，且没有被用作 allocator correctness/reclaim 条件？
- [ ] 新增 cache 或 allocator 状态是否同步更新初始化、CPU hotplug、reclaim 和关机策略？
