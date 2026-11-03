# page-meta — 设计文档

## 定位与范围

为物理基页建立稳定的元数据身份。启动调用方持有物理区间列表，`init()` 原地规范化
并扣除元数据后备区，返回 PageMetaTable。调用方发布统一查询后，把列表中剩余的
非空区间交给 allocator。物理区间复用 memaddr::PhysAddrRange，不包装另一套库存类型。

PageMeta records the permanent reservation of this service's backing storage,
an optional allocation-head reference count, head/tail geometry, and the
allocation round's UsageKind.
The class preserves original allocation accounting, not actual page type, current
users, content permissions or reclaimability. Metadata does not track allocator
inventory. Lookup grants no ownership, content access or release authority.
Current input covers only RAM marked FREE by HAL.

## 架构与职责

- `src/lib.rs`: errors, public initialization and identity queries, and global publication.
- `src/page.rs`: PageMeta, head counter, tail offset, allocation order and reference-state
  transitions. It defines no RAII holder and does not return inventory.
- `src/page/group.rs`: independent-range admission/clone/validation and compound
  group admission/resolution, retirement and exclusive split.
- `src/free_ranges.rs`：整理调用方的空闲物理区间，并从中划出后备内存。
- `src/backend/mod.rs`：选择私有存储与 PFN 索引实现，导出 init 和 Table。
- `src/backend/segments.rs`：当前紧凑分段索引、数组存储、查找和初始化写入。
- `ut-tests/{free_ranges,segments,page,group}.rs`: handoff, bidirectional page identity,
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


## Allocation references and head/tail geometry

Each descriptor identifies one physical 4 KiB page. An independent page has
order zero and a zero head offset. An explicitly compound allocation has one head carrying
its order, original UsageKind and reference count. Each tail stores its distance
in base pages to that head. Tail counters do not represent independent holds.
The offset is a u32 page distance, not a truncated physical address or descriptor
pointer; PFN resolution remains behind the table API. The head order and all
member offsets are stable while any allocation hold exists.

`PageMetaTable::claim_reference_group` validates the entire geometry, coverage,
permanent status and unclaimed state before changing anything. It enters
Initializing at the head, initializes order/class and tail offsets, and publishes
the first head reference with Release after all links are established. Ordinary
errors leave descriptors unchanged. The allocator supplies exclusive unpublished
backing; metadata lookup by itself cannot prove that exclusivity.

`reference_allocation` resolves any selected member to its head PFN and order.
It is unsafe because the caller must retain an existing hold throughout the
read; independent atomic field reads are not a coherent ownership snapshot.
Clone and release operate on the resolved head's single counter. The count limit
excludes Initializing, Splitting and Retiring. Overflow returns ReferenceOverflow.
A subpage hold keeps the entire unsplit allocation alive.

The unique last release acquires earlier holders' releases and enters Retiring.
The holder first snapshots allocation geometry. `finish_reference_retirement`
clears every tail link while backing remains outside allocator inventory, resets
the head order/count, and returns the original class. kalloc then returns the
complete saved range without writing old metadata again. Admission and final
retirement are O(backing pages); clone and member resolution do not visit the
whole range. Raw allocator release does not gain a descriptor scan.

## Independent reference ranges

claim_reference_range validates every descriptor before claiming one independent
reference per page. Each admitted page keeps order zero, offset zero and its
original class. The caller owns all resulting holds; metadata does not store a
range-owner length because several handles may overlap with different bounds.
reference_range validates a transferred range: all pages must be independent or
all selected pages must fit in the first member's single compound. It requires
pre-existing lifetime protection rather than acquiring ownership from snapshots.

clone_reference_range consumes no original holds. Independent mode increments
each page, rolling back every successful prefix increment if a later counter
fails. Originals make rollback retirement impossible. Compound mode increments
only its head. All shape reads occur while caller-held references exclude geometry
changes. Independent range final release is performed per page by the allocator
holder, using existing order-zero retirement; there is no new range counter.

## Exclusive splitting

`split_reference` consumes one whole-allocation ownership obligation only on
success. It resolves geometry, then atomically freezes a head count of exactly
one into Splitting. Other holds, including raw transfers, yield NotExclusive
with no change. Acquire joins prior holders' releases. There is no fallible
operation after freezing: every tail becomes an independent order-zero page
with count one and the original class, and the head is published last as another
order-zero page. No content is moved and no allocator/accounting action occurs.

The caller must end content, mapping and hardware use before the unsafe split.
Reference exclusivity cannot prove hardware retirement. Success transfers one
hold per page to the caller, which must consume the old owner without releasing
it and retain/release every resulting page. kalloc's SplitPages transfer iterator
implements that cleanup obligation without a heap array or second reference
protocol. Splitting is O(backing pages) and does not support transferring arbitrary
shared/mapped references to children.

## Layout and holder boundary

`PageMeta` keeps an AtomicUsize counter, AtomicU32 relative head offset,
AtomicU8 class, AtomicU8 order and the independent immutable permanent bool.
A compile-time assertion requires 16 bytes on 64-bit targets; the public API does
not promise field offsets. Boot storage is still reserved from the actual type
size. The offset range is validated before admission, and direct field encoding
is not exposed as a public tag or allocator policy.

kalloc::PageRef stores the first descriptor and selected page count. It uses
one independent hold per selected page or one compound hold. Its range length
is handle-specific; the selected PFN and compound head PFN can differ. RAII ownership, raw transfers,
usage totals and returning inventory belong to kalloc. page-meta owns only
identity, geometry and reference transitions; it has no reclaim callback, direct
mapping dependency or allocator instance pointer. Dependencies remain
kalloc -> page-meta. global_table borrows the immutable published directory and
grants no ownership.

ReferenceState is a best-effort diagnostic with Tail and Splitting states,
not a coherent role/count snapshot. Unreferenced does not mean free: heap,
PCP, bitmap and raw users can own storage outside the reference protocol.
is_permanent remains a separate bool set before publication; it excludes this
service's metadata backing for the kernel lifetime and has no unreserve API.
UsageKind retains diagnostic allocation accounting, not actual page type or
reclaim policy. Clone/split do not reclassify or repeat the allocation charge.

The local group tests cover tail resolution, complete retirement and reuse,
failed exclusivity, successful split through a tail, class preservation and
coverage/permanent/geometry rejection before any claimed prefix. Range tests
force a later reference overflow and verify complete prefix rollback, plus exact
range coverage failure with unchanged descriptors. Existing
counter-overflow and reservation tests continue to cover the low-level protocol.
