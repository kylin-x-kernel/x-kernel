# kuaccess — 设计文档

## 定位

`kuaccess` 负责内核侧用户内存访问胶水：

- 将 `osvm` 的通用虚拟内存访问入口接到当前线程地址空间；
- 处理“内核在访问用户地址时允许页错误回填”的 trap 路径；
- 提供少量高频的用户态字符串装载辅助函数；
- 提供用户态 32-bit 原子 load 与 cmpxchg 原语（供 futex 等路径使用）。

调用方包括 syscall 实现、用户线程 runtime，以及依赖 `osvm` 指针包装的其他 crate。

## 范围

当前范围包括：

- `src/lib.rs`

## 架构

```text
syscall / runtime
      |
      v
  kuaccess
   |     \
   |      \-- vm_load_string* -> osvm load helpers
   |      \-- atomic_load/cmpxchg_u32 -> architecture user atomics
   |
   \-- VirtMemIo(Vm) -> user_copy + current thread address space
```

## 调用约束 / 执行上下文

- 必须运行在存在 current task 的上下文中。
- `access_user_memory()` 要求 current task 可解析为线程。
- 用户字符串装载会触发用户地址访问，因此允许睡眠/缺页处理。
- trap handler 依赖当前线程的 `accessing_user_memory` 标志，只在该窗口内接管页错误。

## 算法流程

### 用户内存访问

1. 调用方通过 `osvm` 指针包装或 `vm_load_string*()` 发起访问。
2. `Vm` 先做用户地址范围检查。
3. `access_user_memory()` 在当前线程上打开“正在访问用户内存”标志。
4. 底层 `user_copy` 执行读写；若发生页错误，trap handler 转交当前进程地址空间处理。
5. `kuaccess` 消费 `MmSpace::handle_page_fault()` 返回的 typed fault outcome：
   `Resolved` / retry-class outcome 让 fault 指令重试；unmapped、permission、bus、
   OOM、no-progress 和 generic failure 都返回 false，交给架构 exception-table
   fixup 使 `user_copy` / `user_atomic_load_u32` /
   `user_atomic_cmpxchg_u32` 返回失败。
6. 访问结束后恢复线程标志，并将失败映射为 `MemError` / `KError`。

### 用户态原子访问

1. 校验 4 字节对齐与用户地址范围。
2. faultable 形式在 `access_user_memory()` 窗口内调用架构原语，允许 MM 处理
   可恢复缺页；nofault 形式直接依赖 exception-table fixup 返回失败。
3. load 使用自然对齐的只读 32-bit load；cmpxchg 的 exclusive/atomic 序列在
   `IrqSave` 保护下执行。
4. 成功时返回 observed value 或 `(exchanged, observed)`；fault 映射为
   `MemError::NoAccess`。

这些原语供 futex / robust-list 等路径对用户态 futex word 做无 TOCTOU 的读取
或更新。只读 load 不要求映射可写，也不会触发 COW。

### 字符串装载

1. 从用户地址读取字节向量或 NUL 终止字节流。
2. 做 UTF-8 校验。
3. 失败返回 `IllegalBytes`。

## 并发模型

- `Vm` 和 cmpxchg 的 exclusive/atomic 序列使用 `IrqSave`；普通原子 load
  不需要关闭中断。
- 不维护全局共享状态；真正的并发控制由线程状态和地址空间锁负责。

## 设计决策

- 用户字符串装载留在 `kuaccess`，而不是 syscall crate，因为它属于“如何安全访问用户内存”这一职责，不属于某个单独 syscall 族。
- 只保留字符串级 helper，不在这里扩张为新的通用用户态参数解析层。
- trap handler 的外部 ABI 仍是 `bool`，但这个 bool 现在只是架构 trap 分发的适配结果；
  MM 语义来自 `PageFaultOutcome`，避免 `kuaccess` 重新定义缺页分类。
