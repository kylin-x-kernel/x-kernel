# Page Fault Path

## 1. Design Purpose

page fault path 把“地址是否合法、VMA 权限是否允许、页表是否存在、后端页如何准备、最终如何装入 PTE/PMD”串成一个分层状态机。

## 2. User-visible Semantics

- 合法的 lazy allocation/file mapping/COW fault 对用户透明。
- 权限不允许或地址不在 VMA 内通常是 `SIGSEGV`。
- 文件映射超出 `i_size` 的 fault 通常是 `SIGBUS`。
- major/minor fault 统计反映 fault 是否需要慢速 I/O/重试。

## 3. Core Data Structures

- `struct vm_fault`
  - 文件: `include/linux/mm_types.h`
  - 核心字段: `vma`, `address`, `pgoff`, `pud/pmd/pte`, `orig_pte`, `page`, `cow_page`, `flags`
  - 生命周期: 单次 fault 调用栈内有效。
- `struct vm_area_struct`
  - `vm_flags`, `vm_ops`, `vm_page_prot`, `vm_file` 决定具体 fault 类型。

## 4. Key Code Paths

```text
arch page fault entry
  -> handle_mm_fault()
  -> sanitize_fault_flags()
  -> arch_vma_access_permitted()
  -> __handle_mm_fault()
  -> handle_pte_fault()
  -> do_anonymous_page()
     or do_fault()
     or do_wp_page()
```

- `handle_mm_fault()` in `mm/memory.c`
  - 职责: 统一入口，做 flags 合法化、架构权限检查、memcg/fault accounting。
  - 关键分支: hugetlb fault vs normal fault；`VM_DROPPABLE` OOM 降级。
  - 重要注释: `__handle_mm_fault()` 可能丢掉 `mmap_lock`，返回后不能再解引用 `vma`。
- `__handle_mm_fault()` in `mm/memory.c`
  - 职责: 逐级页表 walk/alloc，再落入 `handle_pte_fault()`.
- `handle_pte_fault()` in `mm/memory.c`
  - 根据 `pte none/present/write-protect/file/anon` 选择 fault 子路径。
- `do_fault()`
  - 针对带 `vm_ops->fault` 的文件等后端，再细分 `do_read_fault()/do_cow_fault()/do_shared_fault()`.

## 5. Locking and Lifetime Rules

- 进入 `handle_mm_fault()` 时，调用者已持有 VMA lock 或 `mmap_lock`。
- `filemap_fault()`、`__do_fault()` 等路径可能因 I/O 或 folio lock 释放 `mmap_lock` 并返回 `VM_FAULT_RETRY`。
- PTE 修改在 `pte_offset_map_lock()` 之后进行。
- 若 fault 路径释放了 `mmap_lock`，上层必须重新查 VMA 并重试。

## 6. Important Invariants

- fault 必须先通过 VMA 权限检查，再安装映射。
- file-backed 超 `i_size` 不能静默分配匿名页，必须保持 `SIGBUS` 语义。
- COW 和 shared write fault 不能走错分支。
- `VM_FAULT_RETRY` 不与 `VM_FAULT_ERROR` 同时返回，`filemap_fault()` 有明确约束。

## 7. Linux Compatibility Requirements

- `major/minor` fault 语义与 `VM_FAULT_MAJOR/RETRY` 兼容。
- `PROT_NONE`、`FOLL_FORCE`、instruction fault、write fault 分类必须保留。
- `SIGSEGV` 与 `SIGBUS` 的区分不可简化错。

## 8. Simplification Opportunities

- 第一阶段可不做 hugetlb/THP、userfaultfd、memcg。
- 但必须保留 fault 返回码分层设计，否则以后接入 file mapping 和 COW 会很难。

## 9. Test Scenarios

- 匿名读 fault 分配零页映射。
- 私有文件映射读 fault 触发 file in。
- 超文件末尾 fault 返回 `SIGBUS`。
- 权限 fault 返回 `SIGSEGV`。

## 10. Source Index

- `mm/memory.c:handle_mm_fault`
- `mm/memory.c:__handle_mm_fault`
- `mm/memory.c:handle_pte_fault`
- `mm/memory.c:do_fault`
- `mm/filemap.c:filemap_fault`
- `Documentation/mm/page_tables.rst`
