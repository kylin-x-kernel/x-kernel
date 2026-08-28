# kalloc — 设计文档

## 定位

`kalloc` 是 x-kernel 的内核全局内存分配层。它接收启动阶段交付的空闲内存区域，
向上提供 Rust 全局堆、连续页、DMA32 页和 RAII 页对象，并负责把这些请求路由到
byte allocator、全局 page buddy 以及相应的 per-CPU cache。

主要使用者包括：

- Rust `alloc` 生态以及内核中使用 `Box`、`Vec`、`Arc` 等类型的代码；
- 页表、用户虚拟内存、page cache 和通用连续页使用者；
- `kdma` 等需要 DMA32 物理地址的子系统；
- 使用 [`GlobalPage`](../src/page.rs) 管理连续页生命周期的内核组件；
- 可选的 allocation tracking 和 TEE/DICE C ABI 兼容层。

## 背景

x-kernel 运行在 `no_std` 环境中，既需要满足 Rust `GlobalAlloc` 接口，也需要直接
管理启动内存和不同用途的连续页。单一分配策略无法同时兼顾小对象、页粒度、特殊
对齐、DMA 地址限制和多 CPU 热路径，因此 `kalloc` 采用两级中央 allocator，并在
页和 slab object 两个层次设置 per-CPU cache：

- `BuddyPageAllocator<4096>` 是所有普通内存区域的最终 owner；
- Kconfig 选择的 byte allocator 管理从 page allocator 取得的 heap region；
- 页级 PCP 缓存常用的 1–4 页连续 block；
- slab 配置下的 per-CPU object cache 缓存 8–2048 字节对象。

per-CPU slab cache 是当前 object-cache 设计的初版。它减少稳定小对象工作集对中央
byte allocator 锁的竞争，但不取代中央 allocator，也不提供完整 reclaim、CPU
hotplug 或 NMI 支持。

## 范围

本设计覆盖：

```text
mm/kalloc/
├── Cargo.toml
├── src/
│   ├── lib.rs          # 全局 allocator、初始化、路由、统计和 GlobalAlloc
│   ├── page.rs         # GlobalPage RAII 封装
│   ├── pcp.rs          # 1–4 页 per-CPU page cache
│   ├── slab_cache.rs   # slab 配置下的 per-CPU object cache
│   ├── tracking.rs     # 可选 allocation tracking
│   └── ffi.rs          # TEE/DICE 配置下的 C ABI allocation/memory shims
└── docs/
    ├── design.md
    └── security.md
```

底层 buddy、slab、TLSF 等具体算法属于 `mm/alloc-engine`，本文只描述 `kalloc` 对
它们的组合、同步和调用约束。

## 配置

构建必须通过 Kconfig 选择一种 byte allocator：

| feature | `DefaultByteAllocator` | per-CPU slab object cache |
|---------|------------------------|---------------------------|
| `slab` | `SlabByteAllocator` | 启用 |
| `buddy` | `BuddyByteAllocator` | 不启用 |
| `tlsf` | `TlsfByteAllocator` | 不启用 |

未选择任何一种会触发 `compile_error!`。工程配置约定只选择一种；当前源码没有对
“同时选择多种”单独报错，而是由 `cfg_if!` 的分支顺序选中一种。

其他可选能力：

- `tracking`：记录 Rust 全局堆 allocation 的 layout、backtrace 和 generation；
- `tee` / `dice`：编译 `ffi.rs` 中的 C allocation 与 memory symbols。

## 架构

```text
                       Rust alloc / kernel callers / C shims
                                      |
                                      v
                              GlobalAllocator
                                      |
              +-----------------------+-----------------------+
              |                                               |
              | byte / Rust heap                              | page APIs
              v                                               v
   slab per-CPU object cache                         per-CPU page set (PCP)
       (slab feature only)                              1 / 2 / 3 / 4 pages
              |                                               |
              | miss/full                                     | miss/full
              v                                               v
 SpinNoIrq<DefaultByteAllocator>                 SpinNoIrq<BuddyPageAllocator>
              |                                               ^
              | heap growth                                   |
              +-----------------------------------------------+

Other paths:
  large Rust allocation ---------------------------> page allocator
  DMA32 / alloc_pages_at --------------------------> page allocator directly
  GlobalPage --------------------------------------> page APIs + Drop
  tracking ----------------------------------------> wraps Rust GlobalAlloc path
```

`GlobalAllocator` 的主要状态为：

| 状态 | 保护方式 | 含义 |
|------|----------|------|
| `balloc` | `SpinNoIrq` | 当前配置的中央 byte allocator |
| `palloc` | `SpinNoIrq` | 4 KiB page buddy |
| `heap_ready` | `AtomicBool` | byte allocator 是否已获得首个 heap region |
| `page_ready` | `AtomicBool` | page allocator 是否已获得至少一个 region |
| per-CPU usage counters | 每 CPU、每 `UsageKind` 两个累计 atomic | caller-live 诊断计数 |

