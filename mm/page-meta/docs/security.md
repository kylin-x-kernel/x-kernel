# page-meta — 安全与可靠性分析

## 信任边界

输入是调用方持有的 PhysAddrRange 切片。PA 只表示物理页身份，不被解引用；
真实写入只通过映射回调提供的 NonNull<u8> 完成。init 的 unsafe 契约要求输入 RAM
独占、回调提供完整后备范围的独占可写存储，并维持充分生命周期。运算和覆盖校验
不能证明硬件映射有效，也不要求未被用作元数据的物理页内容已有内核映射。

Public lookup only borrows descriptors; it grants no ownership, content access
or release authority. Reverse lookup checks descriptor identity and cannot create
a hold. Coverage includes complete input pages, not every boot-reserved RAM page
on the machine.

## unsafe 清单与不变量

- 公共 init → backend::init：输入已规范化且无物理重叠；调用方保证 RAM 独占，
  映射回调只为选中的后备区提供指针。with_reserved_storage 只在该次初始化成功后扣除库存。
- Segment Table::initialize: indexes and counts come from build_segments over
  normalized ranges. Array size, isize limits and capacity are checked before
  writing slots. The unsafe contract carries the map callback's PageMeta alignment
  requirement through public init and backend::init to initialize. Slot offsets
  preserve alignment. PageMeta contains AtomicUsize, so mappings must meet the
  actual type alignment rather than the earlier byte-aligned representation.
  Table is constructed only after every descriptor is initialized; no pointer or
  reference exposes uninitialized slots.
- segment lookup：Table 内嵌段表的段边界和紧凑下标证明槽位已初始化且在数组内，
  返回共享借用；输入规范化只在共用 free_ranges 路径执行。covered_pages 从末段
  推导，不依赖另存的计数；构造时已校验全部描述符下标及数组大小。
- segment mark_permanent_range：先校验完整物理范围，再排他调用 PageMeta::mark_permanent；
  不与任何共享描述符借用并发。
- Table's unsafe Send/Sync: moving the table does not move its backing storage.
  Shared reference state uses atomic updates; permanent marking requires exclusive
  access before publication.
- publish：通过 Once 安装元数据表，并允许 static 借用；调用方串行化发布，保证后备
  RAM 和映射永久有效，不能发布生命周期有限的局部测试夹具。

## 生命周期与错误

Initial descriptor values and permanent flags are written before publication.
Global Once provides visibility. Lookup takes no extra lock; reference-state
observations and transitions use per-page atomics. Dropping a local table does
not reclaim its backing; the caller may reclaim it only after all borrows end.
Global metadata lifetime is independent of allocator instances.

| 失败 | 处理 | 保证范围 |
| --- | --- | --- |
| 起止颠倒、地址运算溢出、没有完整页 | InvalidRange / EmptyCoverage | 不探测硬件可访问性 |
| 物理重叠、后端索引容量不足 | 初始化返回错误 | 输入可能已被规范化，不发布元数据表 |
| 后备 RAM 不足 | NoMemory | 不扣除失败的请求 |
| 映射回调或后备初始化失败 | 返回对应错误 | 不扣除该次请求；回调负责其外部映射资源 |
| 未来多次预留中后一次失败 | 返回错误 | 先前成功的扣除保留，不承诺整体回滚 |
| 查询空洞或未发布元数据表 | None | 不表示该物理页不存在 |
| 重复发布 | AlreadyPublished | 调用方仍须串行化发布 |
| 后备区被过早回收、重新交给 allocator | 违反 unsafe 契约 | 可能 UAF 或损坏，类型不拥有原始 RAM |

规范化保持切片长度，合并后多余条目及完全耗尽的区间可能为空。启动集成必须跳过
空条目；只将原地更新后的剩余区间交给 allocator。元数据表发布后 allocator 初始化
失败即停止启动，不进行部分重试。

## 保障范围与审计

