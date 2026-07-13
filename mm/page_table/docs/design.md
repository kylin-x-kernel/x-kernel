# page_table — 设计文档

## 定位

本模块提供 x-kernel 统一的多架构页表实现。它定义了与架构无关的页表操作
trait（`PageTableEntry`、`PagingMetaData`、`PagingHandler`），并实现了
通用的 64 位多级页表 `PageTable64`，支持 x86_64、AArch64、RISC-V、
LoongArch64 四种架构。本模块被 `mm/memspace`、`process/kexec` 等子系统
使用，是内核虚拟内存管理的基础。

## 背景

x-kernel 需要在多种 CPU 架构上管理虚拟地址空间，但各架构的页表项格式、
层级数量、TLB 刷新机制差异很大。如果每个架构独立实现页表操作，会导致
大量重复逻辑（映射/解映射/区域操作/TLB 刷新），且难以保证一致性。
因此需要一套通用框架，将架构差异封装在 PTE 和 Metadata trait 中，
使核心映射逻辑只写一次。

## 范围

涉及的源文件：

```
page_table/
├── src/
│   ├── lib.rs
│   ├── defs.rs
│   ├── macros.rs
│   ├── table64.rs
│   └── arch/
│       ├── mod.rs
│       ├── x86_64.rs
│       ├── aarch64.rs
│       ├── riscv.rs
│       └── loongarch64.rs
└── Cargo.toml
```

## 架构

```
                    ┌─────────────────────────────────┐
                    │        调用者                     │
                    │  (memspace, kexec, ...)          │
                    └───────────┬─────────────────────┘
                                │
                    safe API: map / unmap / query / protect
                                │
                    ┌───────────v─────────────────────┐
                    │       PageTable64<M, PTE, H>     │
                    │       PageTableMut<M, PTE, H>    │
                    │                                  │
                    │  ┌─ walk_page_table! ─────────┐  │
                    │  │  3/4 级遍历 + huge page     │  │
                    │  └────────────────────────────┘  │
                    │                                  │
                    │  ┌─ TLB 刷新延迟批处理 ────────┐  │
                    │  │  ToFlush / finish()          │  │
                    │  └────────────────────────────┘  │
                    └───┬──────────┬───────────────────┘
                        │          │
            ┌───────────v──┐   ┌──v──────────────┐
            │  PTE (trait)  │   │  M: PagingMetaData │
            │  + arch impl  │   │  + H: PagingHandler│
            └───────────────┘   └──────────────────┘
               x86_64              flush_tlb()
               aarch64             alloc/dealloc_frame()
               riscv               p2v()
               loongarch64
```

| 组件 | 职责 |
|------|------|
| `defs.rs` | 定义核心 trait 和类型：`PagingFlags`、`PageTableEntry`、`PagingMetaData`、`PagingHandler`、`PageSize`、`PtError`、`PteSnapshot`、`PteReplaceError`、`TlbFlushReceipt` |
| `table64.rs` | 通用 64 位多级页表实现：`PageTable64`（只读查询）+ `PageTableMut`（可变操作、条件替换 + TLB 批处理） |
| `macros.rs` | `walk_page_table!` / `walk_page_table_create!` 遍历宏，`impl_pte_debug!` / `impl_pte_common_ops!` PTE 辅助宏 |
| `arch/x86_64.rs` | x86_64 PTE（4 级，SEV C-bit 加密），`X64PagingMetaData` |
| `arch/aarch64.rs` | AArch64 PTE（4 级，Arm64Attr 属性），`A64PagingMetaData` |
| `arch/riscv.rs` | RISC-V PTE（Sv39 3 级 / Sv48 4 级），`Sv39MetaData` / `Sv48MetaData` |
| `arch/loongarch64.rs` | LoongArch64 PTE（4 级，LaFlags 属性），`LA64MetaData` |

## 状态机

### PageTableMut TLB 刷新状态

```
  None ──flush(vaddr)──> Addresses ──flush(超过阈值)──> Full
    │                        │                            │
    │                        └────finish()──> None <──────┘
    └────────────────────finish()──> None
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| `None` | `Addresses` | 首次调用 `flush(vaddr)` |
| `Addresses` | `Addresses` | 追加地址，未超 `FLUSH_THRESHOLD`(16) |
| `Addresses` | `Full` | 追加地址超过阈值 |
| `Addresses` | `None` | `finish()` 或 `Drop` |
| `Full` | `None` | `finish()` 或 `Drop` |

### PTE 生命周期

```
  EMPTY ──new_page()/new_table()──> PRESENT ──clear()──> EMPTY
                                      │
                              set_paddr()/set_flags()
                                      │
                                      v
                                   PRESENT (更新)
