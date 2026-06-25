# brk Stack Heap

## 1. Design Purpose

Linux 仍保留传统数据段增长接口 `brk()`，同时支持按 fault 自动扩展栈。两者都是“特殊匿名 VMA 增长”问题，但约束不同。

## 2. User-visible Semantics

- `brk()` 扩展/收缩进程 heap；失败时返回旧 `brk`。
- 栈 VMA 在访问 guard 范围内地址时可向下或向上扩展，取决于架构 `CONFIG_STACK_GROWSUP`。
- 栈扩展受 `stack_guard_gap` 约束，防止与相邻 VMA 靠得过近。

## 3. Core Data Structures

- `mm_struct::start_brk`, `brk`, `start_stack`
  - 文件: `include/linux/mm_types.h`
  - 作用: 记录 heap 与 stack 逻辑边界。
- `VM_GROWSDOWN` / `VM_GROWSUP`
  - 文件: `include/linux/mm.h`
  - 作用: 标识可扩展栈 VMA。
- `stack_guard_gap`
  - 文件: `mm/mmap.c`
  - 作用: 栈与其他映射之间的保护间隙。

## 4. Key Code Paths

```text
brk syscall
  -> SYSCALL_DEFINE1(brk)
  -> check_data_rlimit()
  -> check_brk_limits()
  -> do_brk_flags()

stack growth by fault
  -> find_extend_vma_locked()
  -> expand_stack_locked()
  -> expand_downwards()/expand_upwards()

legacy fault helper
  -> expand_stack()
```

- `SYSCALL_DEFINE1(brk)` in `mm/mmap.c`
  - 收缩: `do_vmi_align_munmap()`
  - 扩展: 校验 `RLIMIT_DATA`、`check_brk_limits()`、`stack_guard_gap`，然后 `do_brk_flags()`
- `do_brk_flags()` in `mm/vma.c`
  - 本质是“匿名可写私有 VMA 的扩展/新建”，并可能与前 VMA merge。
- `find_extend_vma_locked()` / `expand_stack()` in `mm/mmap.c`
  - page fault 时尝试把相邻栈 VMA 扩展到 fault 地址。

## 5. Locking and Lifetime Rules

- `brk()` 走 `mmap_write_lock()`。
- `expand_stack()` 是兼容接口: 先丢 read lock，再抢 write lock，扩展成功后 downgrade 回 read。
- 若栈 VMA 带 `VM_LOCKED`，扩展后会 `populate_vma_page_range()`。

## 6. Important Invariants

- `brk` 扩展不能跨过相邻 VMA 或侵入 stack guard gap。
- shrinking `brk` 总是允许，但必须只 unmap 原 heap VMA 范围。
- 非 `VM_GROWSDOWN/GROWSUP` VMA 不允许隐式扩展。

## 7. Linux Compatibility Requirements

- `brk()` 返回值和传统 Linux ABI 一致。
- `RLIMIT_DATA` 与 `stack_guard_gap` 行为要保留。
- 栈按 fault 增长而不是预分配整段，这点会影响用户程序可观测行为。

## 8. Simplification Opportunities

- 第一阶段可只支持 grow-down 栈。
- 可先固定 guard gap，不实现命令行调参。
- 可把 heap 仅建模为匿名私有 VMA 的特殊增长接口。

## 9. Test Scenarios

- `brk()` 扩展后可写入新 heap。
- `brk()` 收缩后旧地址 fault。
- 深递归/手动触栈导致 stack 自动扩展。
- 栈靠近别的映射时扩展失败。

## 10. Source Index

- `mm/mmap.c:SYSCALL_DEFINE1(brk)`
- `mm/mmap.c:check_brk_limits`
- `mm/vma.c:do_brk_flags`
- `mm/mmap.c:find_extend_vma_locked`
- `mm/mmap.c:expand_stack`
