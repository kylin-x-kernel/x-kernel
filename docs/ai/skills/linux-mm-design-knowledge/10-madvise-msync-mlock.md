# madvise msync mlock

## 1. Design Purpose

这组接口不直接创建地址空间，而是修改“驻留、写回、继承、回收建议、锁页”行为。它们通常复用现有 VMA/fault/unmap 基础设施。

## 2. User-visible Semantics

- `madvise()` 提供建议，部分 advice 改 VMA flags，部分 advice 主动回收/预热/打散。
- `msync(MS_SYNC)` 要求共享文件映射与底层文件同步。
- `MS_ASYNC` 在现代 Linux 中基本是 no-op。
- `mlock/mlock2/munlock/mlockall` 控制页锁定和未来映射默认锁定行为，受 `RLIMIT_MEMLOCK` 限制。

## 3. Core Data Structures

- `mm_struct::locked_vm`, `def_flags`
  - 文件: `include/linux/mm_types.h`
  - 作用: 跟踪锁页数量和 `mlockall(MCL_FUTURE)` 默认行为。
- `VM_LOCKED`, `VM_LOCKONFAULT`
  - 文件: `include/linux/mm.h`
  - 作用: VMA 层面的锁页策略。

## 4. Key Code Paths

```text
madvise
  -> do_madvise()
  -> advice-specific walkers / vma_modify paths

msync
  -> SYSCALL_DEFINE3(msync)
  -> find_vma()
  -> vfs_fsync_range()

mlock
  -> do_mlock()
  -> apply_vma_lock_flags()
  -> mlock_fixup()
  -> mlock_vma_pages_range()
```

- `do_madvise()` in `mm/madvise.c`
  - 入口；不同 advice 复用 `find_vma[_prev]`, `vma_lookup`, `vma_modify_*`。
  - 与本知识库相关的重点是它常常通过 split/merge 改 `VM_DONTCOPY`, `VM_WIPEONFORK` 等标志。
- `SYSCALL_DEFINE3(msync)` in `mm/msync.c`
  - 遍历覆盖区间的 VMA。
  - `MS_INVALIDATE` + `VM_LOCKED` 返回 `-EBUSY`。
  - `MS_SYNC` 对 `VM_SHARED` 且有 `vm_file` 的 VMA 调 `vfs_fsync_range()`。
- `do_mlock()` / `mlock_fixup()` in `mm/mlock.c`
  - 检查 capability 和 `RLIMIT_MEMLOCK`
  - 通过 `vma_modify_flags()` 类似的 split/merge 路径设置 `VM_LOCKED/VM_LOCKONFAULT`
  - 成功后 `__mm_populate()` 或 `mlock_vma_pages_range()`

## 5. Locking and Lifetime Rules

- `madvise` 大多需要 `mmap_lock`，部分 advice 还会触发页表或 rmap 操作。
- `msync` 主要持 `mmap_read_lock()`，在调用 `vfs_fsync_range()` 前会暂时放锁。
- `mlock` 改 flags 需要 `mmap_write_lock()`；实际 fault-in/populate 在解锁后继续。

## 6. Important Invariants

- `MS_ASYNC` 不应强制启动 I/O，这在 `mm/msync.c` 有明确兼容注释。
- `VM_LOCKED` 不适用于某些 special VMA，`mlock_fixup()` 会过滤 `VM_SPECIAL`、hugetlb、gate VMA、DAX、secretmem 等。
- `MCL_FUTURE` 通过 `mm->def_flags` 影响未来创建的 VMA。

## 7. Linux Compatibility Requirements

- `MS_ASYNC` 现代 no-op 语义必须知道，否则会设计错接口。
- `mlock` 资源限制、特权绕过、`MLOCK_ONFAULT` 语义要保留。
- `madvise` 中影响 fork 继承的 flags 语义要兼容，如 `MADV_DONTFORK/WIPEONFORK`。

## 8. Simplification Opportunities

- 第一阶段可只支持少数 advice: `DONTNEED`, `WILLNEED`, `DONTFORK`, `DOFORK`, `WIPEONFORK`, `KEEPONFORK`。
- 可先不做 KSM、collapse、hugepage 相关 advice。
- 可先把 `mlock` 做成纯 flag + prefault，不做复杂 unevictable LRU 集成。

## 9. Test Scenarios

- `mlock` 后页面常驻，超出 `RLIMIT_MEMLOCK` 返回错误。
- `mlock2(MLOCK_ONFAULT)` 仅在访问时 fault-in。
- `msync(MS_SYNC)` 后共享映射修改持久化到文件。
- `madvise(DONTFORK/WIPEONFORK)` 后 fork 行为变化符合预期。

## 10. Source Index

- `mm/madvise.c:do_madvise`
- `mm/msync.c:SYSCALL_DEFINE3(msync)`
- `mm/mlock.c:do_mlock`
- `mm/mlock.c:mlock_fixup`
- `mm/mlock.c:apply_mlockall_flags`
