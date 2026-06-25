# Open Questions

## 1. Scope Boundary Questions

- 第一阶段知识库明确排除了 reclaim/swap/memcg/NUMA/THP，但 `mm/memory.c` 与 `mm/vma.c` 中仍有大量条件编译分支。后续 skill 需要明确哪些字段/分支在 x-kernel 第一阶段直接裁掉，哪些只是在接口上占位。
- 是否需要单独追加一个“Linux MM feature matrix”文档，把 `userfaultfd/pkeys/KSM/secretmem/DAX/hugetlb` 对核心路径的侵入点列出，供后续裁剪时查表。

## 2. Data Structure Questions

- Maple Tree 是不是必须被后续 x-kernel skill 学习，还是只需要抽象成“支持区间查找 + split/merge + predecessor/next”的 VMA 容器？
- `anon_vma` 的最小可行语义集合是什么？只支持 fork+COW 时是否可以先做比 Linux 更窄的血缘模型？

## 3. Locking Questions

- per-VMA lock 是否值得进入 x-kernel 第一阶段知识模型，还是先以 `mmap_lock + PTL + rmap lock` 为主，后续再补充优化版本？
- file-backed truncate / invalidate 与 page fault 的锁序，是否需要单独出一个补充文档做精细化整理？

## 4. Compatibility Questions

- Linux `msync(MS_ASYNC)` 的现代 no-op 行为，x-kernel 是否要兼容到这个细节，还是只兼容 `MS_SYNC` 主路径？
- `READ_IMPLIES_EXEC` 这种 personality 历史兼容项，后续 skill 是否应标为“默认不支持，除非兼容层要求”？

## 5. Validation Questions

- 是否需要为后续 skill 再生成一份“按语义分组的 syscall-level 测试清单”，例如 fork+COW、partial munmap、file tail SIGBUS、stack growth、mlock limits？
- 是否需要把每份知识文档里的测试场景统一映射到现有 Linux 用户态测试套件或 x-kernel 自定义 harness 约定？

## 6. Source Index

- `Documentation/mm/process_addrs.rst`
- `Documentation/mm/page_tables.rst`
- `mm/mmap.c`
- `mm/vma.c`
- `mm/memory.c`
- `mm/filemap.c`
- `mm/mprotect.c`
- `mm/mlock.c`
- `mm/msync.c`
- `mm/madvise.c`
