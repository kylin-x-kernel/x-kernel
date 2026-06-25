# Anonymous Memory

## 1. Design Purpose

匿名内存覆盖 `MAP_PRIVATE|MAP_ANONYMOUS`、`brk`、栈增长和私有文件页 COW 后的匿名化结果。Linux 目标是按 fault 惰性分配物理页，并通过 `anon_vma` 支持 fork/COW/rmap。

## 2. User-visible Semantics

- 新匿名映射读出来是零。
- 第一次读 fault 可映射全局零页；第一次写 fault 分配私有页。
- `MAP_PRIVATE` 文件映射写后得到匿名页副本，但 VMA 本身未必变成纯匿名。

## 3. Core Data Structures

- `struct anon_vma`
  - 文件: `mm/rmap.c`, `include/linux/mm_types.h` 间接使用
  - 作用: 把共享 COW 血缘的匿名 VMA 挂起来，支持 reverse mapping。
- `vm_area_struct::anon_vma`, `anon_vma_chain`
  - 文件: `include/linux/mm_types.h`
  - 生命周期: 可能延迟到首次 fault 时才准备，见 `__vmf_anon_prepare()`.
- `struct folio`
  - 页内容实际承载者；匿名 fault 中通过 `alloc_anon_folio()` 获得。

## 4. Key Code Paths

```text
anon read/write fault
  -> handle_pte_fault()
  -> do_anonymous_page()
  -> vmf_anon_prepare()
  -> alloc_anon_folio()
  -> folio_add_new_anon_rmap()
  -> set_ptes()
```

- `do_anonymous_page()` in `mm/memory.c`
  - 读 fault 且允许零页时: 直接用 `my_zero_pfn()` 建 special PTE。
  - 写 fault: `vmf_anon_prepare()` -> `alloc_anon_folio()` -> `folio_add_new_anon_rmap()` -> `folio_add_lru_vma()` -> `set_ptes()`.
- `__vmf_anon_prepare()` in `mm/memory.c`
  - 若仅持 per-VMA lock 且尚无 `anon_vma`，可能要求退回到持 `mmap_lock` 的慢路径。
- `__anon_vma_prepare()` / `anon_vma_fork()` in `mm/rmap.c`
  - 建立或复制匿名血缘。

## 5. Locking and Lifetime Rules

- 准备 `anon_vma` 可能需要检查相邻 VMA，因此只持 per-VMA lock 不够，源码明确要求可能回退到 `mmap_lock`。
- 安装匿名 PTE 时需要 PTL。
- `anon_vma_chain` 的串接由 `mmap_lock` 和 `page_table_lock`/anon_vma 锁协同保护。

## 6. Important Invariants

- 新匿名页插入前必须先准备好 `anon_vma`。
- 匿名读 fault 可共享零页，但写后必须转私有匿名页。
- `folio_add_new_anon_rmap()` 必须在 PTE 建立流程内正确配对，保证 rmap 和 mapcount 一致。

## 7. Linux Compatibility Requirements

- 匿名映射初值为 0。
- 私有匿名页在 fork/COW 后保持进程隔离。
- `MADV_DONTNEED`/munmap 后再次访问可重新得到零填充语义。

## 8. Simplification Opportunities

- 第一阶段可不做 KSM、swap、large folio。
- 可先不用零页优化，直接首次 fault 分配页，但这会偏离 Linux 的性能设计，不应影响语义。

## 9. Test Scenarios

- 匿名映射读未写返回全零。
- 匿名映射首次写后数据保持。
- fork 后父写不影响子，子写不影响父。

## 10. Source Index

- `mm/memory.c:do_anonymous_page`
- `mm/memory.c:__vmf_anon_prepare`
- `mm/rmap.c:__anon_vma_prepare`
- `mm/rmap.c:anon_vma_fork`
- `mm/rmap.c:folio_add_new_anon_rmap`