Metadata backing never enters ordinary free inventory. kalloc raw release does
not scan each page's references or permanent flag; callers must own the original
allocation's release right. This service does not validate provenance, detect
raw-protocol double frees, or record allocator bitmap/heap/PCP ownership in
metadata. It parses no user input, retains no user-page contents and writes no
persistent data.

审计关注：原地列表扣除与元数据覆盖是否区分；映射是否只针对实际后备区且足够大；
backend 是否隐藏索引几何；发布是否晚于全部初始化写入；调用方是否保留后备映射、
跳过空条目并只交接剩余 RAM；局部测试是否按正确顺序销毁夹具。


## Allocation references, grouping and unsafe callers

PageMeta fields are private. PageMetaTable::claim_reference_range and
claim_reference_group accept only
exclusive unpublished inventory and validates every descriptor before acquiring
any initial holds. Independent ranges keep order/offset zero on every member;
explicit compounds publish their first head hold only after membership setup. A permanent page, physical hole, nonzero count or existing tail/order
prevents admission. Ordinary error paths do not claim a prefix. The immutable
published directory and caller-owned backing establish descriptor lifetime.

Tail membership uses a checked u32 PFN distance. reference_allocation requires a
live hold from the same allocation incarnation, resolves a head with a positive
count, and checks the member fits its order. A live hold excludes retirement and
exclusive split, making geometry stable. Safe lookup, global_table and
ReferenceState cannot establish that hold; snapshots may mix concurrent phases.
They must not be used as release authority or as a lockless acquire protocol.

Clone and release operate only at heads. The counter limit excludes all three
transition sentinels. Initializing protects attribute/link publication and Release
publishes the first owner. Before calling release_reference, the caller must
end all accesses authorized by the consumed hold and wait for any required
TLB/device completion. Last release enters Retiring with AcqRel. The last
holder snapshots geometry before release; finish_reference_retirement clears all
tail membership, resets the head, and returns the original class before kalloc
publishes reusable inventory. No old descriptor writes may follow that return.
Per-field atomics alone do not replace this ownership and publication argument.

split_reference freezes exactly one live head hold with AcqRel; any outstanding
raw transfer still counts and blocks exclusivity. Errors preserve the original
holder. After freezing, all metadata is already known covered, no allocation or
fallible validation remains, and every child receives one independent hold.
Class copies and child publication precede exposing outputs. The unsafe caller
must end content access, mappings, devices and required retirement, consume the
old holder without Drop, and release every resulting child once. These are
ownership transfers, not reference acquisition from a diagnostic count of one.

kalloc's SplitPages iterator owns unconsumed children; its Drop prevents ordinary
early iteration exit from leaking them. mem::forget, a lost raw transfer or a
kernel failure can still leak ownership. Neither this crate nor a count check
proves provenance, unique raw consumption, allocation incarnation or hardware
retirement. Invalid unsafe callers can corrupt the allocator despite state checks.

Metadata stays 16 bytes on 64-bit targets by a compile-time assertion. Offset,
order, class and count use separate atomic fields; no tagged pointer or count/
offset union is decoded. The permanent flag is immutable after publication.
All UsageKind values come from typed admission; split preserves the original
class and final accounting remains allocator-owned. Group admission, retirement
and splitting touch every backing descriptor, so IRQ callers must bound their
work. Ordinary raw release remains unchanged and performs no descriptor scan.

## Independent ranges and clone rollback

Reference-range shape is either all independent pages or one compound covering
all selected pages. The caller retains an original hold for each relevant unit
through validation/clone; range diagnostics cannot acquire those holds. Independent
clone releases only its successfully acquired prefix on failure. Every original
hold remains retained, so rollback cannot select last retirement. Compound clone
changes one head count. Range length belongs to the external holder, not a shared
metadata field; raw consumers must transfer that length without reconstruction
from the first descriptor. Isolated fixture tests force mid-range overflow without
exposing a production count-setting API and restore all modeled holds afterward.