## 初始化状态

allocator 只有单向状态转换，不支持 runtime teardown：

```text
Empty
  |
  | init_page_allocator()，或 global_init() 的第一阶段
  v
PageReady
  |
  | bootstrap_heap_if_needed()、首次 byte allocation，
  | 或 global_init() 的第二阶段
  v
HeapReady

PageReady / HeapReady -- add_memory() --> 保持当前状态并扩展 palloc
```

实际启动路径由 `kruntime` 按以下顺序执行：

1. 初始化主 CPU 的 per-CPU storage；
2. 初始化内存映射并枚举带 `FREE` 标记的 region；
3. 第一段 region 通过 `global_init()` 初始化 page allocator，并从中取得至少
   `MIN_HEAP_SIZE`（32 KiB）作为首个 byte heap；
4. 后续 region 通过 `global_add_memory()` 加入 page allocator；
5. AP 在进入普通内核代码前安装各自的 per-CPU register。

`init_page_allocator()` 支持只发布 page allocator、稍后再惰性建立 byte heap 的路径。
`page_ready` 和 `heap_ready` 使用 Release/Acquire 发布状态；内存区域的加入和 allocator
metadata mutation 仍由对应锁串行化。初始化调用本身依赖启动流程串行执行。

## 核心算法流程

### Rust heap / byte allocation

`GlobalAllocator::alloc(layout)` 按以下顺序路由：

1. `layout.size() > LARGE_ALLOC_THRESHOLD_BYTES`（当前为 1 MiB）时直接进行 page
   allocation，避免超大对象使 byte heap 碎片化；usage 按实际页数统计。更小的中型
   buffer 留在 byte heap，以复用已经从 page allocator 转移的 backing memory，并降低
   对全局 buddy 高阶连续块的压力。
2. slab 配置下，如果 `SizeClass::from_layout()` 接受该 layout，则先访问当前 CPU
   的 slab object cache。
3. 其他小请求进入受 `balloc` 保护的 `DefaultByteAllocator`。
4. 中央 byte allocator 无法满足请求时，从 `palloc` 取得新 region，加入 byte heap
   后重试。

byte heap 首次建立至少使用 32 KiB。后续增长请求参考当前 heap 总量和本次 layout，
取 power-of-two 大小；page allocation 失败时逐步减半，直到一页或能够容纳 layout 的
最小 region。byte heap 已取得的 backing region 不主动归还 page allocator。

deallocation 必须携带原 allocation 的 pointer 和 layout。超过 1 MiB 的大对象返回 page
allocator；slab-cache-eligible 对象优先进入当前 CPU cache；其他对象返回中央 byte
allocator。

### 连续页 allocation

`alloc_pages(num_pages, align_pow2, kind)` 的路径为：

- `num_pages` 为 1–4 且 `align_pow2 <= 4096`：先访问当前 CPU 的 PCP；
- PCP miss：获取 `palloc` 锁并批量 refill；
- 页数更大或要求特殊对齐：直接访问全局 page buddy；
- 成功后按 `UsageKind` 更新诊断统计，`RustHeap` 由上层 heap 路径单独处理。

`dealloc_pages()` 对 1–4 页 block 优先执行 PCP push；cache 满时批量 drain；更大的
block 直接归还 buddy。特殊对齐取得的小 block 在 free 后也可以作为普通同页数 block
进入 PCP，因为其地址至少满足普通页对齐。

`alloc_pages_at()` 需要指定起始虚拟地址，因此绕过 PCP，直接调用 buddy 的定址接口。

### 页级 PCP

`pcp.rs` 为每个 CPU 保存四个固定容量数组：

| block 大小 | 每 CPU 容量 |
|------------|-------------|
| 1 页 | 12 个 |
| 2 页 | 6 个 |
| 3 页 | 6 个 |
| 4 页 | 6 个 |

cache hit 只更新当前 CPU 数组和计数。refill 目标为容量的 `2/3`：

1. 优先向 buddy 申请覆盖整个 batch 的连续区间，减少持锁次数；
2. 若大块申请因碎片化失败，退化为逐 block 申请，允许 partial refill；
3. buddy 对非 power-of-two 页数产生的 rounding slack 会立即按合法 buddy chunk 归还；
4. cache full 时收集当前 free block 和一批 cache block，按地址排序、合并相邻区间，
   再通过 `split_to_chunks()` 归还 buddy。

PCP 中的页对 caller 已经 free，但在全局 buddy 看来仍是 allocated。

### per-CPU slab object cache（slab 配置）

`slab_cache.rs` 为每个 CPU、每个 `SizeClass` 保存一个侵入式 LIFO list。支持的 class
为 8、16、32、64、128、256、512、1024 和 2048 字节。