```

## 算法流程

### 页表遍历（walk_page_table!）

以 4 级页表为例：

1. 从 `root_paddr` 获取 P4 表
2. 用 `p4_idx(vaddr)` 索引 P4 表，获取 P4E
3. 若 P4E 指向下一级，获取 P3 表；否则返回 `NotMapped`
4. 检查 P3E 是否为 huge page（1G），若是则直接返回
5. 用 `p3_idx(vaddr)` 索引 P3 表，获取 P3E → P2 表
6. 检查 P2E 是否为 huge page（2M），若是则直接返回
7. 用 `p2_idx(vaddr)` 索引 P2 表，获取 P2E → P1 表
8. 用 `p1_idx(vaddr)` 索引 P1 表，返回 P1E（4K 页）

3 级页表（Sv39）跳过 P4 层，直接从 P3 开始。

### 映射创建（walk_page_table_create!）

1. 遍历各级页表，若中间表项为空则调用 `alloc_table()` 分配新页表帧
2. 到达目标层级后返回可变 PTE 引用
3. 调用者写入 PTE 内容

### 区域映射（map_region）

1. 校验 vaddr 和 size 按 4K 对齐
2. 循环中优先尝试大页（1G → 2M → 4K），条件：地址对齐 + 物理地址对齐 + 剩余大小足够
3. 逐页调用 `map()`，推进 vaddr 和剩余大小
4. 任一页映射失败则整体失败

### TLB 刷新批处理

1. `PageTableMut` 内部维护 `ToFlush` 枚举
2. 每次 `map/unmap/remap/protect/replace_if_same` 成功修改 PTE 后调用 `flush(vaddr)` 记录待刷新地址
3. 地址数 ≤ 16 时逐个刷新；超过阈值则标记为 `Full`
4. `finish()` 时批量执行：`Addresses` 逐个刷新，`Full` 刷新整个 TLB，并返回 `TlbFlushReceipt`
5. `Drop` 自动调用 `finish()`，确保不会遗漏刷新

`TlbFlushReceipt` 是页表修改后的显式排序边界。它不暴露架构细节，只表示调用点已经完成一次
flush finalization。上层若要释放刚从 PTE 中移除的物理帧或对象页，必须把释放动作放在
`finish()` 之后；仅依赖 `PageTableMut::drop` 虽然能避免遗漏 flush，但不能在代码上表达
“释放发生在 flush 之后”的资源生命周期约束。

### PTE snapshot 与条件替换

`PageTable64::query_entry(vaddr)` 返回 `PteSnapshot`，表示一次页表遍历观察到的
present leaf PTE。它包含：

- leaf PTE 的基础物理地址；
- 解码后的 `PagingFlags`；
- leaf 页大小；
- 仅由 `page_table` 内部解释的 raw PTE bits，用于条件替换比较。

`PageTableMut::replace_if_same(vaddr, expected, paddr, flags)` 是 COW 等事务型路径的
提交原语：

1. 上层先用 `query_entry()` 观察当前 PTE；
2. 上层在页表外准备替换资源，例如 COW 新页；
3. 提交时调用 `replace_if_same()`；
4. 如果当前 leaf PTE 与 `expected` 完全一致，则替换物理页和权限并记录 TLB flush；
5. 如果当前 PTE 已变化或不再 present，返回 `PteReplaceError::Changed`，不覆盖当前映射；
6. 地址非法、物理地址非法或页表遍历异常返回 `PteReplaceError::PageTable`。

这个接口只表达页表层的 compare/replace 语义，不决定 COW、匿名页或文件页的资源释放策略。
调用者负责在 `Changed` 时丢弃已准备但未提交的资源，并根据 fault 语义选择 retry。

## 并发模型

- **`PageTable64` 本身不是 `Sync`**：内部无锁，多线程同时修改同一页表需要外部同步。
- **`PageTableMut` 借用 `&mut PageTable64`**：Rust 借用规则保证同一时刻只有一个可变引用，天然互斥。
- **SMP TLB 刷新**：`feature = "smp"` 时，`flush_tlb_all_cpus()` 通过 `TlbFlushIf` 接口
  （`kiface` 实现）向远端 CPU 发送 IPI shootdown。AArch64 使用硬件
  Inner Shareable TLBI 指令，无需软件 IPI。
- **`PagingMetaData` 约束 `Send + Sync`**：确保元数据可跨线程访问。
- **`PageTableEntry` 约束 `Send + Sync`**：PTE 可安全在线程间传递。

## 设计决策

### 为什么用 trait 参数化而非泛型常量

`PageTable64<M, PTE, H>` 通过三个 trait 参数将架构差异、帧分配、TLB 刷新
全部外提：
- `M: PagingMetaData` — 页表级数、地址位宽、TLB 刷新
- `PTE: PageTableEntry` — 页表项编解码
- `H: PagingHandler` — 帧分配和物理地址转换

这避免了在核心逻辑中使用 `cfg(target_arch)` 分支，使映射算法只写一次。

### 为什么 PageTableMut 采用延迟 TLB 刷新

每次 map/unmap 立即刷新 TLB 在批量操作时开销巨大（尤其 SMP shootdown）。
`PageTableMut` 将刷新请求暂存，在 `finish()` 或 `Drop` 时统一处理：
- 少量地址：逐个刷新，精确高效
- 大量地址：全量刷新，避免逐个开销
- 阈值 16：经验值，平衡精确性和开销

对释放旧物理页的路径，调用者应显式调用 `finish()` 并在得到 `TlbFlushReceipt` 后释放资源。
`Drop` 仍保留为兜底机制，防止普通映射修改遗漏 TLB flush。

### 为什么提供 replace_if_same 而不是复用 remap

`remap()` 是无条件覆盖：只要地址当前 present，就替换物理地址和权限。COW 写故障不能使用
这种语义，因为 fault 线程在复制页面期间，其他路径可能已经修改同一 PTE。无条件覆盖会丢失
并发更新，甚至把已解决的 fault 回退到旧状态。

`replace_if_same()` 将“观察 PTE”和“提交替换”绑定成显式 compare/replace contract：
页表层只判断 PTE 是否仍等于 snapshot；内存对象层负责 prepare/commit/abort 资源。典型
COW 提交流程可以写成：

```text
query_entry()
  -> prepare replacement page outside page-table mutation
  -> replace_if_same()
       Ok(_) => commit object state
       Changed => abort prepared page and retry fault
       PageTable(error) => abort and propagate error
