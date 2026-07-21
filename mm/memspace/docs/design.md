# memspace — 设计文档
## 定位

`memspace` 是 X-Kernel 的地址空间 owner。它维护 `MmSpace`、`VmAreaSet`、
`VmArea`、页表根和 VMA-side runtime dispatch。

它不拥有 file-backed cached content，也不拥有 anonymous page content。
内容对象分别由 `pagecache::Mapping` 和 `mm/anon` 管理；`memspace` 只保存
VMA metadata、运行时执行引用和页表状态。

## 背景

当前模型显式区分三类职责：

- `VmAreaSet`：地址空间里的 VMA metadata 容器。
- `VmArea`：单个映射实例的 range、permission、backing、file metadata 和
  inheritance metadata。
- `VmRuntimeRef` / `VmRuntimeOps`：VMA-side map/unmap/protect/fault/fork/mremap
  执行入口，角色接近 Linux `vm_operations_struct`。
- `MmUserHandle` / `MmPin`：地址空间生命周期 capability。`MmUserHandle`
  对应 Linux `mm_users`，表示仍有 runtime 或临时路径 active 使用用户映射；
  `MmPin` 预留 Linux `mm_count` 风格对象 pin，只保证 `MmSpace` 对象和稳定
  identity 可观察，不延长用户映射生命周期。

Linux 对应关系：

- `MmSpace` 对应 `mm_struct`。
- `VmArea` 对应 `vm_area_struct`。
- `VmRuntimeOps` 对应 VMA 侧 `vm_operations_struct`。
- `VmBackingInfo` 描述 VMA 指向的 file/anon/linear backing object。

## 范围

- `src/aspace.rs`
- `src/vma.rs`
- `src/fault.rs`
- `src/backend/*`
- `src/iomap.rs`

## 架构

```text
MmSpace
  -> MmUserLifetime
  -> PageTable
  -> MmCpuResidency
  -> VmAreaSet
       -> VmArea
            -> VmBackingInfo
            -> FileMappingInfo
            -> VmRuntimeRef
                 -> Linear
                 -> AnonShared
                 -> AnonPrivate
                 -> FileShared
                 -> FilePrivate

object owner
  -> vmobj::ObjectInvalidateRequest
  -> MmSpace invalidate sink
  -> VMA lookup and runtime unmap
```

## 调用约束 / 执行上下文

- `MmSpace` 由外层 `Mutex` 或等价独占访问保护。
- map/unmap/protect/fault/fork/mremap 路径可能分配内存，运行于可睡眠上下文。
- 不适用于中断上下文。
- 调用者必须在 syscall、loader 或内核 VM 边界完成用户输入校验。

## 算法流程

### VMA 查询

1. `MmSpace::find_vma()` 在 `VmAreaSet` 中定位 VMA。
2. `VmArea::backing()` 返回 `VmBackingInfo`。
3. `VmArea::page_offset()` 表示 Linux `vm_pgoff` 风格的 backing page offset。
4. `VmArea::file_mapping()` 返回 file-backed introspection metadata。
5. `VmArea::runtime()` 只在 `memspace` 内部用于执行 map/unmap/fault 操作。

### Futex backing 解析

`MmSpace::resolve_futex_backing()` 是 non-private futex key 的 MM 边界：

- private VMA 返回 `(mm_id, virtual_address)`；
- shared anon/file VMA 返回
  `(VmObjectId, backing_page_index, byte_offset_in_page)`；
- shared offset 通过 `VmArea::backing_offset_for()` 计算，因此 VMA split、trim、
  不同 virtual mapping 和非零 file offset 都保持同一 object-relative identity。

该 API 只读取 VMA metadata，不触发缺页。调用者仍需使用 kuaccess 完成实际
用户字访问与 fault-in。

### VMA 插入

1. 调用方构造 `VmArea` 和对应 `VmRuntimeRef`。
2. `MmSpace::map_runtime_vma()` 校验 VMA 范围和 runtime backing 一致性。
3. runtime 执行 eager map 或注册 lazy fault 入口。
4. `VmAreaSet::try_insert()` 拒绝重叠 VMA。
5. runtime 注册 file/anon object view 时使用当前 `mm_id` 和 `InvalidateHandle`。

### VMA 变形

- `VmAreaSet::unmap()` 支持 exact removal、front/back trim 和 middle split。
- `VmAreaSet::protect()` 支持 partial range split，只更新 current permission。
- `VmAreaSet::merge_adjacent_where()` 只合并语义等价且 runtime kind 一致的相邻 VMA。
- file-backed `FileMappingInfo.offset` 和 `page_offset` 在 split/trim 时同步更新。

### mremap relocation

`mremap` move-style relocation is not modeled as ordinary `unmap`.

```text
source snapshot
  -> map_relocated_snapshot()
  -> move_pages()
  -> drop_mapping_metadata()
```

The three ownership layers are explicit:

- `VmArea` metadata: this virtual range has a mapping role;
- present PTE residency: this virtual page currently points at a frame;
- backing object ownership: anon/file-private/pagecache object owns content
  lineage.

Relocation may change the first two layers without destroying the third.

### Page fault

```text
FaultInput
  -> MmSpace::handle_fault_input()
  -> VMA lookup
  -> permission check
  -> FaultContext with VMA-derived offsets
  -> VmRuntimeRef::handle_fault()
  -> PageFaultOutcome
```

`FaultOutcome` 保留 Linux 用户可见 fault class：

- resolved；
- retry / COW retry；
- unmapped；
- access denied；
- file-range `BusError`；
- out of memory；
- no progress / failed。

### Object-driven invalidate

