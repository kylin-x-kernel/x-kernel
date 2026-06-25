# File-backed mmap

## 1. Design Purpose

file-backed mmap 让文件页通过 page cache 暴露到用户虚拟地址空间。Linux 把“VMA 元数据”和“具体页内容准备”解耦：前者在 mmap 建立，后者在 fault 时经 `vm_ops->fault`/`filemap_fault()` 取入。

## 2. User-visible Semantics

- `MAP_SHARED` 让多个映射共享底层页缓存内容。
- `MAP_PRIVATE` 读共享文件内容，但后续写 fault 进入匿名 COW。
- 访问超文件 `i_size` 的页通常触发 `SIGBUS`。
- `MS_SYNC` 针对 `VM_SHARED` 文件映射能触发同步写回语义。

## 3. Core Data Structures

- `vm_area_struct::vm_file`, `vm_pgoff`, `vm_ops`, `shared`
  - 文件: `include/linux/mm_types.h`
  - 作用: 描述文件后端和在 `address_space->i_mmap` 中的位置。
- `struct address_space`
  - 文件: `include/linux/fs.h`
  - 作用: page cache 宿主，`i_mmap` 追踪所有相关 VMA。
- `generic_file_vm_ops`
  - 文件: `mm/filemap.c`
  - 关键回调: `.fault = filemap_fault`, `.map_pages`, `.page_mkwrite`.

## 4. Key Code Paths

```text
file mmap setup
  -> do_mmap()
  -> mmap_region()
  -> __mmap_new_file_vma()
  -> file->f_op->mmap()
  -> generic_file_mmap()
  -> vma->vm_ops = &generic_file_vm_ops

file read fault
  -> handle_pte_fault()
  -> do_fault()
  -> __do_fault()
  -> filemap_fault()
  -> finish_fault()
```

- `__mmap_new_file_vma()` in `mm/vma.c`
  - 调 `mmap_file()`/`file->f_op->mmap` 初始化 VMA；失败时 `unmap_region()` 回滚驱动部分映射。
- `generic_file_mmap()` in `mm/filemap.c`
  - 常规文件系统设置 `vm_ops`。
- `filemap_fault()` in `mm/filemap.c`
  - 流程: 检查 `i_size` -> page cache 查 folio -> 可能 readahead -> 可能读盘 -> 返回 locked page 或 `VM_FAULT_RETRY`.
- `finish_fault()` in `mm/memory.c`
  - 最终把 page cache folio 安装到 PTE/PMD。

## 5. Locking and Lifetime Rules

- file-backed VMA 挂入/移出 `i_mmap` 需持 `mapping->i_mmap_rwsem`。
- `filemap_fault()` 可能持 `invalidate_lock_shared(mapping)`，防止 truncation/invalidations 竞态。
- folio lock 与 `mmap_lock` 之间可能导致 drop-and-retry；`lock_folio_maybe_drop_mmap()` 是关键点。

## 6. Important Invariants

- `i_size` 外不得映射普通文件页，必须保留 `SIGBUS` 语义。
- shared writable mapping 需要脏页跟踪；`vma_wants_writenotify()` 可能故意让 PTE 初始只读，靠写 fault 标脏。
- `MAP_PRIVATE` 文件页一旦 COW，对应页可转入 anon rmap，但 VMA 仍可保留 `vm_file`。

## 7. Linux Compatibility Requirements

- page cache 共享语义必须保留。
- truncation 与 mmap 并发时的 `SIGBUS`/重试行为要兼容。
- `MS_SYNC` 只对 `VM_SHARED` 且有文件后端的 VMA 有实际同步路径。

## 8. Simplification Opportunities

- 第一阶段可只支持普通页缓存文件，不做 DAX、hugetlb、复杂 `map_pages`。
- 可先不做 fault-around/readahead 优化，但要保留基本 `filemap_fault()` 语义。

## 9. Test Scenarios

- 两个进程 `MAP_SHARED` 同一文件，一方写另一方可见。
- `MAP_PRIVATE` 同一文件，一方写不影响另一方。
- fault 到文件末尾外页得到 `SIGBUS`。

## 10. Source Index

- `mm/vma.c:__mmap_new_file_vma`
- `mm/filemap.c:generic_file_mmap`
- `mm/filemap.c:filemap_fault`
- `mm/memory.c:finish_fault`
- `include/linux/fs.h`
