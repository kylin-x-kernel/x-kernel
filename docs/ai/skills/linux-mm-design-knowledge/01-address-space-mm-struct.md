# Address Space And mm_struct

## 1. Design Purpose

`mm_struct` 是 Linux 用户地址空间的总拥有者。它把 VMA 索引、页表根、统计、锁、地址布局参数和 fork/COW 相关序列号收拢到一个对象里。

## 2. User-visible Semantics

- 一个线程组通常共享同一个 `mm_struct`。
- `/proc/<pid>/maps`、`brk()`、`mmap()` 布局、page fault 成败、fork 后地址空间复制，最终都以 `mm_struct` 为准。
- `mm->mmap_base`, `task_size`, `start_brk/brk/start_stack` 决定用户态地址布局与增长方向。

## 3. Core Data Structures

- `struct mm_struct`
  - 文件: `include/linux/mm_types.h`
  - 核心字段:
    - `mm_mt`: 所有 VMA 的 Maple Tree。
    - `pgd`: 页表根。
    - `mmap_base`, `mmap_legacy_base`, `task_size`: unmapped area 选择与 ASLR 布局。
    - `mm_users`, `mm_count`: 用户引用与结构体引用分离。
    - `map_count`: VMA 数量，上限受 `sysctl_max_map_count` 控制，`do_mmap()` 检查。
    - `page_table_lock`: 页表与部分计数保护。
    - `mmap_lock`: 地址空间元数据总锁。
    - `write_protect_seq`: fork 期间建立 COW 的写保护序列。
    - `locked_vm`, `total_vm`, `data_vm`, `exec_vm`, `stack_vm`: 资源统计。
    - `start_code/end_code/start_data/end_data/start_brk/brk/start_stack`: 进程映像边界。
  - 生命周期:
    - `mmget/mmput` 管 `mm_users`
    - `mmgrab/mmdrop` 管 `mm_count`
    - `mm_users` 降到 0 后进入 `exit_mmap()`
- `struct maple_tree`
  - 文件: `include/linux/mm_types.h`
  - 设计意图: 高效按地址查找、迭代、split/merge VMA。

## 4. Key Code Paths

```text
fork
  -> dup_mm()
  -> dup_mmap()
  -> __mt_dup(&oldmm->mm_mt, &mm->mm_mt)
  -> copy_page_range()

exit
  -> mmput()
  -> exit_mmap()
  -> mt_clear_in_rcu(&mm->mm_mt)
  -> free_pgtables()
  -> __mt_destroy(&mm->mm_mt)
```

- `dup_mmap()` in `mm/mmap.c`
  - 职责: 复制 VMA 元数据，建立 child `mm` 的地址空间。
  - 关键分支: `VM_DONTCOPY`, `VM_WIPEONFORK`, file-backed VMA 插入 `i_mmap`, `copy_page_range()`.
- `exit_mmap()` in `mm/mmap.c`
  - 职责: unmap 全部 VMA、刷 TLB、释放页表、再释放 VMA。
  - 关键分支: 先 `unmap_vmas()`，再在 write lock 下 `free_pgtables()`，最后 `tear_down_vmas()`。

## 5. Locking and Lifetime Rules

- `mmap_lock`
  - 读锁: 查找 VMA、page fault 慢路径、`msync()` 遍历。
  - 写锁: `mmap/munmap/mprotect/brk/mlock` 等修改地址空间元数据。
- `mm_lock_seq` / per-VMA lock
  - 文件: `include/linux/mmap_lock.h`
  - 用于 page fault 的乐观读锁方案。
- `write_protect_seq`
  - 文件: `include/linux/mm_types.h`
  - 用于 fork 时 page table write-protect 与后续 COW 可见性。
- `mm_struct` free 规则
  - VMA 与页表必须先清空，再允许结构体真正释放。

## 6. Important Invariants

- `mm_mt` 中的 VMA 覆盖了该地址空间的全部普通用户映射。
- `map_count` 与树中 VMA 数量一致。
- `pgd` 是 fault walk 的根；`exit_mmap()` 后不能再被用户访问路径使用。
- `mm_users == 0` 后地址空间不能再被普通用户线程使用。

## 7. Linux Compatibility Requirements

- fork 复制出的地址空间必须保留 VMA 布局、权限、文件偏移和大部分 flags。
- `VM_DONTCOPY`/`VM_WIPEONFORK` 等 fork 特殊语义必须兼容。
- `max_map_count`、`RLIMIT_DATA`、`RLIMIT_MEMLOCK` 等资源限制影响必须保留。

## 8. Simplification Opportunities

- 新内核第一阶段可不做 `mm_lock_seq` 和 per-VMA lock。
- 可先不做复杂统计字段，只保留 `map_count`, `brk`, `pgd` 和基础计数。
- 可先省略 `mmu_notifier_subscriptions`、futex/AIO/memcg 相关成员。

## 9. Test Scenarios

- fork 后 child 继承 VMA 布局但不共享 `mm_struct` 身份。
- 创建大量小映射直到触发 `max_map_count`。
- 进程退出后地址空间完全回收，无悬挂页表。

## 10. Source Index

- `include/linux/mm_types.h`
- `include/linux/mmap_lock.h`
- `mm/mmap.c:dup_mmap`
- `mm/mmap.c:exit_mmap`
- `Documentation/mm/active_mm.rst`
