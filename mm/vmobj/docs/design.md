# vmobj — 设计文档
## 定位

`vmobj` 提供 memory-management 主平面里的 object-neutral 公共语言。它不拥有 file-backed content，也不拥有 anonymous content；它负责的是：

- 稳定 object identity；
- object-side mapped view / rmap 记录；
- object 驱动的 view-hit invalidation work；
- object 到 `MmSpace` 的中性失效请求表达。

Linux 对应关系：

- file-backed 路径：`address_space` / `i_mmap`
- anonymous 路径：`anon_vma` / rmap

## 范围

- `src/lib.rs`
- 只定义 object/rmap 语言，不定义 page cache、anonymous page owner、VMA tree、页表执行。

## 架构

```text
kvfs::AddressSpace / anon objects
    -> MappingView / ObjectViewHit / ObjectInvalidateWork
         -> MmSpace apply side
```

这里的关键原则是：

- `MappingView` 是 object-side rmap entry；
- `ObjectInvalidateWork` 是 object owner 发出的正式失效工作项；
- `MmSpace` 只消费这些 work，不定义其语义。

## 设计决策

1. 把 `VmObjectId` 放到独立 crate，而不是继续挂在 `memspace`
   原因：它是 object-neutral identity，file-backed 和 anonymous object 都要复用。
   当前它已经是 typed identity：
   - `VmObjectId::File(FileObjectId)`
   - `VmObjectId::Anon(AnonObjectId)`
   这样 file-backed `Mapping` 和 anonymous object 不会再落进同一个无类型整数命名空间。

2. `MappingView` / `MappingViewRange` 放到独立 crate
   原因：它们描述的是 object -> mapped VMA 的关系，已经超出 `pagecache` 自身 ownership。

3. 用 `ObjectViewHit` 和 `ObjectInvalidateWork` 表达 object-side 失效
   原因：truncate、evict、hole-punch、collapse-range 都应走统一的 view-hit invalidation 主线，而不是各自发裸 range。

4. `MappingViewNotifier` 只消费 `ObjectInvalidateWork`
   原因：notifier 是 view 的消费器，不是 view 的定义核心。

5. `MappingViewId` 由 `mm/vmobj` 统一分配
   原因：file-backed `kvfs::AddressSpace` 和 anonymous objects 都要登记到同一套
   object-view/rmap 语言上；view identity 不能再由单个 object branch 私自起号。

6. `MappingViewRange` / `ObjectViewHit` / `ObjectInvalidateWork` 的坐标不变量在
   `vmobj` crate 内部冻结
   原因：object-driven invalidate 的核心风险是把 object byte range 和 VMA byte
   range 混用。`MappingView::new()` 现在拒绝空 range 或溢出 range；
   `ObjectViewHit::try_new()` 只接受完全落在 view 内的 object hit；
   `ObjectInvalidateWork::new()` 要求 work range 覆盖所有 hit。调用方仍可以用
   `MappingView::page_hit()` 做 clipping，但不应在 file/anon 分支里重新发明一套
   range 语言。

## 冻结后的 API 契约

- `VmObjectId` 只表达 object identity，不承载可解引用对象指针。
- `MappingView` 是唯一的 object -> VMA view 记录。
- `ObjectViewHit` 是唯一的 object event -> VMA hit 记录。
- `ObjectInvalidateWork` 是 object owner 产生的批量 invalidation work。
- `ObjectInvalidateRequest` 是投递给一个 `MmSpace` consumer 的单-view request。
- `MappingViewNotifier` 只能消费 `ObjectInvalidateWork` 和对应 `ObjectViewHit`；
  notifier 不应自定义新的 file/anon range 结构。
- `ObjectInvalidateWork` 携带 `VmObjectId`，notifier 必须通过
  `ObjectInvalidateWork::request_for_hit()` 派生 `ObjectInvalidateRequest`；
  notifier 不应再单独保存 object id 后手工拼 request。
- 页对齐裁剪这类 object-hit transformation 使用 `ObjectViewHit` helper
  表达，例如 file-backed truncate 的 page-aligned suffix，不在
  `filemap` 内重新构造一套 view/hit。