```

### 为什么用宏实现页表遍历

`walk_page_table!` 和 `walk_page_table_create!` 需要同时支持不可变引用
（`query`）和可变引用（`map/unmap`），且 3/4 级页表结构不同。
用宏可以在编译期生成对应代码，避免运行时分支，同时复用遍历逻辑。
Rust 泛型无法方便地表达"对 `&T` 和 `&mut T` 使用不同调用方式"的需求。

### 为什么 TlbFlushIf 用 kiface 而非直接依赖

`page_table` 被内存管理子系统依赖，而 IPI 子系统又依赖内存管理。
直接依赖会形成循环。`kiface` 在链接时绑定 exactly-one 实现，
编译期只依赖接口定义，打破循环依赖。

### 为什么 x86_64 PTE 使用 EncodedPtePhys 封装

AMD SEV 需要在 PTE 物理地址中嵌入 C-bit（加密位），其位置由
`kbuild_config::SEV_CBIT_POS` 决定。`EncodedPtePhys` 封装了 C-bit 的
设置和清除逻辑，使 `PageTableEntry` 实现不需要直接处理 SEV 细节。

### 为什么 RISC-V 支持 Sv39 和 Sv48 两种模式

Sv39（3 级，39 位虚拟地址）是 RISC-V 规范的必选模式，Sv48（4 级，48 位）
是可选扩展。两种模式共享 `Rv64PageEntry`，仅 `PagingMetaData` 的级数和
地址位宽不同。通过 `Sv39MetaData` / `Sv48MetaData` 类型参数区分，
`PageTable64` 的通用遍历逻辑自动适配。

## Drop / 资源释放

- **`PageTable64::drop`**：递归释放所有已分配的页表帧。
  - `feature = "copy-from"` 时，跳过 `borrowed_entries` 标记的借入条目，
    避免释放从源页表共享的子树。
  - 释放顺序：先递归释放子表，再释放当前层帧。
- **`PageTableMut::finish`**：刷新待处理 TLB 并返回 `TlbFlushReceipt`，供上层作为
  flush-before-free 的显式排序点。
- **`PageTableMut::drop`**：自动调用 `finish()` 刷新 TLB，作为遗漏显式 finalization 的兜底。
