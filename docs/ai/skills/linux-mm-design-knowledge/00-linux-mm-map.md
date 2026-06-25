# Linux User VM MM Map

## 1. Design Purpose

建立用户态虚拟内存子系统的工程地图，回答 Linux 把“地址空间对象、VMA、页表、fault、fork/COW、file mapping、brk/stack、advice/locking”分别放在哪里，以及哪些路径决定可见行为。

## 2. User-visible Semantics

- 进程拥有独立地址空间；`mmap()/munmap()/mprotect()/brk()/madvise()/mlock()/msync()` 改变其布局和行为。
- 用户访问一个未映射、权限不允许、或需要 lazy allocation/COW/file-in 的地址，会触发 page fault；成功时透明恢复，失败时通常变成 `SIGSEGV` 或 `SIGBUS`。
- `fork()` 后父子进程最初共享页内容，但私有可写映射经写 fault 进入 COW。
- `MAP_SHARED` 文件映射的脏页通过 page cache 与文件回写语义关联；`MAP_PRIVATE` 文件映射读共享、写私有。

## 3. Core Data Structures

- `struct mm_struct`
  - 文件: `include/linux/mm_types.h`
  - 核心字段: `mm_mt`, `pgd`, `map_count`, `mmap_lock`, `page_table_lock`, `mm_users`, `mm_count`, `write_protect_seq`, `start_brk/brk/start_stack`
  - 设计意图: 作为单个进程地址空间的顶层对象，统一承载 VMA 树、页表根、地址空间统计与锁。
  - 生命周期: task 持有 `mm_users`; 内核临时持有 `mm_count`; `exit_mmap()` 负责释放 VMA 与页表。
- `struct vm_area_struct`
  - 文件: `include/linux/mm_types.h`
  - 核心字段: `vm_start/vm_end`, `vm_mm`, `vm_flags`, `vm_page_prot`, `vm_pgoff`, `vm_file`, `anon_vma`, `anon_vma_chain`, `shared`, `vm_ops`
  - 设计意图: 表示一段属性一致的虚拟地址范围；是 mmap、fault、rmap、truncate、COW 的核心边界对象。
  - 生命周期: 挂在 `mm->mm_mt` 中；可能被 split/merge/detach；释放前必须先从树与 rmap 结构摘除。
- `struct maple_tree`
  - 文件: `include/linux/mm_types.h`
  - 作用: `mm->mm_mt` 存放全部 VMA，替代旧的 VMA rb-tree。
- `struct vm_fault`
  - 文件: `include/linux/mm_types.h`
  - 作用: fault fast/slow path 的上下文容器，携带 `vma/address/pgoff/pmd/pte/orig_pte/flags/page`。
- `struct mmu_gather`
  - 文件: `include/asm-generic/tlb.h`
  - 作用: 聚合 unmap 和 TLB flush，保证 “先断开映射，再刷 TLB，最后释放页/页表”。
- `pte/pmd/pud/p4d/pgd`
  - 文件: `Documentation/mm/page_tables.rst`, `include/asm-generic/pgtable-*.h`
  - 作用: 统一的五级软件层次，硬件级数不足时通过 folding 跳过。

## 4. Key Code Paths

```text
mmap syscall
  -> ksys_mmap_pgoff()
  -> vm_mmap_pgoff()
  -> do_mmap()
  -> mmap_region()

munmap syscall
  -> __vm_munmap()
  -> do_munmap()
  -> do_vmi_munmap()
  -> do_vmi_align_munmap()
  -> unmap_region()
  -> free_pgtables()

page fault
  -> arch fault entry
  -> handle_mm_fault()
  -> __handle_mm_fault()
  -> handle_pte_fault()
  -> do_anonymous_page() / do_fault() / do_wp_page()

fork
  -> dup_mmap()
  -> anon_vma_fork()
  -> copy_page_range()

exit
  -> exit_mmap()
  -> unmap_vmas()
  -> free_pgtables()
  -> tear_down_vmas()
```

- 主要文件:
  - VMA 布局与 mmap/unmap: `mm/mmap.c`, `mm/vma.c`
  - fault 与页表安装: `mm/memory.c`
  - file-backed fault: `mm/filemap.c`
  - protection changes: `mm/mprotect.c`
  - mlock/msync/madvise: `mm/mlock.c`, `mm/msync.c`, `mm/madvise.c`
  - 锁规则文档: `Documentation/mm/process_addrs.rst`

## 5. Locking and Lifetime Rules

- `mmap_lock` 是地址空间元数据总锁，读锁允许并发 fault，写锁允许 VMA split/merge/attach/detach。
- `page_table_lock` 和 PTE/PMD 级锁保护页表项更新。
- `anon_vma->rwsem` 与 `mapping->i_mmap_rwsem` 是 rmap 方向的稳定化锁。
- per-VMA lock 由 `include/linux/mmap_lock.h` 定义，page fault 可走 `lock_vma_under_rcu()` 优化路径。
- VMA 生命周期规则见 `Documentation/mm/process_addrs.rst`: 先稳定对象，再遍历页表；free page table 前必须先让 VMA 对 rmap 不可达。
- TLB flush 时机由 `mmu_gather` 统一约束，见 `include/asm-generic/tlb.h`。

## 6. Important Invariants

- VMA 在同一 `mm` 中不能重叠。
- `vm_flags/vm_page_prot` 必须与用户可见权限语义一致。
- 页表安装必须发生在 VMA 已稳定、权限已检查之后。
- `munmap` 允许引发 VMA split/merge，并会先 detach 再 teardown。
- fork 的私有写映射必须在后续写入时变成 COW。

## 7. Linux Compatibility Requirements

- `mmap/munmap/mprotect/brk/msync/mlock/madvise` 的 errno 和边界行为要兼容。
- `SIGSEGV`/`SIGBUS` 的 fault 分类要保留。
- `MAP_PRIVATE`、`MAP_SHARED`、`READ_IMPLIES_EXEC`、`MAP_FIXED_NOREPLACE` 等语义要保留。
- 允许 lazy allocation、fault-driven file in、fork COW。

## 8. Simplification Opportunities

- 第一阶段可不做: swap/reclaim/memcg/NUMA/THP/userfaultfd/pkeys/DAX/KSM。
- 可不实现 per-VMA lock，先只保留 `mmap_lock + page table lock`。
- 可先用更简单的 VMA tree，而不是完整 Maple Tree。
- 可先只支持匿名私有映射和普通 page-cache 文件映射。

## 9. Test Scenarios

- `mmap(MAP_PRIVATE|MAP_ANONYMOUS)` 后读零页、写后分配匿名页。
- `fork()` 后父子对同一页分别写，验证隔离。
- `MAP_SHARED` 文件映射写脏后 `msync(MS_SYNC)` 可见到文件。
- `munmap()` 跨 VMA 边界，验证部分拆分。
- `mprotect(PROT_NONE/PROT_READ/PROT_WRITE)` 后 fault 行为与信号类型正确。

## 10. Source Index

- `include/linux/mm_types.h`
- `include/linux/mm.h`
- `include/linux/mmap_lock.h`
- `include/asm-generic/tlb.h`
- `include/asm-generic/pgtable-nop4d.h`
- `mm/mmap.c`
- `mm/vma.c`
- `mm/memory.c`
- `mm/filemap.c`
- `mm/mprotect.c`
- `mm/mlock.c`
- `mm/msync.c`
- `mm/madvise.c`
- `Documentation/mm/process_addrs.rst`
- `Documentation/mm/page_tables.rst`
