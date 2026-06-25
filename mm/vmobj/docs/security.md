# vmobj — 安全文档
## 信任边界

`vmobj` 只承载 object-neutral 语言层，不直接执行页表修改，也不直接拥有对象内容。

它的主要安全责任是：

- object identity 不被复用或混淆；
- mapped view 坐标稳定且自洽；
- object-side invalidation work 不能把 object 坐标和 VMA 坐标混在一起。

## 关键约束

1. `VmObjectId` 只用于稳定身份比较，不用于解引用。
   它必须保留 object family 信息，避免 file-backed 与 anonymous object
   复用同一裸整数命名空间。
2. `MappingViewRange` 同时保存：
   - VMA 坐标
   - object 坐标
   调用方不得自行从一组字段“猜”另一组坐标。
3. `ObjectViewHit` 必须落在其 `MappingView` 覆盖范围内。
4. `ObjectInvalidateWork` 只是 object-side work item；是否 zap PTE，仍由 `MmSpace` 按当前 VMA/object 关系复核。
5. `MappingViewRange` 不允许为空或发生 end offset 溢出。
6. `ObjectInvalidateWork` 的 object range 必须覆盖其中每个 `ObjectViewHit`。
7. file-backed 与 anonymous object 都必须复用 `ObjectViewHit` /
   `ObjectInvalidateWork`，不能各自定义第二套 invalidate 坐标语言。
8. `ObjectInvalidateWork.object()` 是 request 派生的唯一 object identity 来源；
   notifier 不得保存另一份 object id 后与 hit 重新组合。

## 审计清单

- 新 object owner 注册 view 时是否使用 `MappingViewSpec` / `MappingView`。
- object event 转换成 VMA hit 时是否使用 `MappingView::page_hit()` 或
  `ObjectViewHit::try_new()`。
- notifier 是否只投递 `ObjectInvalidateRequest`，而不是携带自定义 range。
- 手工构造 `ObjectViewHit::new()` 的调用点是否已经证明 hit 完全落在 view 内。
- object-driven invalidate 是否只通过 `ObjectInvalidateRequest` 进入
  `MmSpace`，而不是保留 file/page-index 风格的旁路入口。
