# Linux MM Design Knowledge

这组文档不是 Linux MM 入门教程，而是给后续 `x-kernel` memory design skill 用的工程参考材料。关注点是：

1. Linux 为什么这样设计。
2. 哪些用户态语义必须兼容。
3. 哪些结构体和关键路径承担了职责。
4. 哪些锁、生命周期和不变量决定实现边界。
5. 哪些机制在新内核第一阶段可以裁剪。

## Document Order

- `00-linux-mm-map.md`: 整体地图，先建立子系统轮廓。
- `01-address-space-mm-struct.md`: 地址空间顶层对象。
- `02-vma-design.md`: VMA 作为段对象的职责与变形规则。
- `03-mmap-munmap-mprotect.md`: 布局与权限变更接口。
- `04-page-table-design.md`: 页表层次、folding 与 TLB teardown。
- `05-page-fault-path.md`: 统一 fault 状态机。
- `06-anonymous-memory.md`: 匿名内存与 lazy allocation。
- `07-file-backed-mmap.md`: 文件映射与 page cache fault。
- `08-cow-design.md`: fork/COW 路径。
- `09-brk-stack-heap.md`: heap 与 stack 的特殊增长语义。
- `10-madvise-msync-mlock.md`: 非布局类 VM 控制接口。
- `99-open-questions.md`: 后续 skill 继续抽取时需要回答的问题。

## How To Use In A Future x-kernel Skill

- 先读 `00`，确定当前设计任务落在哪条 Linux 路径上。
- 再按主题读对应文档，只抽取“必须兼容的用户语义”和“真正决定行为的数据结构/函数”。
- 对每个机制，区分三层：
  - 必须兼容的 Linux user-visible semantics
  - Linux 为了完整生态而引入的内部复杂性
  - x-kernel 第一阶段可裁剪的实现点
- 设计新机制时，优先用这些文档中的 `Important Invariants` 和 `Compatibility Requirements` 作为约束，而不是照抄 Linux 结构体或函数数量。

## How It Fits The Multi-Agent Workflow

这套知识库现在默认作为：

- `docs/ai/skills/xkernel-mm-design-workflow/agents/linux-mm-expert.md`

对应角色的知识底座。

在三 Agent 设计流程里，它的职责是：

- 为 Linux MM Expert 提供 source-grounded semantic baseline；
- 为 Arbiter 提供 Linux compatibility 的核对依据；
- 约束 x-kernel 设计者不要跳过 Linux 语义直接拍脑袋定结构。

它不直接输出 x-kernel 方案；
它的角色是“给出 Linux 事实、语义和可裁剪边界”。

## Source Basis

本知识库只基于本地 Linux 源码树分析，主要来源：

- `~/code/linux-stable/mm/`
- `~/code/linux-stable/include/linux/mm*.h`
- `~/code/linux-stable/include/linux/mmap_lock.h`
- `~/code/linux-stable/include/asm-generic/pgtable*.h`
- `~/code/linux-stable/include/asm-generic/tlb.h`
- `~/code/linux-stable/Documentation/mm/`

## Source Snapshot And Use Rule

默认 Linux 源码树：

- `~/code/linux-stable`

当前已确认的默认版本：

- Linux `v7.0`
- `Makefile`: `VERSION = 7`, `PATCHLEVEL = 0`, `SUBLEVEL = 0`
- git tag/describe: `v7.0`
- observed commit: `028ef9c96e96`

每次被 `xkernel-mm-design-workflow` 使用前，agent 必须先确认 Linux 源码树：

1. 默认检查 `~/code/linux-stable` 是否存在。
2. 如果默认路径不存在，先询问用户提供 Linux source tree path。
3. 如果路径存在，读取 `Makefile` 或 git metadata，记录本次使用的 Linux
   version。
4. 如果版本不是默认 `v7.0`，或任务依赖版本敏感语义，先询问用户是否继续使用
   当前 tree。
5. 对争议性或兼容性关键结论，必须回查本地 Linux 源码，不只依赖本文档摘要。

## Current Scope

已覆盖：

- 用户态虚拟地址空间
- VMA
- mmap/munmap/mprotect
- 页表 walk 与 fault
- 匿名内存
- 文件映射
- fork/COW
- brk/stack
- madvise/msync/mlock

明确未覆盖：

- reclaim
- swap
- memcg
- NUMA
- THP

## Expected Next Step

后续如果要继续强化 skill，建议先补：

- 基于这些文档提炼一个“x-kernel 第一阶段兼容清单”
- 单独拆一份 “fault/truncate/rmap/lock order” 深入文档
- 把每个主题的 `Test Scenarios` 整理成可执行测试需求表