每个空闲对象的第一个 machine word 保存 next link；cache metadata 只有 head 和对象数。
所有 cache-eligible 对象都从中央 allocator 使用 `(class_size, class_size)` canonical
layout 取得，由此保证：

- 原请求的 size 和 alignment 得到满足；
- 同一 class 的 refill 和 drain 使用一致 layout；
- CPU A allocation 的对象可以在 CPU B free，并进入 CPU B 的同 class cache；
- 不需要逐对象 owner CPU 或原始 layout metadata。

allocation hit 直接 pop；空 cache 在一次中央锁临界区内批量 refill。每批对象数取
`min(2 页 object payload, 32 个对象)`，其中一个对象直接返回 caller，其余进入 cache。
高水位设为 `2 * batch - 1`；cache 已满时，下一次 free 把当前对象和 cache 中的
`batch - 1` 个对象归还中央 allocator，随后保留一个 batch。由此 refill 和 drain
每次最多分别调用中央 allocator 32 次。

| class bytes | batch objects | high watermark |
|-------------|---------------|----------------|
| 8–256 | 32 | 63 |
| 512 | 16 | 31 |
| 1024 | 8 | 15 |
| 2048 | 4 | 7 |

cache 中对象仍被中央 `SlabByteAllocator` 视为 allocated。该 ownership 选择避免了
每次 fast-path free 都修改中央 freelist，但意味着中央 `used_bytes()` 包含 caller 已经
free 到 per-CPU cache 的对象。

### DMA32 与定址页

`alloc_dma_pages()` 绕过 PCP，先从全局 buddy 取得连续页，再通过 `v2p` 检查整个物理
区间是否低于 4 GiB。区间不满足限制时立即归还并返回 `NoMemory`。该接口只负责内存
位置；页属性切换、平台 share/unshare 和设备生命周期由 `kdma` 等上层完成。

当前 page allocator 没有独立 DMA32 zone，因此它采用“先分配、后检查”，不能保证在
存在低端空闲页时一定找到它们。

### `GlobalPage`

`GlobalPage` 是连续 4 KiB 页的 RAII owner：

- `alloc()`、`alloc_zero()` 和 `alloc_contiguous()` 通过全局 page API 申请；
- `fill()`、`zero()`、`as_slice()` 和 `as_slice_mut()` 在 owner 范围内提供字节访问；
- `Drop` 使用相同页数和 `UsageKind::Global` 自动归还；
- `from_raw()` 用于从已知唯一 ownership 的 raw allocation 重建 owner，因此是 unsafe。

### Usage accounting

`UsageKind` 区分 Rust heap、虚拟内存、page cache、页表、DMA 和 `GlobalPage`。

- 每个 CPU、每个种类分别累计 `allocated_bytes` 和 `freed_bytes`，热路径只修改当前
  CPU 的 atomic cache line；
- allocation 和 free 可以发生在不同 CPU，快照按所有 CPU 的累计分配量减累计释放量；
- 小型 Rust heap allocation 按 caller requested bytes 计数；
- 直接页支持的 Rust heap allocation 和其他页用途按页对齐后的 bytes 计数；
- `usages()` 先 acquire 读取所有 CPU 的释放累计量，再读取分配累计量，并以 checked
  arithmetic 求差。合法跨 CPU free 不会导致本地计数下溢；总释放量超过总分配量则在
  汇总时 fail-fast；
- 快照不保证跨 CPU 或跨种类线性化，并发 allocation/free 可能使结果相对任一时刻的
  精确值暂时偏大或偏小；统计不参与 allocator ownership 或回收决策。

`used_bytes()` / `available_bytes()` 展示中央 byte allocator 的视图；`used_pages()` /
`available_pages()` 展示全局 buddy 的视图。byte heap backing、PCP block 和 per-CPU
slab object 会使这些中央视图与 caller-live usage 存在预期差异。

### Allocation tracking

`tracking` feature 在 Rust `GlobalAlloc` 路径外层维护：

- 以 allocation 地址为 key 的 `BTreeMap`；
- 原始 `Layout`、allocation backtrace 和递增 generation；
- per-CPU `IN_GLOBAL_ALLOCATOR` 递归保护，避免 tracker 自身 allocation 再次进入
  tracking；
- `enable_tracking()` / `disable_tracking()` 和 generation-range visitor API。

tracking 只用于诊断，不改变 allocator ownership。它引入全局 tracking lock、backtrace
采集和 map 操作，因此启用后的性能不能代表普通 allocation 路径。直接调用
`GlobalAllocator` page/byte API 的路径（包括本 crate 的 C shims）不经过这个 wrapper，
不会自动加入 tracking map。

### C ABI shims