```text
pagecache / anon object
  -> vmobj::ObjectInvalidateRequest
  -> InvalidateHandle
  -> MmSpace invalidate sink
  -> drain_pending_invalidations()
  -> apply_invalidate_request()
  -> runtime.unmap()
```

`MmSpace` 收到 object-side invalidate 后，重新按当前 VMA metadata 检查：

- object id 是否匹配；
- shared/private view kind 是否匹配；
- object offset 与 VMA offset 是否仍一致；
- request range 是否仍覆盖当前 VMA overlap。

匹配后只 zap present PTE，保留 VMA metadata。下一次 fault 重新按当前 object 状态裁决。

若单个 request 应用失败，`MmSpace` 将其放回队列尾部并结束本轮 drain，避免同一边界无限重试。

### Private anonymous discard / COW

- `AnonymousPrivate` 和 `FilePrivate` 的 post-write private pages 由
  `AnonPrivateObject` 持有。
- `MADV_DONTNEED` 通过 runtime 将 VMA range 转成 object range，再由
  private-anon helper detach object page slots、unmap present PTE、完成 TLB
  finalization 后释放 frame。
- COW write fault 使用 object slot 比较和 `PageTableMut::replace_if_same()` 的
  PTE snapshot 比较。任一侧发生变化都返回 retry-class outcome。

### `msync`

`MmSpace::msync_range()` owns Linux-style VMA range traversal for `msync`.
It records unmapped holes as `NoMemory` while still dispatching later mapped
VMAs in the range.

Only shared file runtimes perform writeback. Anonymous, linear, and private
file runtimes return not-applicable through the runtime hook, so private COW
pages are never written back to the source file.

`MS_INVALIDATE` returns not supported in the current model because locked VMA
and invalidate semantics are not represented yet.

### Protection Changes

`MmSpace::protect_mapping_range()` separates VMA permission state from present
PTE flags. The requested flags are recorded in `VmAreaSet::protect()`, while
`VmRuntimeOps::on_protect()` returns the effective PTE flags to install for
already-present mappings.

This allows runtimes such as shared file mappings to keep writable VMAs backed
by write-protected PTEs until the first write fault performs runtime-specific
dirty tracking.

### Fork

1. 新建 child `MmSpace`。
2. 复制每个 `VmArea` metadata。
3. shared runtime 共享 object identity。
4. private runtime 通过 `AnonPrivateObject` lineage 和页表写保护形成 COW。
5. 父/子页表修改完成 TLB finalization 后，才提交 child object page state。

## 并发模型

- `MmSpace` 由外层锁串行化。
- `MmUserHandle` 的 user-count 状态转换不依赖 `MmSpace` 主锁；从裸
  `Arc<Mutex<MmSpace>>` mint handle 时会短暂读取 mm-owned lifetime，最后一个
  user 释放时会获取 `MmSpace` 主锁并执行 `clear()`。
- `MmUserHandle::clone_user_unless_zero()` 与最后一个 user release 通过同一个原子
  状态转换互斥；一旦进入 torn-down 状态，后续 `MmPin` 不能再升级为 active user。
- `MmCpuResidency` 由 `MmSpace` 拥有，用于记录“哪些 CPU 可能还保留该地址空间
  的 TLB 状态”。调度切换路径在切入新 mm 前先 set CPU，硬件切换完成后再从旧 mm
  clear CPU，保证 shootdown 最多过度命中、不会漏掉目标 CPU。
- `InvalidateSink` 使用 `SpinNoIrq` 保护短队列操作，不在该锁内执行 runtime
  unmap 或获取 `MmSpace` 主锁。
- `PageTable::modify()` 提供页表独占修改 guard。
- `VmAreaSet` 不暴露独立可变共享访问。
- file/anon object 的内部并发由对应 object crate 自己维护。

## 设计决策

1. `VmArea` 保存 Linux 风格 VMA metadata。
   原因：split/protect/unmap/fork/mremap 都需要稳定 metadata，而不是从 runtime
   私有状态反推。

2. `VmRuntimeRef` 只表示 VMA-side execution reference。
   原因：runtime 可以执行 fault 和 PTE 操作，但不能成为 file cache 或 anon page
   owner。

3. `VmBackingInfo` 显式区分 `Linear`、`AnonymousShared`、`AnonymousPrivate`、
   `FileShared`、`FilePrivate`。
   原因：file-private mapping 必须同时保留 file source object、anon result
   object 和 anon lineage。

4. `FaultInput` 与 `FaultContext` 分离。
   原因：trap/user runtime 输入只包含 CPU 报告的 fault facts；VMA-derived
   object offsets 必须在 VMA lookup 后附加。

5. Object-driven invalidate 使用 `vmobj` request。
   原因：file-backed 和 anonymous object 都需要同一套 object -> VMA view 语言。

6. `msync` 使用 runtime hook 而不是 syscall 反查 pagecache。
   原因：`MmSpace` owns VMA traversal, while `filemap` owns file-backed runtime
   source state. This keeps syscall ABI, VMA dispatch, and content-object
   writeback in separate owner crates.

7. relocation source retirement is separate from ordinary runtime unmap.
   原因：ordinary private/file-private unmap may detach backing object pages;
   successful relocation only retires the old virtual role.

## Drop / 资源释放

- `MmSpace::clear()` 通过 runtime unmap 清理全部 VMA 的 present PTE。
- `PageTableMut::finish()` 是释放 detached frames 前的 TLB finalization boundary。
- `VmAreaSet` 清空后，VMA metadata 和 runtime references 一起释放。
- File/anon object view guards 随 runtime/object 引用释放并注销 view。
