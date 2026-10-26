# memspace — 安全与可靠性分析
## 信任模型

分析范围包含 `src/` 的生产实现和 `ut-tests/` 的内核测试夹具。
测试文件通过 `unittest::path_mod_test!` 挂载在原私有模块中，仅在
`cfg(unittest)` 下编译；ASID 测试仍受父模块的 AArch64 架构门控。
测试迁移不改变生产可见性、unsafe 前置条件或地址空间资源的生命周期。

`memspace` 信任：

- syscall、loader、kernel VM 调用方已完成输入来源校验；
- `page_table` 正确执行 PTE 修改和 TLB finalization；
- KVFS file address-space、anon 与 vmobj crate 正确维护各自 object lifetime；
  `pagecache` 只维护其私有 folio storage；
- 调用方通过外层锁保证同一 `MmSpace` 的独占可变访问。

`memspace` 负责：

- 地址空间边界检查；
- VMA 非重叠、有序和 metadata 一致性；
- VMA permission 与 page-table permission 的同步；
- fault class 的保存和上报；
- object-side invalidate request 的复核和 PTE zap。
- `MmUserHandle` / `MmPin` 生命周期 capability，区分 active user mappings
  与对象 pin。

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
- 私有映射的 sparse range unmap 只跳过不存在的页表子树和 non-present
  项；实际叶大小必须符合 runtime。页表遍历失败时保留 backing ownership，
  detached slots 的 RAII rollback 恢复对象记录；已经清除的 PTE 前缀可在
  后续 fault 中重新建立，不能在失败路径提前释放 frame。原地址空间锁与
  TLB guard 仍覆盖该过程，不释放中间页表。
- `mremap` move-style source retirement must not call ordinary runtime
  `unmap()` on the moved source range.
- `relocate_mapping()` prepares every destination PTE before removing source
  PTEs. On failure, it finalizes rollback TLB invalidation before dropping only
  destination metadata and runtime references; rollback cannot detach the
  anonymous slots still owned by the source.
- Mapping-policy registrations use a separate metadata-only retirement hook;
  rollback does not call runtime `unmap()` and therefore cannot release source
  anonymous slots or alter source PTE ownership.
- `resolve_futex_backing()` 对 shared VMA 必须返回 object-relative offset；
  不能把 VMA-relative address 当作跨进程 key。
- `MmUserHandle` 是唯一能保留用户映射生命周期的 capability；`MmPin` 和普通
  `Arc<Mutex<MmSpace>>` observer 只能保留对象可观察性，不能阻止最后一个 user
  释放时清空映射。
- `MmPin` can be cloned in a short critical section. Final release can destroy
  the page table and requires sleepable context outside spinlocks, after CPUs
  stop using that root. Cloning cannot resurrect active users of a torn-down MM.
  An exiting thread's pin does not authorize further access to user memory.

## 线程安全

- `MmSpace` mutable operations require external exclusive access.
- `InvalidateSink` spinlock only protects queue mutation; it must not cover
  runtime unmap, page-table mutation, or sleepable lock acquisition.
- `PageTable::modify()` enforces one mutable page-table guard at a time.
- file/anon object locks must not be acquired while holding `InvalidateSink`'s
  spinlock.
- `msync_range()` may block through file-backed runtime/provider sync and is
  only valid in process context.
- Scheduling adds the CPU to the new `MmCpuResidency` before activating its
  root; old-MM CPU bits remain conservative rather than being cleared at each
  switch. Page-table flush paths must not reconstruct the target set.
- `MmUserHandle::clone_user_unless_zero()` 和最后一个 user release 使用同一个原子
  状态转换；进入 torn-down 状态后，`MmPin::try_upgrade_user()` 必须失败。
  Cloning can acquire an active user under a short spinlock, but the returned
  handle must be released outside it. Last-user release locks the MM and clears
  mappings; it requires sleepable context without the MM lock already held.

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

8. mm object observer 阻止退出清理或重新激活已 teardown 的地址空间。
   - 防护：只有 `MmUserHandle` 增加 active user 计数；`MmPin` 只 pin 对象，且
     upgrade 与最后一个 release 通过单原子状态互斥。

## 故障模式与影响分析（FMEA）

| 故障 | 条件 | 处理 | 影响 |
|---|---|---|---|
| VMA overlap | 插入路径未校验 | `install_runtime_mapping` 在改页表和注册 object view 之前先用 `VmAreaSet::overlaps()` 预检并返回 `AlreadyExists`，`VmAreaSet::try_insert()` 再拒绝一次 | 避免地址空间歧义；用户态 syscall 序列能触发的重叠退化为可恢复错误，不再 panic 整机 |
| Runtime/backing mismatch | runtime 与 VMA metadata 不一致 | 返回 `BadAddress` 并记录 warning | fault/map 操作失败 |
| Invalidate apply failure | runtime unmap 或 page-table path 失败 | request 回队，本轮 drain 停止 | stale PTE 延迟清除 |
| COW conflict | object slot 或 PTE 变化 | 返回 retry-class outcome | fault retry |
| OOM | frame 或页表分配失败 | 返回 `OutOfMemory` / `NoMemory` | syscall 或 fault 失败 |
| File backend fault | 文件页越界、I/O失败或配额耗尽 | `BusError` outcome | 上层投递 SIGBUS，普通 write 的 ENOSPC 不变 |
| `mremap` source teardown destroys moved private backing | 误把 relocation 当 ordinary unmap | metadata-only retirement | 避免 moved mapping 指向已释放对象页 |
| Stale mm user acquire | 最后一个 user release 与新 user acquire 并发 | 单原子状态 CAS；torn-down 后 upgrade/acquire 失败 | 避免退出清理后重新使用已清空映射 |

## 故障管理

- 地址空间编辑失败通过 `KResult` 返回。
- Fault path 返回 `FaultOutcome`，不得在 `memspace` 边界压成 bool。
- Retry-class fault 包括 ordinary retry 和 COW conflict retry。
- `BusError` 表示 backing object 级错误，包括 mapped-file EOF/range fault、I/O
  失败及 `StorageFull`。配额耗尽不表示地址权限错误，不应归为 SIGSEGV；分配失败
  `NoMemory` 仍单独归类为 `OutOfMemory`。
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
- [ ] 新增地址空间 observer 是否只持有 `MmPin` 或普通 `Arc`，不误用
  `MmUserHandle` 延长用户映射生命周期。

## 失效完成通知顺序

运行时完成回调必须位于 PTE 删除和 TLB finish 之后，且由同一 MM 独占锁
串行；只能释放通知区间的运行时引用，不能额外清除其他映射。失效失败
不得触发回调，故仍可能可见的 PTE 始终有 backing 引用保护。

## DMA Mapping Capability

memspace directly implements kiface-mm-api DmaMappingIf. The caller selects Coherent
or Normal mode, not arbitrary flags. The provider serializes protection with
the existing kernel layout lock and preserves previous error behavior.
Coherent is READ|WRITE|UNCACHED|SHARED; Normal is READ|WRITE. The caller owns the
allocation and quiesces device access before restoration. Allocation, platform
sharing, rollback and device-cache synchronization are outside this capability.
