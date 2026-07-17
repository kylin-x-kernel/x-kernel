# memspace — 安全与可靠性分析
## 信任模型

`memspace` 信任：

- syscall、loader、kernel VM 调用方已完成输入来源校验；
- `page_table` 正确执行 PTE 修改和 TLB finalization；
- file/anon/pagecache/vmobj crate 正确维护各自 object lifetime；
- 调用方通过外层锁保证同一 `MmSpace` 的独占可变访问。

`memspace` 负责：

- 地址空间边界检查；
- VMA 非重叠、有序和 metadata 一致性；
- VMA permission 与 page-table permission 的同步；
- fault class 的保存和上报；
- object-side invalidate request 的复核和 PTE zap。

## 外部边界 / 攻击面

- `mmap` / `munmap` / `mprotect` / `mremap` / `brk` 等地址空间编辑请求。
- CPU page fault 或 user-runtime fault request。
- `fork()` / clone address-space copy。
- file/anon object-side invalidation request。
- kernel linear mapping 和 I/O mapping helper。

## unsafe 代码清单

`src/aspace.rs`

- `read()` / `write()` 中的 `copy_nonoverlapping`
  - 依赖页表查询得到的物理地址可通过 direct map 转成有效内核地址。
  - 依赖调用方保证目标用户地址范围已通过 VMA/permission 检查。

`src/vma.rs`、`src/fault.rs` 和当前 backend metadata 代码不引入新的 `unsafe`。

## 内存安全不变量

- 每个 VMA 范围必须完全落在 `MmSpace` 地址空间边界内。
- `VmAreaSet::try_insert()` 必须拒绝重叠 VMA。
- `VmAreaSet::unmap()` / `protect()` 必须在 split/trim 后保持 VMA 非重叠、有序。
- `page_offset()` 与 `FileMappingInfo.offset` 必须随 VMA split/trim 同步调整。
- `VmArea::max_flags()` 必须在 split/protect/fork/mremap 后保留原始最大权限。
- `VmArea::backing()` 与 `VmRuntimeRef::backing_info()` 必须一致。
- `mprotect` 更新 VMA current permission 时，present PTE flags 必须使用
  `VmRuntimeOps::on_protect()` 返回值；不能无条件把 requested flags 直接写入页表。
- `msync_range()` 只能让 shared file runtime 执行 writeback；anonymous、
  linear 和 private file mappings 不能写回 file source。
- `FilePrivate` backing 必须同时保留 file source object、anon result object 和
  anon lineage。
- private-anon page state 必须由 `AnonPrivateObject` 持有，runtime 不能拥有独立
  private frame table。
- detach frame、unmap PTE、TLB finalization、frame release 必须保持顺序。
- `mremap` move-style source retirement must not call ordinary runtime
  `unmap()` on the moved source range.
- `resolve_futex_backing()` 对 shared VMA 必须返回 object-relative offset；
  不能把 VMA-relative address 当作跨进程 key。

## 线程安全

- `MmSpace` mutable operations require external exclusive access.
- `InvalidateSink` spinlock only protects queue mutation; it must not cover
  runtime unmap, page-table mutation, or sleepable lock acquisition.
- `PageTable::modify()` enforces one mutable page-table guard at a time.
- file/anon object locks must not be acquired while holding `InvalidateSink`'s
  spinlock.
- `msync_range()` may block through file-backed runtime/provider sync and is
  only valid in process context.
- `MmCpuResidency` 只允许在调度切换边界做“先加入新 mm、后移除旧 mm”的保守更新，
  不能在 page-table flush 路径中重建目标集合。

## 威胁分析

1. VMA metadata 与 runtime backing 漂移。
   - 防护：`runtime_for_vma()` 比较 `runtime.backing_info()` 和
     `vma.backing()`，不一致时拒绝继续执行。

2. File-private source object 与 anon result object 混淆。
   - 防护：`VmBackingKind::FilePrivate` 显式保存两个 object id 和 lineage。

3. Object-side invalidate 错误 zap 无关 VMA。
   - 防护：`apply_invalidate_request()` 重新检查 object id、view kind、VMA
     overlap 和 object offset。

4. Invalidate request 失败后丢失。
   - 防护：失败 request 重新入队；本轮 drain 结束，下一边界再重试。

5. COW write fault 覆盖竞争 PTE。
   - 防护：COW path 同时比较 object slot 和 `PteSnapshot`，变化时返回 retry。

6. TLB stale entry 访问已释放 frame。
   - 防护：释放 detached frame 前显式完成 `PageTableMut::finish()`。

7. 调度切换时过早把当前 CPU 从旧 mm 的目标集合移除。
   - 防护：切入路径先把 CPU 发布到新 mm，待硬件页表切换完成后再从旧 mm 清除。

7. relocation source retirement误用 ordinary unmap。
   - 防护：successful move uses metadata-only retirement after destination
     install and PTE transfer.

## 故障模式与影响分析（FMEA）

| 故障 | 条件 | 处理 | 影响 |
|---|---|---|---|
| VMA overlap | 插入路径未校验 | `VmAreaSet::try_insert()` 拒绝 | 避免地址空间歧义 |
| Runtime/backing mismatch | runtime 与 VMA metadata 不一致 | 返回 `BadAddress` 并记录 warning | fault/map 操作失败 |
| Invalidate apply failure | runtime unmap 或 page-table path 失败 | request 回队，本轮 drain 停止 | stale PTE 延迟清除 |
| COW conflict | object slot 或 PTE 变化 | 返回 retry-class outcome | fault retry |
| OOM | frame 或页表分配失败 | 返回 `OutOfMemory` / `NoMemory` | syscall 或 fault 失败 |
| File EOF fault | file object 拒绝越界页 | `BusError` outcome | 上层映射为 SIGBUS-class 行为 |
| `mremap` source teardown destroys moved private backing | 误把 relocation 当 ordinary unmap | metadata-only retirement | 避免 moved mapping 指向已释放对象页 |

## 故障管理

- 地址空间编辑失败通过 `KResult` 返回。
- Fault path 返回 `FaultOutcome`，不得在 `memspace` 边界压成 bool。
- Retry-class fault 包括 ordinary retry 和 COW conflict retry。
- `BusError` 表示 backing object 级错误，例如 mapped-file EOF/range fault。
- Invalidate drain 失败只影响当前 request；request 保留在队列中等待下一边界。

## 已知限制

- `AddrSpace` 仍作为 `MmSpace` 的兼容别名存在。
- `BackendOps` 仍用于 anonymous/linear runtime 的共享执行 helper。
- Writable regular-file `MAP_SHARED`、reclaim/swap/memcg/NUMA/THP 不由
  `memspace` 当前路径实现。
- `MS_INVALIDATE`、locked-page `EBUSY` 和释放 `MmSpace` 锁后再执行慢速
  provider sync 未实现。
- Invalidate sink 是 queue/drain 模型，不提供 Linux 同步 rmap 树的强同步语义。

## 审计清单

- [ ] 新 runtime 的 `backing_info()` 是否与安装的 `VmArea.backing()` 一致。
- [ ] VMA split/trim/protect 是否保持 `page_offset` 和 file offset 连续性。
- [ ] Fault path 是否先做 VMA permission check，再调用 runtime。
- [ ] File-private path 是否同时保留 file object 和 anon object identity。
- [ ] Private-anon frame release 是否发生在 TLB finalization 之后。
- [ ] Invalidate request 是否通过 `vmobj` object/view language 进入 `MmSpace`。
- [ ] `InvalidateSink` spinlock 内是否只做队列操作。
- [ ] Unsupported Linux semantics 是否显式返回错误而不是静默降级。
