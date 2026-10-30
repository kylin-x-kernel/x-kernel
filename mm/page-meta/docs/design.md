# page-meta — 设计文档

## 定位与范围

为物理基页建立稳定的元数据身份。启动调用方持有物理区间列表，`init()` 原地规范化
并扣除元数据后备区，返回 PageMetaTable。调用方发布统一查询后，把列表中剩余的
非空区间交给 allocator。物理区间复用 memaddr::PhysAddrRange，不包装另一套库存类型。

PageMeta records the permanent reservation of this service's backing storage,
an optional page-lifetime reference count, and the allocation round's UsageKind.
The class preserves original allocation accounting, not actual page type, current
users, content permissions or reclaimability. Metadata does not track allocator
inventory. Lookup grants no ownership, content access or release authority.
Current input covers only RAM marked FREE by HAL.

## 架构与职责

- `src/lib.rs`: errors, public initialization and identity queries, and global publication.
- `src/page.rs`: PageMeta, the unique counter, accounting class and reference-state
  transitions. It defines no RAII holder and does not return inventory.
- `src/free_ranges.rs`：整理调用方的空闲物理区间，并从中划出后备内存。
- `src/backend/mod.rs`：选择私有存储与 PFN 索引实现，导出 init 和 Table。
- `src/backend/segments.rs`：当前紧凑分段索引、数组存储、查找和初始化写入。
- `ut-tests/{free_ranges,segments,page}.rs`: handoff, bidirectional page identity,
  reference-state transitions and boundary cases.
- `core/kruntime/ut-tests/page_metadata.rs`：既有 allocator 交接和全局查询。

```text
kruntime 持有物理区间列表
  → page_meta::init(&mut ranges, map_storage)
       → 共用函数规范化范围
       → backend 建立覆盖索引、申请后备区、初始化
       → 共用函数扣除后备区，保留剩余物理区间
  → publish(table)
  → 遍历非空剩余区间，p2v 后交给原有 allocator
```

PageMeta 的初始状态和永久标记由共用方法定义。backend 负责计算存储需求、组织
索引并返回稳定借用；不决定全局发布策略，不了解 allocator 的 bitmap 或空闲链表。
调用方映射回调只接收 backend 实际申请的完整后备区，不要求全部描述页内容已有
内核映射。元数据页的描述符与它所描述的物理页内容是不同对象。

## 初始化和库存规则

1. 共用 normalize 校验起止顺序和地址运算，向内对齐到完整 4 KiB 页，忽略空区间，
   按 PA 排序并拒绝物理重叠，合并相邻物理区间。未使用条目置空，切片长度不变。
2. backend 直接遍历共用层已规范化的范围，在任何库存扣除前构造段表及描述符计数；
   不重复排序、合并或输入范围校验。预留页自身也必须有 PageMeta，不能把余额误作覆盖。
3. 共用 with_reserved_storage 按整页从最大可用区间尾部选取存储，backend 映射并初始化它。
   只有该次初始化成功才扣除库存；映射或初始化失败不扣除该次请求。
4. covered_pages() derives coverage from the final segment. lookup(PFN) and
   pfn_of(metadata) provide bidirectional identity conversion. Reverse lookup
   checks descriptor membership and uses a binary segment search; it needs no
   page-content mapping. Table stores no coverage count, and PageMetaTable stores
   no metadata-storage count.
5. kruntime 串行 publish，再按 PA 升序以首个非空区间初始化 allocator，依次交接后续非空区间，
   保持 buddy 的低地址优先分配顺序，避免 DMA32 请求先命中高地址 RAM。
   空条目必须跳过；allocator 自行扣除其内部 metadata。启动日志的元数据开销由
   覆盖页数减去交接前剩余页数临时计算，不作为表的持久状态。

错误不会发布元数据表。输入切片在错误时可能已经被规范化；若未来后端多次预留，先前
成功的扣除仍然生效，不承诺跨请求回滚。元数据表发布后 allocator 初始化失败，启动按
fatal-boot 策略停止，不重试整个事务。

## 后端边界和限制

