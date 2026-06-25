# Page Table Design

## 1. Design Purpose

Linux MM 用统一五级软件页表接口屏蔽不同硬件层级，同时让 fault、unmap、fork、TLB shootdown 代码尽量架构无关。

## 2. User-visible Semantics

- 用户看见的是页粒度权限和 fault 行为，而不是具体页表层数。
- huge mapping、权限变化、`SIGSEGV/SIGBUS` 都通过页表状态体现，但层级细节对用户透明。

## 3. Core Data Structures

- `pgd_t/p4d_t/pud_t/pmd_t/pte_t`
  - 文件: `Documentation/mm/page_tables.rst`, `include/asm-generic/pgtable-*.h`
  - 设计意图: 统一遍历接口；层级不足时 folding。
- `mm->pgd`
  - 文件: `include/linux/mm_types.h`
  - 生命周期: 地址空间存在期间有效，`exit_mmap()` 结束后不再可用。
- `page_table_lock`, `pte_lock`, `pmd_lock`
  - 文件: `include/linux/mm.h`
  - 设计意图: 多 CPU 下串行化页表项安装与拆除。
- `struct mmu_gather`
  - 文件: `include/asm-generic/tlb.h`
  - 生命周期: 一次 unmap/protection teardown 会话内临时有效。

## 4. Key Code Paths

```text
fault walk
  -> pgd_offset()
  -> p4d_alloc()
  -> pud_alloc()
  -> pmd_alloc()
  -> handle_pte_fault()

pte install
  -> pte_alloc()
  -> pte_offset_map_lock()
  -> set_pte_at() / set_ptes()
  -> update_mmu_cache_range()

unmap
  -> zap_page_range_single()
  -> ptep_get_and_clear / clear path
  -> tlb_gather_mmu()
  -> tlb_finish_mmu()
```

- `__handle_mm_fault()` in `mm/memory.c`
  - 逐级分配/获取 `p4d/pud/pmd`，必要时直接处理 huge pmd/pud，否则落到 PTE。
- `pte_alloc`, `pte_offset_map_lock`, `pmd_lock` in `include/linux/mm.h`
  - 提供统一锁与页表页分配包装。
- `Documentation/mm/page_tables.rst`
  - 解释为什么 Linux 坚持统一五级接口。

## 5. Locking and Lifetime Rules

- 安装 PTE 前必须保持 VMA 稳定，通常靠 `mmap_lock` 或 VMA lock。
- 修改具体 PTE/PMD 时要持有对应 PTL。
- 释放页表的正确顺序见 `include/asm-generic/tlb.h`:
  1. unhook page
  2. invalidate TLB
  3. free page/table

## 6. Important Invariants

- 折叠层级不能改变上层 MM 通用代码的调用形态。
- 在页表未存在时，higher-level `*_alloc()` 可返回 OOM。
- 同一地址同时只能有一条一致的最终映射解释。
- TLB flush 之前不能复用被旧映射指向的页。

## 7. Linux Compatibility Requirements

- Linux 风格的 lazy page table allocation 必须保留。
- `PROT_NONE`、read-only COW、shared writable dirty tracking 最终都必须落成正确 PTE 位语义。

## 8. Simplification Opportunities

- 新内核第一阶段可仅支持基础 4KiB PTE 路径，不做 THP/hugetlb。
- 仍建议保留“统一五级接口 + 可 folding”的软件抽象，避免以后重构。
- 可先只做单一普通页大小的 TLB flush 路径。

## 9. Test Scenarios

- 访问新匿名页只分配需要的页表层级。
- 连续未映射大 hole 不应分配下级页表。
- unmap 后旧物理页在 TLB flush 前后不可被错误复用。

## 10. Source Index

- `Documentation/mm/page_tables.rst`
- `include/asm-generic/pgtable-nop4d.h`
- `include/linux/mm.h`
- `include/asm-generic/tlb.h`
- `mm/memory.c:__handle_mm_fault`
