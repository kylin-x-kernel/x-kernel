# VMA Design

## 1. Design Purpose

VMA 是 Linux 用户地址空间的段对象。它把“连续地址范围 + 一致权限/后端/策略/生命周期回调”绑定在一起，避免每页都存元数据。

## 2. User-visible Semantics

- 一次 `mmap()` 通常新增一段 VMA；`munmap()`、`mprotect()`、`mlock()`、`madvise()` 可能 split/merge 现有 VMA。
- 两段地址即使连续，只要权限、后端文件、offset 线性关系、fork/advice flags 不一致，就不能合并成一个 VMA。
- 栈 VMA 可按 `VM_GROWSDOWN` 延展，普通 VMA 不行。

## 3. Core Data Structures

- `struct vm_area_struct`
  - 文件: `include/linux/mm_types.h`
  - 核心字段:
    - `vm_start/vm_end`: `[start, end)` 范围。
    - `vm_mm`: 所属地址空间。
    - `vm_flags`/`vm_page_prot`: 用户权限与页表权限模板。
    - `vm_pgoff`: 对 file-backed 或匿名线性偏移都关键。
    - `vm_file`: 文件后端；匿名映射则为 `NULL`，`MAP_SHARED|MAP_ANONYMOUS` 会转成 shmem file。
    - `anon_vma`, `anon_vma_chain`: 匿名 rmap/COW/fork 关系。
    - `shared`: 插入 `address_space->i_mmap` 区间树。
    - `vm_ops`: fault/open/close/page_mkwrite/mprotect 等回调。
  - 生命周期: 在 `mm->mm_mt` 中查找；修改时可能 split/merge；detach 后才能真正 free。

## 4. Key Code Paths

```text
address lookup
  -> vma_lookup()
  -> find_vma()
  -> find_vma_prev()

shape change
  -> vma_modify()
  -> vma_merge_existing_range()
  -> split_vma()

new mapping
  -> __mmap_region()
  -> vma_merge_new_range()
  -> __mmap_new_vma()
```

- `find_vma()`, `find_vma_prev()` in `mm/mmap.c`
  - 基于 `mm->mm_mt` 做地址查找。
- `vma_modify_flags()` in `mm/vma.c`
  - 在 flags 改变场景下先尝试 merge，不行再 split。
- `vma_merge_new_range()` / `vma_merge_existing_range()` in `mm/vma.c`
  - 决定新老 VMA 是否可以吸收或重组。
- `split_vma()` / `__split_vma()` in `mm/vma.c`
  - 处理局部 `munmap/mprotect/mlock/madvise`。

## 5. Locking and Lifetime Rules

- 读元数据: `mmap_read_lock()` 或 `lock_vma_under_rcu()`。
- 改元数据: `mmap_write_lock()`，多数场景还要 `vma_start_write()`。
- 修改 `anon_vma` 链和部分 rmap 字段时需要 `page_table_lock`/rmap 锁配合。
- file-backed VMA 插入/移除 `i_mmap` 树需要 `mapping->i_mmap_rwsem`。

## 6. Important Invariants

- 同一 `mm` 内 VMA 不重叠。
- 可 merge 的 VMA 必须在 flags、policy、file、`vm_pgoff` 线性关系等方面兼容。
- `vm_start/vm_end` 改变时，rmap 区间树也必须保持一致。
- 文件映射 VMA 可能同时在 `i_mmap` 和 `anon_vma` 中，典型于 `MAP_PRIVATE` 文件页已 COW。

## 7. Linux Compatibility Requirements

- `mprotect()`、`munmap()` 对部分区间操作会导致 split，这个用户可通过 `/proc/maps` 观察到。
- `MAP_SHARED|MAP_ANONYMOUS` 在 Linux 上依赖 shmem 语义。
- 栈 guard gap 与 `VM_GROWSDOWN` 语义要保留。

## 8. Simplification Opportunities

- 可不做 per-VMA lock。
- 可先不做复杂 merge 条件中的 NUMA policy、userfaultfd、anon name。
- 可先用简单平衡树代替 Maple Tree，但必须保留“不重叠 + 支持 split/merge”的抽象。

## 9. Test Scenarios

- 相邻同属性映射应合并。
- 相邻不同权限映射不能合并。
- 对 VMA 中段 `mprotect()` 后应 split 成三段或两段。
- 文件私有映射写后，VMA 仍 file-backed，但页可进入 anon rmap。

## 10. Source Index

- `include/linux/mm_types.h:struct vm_area_struct`
- `mm/mmap.c:find_vma`
- `mm/mmap.c:find_vma_prev`
- `mm/vma.c:vma_modify`
- `mm/vma.c:vma_merge_existing_range`
- `mm/vma.c:vma_merge_new_range`
- `mm/vma.c:split_vma`
- `Documentation/mm/process_addrs.rst`