公开 API 不暴露 segment 数量、数组下标、后备连续性或索引容量。当前私有后端先
在局部变量中计算段表、计数和存储大小，再预留并初始化连续后备数组，最后一次性
构造有效 Table。Table 只保存段表、有效段数和描述符指针。构造时所需 entry_count 仅用于
计算数组字节数和初始化槽位，不在构造后继续保存。
容量暂为 64 个合并后的物理段，属于该
实现的限制，与调用方列表长度或 allocator 的上限无关。后端通过编译期模块选择。

未来后端可以改变索引、存储分块和申请次数。需要额外虚拟地址/页表能力的方案，
仍须由启动集成提供对应资源；当前接口不承诺自行建立映射。

## 并发、上下文和生命周期

主核在 heap、其他 allocator 使用者和从核启动前串行初始化；内部不使用 heap。
映射回调也必须适用于该上下文。初始化和永久标记完成后，klazy::Once 负责发布
可见性；共享 lookup 不分配、不等待、不取锁、不写描述符。

元数据表可移动，描述符地址不变。元数据表不在 Drop 时归还原始 RAM。调用方在元数据表及全部
借用销毁前保留后备区与映射；全局发布后须永久有效。局部测试先销毁元数据表、allocator
和借用，再回收夹具。发布后的查询与当前任务或某个 allocator 实例无关。

kalloc raw release relies on the original allocation ownership contract rather
than checking every page's references or permanent flag. Callers must not return
reference-owned or permanent pages as raw allocations. This service neither
synchronizes buddy bitmap reservations nor detects raw-protocol double frees.
永久标记为 false 不代表该页可分配或可释放。


## Single-page references and allocation attributes

This crate stores the unique counter and exposes four unsafe transitions:
claim_reference establishes a new allocation's first hold; clone_reference adds
a hold; release_reference consumes one and selects the sole last owner;
finish_retirement returns the original class and resets the state to Unreferenced.
Counter and class fields stay private. Lookup and snapshots do not establish the
ownership required to call these operations.

The state sequence is Unreferenced -> Initializing -> Referenced { references: 1 }
-> Retiring -> Unreferenced. Claim atomically acquires Initializing, writes
UsageKind, then publishes the first hold with Release. Clone does not change
the class. The internal AtomicU8 uses Relaxed and is not an opaque public tag.
A new allocation round can begin only after the old storage returns to inventory.

The count limit excludes Initializing and Retiring sentinels; overflow returns
ReferenceOverflow. Final release uses AcqRel to acquire earlier holders' releases.
The last owner saves page identity, then finish_retirement snapshots the original
class and resets the count with Release. The owner returns inventory afterwards
and must never write the descriptor after publishing reusable storage. The
permanent reservation is a separate bool written before publication and read-only
thereafter; claim rejects it before changing the counter. Unreferenced is not a
free-inventory state: heap, PCP, buddy bitmap and raw users may still own pages
outside this reference protocol.

## Holder and final reclaim boundary

The thin RAII holder is kalloc::PageRef and stores only one &'static PageMeta.
Its holds all use this crate's single counter. kalloc accepts exclusive new pages
from global inventory and handles raw transfer, final accounting and inventory
return. This crate stores no reclaim function, accepts no runtime provider
registration, and depends on neither kalloc nor the direct map. Dependencies
remain kalloc -> page-meta.

PageRef calls the constrained operations during clone/drop; final Drop completes
retirement and directly calls kalloc's local release path. Metadata initialization
only publishes the global physical table; it installs no extra reclaim endpoint.
The private backend resolves physical identity, while users manage content mappings.

UsageKind's typed contract and per-round record live in the same page module.
kalloc re-exports the type to preserve existing callers. Existing modules still
own cumulative totals, RustHeap accounting exceptions and procfs output. Keeping
the class preserves observable statistics without turning it into an MM decision.

The current 64-bit metadata layout is 16 bytes, including the counter, class and
independent permanent attribute. Boot reserves storage from the actual type size;
the public API does not promise a field layout. Metadata, holders and allocator
inventory have distinct lifetimes.

is_permanent only says that this service's metadata backing never returns to
ordinary inventory during the current kernel lifetime. It is not a complete
machine RAM reservation policy and has no unreserve API. It is independent of
the reference count.
