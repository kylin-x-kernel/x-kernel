# mmap munmap mprotect

## 1. Design Purpose

这组接口负责定义和重塑用户地址空间。Linux 把“选址、资源检查、VMA 形状变化、页表清理、TLB flush”拆成多个层次，以兼顾语义完整性和实现复用。

## 2. User-visible Semantics

- `mmap()` 创建映射，可能是 hint，也可能由 `MAP_FIXED*` 强制定位。
- `MAP_FIXED_NOREPLACE` 冲突时返回 `-EEXIST`。
- `munmap()` 删除区间；允许跨多个 VMA；hole 不报错但参数非法会报错。
- `mprotect()` 改权限；部分区间修改会 split VMA；若请求超出现有映射区间则失败。

## 3. Core Data Structures

- `struct mmap_state` / `struct vm_area_desc`
  - 文件: `mm/vma.c`
  - 作用: `mmap_region()` 的内部组装状态。
- `struct vma_munmap_struct`
  - 文件: `mm/vma.c`
  - 作用: 收集即将 detach/unmap 的 VMA 区间。
- `struct mmu_gather`
  - 文件: `include/asm-generic/tlb.h`
  - 作用: `mprotect`/`munmap`/`exit_mmap` 的批量 TLB 与页表释放。

## 4. Key Code Paths

```text
mmap
  -> SYSCALL_DEFINE6(mmap_pgoff)
  -> ksys_mmap_pgoff()
  -> vm_mmap_pgoff()
  -> do_mmap()
  -> __get_unmapped_area()
  -> mmap_region()
  -> __mmap_region()
  -> vma_merge_new_range() / __mmap_new_vma()

munmap
  -> SYSCALL_DEFINE2(munmap)
  -> __vm_munmap()
  -> do_munmap()
  -> do_vmi_munmap()
  -> do_vmi_align_munmap()
  -> unmap_region()
  -> free_pgtables()

mprotect
  -> SYSCALL_DEFINE3(mprotect)
  -> do_mprotect_pkey()
  -> mprotect_fixup()
  -> vma_modify_flags()
  -> change_protection()
```

- `do_mmap()` in `mm/mmap.c`
  - 检查: `len`, overflow, `max_map_count`, `MAP_FIXED_NOREPLACE`, `MAP_LOCKED`, `mlock_future_ok()`
  - 权限转换: `calc_vm_prot_bits()`, `calc_vm_flag_bits()`
  - 选址: `__get_unmapped_area()`
- `mmap_region()` / `__mmap_region()` in `mm/vma.c`
  - 处理覆盖旧 VMA、commit accounting、调用 `file->f_op->mmap` 或 `shmem_zero_setup()`。
- `do_vmi_munmap()` in `mm/vma.c`
  - 先 gather overlapping VMA，再 clear tree，再完成 unmap。
- `mprotect_fixup()` in `mm/mprotect.c`
  - 先处理 commit/accounting，再 `vma_modify_flags()`，再 `change_protection()`。

## 5. Locking and Lifetime Rules

- `mmap()`/`munmap()`/`mprotect()` 都以 `mmap_write_lock()` 为主。
- `mprotect_fixup()` 修改 `vm_flags` 前调用 `vma_start_write(vma)`。
- `munmap` 在 free page tables 之前，会先把 VMA 从 maple tree/rmap 可见路径上摘掉。
- `change_protection()` 和 `free_pgtables()` 通过 `mmu_gather` 管理 TLB flush。

## 6. Important Invariants

- `do_mmap()` 返回的地址不保证是新 VMA 起点，因为可能 merge 到旧 VMA。
- `munmap` 必须允许 partial unmap，因此 split 是正常路径。
- `mprotect` 不得赋予超出 `VM_MAYREAD/WRITE/EXEC` 的权限。
- 改权限与 unmap 之后，页表/TLB 状态必须和新 VMA 语义一致。

## 7. Linux Compatibility Requirements

- `READ_IMPLIES_EXEC` 行为保留。
- `MAP_FIXED_NOREPLACE`, `MAP_LOCKED`, `PROT_GROWSDOWN/GROWSUP`, `pkey_mprotect` 的基本错误码要兼容。
- `MSYNC`/fault/truncate 观察到的 VMA 权限变化必须与 `mprotect` 一致。

## 8. Simplification Opportunities

- 第一阶段可不支持 `pkey_mprotect`。
- 可不支持 `userfaultfd` 相关拆装配合。
- 可不支持全部 legacy `remap_file_pages()` 兼容路径。

## 9. Test Scenarios

- `MAP_FIXED_NOREPLACE` 与现有映射冲突返回 `EEXIST`。
- `munmap` 中间一段后，前后保留且地址区间失效。
- `mprotect` 把私有只读映射改可写后，后续写 fault 正常触发 COW。

## 10. Source Index

- `mm/mmap.c:do_mmap`
- `mm/mmap.c:ksys_mmap_pgoff`
- `mm/mmap.c:do_munmap`
- `mm/vma.c:mmap_region`
- `mm/vma.c:do_vmi_munmap`
- `mm/mprotect.c:do_mprotect_pkey`
- `mm/mprotect.c:mprotect_fixup`
