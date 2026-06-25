# Copy-On-Write Design

## 1. Design Purpose

COW 让 `fork()` 不必立即复制全部私有页，只复制 VMA/页表元数据，并在第一次写入时复制实际内容。

## 2. User-visible Semantics

- `fork()` 后父子看到相同初始内容。
- 对私有可写页的第一次写入会变慢，但之后双方互不影响。
- `MAP_SHARED` 写 fault 不做 COW，只做写使能/脏页跟踪。

## 3. Core Data Structures

- `mm_struct::write_protect_seq`
  - 文件: `include/linux/mm_types.h`
  - 设计意图: fork 期间统一标记 “正在为未来 COW 做写保护”。
- `vm_area_struct::anon_vma`
  - 作用: 父子 VMA 的匿名血缘。
- `vm_fault.orig_pte`, `vm_fault.page`, `vm_fault.cow_page`
  - 作用: 写 fault 时比较旧 PTE、拷贝旧内容、安装新页。

## 4. Key Code Paths

```text
fork setup
  -> dup_mmap()
  -> anon_vma_fork()
  -> copy_page_range()
  -> copy_pte_range()

write fault on private page
  -> handle_pte_fault()
  -> do_wp_page()
  -> wp_page_copy() or wp_page_reuse()
```

- `dup_mmap()` in `mm/mmap.c`
  - 复制 VMA，并对非 `VM_WIPEONFORK` 场景调用 `copy_page_range()`.
- `copy_page_range()` in `mm/memory.c`
  - 遍历父页表，建立 child 页表，同时把需要 COW 的映射变成写保护。
- `do_wp_page()` in `mm/memory.c`
  - shared mapping: `wp_page_shared()`/`wp_pfn_shared()`
  - private mapping: 若 `PageAnonExclusive` 或可独占复用则 `wp_page_reuse()`；否则 `wp_page_copy()`.
- `wp_page_copy()` in `mm/memory.c`
  - 分配新 folio -> 拷贝旧页 -> `mmu_notifier_invalidate_range_start()` -> 重新检查 `orig_pte` -> `ptep_clear_flush()` -> 安装新 PTE -> 更新 rmap -> 结束 invalidate。

## 5. Locking and Lifetime Rules

- `do_wp_page()` 进入时持有非独占 `mmap_lock` 和已锁定 PTE。
- `wp_page_copy()` 在复制阶段不持 PTL，回写前重新获取 PTL 并再次比对 `orig_pte`。
- 旧页 mapcount/rmap 的减少严格排在新 PTE 安装之后，源码中有顺序注释，避免旧页过早被复用。

## 6. Important Invariants

- 父子私有页在首次写后必须彻底隔离。
- shared writable mapping 不能错误进入匿名 COW。
- 旧 PTE 清除和 TLB flush 必须发生在旧页可被复用之前。
- `orig_pte` 如果变化，当前 fault 不能盲目覆盖，必须重试/退出。

## 7. Linux Compatibility Requirements

- fork 后 `MAP_PRIVATE` 写隔离语义必须严格兼容。
- `VM_WIPEONFORK`、`VM_DONTCOPY` 特例必须保留。
- `userfaultfd`/soft-dirty 等附加位在完整 Linux 中会参与 COW；新内核若裁剪需明确不兼容范围。

## 8. Simplification Opportunities

- 第一阶段可不做 KSM、UFFD-WP、soft-dirty、large anon folio 复用优化。
- 但 `copy_page_range + do_wp_page + wp_page_copy` 的分层结构值得保留。

## 9. Test Scenarios

- fork 后父写子不变，子写父不变。
- `MAP_SHARED` 文件页写 fault 不复制物理页。
- 连续多次写同一私有页，仅第一次进入复制路径。

## 10. Source Index

- `include/linux/mm_types.h:write_protect_seq`
- `mm/mmap.c:dup_mmap`
- `mm/memory.c:copy_page_range`
- `mm/memory.c:do_wp_page`
- `mm/memory.c:wp_page_copy`
- `mm/rmap.c:anon_vma_fork`