启用 `tee` 或 `dice` 后，`ffi.rs` 导出 `malloc`、`free`、`calloc`、`memcpy`、
`memmove`、`memset`、`memcmp` 和 `__memcpy_chk`。C allocation 在返回给 caller 的地址
前保存一个 `usize` 大小的 user-size header，`free` 据此重建原始 layout。所有 raw
pointer 与 length 合法性仍由 C caller 保证。

## 调用约束 / 执行上下文

- **初始化前提**：page API 必须在 `page_ready` 后使用；byte/Rust heap allocation
  还要求已有 page region，使其能够建立或扩展 byte heap。
- **per-CPU 前提**：PCP、slab cache 和 tracking recursion guard 都要求当前 CPU 的
  per-CPU storage 已安装。主 CPU 和 AP 的正式运行路径满足这一顺序。
- **不要求当前进程**：核心 allocator 只依赖 CPU 执行上下文，不依赖当前用户进程。
- **不睡眠**：同步使用自旋锁、IRQ guard 和原子，不调用阻塞等待；锁竞争和 batch
  refill/drain 仍可能延长不可抢占或 IRQ-off 时间。
- **中断上下文**：per-CPU hit 可以在普通 IRQ 约束下完成；miss/full 可能获取中央锁
  并扩展 heap。接口没有 task/softirq/hardirq allocation policy，也不保证短延迟。
- **NMI**：不支持在 NMI 中 allocation/free。关闭普通 local IRQ 不能排除 NMI 重入。
- **早期启动**：`global_init()` 可在调度器建立前使用，但传入 region 必须已经位于可用
  kernel virtual mapping 中，且主 CPU per-CPU storage 已初始化。

## 并发模型

| 数据或路径 | 同步 | 备注 |
|------------|------|------|
| 中央 byte allocator | `SpinNoIrq` | 所有 allocator metadata mutation 串行化 |
| 全局 page buddy | `SpinNoIrq` | page slow path、DMA 和定址请求串行化 |
| 当前 CPU PCP | local IRQ off + current-CPU access | 不允许 remote mutation |
| 当前 CPU slab cache | local IRQ off + current-CPU access | cache hit 不取 `balloc` 锁 |
| `page_ready` / `heap_ready` | Acquire/Release atomics | 发布初始化状态；初始化流程仍要求串行 |
| 各类 usage | current-CPU cumulative atomics；free Release / snapshot Acquire | 支持跨 CPU free，只提供诊断值 |
| tracking state | `SpinNoIrq` + per-CPU recursion guard | 仅 `tracking` feature |

byte heap 扩容时允许在持有 `balloc` 后进入 page allocation，形成 `balloc -> palloc` 的
锁顺序。其他路径不得在持有 `palloc` 时再获取 `balloc`；bootstrap 会先释放 page lock，
再初始化 byte allocator。usage accounting 不引入额外的 allocator 锁顺序。

## 设计决策

### 中央 ownership 与 per-CPU fast path

中央 allocator 保持唯一的 region ownership 和复杂合并逻辑，per-CPU cache 只保存已经
从中央取出的 block/object。这样 fast path 简单，代价是 cache residency 对中央 allocator
不可见为 free，需要另行设计全 CPU reclaim。

### page cache 与 object cache 分层

PCP 缓存页 block，降低 `palloc` 锁竞争；slab object cache 缓存固定 class 对象，降低
`balloc` 锁竞争。二者服务不同粒度，不互相替代。byte heap 扩容仍可能经过 PCP 或全局
page buddy。

### slab canonical layout

用 class canonical layout 代替 caller 原始 layout，会增加 class 内部碎片和对齐要求，
但换来跨 CPU free、无逐对象 metadata，以及确定的中央 deallocation layout。这是 slab
object cache 初版的核心 ownership 约束。

### 诊断统计不参与正确性

usage、`used_bytes()` 和 `used_pages()` 分别反映不同层次，且读取时系统仍可并发变化。
它们用于观测而非 allocation admission、reclaim 或安全校验，避免为线性化快照扩大锁
范围。分开累计 allocation/free 也意味着 usage underflow 从 deallocation 热路径的
即时检查，变为 `usages()` 汇总时的检查。

## Drop / 资源释放

- `GLOBAL_ALLOCATOR`、中央 allocator、per-CPU cache 和 tracking state 为静态生命周期，
  关机前不 drop。
- 加入 page allocator 的启动内存 region 永久转移给 `kalloc`；不支持移除 region。
- byte allocator 从 page allocator 取得的 heap region 不会在 runtime 整体归还。
- PCP 和 slab cache 在达到各自水位时批量归还，但没有 shutdown、CPU-offline 或全局
  pressure flush。
- 直接 page API 的 caller 必须显式成对 deallocate；`GlobalPage` 通过 `Drop` 自动完成。
- C `malloc`/`calloc` allocation 必须由同模块的 `free` 释放；其 size header 与用户地址
  具有相同生命周期。
